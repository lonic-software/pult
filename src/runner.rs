use std::path::Path;
use std::process::Command;

#[cfg(not(unix))]
use anyhow::Context;
use anyhow::Result;

use crate::journal::Journal;

/// How a run relates to the run journal (see journal.rs / the run-journal
/// spec). Decided by the caller (exec.rs), not here:
///
/// - `Off` — no journal (disabled, failed to open, `--print` never gets
///   here, ephemeral `pult x`). Byte-for-byte the pre-journal behavior:
///   inherited stdio, events channel only when stderr is a terminal.
/// - `MetaOnly` — interactive commands: the terminal owns the child's
///   stdio (a working PTY is the whole contract of `interactive: true`),
///   so pult can't see output to journal it. Same inherited-stdio
///   execution as `Off`; the journal records meta + the final exit only.
/// - `Full` — non-interactive commands: the child's stdout/stderr become
///   pipes pult tees through itself (bytes forwarded unchanged, each line
///   journaled), and pult claims the `PULT_EVENTS` channel whenever it's
///   free — even with no terminal attached, which is new: the journal is
///   now a consumer of step/progress/status regardless of who's watching.
///   The tradeoff, documented rather than hidden: the child sees a pipe,
///   not a tty, so tools that sniff `isatty` drop color/live-progress
///   output. `interactive: true` remains the escape hatch for anything
///   that genuinely needs the terminal.
pub enum Journaling {
    Off,
    MetaOnly(Journal),
    Full(Journal),
}

/// Run a plain command line via `sh -c`. Interactive sessions (e.g. `aws
/// ecs execute-command`) reach this with inherited stdio and get a working
/// PTY — the reason the guided flow avoids a full-screen render loop
/// (spec §9); journaled non-interactive runs are teed instead (see
/// [`Journaling`]).
///
/// `display` is the line shown in the banner and any error — the caller passes
/// a secret-redacted rendering, so `cmdline` itself is never written anywhere.
pub fn run_sh(cmdline: &str, display: &str, dir: &Path, journaling: Journaling) -> Result<i32> {
    eprintln!("└  running: {display}");
    execute(
        Command::new("sh").arg("-c").arg(cmdline).current_dir(dir),
        display,
        journaling,
    )
}

/// Run a compiled step script via bash (the composed scripts rely on
/// `set -o pipefail`). Same journaling contract as [`run_sh`].
pub fn run_bash(script: &str, label: &str, dir: &Path, journaling: Journaling) -> Result<i32> {
    eprintln!("└  running: {label} (compiled steps — `--print` shows the script)");
    execute(
        Command::new("bash").arg("-c").arg(script).current_dir(dir),
        label,
        journaling,
    )
}

fn execute(command: &mut Command, what: &str, journaling: Journaling) -> Result<i32> {
    match journaling {
        Journaling::Off => execute_inherited(command, what),
        Journaling::MetaOnly(journal) => {
            // Journaling must never change how the run behaves, and must
            // record spawn-level failure (exit code null) as well as a
            // normal exit before the error propagates.
            match execute_inherited(command, what) {
                Ok(code) => {
                    journal.finish(Some(code), false);
                    Ok(code)
                }
                Err(e) => {
                    journal.finish(None, false);
                    Err(e)
                }
            }
        }
        Journaling::Full(journal) => match journaled::execute(command, what, &journal) {
            Ok((code, stopped)) => {
                journal.finish(Some(code), stopped);
                Ok(code)
            }
            Err(e) => {
                journal.finish(None, false);
                Err(e)
            }
        },
    }
}

/// The pre-journal execution path, unchanged: inherited stdio, and (unix)
/// a `PULT_EVENTS` transport only when stderr is a terminal.
#[cfg(unix)]
fn execute_inherited(command: &mut Command, what: &str) -> Result<i32> {
    events_unix::execute(command, what)
}

#[cfg(not(unix))]
fn execute_inherited(command: &mut Command, what: &str) -> Result<i32> {
    let status = command
        .status()
        .with_context(|| format!("failed to run `{what}`"))?;
    Ok(exit_code(status))
}

/// Journaled (teed) execution — the `Journaling::Full` path. Returns
/// `(exit_code, stopped)`; `stopped` = a SIGINT/SIGTERM was observed by
/// pult while the child ran (unix), so the journal can distinguish "was
/// stopped" from "failed on its own".
mod journaled {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::process::{Command, Stdio};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use anyhow::{Context, Result};

    use crate::journal::Journal;

    pub(super) fn execute(
        command: &mut Command,
        what: &str,
        journal: &Journal,
    ) -> Result<(i32, bool)> {
        command.stdout(Stdio::piped()).stderr(Stdio::piped());

        // Unix: watch for stop signals and claim the events channel when
        // free. Both are no-ops elsewhere (Windows journals lines + exit
        // only, and stop detection falls to reader-side crash detection —
        // see the run-journal spec's Windows notes).
        #[cfg(unix)]
        super::events_unix::install_stop_flag();
        #[cfg(unix)]
        let events = super::events_unix::claim_events_channel(command, journal.clone());

        let spawn_result = command.spawn();
        #[cfg(unix)]
        let events = events.after_spawn();

        let mut child = spawn_result.with_context(|| format!("failed to run `{what}`"))?;

        // Tee both streams: bytes forwarded to our own stdout/stderr
        // unchanged (flushed per line so a piping parent streams live),
        // each line journaled with its trailing newline stripped.
        let out_done = Arc::new(AtomicBool::new(false));
        let err_done = Arc::new(AtomicBool::new(false));
        if let Some(stdout) = child.stdout.take() {
            let journal = journal.clone();
            let done = Arc::clone(&out_done);
            std::thread::spawn(move || tee(stdout, std::io::stdout(), journal, "stdout", done));
        } else {
            out_done.store(true, Ordering::Relaxed);
        }
        if let Some(stderr) = child.stderr.take() {
            let journal = journal.clone();
            let done = Arc::clone(&err_done);
            std::thread::spawn(move || tee(stderr, std::io::stderr(), journal, "stderr", done));
        } else {
            err_done.store(true, Ordering::Relaxed);
        }

        let status = child
            .wait()
            .with_context(|| format!("failed to run `{what}`"))?;

        // The pipes close when the child *and every descendant that
        // inherited them* are done writing — usually right around `wait`
        // returning. Give the tee threads a short bounded drain, then move
        // on: a daemonized grandchild holding the pipe open must not hang
        // pult (its later output is dropped by the journal's done latch,
        // and its bytes still reach our stdout/stderr until we exit).
        let deadline = Instant::now() + Duration::from_millis(500);
        while !(out_done.load(Ordering::Relaxed) && err_done.load(Ordering::Relaxed))
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }

        #[cfg(unix)]
        events.finish();

        let stopped = {
            #[cfg(unix)]
            {
                super::events_unix::stop_requested()
            }
            #[cfg(not(unix))]
            {
                false
            }
        };
        Ok((super::exit_code(status), stopped))
    }

    /// Copy `reader` to `out` byte-for-byte while journaling each line.
    /// `read_until` (not `lines()`) so carriage returns and partial-line
    /// content pass through to the live stream unmodified; only the
    /// journal's copy is trimmed.
    fn tee(
        reader: impl Read,
        mut out: impl Write,
        journal: Journal,
        stream: &'static str,
        done: Arc<AtomicBool>,
    ) {
        let mut reader = BufReader::new(reader);
        let mut buf: Vec<u8> = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let _ = out.write_all(&buf);
                    let _ = out.flush();
                    let text = String::from_utf8_lossy(&buf);
                    journal.line(stream, text.trim_end_matches(['\n', '\r']));
                }
            }
        }
        done.store(true, Ordering::Relaxed);
    }
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
    use crate::journal::Journal;

    // Rust 2024 requires FFI declarations inside `unsafe extern` blocks. No
    // new dependency: the binary already links the system libc on unix, this
    // just declares the symbols we need instead of pulling in the `libc`
    // crate for them. `fcntl`'s trailing arg is genuinely variadic in C;
    // calling it below with 2 args (F_GETFD) or 3 (F_SETFD) is valid either
    // way.
    unsafe extern "C" {
        fn dup2(oldfd: c_int, newfd: c_int) -> c_int;
        fn fcntl(fd: c_int, cmd: c_int, ...) -> c_int;
        fn signal(signum: c_int, handler: usize) -> usize;
    }

    // Values are POSIX-standard and identical across Linux and macOS
    // (asm-generic/fcntl.h, sys/fcntl.h) — stable enough to hardcode rather
    // than take on `libc` just for a handful of constants.
    const F_GETFD: c_int = 1;
    const F_SETFD: c_int = 2;
    const FD_CLOEXEC: c_int = 1;
    const SIGINT: c_int = 2;
    const SIGTERM: c_int = 15;

    /// Set once a SIGINT/SIGTERM arrives while a journaled child runs —
    /// how `finish` knows to record `stopped: true`. pult surviving the
    /// signal (instead of dying mid-journal) is the entire point: the
    /// child shares our process group, so a Ctrl+C or a desktop-app stop
    /// signals it too; we wait out its death and then write the record.
    static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

    extern "C" fn on_stop_signal(_sig: c_int) {
        STOP_REQUESTED.store(true, Ordering::Relaxed);
    }

    /// Install the stop-flag handlers (journaled runs only — interactive
    /// and unjournaled runs keep default signal disposition, dying with
    /// their child exactly as before). `signal()` on both Linux (glibc)
    /// and macOS gives BSD semantics — syscalls restart — so the blocked
    /// `child.wait()` resumes rather than EINTRs.
    pub(super) fn install_stop_flag() {
        unsafe {
            signal(SIGINT, on_stop_signal as *const () as usize);
            signal(SIGTERM, on_stop_signal as *const () as usize);
        }
    }

    pub(super) fn stop_requested() -> bool {
        STOP_REQUESTED.load(Ordering::Relaxed)
    }

    /// The legacy (unjournaled) execution path, unchanged in behavior:
    /// inherited stdio, and an events transport chosen from three cases.
    pub(super) fn execute(command: &mut Command, what: &str) -> Result<i32> {
        // 1. Passthrough: a parent (e.g. the desktop app, pre-journal) owns
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
        let channel = match wire_up(command) {
            Some(channel) => channel,
            None => return run_plain(command, what),
        };
        let rendered = Arc::new(AtomicBool::new(false));
        let reader_rendered = Arc::clone(&rendered);
        // Read+render on a detached thread: EOF normally arrives once the
        // child (and anything it spawned that inherited fd 3) closes it. If
        // a grandchild keeps fd 3 open, this thread simply keeps blocking in
        // `read` — we never join it, so it can never hang process exit.
        std::thread::spawn(move || {
            let mut renderer = events::Renderer::new();
            read_events(channel.reader, |event| {
                let mut stderr = std::io::stderr();
                if renderer.handle(event, &mut stderr) {
                    reader_rendered.store(true, Ordering::Relaxed);
                }
            })
        });

        let spawn_result = command.spawn();
        // Drop pult's own copy of the write end regardless of spawn outcome
        // — otherwise the reader thread could never see EOF, since one
        // writer (ours) would always still be open.
        drop(channel.writer);

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

    /// The events transport for a *journaled* run — the difference from the
    /// legacy path above: the channel is claimed whenever it's free, not
    /// only when stderr is a terminal, because the journal consumes events
    /// even with nobody watching. OSC rendering still happens only on a
    /// terminal. When a parent already owns a (valid) `PULT_EVENTS`, its
    /// claim wins and the journal simply won't carry step/progress/status
    /// for this run — the spec's documented passthrough tradeoff.
    pub(super) fn claim_events_channel(command: &mut Command, journal: Journal) -> EventsClaim {
        if let Some(val) = std::env::var_os("PULT_EVENTS") {
            let val = val.to_string_lossy().into_owned();
            if is_valid_events_fd(&val) {
                return EventsClaim::Passthrough;
            }
            eprintln!("pult: ignoring invalid PULT_EVENTS={val}");
            command.env_remove("PULT_EVENTS");
        }
        let Some(channel) = wire_up_own_fd(command) else {
            return EventsClaim::None;
        };
        let render = std::io::stderr().is_terminal();
        let rendered = Arc::new(AtomicBool::new(false));
        let reader_rendered = Arc::clone(&rendered);
        let reader = channel.reader;
        std::thread::spawn(move || {
            let mut renderer = events::Renderer::new();
            read_events(reader, |event| {
                journal.event(event);
                if render {
                    let mut stderr = std::io::stderr();
                    if renderer.handle(event, &mut stderr) {
                        reader_rendered.store(true, Ordering::Relaxed);
                    }
                }
            })
        });
        EventsClaim::Claimed {
            writer: Some(channel.writer),
            rendered,
        }
    }

    /// Bookkeeping for `claim_events_channel` across the spawn boundary:
    /// the write end must be dropped right after `spawn` (EOF contract),
    /// and any rendered OSC badge cleared after the run.
    pub(super) enum EventsClaim {
        Passthrough,
        None,
        Claimed {
            writer: Option<std::io::PipeWriter>,
            rendered: Arc<AtomicBool>,
        },
    }

    impl EventsClaim {
        pub(super) fn after_spawn(mut self) -> Self {
            if let EventsClaim::Claimed { writer, .. } = &mut self {
                drop(writer.take());
            }
            self
        }

        pub(super) fn finish(self) {
            if let EventsClaim::Claimed { rendered, .. } = self
                && rendered.load(Ordering::Relaxed)
            {
                events::render_final(&mut std::io::stderr());
            }
        }
    }

    struct Channel {
        reader: std::io::PipeReader,
        writer: std::io::PipeWriter,
    }

    /// Create the events pipe and wire fd 3 into the child (pre_exec dup2 +
    /// `PULT_EVENTS=3`). `None` = pult itself already has fd 3 open: the
    /// invoker deliberately passed a descriptor (e.g. `pult import
    /// 3<seed.txt`) — their playbook wins over our progress channel.
    fn wire_up(command: &mut Command) -> Option<Channel> {
        if unsafe { fcntl(3, F_GETFD) } != -1 {
            return None;
        }

        let (reader, writer) = std::io::pipe().ok()?;
        let write_fd = writer.as_raw_fd();

        // SAFETY: this closure runs in the child after `fork`, before `exec`,
        // in the child's own copy of the fd table — dup2/fcntl here are
        // async-signal-safe and only affect that copy. `write_fd` stays
        // valid until we drop `writer`, which happens only after `spawn`
        // has forked.
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
        Some(Channel { reader, writer })
    }

    /// `wire_up` for journaled runs: the pipe's write end is passed through
    /// on *its own fd number* (`PULT_EVENTS=<n>` — the protocol's "always
    /// read the number from `$PULT_EVENTS`" note is load-bearing here), not
    /// dup2'd onto fd 3. Two reasons fd 3 can't be assumed free anymore:
    /// the journal's own `events.jsonl` handle typically occupies it, and an
    /// invoker-passed descriptor (`pult import 3<seed.txt`) deserves to
    /// survive untouched rather than win a coin toss against the channel —
    /// this way both coexist, and there is no `None`-because-fd-3-is-busy
    /// case at all.
    fn wire_up_own_fd(command: &mut Command) -> Option<Channel> {
        let (reader, writer) = std::io::pipe().ok()?;
        let write_fd = writer.as_raw_fd();

        // SAFETY: runs in the child after `fork`, before `exec`, on the
        // child's own fd table. `std::io::pipe()` creates both ends
        // CLOEXEC; clearing the flag on the write end is all the child
        // needs — the fd number itself is already correct (fork copies the
        // table verbatim, and std's stdio rewiring only touches 0–2).
        unsafe {
            command.pre_exec(move || {
                let flags = fcntl(write_fd, F_GETFD);
                if flags == -1 || fcntl(write_fd, F_SETFD, flags & !FD_CLOEXEC) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        command.env("PULT_EVENTS", write_fd.to_string());
        Some(Channel { reader, writer })
    }

    fn read_events(reader: std::io::PipeReader, mut handle: impl FnMut(&events::Event)) {
        for line in BufReader::new(reader).lines() {
            let Ok(line) = line else { break };
            if let Some(event) = events::parse(&line) {
                handle(&event);
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
