use std::path::Path;
use std::process::Command;

#[cfg(not(unix))]
use anyhow::Context;
use anyhow::Result;

/// Run a plain command line via `sh -c` with inherited stdio, so interactive
/// sessions (e.g. `aws ecs execute-command`) get a working PTY — the reason
/// the guided flow avoids a full-screen render loop (spec §9).
///
/// `display` is the line shown in the banner and any error — the caller passes
/// a secret-redacted rendering, so `cmdline` itself is never written anywhere.
pub fn run_sh(cmdline: &str, display: &str, dir: &Path) -> Result<i32> {
    eprintln!("└  running: {display}");
    execute(
        Command::new("sh").arg("-c").arg(cmdline).current_dir(dir),
        display,
    )
}

/// Run a compiled step script via bash (the composed scripts rely on
/// `set -o pipefail`). Same inherited stdio.
pub fn run_bash(script: &str, label: &str, dir: &Path) -> Result<i32> {
    eprintln!("└  running: {label} (compiled steps — `--print` shows the script)");
    execute(
        Command::new("bash").arg("-c").arg(script).current_dir(dir),
        label,
    )
}

/// Run `command`, choosing a `PULT_EVENTS` transport (unix only — see
/// `events_unix` for the three cases: passthrough, own channel, neither).
#[cfg(unix)]
fn execute(command: &mut Command, what: &str) -> Result<i32> {
    events_unix::execute(command, what)
}

#[cfg(not(unix))]
fn execute(command: &mut Command, what: &str) -> Result<i32> {
    let status = command
        .status()
        .with_context(|| format!("failed to run `{what}`"))?;
    Ok(exit_code(status))
}

#[cfg(unix)]
mod events_unix {
    use std::ffi::c_int;
    use std::io::{BufRead, BufReader, IsTerminal};
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;
    use std::process::Command;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use anyhow::{Context, Result};

    use crate::events;

    // Rust 2024 requires FFI declarations inside `unsafe extern` blocks. No
    // new dependency: the binary already links the system libc on unix, this
    // just declares the symbols we need instead of pulling in the `libc`
    // crate for them. `fcntl`'s trailing arg is genuinely variadic in C;
    // calling it below with 2 args (F_GETFD) or 3 (F_SETFD) is valid either
    // way.
    unsafe extern "C" {
        fn dup2(oldfd: c_int, newfd: c_int) -> c_int;
        fn fcntl(fd: c_int, cmd: c_int, ...) -> c_int;
    }

    // Values are POSIX-standard and identical across Linux and macOS
    // (asm-generic/fcntl.h, sys/fcntl.h) — stable enough to hardcode rather
    // than take on `libc` just for four constants.
    const F_GETFD: c_int = 1;
    const F_SETFD: c_int = 2;
    const FD_CLOEXEC: c_int = 1;

    pub(super) fn execute(command: &mut Command, what: &str) -> Result<i32> {
        // 1. Passthrough: a parent (e.g. a future desktop app) already owns
        // the channel — its env var and fd inherit through on their own.
        // pult must not create its own pipe or translate anything.
        //
        // But only honor a value that could plausibly be an fd: bash's
        // `>&word` redirects to a *file* named `word` when `word` isn't a
        // bare number, so an inherited `PULT_EVENTS=events.log` would make
        // every injected `step` guard truncate a file by that name instead
        // of writing to a channel. If the value doesn't parse as an fd
        // number, strip it before falling through so the child never sees
        // the garbage (the own-channel path below sets its own numeric
        // value, which is always valid).
        if let Some(val) = std::env::var_os("PULT_EVENTS") {
            let val = val.to_string_lossy().into_owned();
            if is_valid_events_fd(&val) {
                return run_plain(command, what);
            }
            eprintln!("pult: ignoring invalid PULT_EVENTS={val}");
            command.env_remove("PULT_EVENTS");
        }

        // 3. Neither: non-tty (CI, pipes) — zero behavior change.
        if !std::io::stderr().is_terminal() {
            return run_plain(command, what);
        }

        // 2. Own channel: stderr is a terminal and no parent owns the fd —
        // pult creates the pipe, renders OSC 9;4 itself.
        run_with_own_channel(command, what)
    }

    /// Whether `s` is a value `PULT_EVENTS` could legitimately hold: a bare
    /// decimal fd number, no sign or surrounding whitespace, small enough to
    /// be a real descriptor. Anything else (a path, `""`, `-1`, ` 3`, a
    /// number too large to be a real fd) is rejected — see the passthrough
    /// comment in `execute` for why this matters under bash's `>&word`.
    pub(super) fn is_valid_events_fd(s: &str) -> bool {
        !s.is_empty()
            && s.bytes().all(|b| b.is_ascii_digit())
            && s.parse::<u32>().is_ok_and(|n| n <= 4096)
    }

    fn run_plain(command: &mut Command, what: &str) -> Result<i32> {
        let status = command
            .status()
            .with_context(|| format!("failed to run `{what}`"))?;
        Ok(super::exit_code(status))
    }

    fn run_with_own_channel(command: &mut Command, what: &str) -> Result<i32> {
        // If pult itself already has fd 3 open, an inherited fd 3 means the
        // invoker deliberately passed a descriptor (e.g. `pult import
        // 3<seed.txt`) — their playbook wins over our progress channel, so
        // run without one rather than clobbering it.
        if unsafe { fcntl(3, F_GETFD) } != -1 {
            return run_plain(command, what);
        }

        let (reader, writer) = std::io::pipe()
            .with_context(|| format!("failed to create events pipe for `{what}`"))?;
        let write_fd = writer.as_raw_fd();

        // SAFETY: this closure runs in the child after `fork`, before `exec`,
        // in the child's own copy of the fd table — dup2/fcntl here are
        // async-signal-safe and only affect that copy. `write_fd` stays
        // valid until we drop `writer` below, which happens only after
        // `spawn` has forked.
        unsafe {
            command.pre_exec(move || {
                if write_fd == 3 {
                    // Trap: if the write end already landed on fd 3 (a lower
                    // fd happened to be free when the pipe was created),
                    // `dup2(3, 3)` is defined as a no-op on Linux/macOS — it
                    // does NOT clear FD_CLOEXEC. `std::io::pipe()` creates
                    // both ends CLOEXEC, so without this branch fd 3 would
                    // close right at `exec`, and the child's `>&3` writes
                    // would silently fail. Clear CLOEXEC directly instead.
                    let flags = fcntl(write_fd, F_GETFD);
                    if flags == -1 || fcntl(write_fd, F_SETFD, flags & !FD_CLOEXEC) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                } else if dup2(write_fd, 3) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        // The child reads its fd number from here — same convention as
        // stdin/stdout/stderr, just a channel of pult's own.
        command.env("PULT_EVENTS", "3");

        // Shared with the reader thread: whether we ever actually rendered
        // an OSC sequence. A command that never emits a single protocol
        // line must produce zero bytes of OSC — including no final
        // clear — so the post-wait finalization below only fires once this
        // is true.
        let rendered = Arc::new(AtomicBool::new(false));
        let reader_rendered = Arc::clone(&rendered);

        // Read+render on a detached thread: EOF normally arrives once the
        // child (and anything it spawned that inherited fd 3) closes it. If
        // a grandchild keeps fd 3 open, this thread simply keeps blocking in
        // `read` — we never join it, so it can never hang process exit.
        std::thread::spawn(move || read_events(reader, reader_rendered));

        let spawn_result = command.spawn();
        // Drop pult's own copy of the write end regardless of spawn outcome
        // — otherwise the reader thread could never see EOF, since one
        // writer (ours) would always still be open.
        drop(writer);

        let mut child = spawn_result.with_context(|| format!("failed to run `{what}`"))?;
        let status = child
            .wait()
            .with_context(|| format!("failed to run `{what}`"))?;
        let code = super::exit_code(status);

        // Always clear, never leave a persistent error badge — but only if
        // we rendered something in the first place (see `rendered` above).
        // A stuck red progress badge from a run that failed for unrelated
        // reasons is worse than no badge, so there is no error state here.
        if rendered.load(Ordering::Relaxed) {
            events::render_final(&mut std::io::stderr());
        }
        Ok(code)
    }

    fn read_events(reader: std::io::PipeReader, rendered: Arc<AtomicBool>) {
        let mut renderer = events::Renderer::new();
        let mut stderr = std::io::stderr();
        for line in BufReader::new(reader).lines() {
            let Ok(line) = line else { break };
            if let Some(event) = events::parse(&line)
                && renderer.handle(&event, &mut stderr)
            {
                rendered.store(true, Ordering::Relaxed);
            }
        }
    }
}

#[cfg(unix)]
fn exit_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .or_else(|| status.signal().map(|s| 128 + s))
        .unwrap_or(1)
}

#[cfg(not(unix))]
fn exit_code(status: std::process::ExitStatus) -> i32 {
    status.code().unwrap_or(1)
}

#[cfg(all(test, unix))]
mod tests {
    use super::events_unix::is_valid_events_fd;

    #[test]
    fn accepts_plain_fd_numbers() {
        assert!(is_valid_events_fd("3"));
        assert!(is_valid_events_fd("0"));
        assert!(is_valid_events_fd("4096"));
    }

    #[test]
    fn rejects_a_file_path() {
        assert!(!is_valid_events_fd("events.log"));
        assert!(!is_valid_events_fd("/tmp/events.log"));
    }

    #[test]
    fn rejects_empty_string() {
        assert!(!is_valid_events_fd(""));
    }

    #[test]
    fn rejects_signs_and_whitespace() {
        assert!(!is_valid_events_fd("+3"));
        assert!(!is_valid_events_fd("-1"));
        assert!(!is_valid_events_fd(" 3"));
        assert!(!is_valid_events_fd("3 "));
    }

    #[test]
    fn rejects_a_number_too_large_to_be_an_fd() {
        assert!(!is_valid_events_fd("4097"));
        assert!(!is_valid_events_fd("99999999999999999999"));
    }
}
