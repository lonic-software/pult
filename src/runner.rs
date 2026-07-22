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
        // free. Both are no-ops on other targets, but this whole function
        // is unreachable there in practice anyway: journaling is off
        // entirely on non-unix (M4, journal.rs's `Journal::start`), so
        // `Journaling::Full` — the only caller of `journaled::execute` — is
        // never constructed off-unix. The `#[cfg(not(unix))]` arms below
        // exist only so this module still compiles for those targets.
        #[cfg(unix)]
        super::events_unix::install_stop_flag();
        #[cfg(unix)]
        let events = super::events_unix::claim_events_channel(command, journal.clone());

        let spawn_result = command.spawn();
        #[cfg(unix)]
        let events = events.after_spawn();
        // Passthrough/None claims have no reader thread of ours, so they
        // count as already-done; Claimed's flips once `read_events` returns
        // (the pipe's EOF) — see `claim_events_channel`.
        let events_done = {
            #[cfg(unix)]
            {
                events.done_flag()
            }
            #[cfg(not(unix))]
            {
                Arc::new(AtomicBool::new(true))
            }
        };

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
        // returning. Give the tee threads AND the events reader thread
        // (M2: a `step 3/3` emitted right before the child exits can race
        // `finish` otherwise) a short bounded drain, then move on: a
        // daemonized grandchild holding a pipe open must not hang pult (its
        // later output is dropped by the journal's done latch, and its
        // bytes still reach our stdout/stderr until we exit).
        let deadline = Instant::now() + Duration::from_millis(500);
        while !(out_done.load(Ordering::Relaxed)
            && err_done.load(Ordering::Relaxed)
            && events_done.load(Ordering::Relaxed))
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
    ///
    /// Journal FIRST, forward second (M3): `out` is a live stream pult
    /// doesn't control the far end of (a piping parent, e.g. `pult deploy |
    /// less`, can leave `write_all` blocked indefinitely). If forwarding ran
    /// first, a blocked consumer would starve the journal of a line the
    /// child already produced — the 500ms drain deadline in `execute` would
    /// pass, `finish` would latch, and that line would never be journaled
    /// even though the run recorded a clean exit. Journaling first means
    /// the record is safe before a blocked `out` can do any damage.
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
                    let text = String::from_utf8_lossy(&buf);
                    journal.line(stream, text.trim_end_matches(['\n', '\r']));
                    let _ = out.write_all(&buf);
                    let _ = out.flush();
                }
            }
        }
        done.store(true, Ordering::Relaxed);
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::journal::{StartInfo, repo_key};
        use indexmap::IndexMap;
        use std::time::Instant;

        // M2 (the events reader thread's done flag, wired through
        // `EventsClaim::done_flag` into `execute`'s three-flag drain wait)
        // has no *deterministic* test here alongside M3's `tee` test below.
        // Unlike `tee`, a standalone function a test can drive with a
        // hand-rolled blocking `Write`, M2's race is between the events
        // reader thread finishing one more `read_events` iteration and
        // `execute`'s `child.wait()` returning — and that race only exists
        // inside `execute` itself, which spawns a real child end to end
        // (`Command::spawn`, pipes, `pre_exec`) rather than exposing a seam
        // a test could drive with a fake clock or a fake child. Forcing the
        // exact interleaving deterministically would mean adding an
        // injectable child-process/clock abstraction to production code for
        // this one test, which the task explicitly rules out.
        //
        // What's below instead (`full_execute_journals_a_late_step_event`)
        // is real, non-mocked coverage: an actual child process emits a
        // step event over the real events channel and exits immediately
        // after, exercised through the genuine `execute` (real pipes, real
        // threads, the real 500ms deadline). It proves the wiring is live
        // in the common case. It is *not* a reliable revert-check, though,
        // and empirically so, not just in theory: this command has no
        // stdout/stderr output of its own, so `out_done`/`err_done` latch
        // almost immediately once the child's pipes close — without
        // `events_done` in the wait condition, the loop can fall through
        // before the events reader thread has caught up, which is exactly
        // this fix's race. Reverting the `events_done` clause and running
        // this test 20x in a loop failed it only 2/20 times: real, but too
        // rare to trust as a red/green gate. Left in as a live sanity check
        // (and a faint tripwire), not a substitute for a deterministic one.

        /// A `Write` whose every call blocks the calling thread forever —
        /// stands in for a piping parent that never drains its end (`pult
        /// deploy | less`, paused).
        struct BlockingWriter;

        impl Write for BlockingWriter {
            fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
                loop {
                    std::thread::park();
                }
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        /// M3: `tee` must journal a line before attempting to forward it.
        /// `BlockingWriter` never returns from `write_all`, so if `tee`
        /// still journaled the line first, it shows up in `events.jsonl`
        /// even though the forward never completes (and never will — the
        /// spawned thread below is deliberately leaked, parked forever,
        /// same as a real blocked consumer would leave it).
        ///
        /// Goes red if the journal-then-forward order in `tee` is reverted
        /// to forward-then-journal: the blocked `write_all` would run
        /// first, `journal.line` would never be reached, and the poll below
        /// would time out with nothing written.
        #[test]
        fn tee_journals_before_a_blocked_forward_completes() {
            let state = tempfile::tempdir().unwrap();
            let repo = tempfile::tempdir().unwrap();
            let manifest = repo.path().join("pult.yaml");
            let journal = Journal::start_at(
                state.path(),
                StartInfo {
                    repo_dir: repo.path(),
                    manifest: &manifest,
                    command_id: "deploy",
                    command_title: "Deploy",
                    params: IndexMap::new(),
                    interactive: false,
                    run_id: None,
                },
            )
            .unwrap();

            let (reader, mut writer) = std::io::pipe().unwrap();
            let done = Arc::new(AtomicBool::new(false));
            let journal_thread = journal.clone();
            std::thread::spawn(move || {
                tee(reader, BlockingWriter, journal_thread, "stdout", done);
            });
            writer.write_all(b"straggler line\n").unwrap();
            // `writer` stays alive (not dropped) for the rest of the test —
            // EOF is irrelevant here, the point is the blocked forward, not
            // the pipe's lifetime.

            let canonical = std::fs::canonicalize(repo.path()).unwrap();
            let runs_dir = state
                .path()
                .join("repos")
                .join(repo_key(&canonical))
                .join("runs");
            let run_dir = std::fs::read_dir(&runs_dir)
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path();
            let events_path = run_dir.join("events.jsonl");

            // Poll instead of a fixed sleep: the forward never completes,
            // so there's no event to wait on other than the journal file
            // itself gaining the line, bounded so a genuine regression
            // fails the test instead of hanging it.
            let deadline = Instant::now() + Duration::from_secs(2);
            let contents = loop {
                let contents = std::fs::read_to_string(&events_path).unwrap_or_default();
                if contents.contains("straggler line") || Instant::now() >= deadline {
                    break contents;
                }
                std::thread::sleep(Duration::from_millis(10));
            };
            assert!(
                contents.contains("straggler line"),
                "journal did not receive the line before the blocked forward: {contents}"
            );
        }

        /// M2, real (non-adversarial) coverage: an actual child emits a
        /// `step` event over `PULT_EVENTS` and exits immediately after —
        /// through the genuine `execute` (real pipes, real threads, the
        /// real 500ms deadline), not a mock. Confirms the events channel
        /// wiring and the drain wait are live end to end, even though (per
        /// the module comment above) it can't force the exact interleaving
        /// M2 targets.
        #[test]
        fn full_execute_journals_a_late_step_event() {
            let state = tempfile::tempdir().unwrap();
            let repo = tempfile::tempdir().unwrap();
            let manifest = repo.path().join("pult.yaml");
            let journal = Journal::start_at(
                state.path(),
                StartInfo {
                    repo_dir: repo.path(),
                    manifest: &manifest,
                    command_id: "deploy",
                    command_title: "Deploy",
                    params: IndexMap::new(),
                    interactive: false,
                    run_id: None,
                },
            )
            .unwrap();

            let mut command = Command::new("bash");
            command
                .arg("-c")
                .arg(r#"echo "step 1/1 done" >&$PULT_EVENTS"#);
            let (code, stopped) = execute(&mut command, "test", &journal).unwrap();
            assert_eq!(code, 0);
            assert!(!stopped);

            let canonical = std::fs::canonicalize(repo.path()).unwrap();
            let run_dir = std::fs::read_dir(
                state
                    .path()
                    .join("repos")
                    .join(repo_key(&canonical))
                    .join("runs"),
            )
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
            let events = std::fs::read_to_string(run_dir.join("events.jsonl")).unwrap();
            assert!(
                events.contains(r#""kind":"step""#),
                "step event emitted right before child exit must reach the journal: {events}"
            );
        }
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
        // M2: this thread is never joined, so without a done flag the
        // journaled drain in `execute` has no way to know a straggler event
        // (e.g. a `step 3/3` emitted right before the child exits) is still
        // in flight, and can call `finish` while it's still in the pipe.
        let done = Arc::new(AtomicBool::new(false));
        let reader_done = Arc::clone(&done);
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
            });
            // `read_events` only returns on EOF (the pipe closed) — every
            // event already read reached the journal above.
            reader_done.store(true, Ordering::Relaxed);
        });
        EventsClaim::Claimed {
            writer: Some(channel.writer),
            rendered,
            done,
        }
    }

    /// Bookkeeping for `claim_events_channel` across the spawn boundary:
    /// the write end must be dropped right after `spawn` (EOF contract),
    /// any rendered OSC badge cleared after the run, and (M2) the reader
    /// thread's done flag exposed so the journaled drain loop can wait on
    /// it alongside the tee threads.
    pub(super) enum EventsClaim {
        Passthrough,
        None,
        Claimed {
            writer: Option<std::io::PipeWriter>,
            rendered: Arc<AtomicBool>,
            done: Arc<AtomicBool>,
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

        /// Whether the events reader thread has hit EOF — always `true`
        /// for `Passthrough`/`None`, which have no reader thread of ours.
        pub(super) fn done_flag(&self) -> Arc<AtomicBool> {
            match self {
                EventsClaim::Claimed { done, .. } => Arc::clone(done),
                EventsClaim::Passthrough | EventsClaim::None => Arc::new(AtomicBool::new(true)),
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
