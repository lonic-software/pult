//! The run journal — every run written to disk by this process, regardless
//! of who launched it or who is watching (see pult-desktop's
//! docs/run-journal.md for the full protocol spec, schema 1; that document
//! moves here once the protocol settles).
//!
//! Layout, under a per-user state dir (never inside the repo):
//!
//! ```text
//! <state>/repos/<repo-key>/
//!   repo.json                  # { "schema": 1, "dir": "<canonical dir>" }
//!   runs/<run-id>/
//!     meta.json                # atomic rewrites: running -> exited | stopped
//!     events.jsonl             # append-only: line/step/progress/status/exit
//! ```
//!
//! Design rules this module enforces:
//!
//! - **Journaling never breaks a run.** Every filesystem error here degrades
//!   to a one-time stderr warning and a disabled journal — the command runs
//!   exactly as it would have without one.
//! - **Single writer.** The pult process owns a run's files; readers (the
//!   desktop app, `pult runs`) only ever read. "Crashed" is therefore a
//!   *reader-derived* state: `status: "running"` in meta plus a dead pid —
//!   a crashed writer can't record its own crash.
//! - **`meta.json` is fsynced on status transitions** (write tmp + rename);
//!   event lines are flushed but not fsynced — after power loss a run may
//!   lose trailing output but never its outcome-vs-crashed distinction.
//! - **The `exit` event is terminal.** `finish` marks the journal done and
//!   later appends (a straggler grandchild's output draining after the
//!   child exited) are dropped, so readers can treat `exit` as the last
//!   record of a completed run.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::events::Event;

/// Journal protocol schema — additive-only, same discipline as `--list`'s.
const SCHEMA: u32 = 1;

/// Default per-(repo, command) retention; `PULT_RUNS_KEEP` overrides.
const DEFAULT_KEEP: usize = 20;

/// `meta.json` — one run's identity, audit record, and outcome. Field
/// semantics are contract (schema 1); additions must be additive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meta {
    pub schema: u32,
    pub run_id: String,
    pub repo_dir: PathBuf,
    pub manifest: PathBuf,
    pub command_id: String,
    pub command_title: String,
    /// Pre-redacted by the caller: secret params arrive here as the literal
    /// `"<redacted>"` — the value itself must never reach this module.
    pub params: IndexMap<String, String>,
    /// `"cli"`, `"desktop"`, … — informational in schema 1 (`PULT_ORIGIN`
    /// env, set by wrapping surfaces; defaults to `"cli"`).
    pub origin: String,
    pub interactive: bool,
    pub pult_version: String,
    /// Pid of the *journaling pult process*, not the child: the writer is
    /// what liveness/crash detection must probe, and pult outlives its
    /// child within a run.
    pub pid: u32,
    /// pult's process group (unix only) — the stop capability: readers
    /// signal this group (TERM, grace, KILL) to stop a run they didn't
    /// spawn. Absent on Windows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pgid: Option<i32>,
    pub started_at: String,
    pub status: Status,
    pub exit_code: Option<i32>,
    pub ended_at: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Running,
    Exited,
    Stopped,
}

/// Everything `start` needs to open a journal. `params` must already be
/// redacted (see `Meta::params`).
pub struct StartInfo<'a> {
    pub repo_dir: &'a Path,
    pub manifest: &'a Path,
    pub command_id: &'a str,
    pub command_title: &'a str,
    pub params: IndexMap<String, String>,
    pub interactive: bool,
    /// Caller-supplied id (`--run-id`, e.g. from the desktop app); a
    /// UUIDv7 is generated when absent so directory names sort by time.
    pub run_id: Option<&'a str>,
}

/// A live journal. Cheap to clone (tee/reader threads each hold one);
/// all appends serialize through one lock so `events.jsonl` lines never
/// interleave mid-record.
#[derive(Clone)]
pub struct Journal(Arc<Mutex<Inner>>);

struct Inner {
    events: fs::File,
    meta: Meta,
    meta_path: PathBuf,
    done: bool,
    /// One warning per journal, not one per failed write.
    warned: bool,
}

impl Journal {
    /// Open a journal for a run about to start. `None` means journaling is
    /// off (disabled via `PULT_JOURNAL=0`, no resolvable state dir, or an
    /// I/O failure — already warned) and the run proceeds unjournaled.
    pub fn start(info: StartInfo) -> Option<Journal> {
        if std::env::var_os("PULT_JOURNAL").is_some_and(|v| v == "0") {
            return None;
        }
        let state = state_dir()?;
        match Self::start_at(&state, info) {
            Ok(j) => Some(j),
            Err(e) => {
                eprintln!("pult: warning: run journal disabled: {e:#}");
                None
            }
        }
    }

    /// The fallible core of `start`, with the state dir explicit — the seam
    /// tests use to journal into a tempdir without touching process env.
    fn start_at(state: &Path, info: StartInfo) -> Result<Journal> {
        // Canonicalize so one repo reached via different paths (symlinks,
        // `../`) journals to one key. Fall back to the raw path — a journal
        // under a slightly-off key beats no journal.
        let repo_dir =
            fs::canonicalize(info.repo_dir).unwrap_or_else(|_| info.repo_dir.to_path_buf());
        let repo_root = state.join("repos").join(repo_key(&repo_dir));
        let runs_root = repo_root.join("runs");
        fs::create_dir_all(&runs_root)
            .with_context(|| format!("failed to create {}", runs_root.display()))?;

        // The human-readable reverse mapping (key -> dir); written once,
        // refreshed if the canonical path ever changes for the same key.
        let repo_json = repo_root.join("repo.json");
        if !repo_json.exists() {
            let doc = serde_json::json!({ "schema": SCHEMA, "dir": repo_dir });
            let _ = fs::write(&repo_json, format!("{:#}\n", doc));
        }

        prune(&runs_root, keep_limit(), Some(info.command_id));

        let run_id = match info.run_id {
            Some(id) => id.to_string(),
            None => uuid::Uuid::now_v7().to_string(),
        };
        let run_dir = runs_root.join(&run_id);
        fs::create_dir(&run_dir)
            .with_context(|| format!("failed to create run dir {}", run_dir.display()))?;

        let meta = Meta {
            schema: SCHEMA,
            run_id,
            repo_dir,
            manifest: info.manifest.to_path_buf(),
            command_id: info.command_id.to_string(),
            command_title: info.command_title.to_string(),
            params: info.params,
            origin: std::env::var("PULT_ORIGIN").unwrap_or_else(|_| "cli".to_string()),
            interactive: info.interactive,
            pult_version: env!("CARGO_PKG_VERSION").to_string(),
            pid: std::process::id(),
            pgid: process_group(),
            started_at: rfc3339_utc(now_ms()),
            status: Status::Running,
            exit_code: None,
            ended_at: None,
        };

        let meta_path = run_dir.join("meta.json");
        write_meta(&meta_path, &meta)?;
        let events = fs::OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(run_dir.join("events.jsonl"))
            .with_context(|| format!("failed to create events.jsonl in {}", run_dir.display()))?;

        Ok(Journal(Arc::new(Mutex::new(Inner {
            events,
            meta,
            meta_path,
            done: false,
            warned: false,
        }))))
    }

    /// One line of child output. `stream` is `"stdout"` or `"stderr"`.
    pub fn line(&self, stream: &str, text: &str) {
        self.append(serde_json::json!({
            "ts": now_ms(), "kind": "line", "stream": stream, "text": text,
        }));
    }

    /// One parsed `PULT_EVENTS` protocol event (step/progress/status).
    pub fn event(&self, event: &Event) {
        let doc = match event {
            Event::Step { k, n, name } => serde_json::json!({
                "ts": now_ms(), "kind": "step", "k": k, "n": n, "name": name,
            }),
            Event::Progress { pct, text } => serde_json::json!({
                "ts": now_ms(), "kind": "progress", "pct": pct, "text": text,
            }),
            Event::Status(text) => serde_json::json!({
                "ts": now_ms(), "kind": "status", "text": text,
            }),
        };
        self.append(doc);
    }

    /// Terminal record: append the `exit` event, mark the journal done
    /// (late appends become no-ops), and rewrite meta with the outcome.
    /// `stopped` = a stop/interrupt was requested and the run ended.
    pub fn finish(&self, code: Option<i32>, stopped: bool) {
        let mut inner = self.0.lock().unwrap();
        if inner.done {
            return;
        }
        let doc = serde_json::json!({
            "ts": now_ms(), "kind": "exit", "code": code, "stopped": stopped,
        });
        Self::append_locked(&mut inner, doc);
        inner.done = true;
        inner.meta.status = if stopped {
            Status::Stopped
        } else {
            Status::Exited
        };
        inner.meta.exit_code = code;
        inner.meta.ended_at = Some(rfc3339_utc(now_ms()));
        let (path, meta) = (inner.meta_path.clone(), inner.meta.clone());
        if let Err(e) = write_meta(&path, &meta) {
            Self::warn_once(&mut inner, &e);
        }
    }

    fn append(&self, doc: serde_json::Value) {
        let mut inner = self.0.lock().unwrap();
        if inner.done {
            return;
        }
        Self::append_locked(&mut inner, doc);
    }

    fn append_locked(inner: &mut Inner, doc: serde_json::Value) {
        // Compact one-line JSON + newline, flushed per record — a reader
        // may see a torn *final* line after a crash (spec: treat as "not
        // yet written"), never a torn earlier one.
        let result = writeln!(inner.events, "{doc}").and_then(|_| inner.events.flush());
        if let Err(e) = result {
            Self::warn_once(inner, &anyhow::Error::from(e));
        }
    }

    fn warn_once(inner: &mut Inner, e: &anyhow::Error) {
        if !inner.warned {
            inner.warned = true;
            eprintln!("pult: warning: run journal write failed: {e:#}");
        }
    }
}

/// Atomic meta write: tmp + fsync + rename, so a reader never observes a
/// half-written meta and a power loss never loses an already-recorded
/// outcome.
fn write_meta(path: &Path, meta: &Meta) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    let json = serde_json::to_string_pretty(meta).context("failed to serialize run meta")?;
    fs::write(&tmp, format!("{json}\n"))
        .with_context(|| format!("failed to write {}", tmp.display()))?;
    let f = fs::File::open(&tmp).with_context(|| format!("failed to reopen {}", tmp.display()))?;
    f.sync_all().ok();
    fs::rename(&tmp, path)
        .with_context(|| format!("failed to move meta into place at {}", path.display()))?;
    Ok(())
}

/// The per-user state dir (`<state>` in the layout above): `PULT_STATE_DIR`
/// wins; otherwise `~/.local/state/pult` (Linux, via XDG), `~/Library/
/// Application Support/pult/state` (macOS), `%LOCALAPPDATA%\pult\state`
/// (Windows). `None` disables journaling (no home dir at all).
pub fn state_dir() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("PULT_STATE_DIR") {
        if p.is_empty() {
            return None;
        }
        return Some(PathBuf::from(p));
    }
    #[cfg(target_os = "macos")]
    {
        dirs::data_dir().map(|d| d.join("pult").join("state"))
    }
    #[cfg(windows)]
    {
        dirs::data_local_dir().map(|d| d.join("pult").join("state"))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        dirs::state_dir()
            .map(|d| d.join("pult"))
            .or_else(|| dirs::home_dir().map(|h| h.join(".local/state/pult")))
    }
}

/// First 16 hex chars of SHA-256 of the canonical repo dir — the directory
/// name a repo's journals live under.
pub fn repo_key(canonical_dir: &Path) -> String {
    let digest = Sha256::digest(canonical_dir.to_string_lossy().as_bytes());
    let mut key = String::with_capacity(16);
    for byte in &digest[..8] {
        key.push_str(&format!("{byte:02x}"));
    }
    key
}

pub fn keep_limit() -> usize {
    std::env::var("PULT_RUNS_KEEP")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_KEEP)
}

/// Writer-side retention, run before each new journal is created (and by
/// `pult runs prune`): keep the most recent `keep` runs per command_id,
/// never removing a `running` run whose writer is still alive. Best-effort
/// throughout — retention must never block a run.
///
/// `only_command`: prune just that command's history (the per-run-start
/// fast path); `None` prunes every command (`pult runs prune`).
pub fn prune(runs_root: &Path, keep: usize, only_command: Option<&str>) -> usize {
    let Ok(entries) = fs::read_dir(runs_root) else {
        return 0;
    };
    let mut by_command: IndexMap<String, Vec<(String, PathBuf, Meta)>> = IndexMap::new();
    for entry in entries.flatten() {
        let run_dir = entry.path();
        let Some(meta) = read_meta(&run_dir) else {
            continue;
        };
        if only_command.is_some_and(|c| c != meta.command_id) {
            continue;
        }
        by_command
            .entry(meta.command_id.clone())
            .or_default()
            .push((meta.started_at.clone(), run_dir, meta));
    }
    let mut removed = 0;
    for (_, mut runs) in by_command {
        // RFC3339 UTC sorts lexically — newest first after reverse. Runs
        // starting within the same millisecond tie on `started_at`; the
        // run_id tie-break (UUIDv7, also time-ordered) keeps the order
        // deterministic rather than platform-dependent.
        runs.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.2.run_id.cmp(&a.2.run_id)));
        for (_, run_dir, meta) in runs.into_iter().skip(keep) {
            if meta.status == Status::Running && writer_alive(&meta) {
                continue;
            }
            if fs::remove_dir_all(&run_dir).is_ok() {
                removed += 1;
            }
        }
    }
    removed
}

/// Read one run dir's meta; `None` for anything unreadable/foreign (readers
/// skip, never error — forward compatibility with future layout additions).
pub fn read_meta(run_dir: &Path) -> Option<Meta> {
    let raw = fs::read_to_string(run_dir.join("meta.json")).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Whether the journaling process recorded in `meta` is still alive —
/// the probe behind reader-derived crash detection. Windows has no cheap
/// equivalent wired up yet; err on "alive" there (a stale "running" is
/// less wrong than a live run shown crashed).
pub fn writer_alive(meta: &Meta) -> bool {
    #[cfg(unix)]
    {
        // kill(pid, 0): existence probe, no signal delivered. EPERM still
        // means "exists".
        unsafe extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
        }
        let pid = meta.pid as i32;
        if pid <= 0 {
            return false;
        }
        unsafe { kill(pid, 0) == 0 || last_errno_is_eperm() }
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        true
    }
}

#[cfg(unix)]
fn last_errno_is_eperm() -> bool {
    std::io::Error::last_os_error().raw_os_error() == Some(1) // EPERM
}

#[cfg(unix)]
fn process_group() -> Option<i32> {
    unsafe extern "C" {
        fn getpgrp() -> i32;
    }
    Some(unsafe { getpgrp() })
}

#[cfg(not(unix))]
fn process_group() -> Option<i32> {
    None
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Unix-ms → RFC 3339 UTC (`2026-07-17T13:05:12.412Z`), hand-rolled so the
/// binary doesn't grow a chrono dependency for one format. Date math is the
/// standard civil-from-days algorithm (Howard Hinnant's).
pub fn rfc3339_utc(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let millis = ms % 1000;
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        sod / 3600,
        (sod % 3600) / 60,
        sod % 60,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info<'a>(repo: &'a Path, manifest: &'a Path) -> StartInfo<'a> {
        StartInfo {
            repo_dir: repo,
            manifest,
            command_id: "deploy",
            command_title: "Deploy",
            params: IndexMap::new(),
            interactive: false,
            run_id: None,
        }
    }

    #[test]
    fn rfc3339_formats_known_timestamps() {
        assert_eq!(rfc3339_utc(0), "1970-01-01T00:00:00.000Z");
        // 2026-07-17 13:05:12.412 UTC
        assert_eq!(rfc3339_utc(1_784_293_512_412), "2026-07-17T13:05:12.412Z");
        // Leap-day sanity.
        assert_eq!(rfc3339_utc(1_709_164_800_000), "2024-02-29T00:00:00.000Z");
    }

    #[test]
    fn repo_key_is_stable_hex() {
        let key = repo_key(Path::new("/tmp/demo"));
        assert_eq!(key.len(), 16);
        assert!(key.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(key, repo_key(Path::new("/tmp/demo")));
        assert_ne!(key, repo_key(Path::new("/tmp/demo2")));
    }

    #[test]
    fn start_line_finish_writes_the_documented_files() {
        let state = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let manifest = repo.path().join("pult.yaml");

        let journal = Journal::start_at(state.path(), info(repo.path(), &manifest)).unwrap();
        journal.line("stdout", "building image…");
        journal.event(&Event::Step {
            k: 1,
            n: 3,
            name: "build".into(),
        });
        journal.finish(Some(0), false);
        // Terminal: further appends are dropped, exit stays the last record.
        journal.line("stdout", "straggler after exit");

        let canonical = fs::canonicalize(repo.path()).unwrap();
        let repo_root = state.path().join("repos").join(repo_key(&canonical));
        let repo_doc: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(repo_root.join("repo.json")).unwrap())
                .unwrap();
        assert_eq!(repo_doc["dir"], serde_json::json!(canonical));

        let run_dir = fs::read_dir(repo_root.join("runs"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap();
        let meta = read_meta(&run_dir.path()).unwrap();
        assert_eq!(meta.schema, SCHEMA);
        assert_eq!(meta.command_id, "deploy");
        assert_eq!(meta.status, Status::Exited);
        assert_eq!(meta.exit_code, Some(0));
        assert!(meta.ended_at.is_some());
        assert_eq!(meta.pid, std::process::id());

        let events = fs::read_to_string(run_dir.path().join("events.jsonl")).unwrap();
        let lines: Vec<serde_json::Value> = events
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 3, "straggler must be dropped: {events}");
        assert_eq!(lines[0]["kind"], "line");
        assert_eq!(lines[0]["text"], "building image…");
        assert_eq!(lines[1]["kind"], "step");
        assert_eq!(lines[1]["k"], 1);
        assert_eq!(lines[2]["kind"], "exit");
        assert_eq!(lines[2]["code"], 0);
        assert_eq!(lines[2]["stopped"], false);
    }

    #[test]
    fn stopped_finish_records_stopped_status() {
        let state = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let manifest = repo.path().join("pult.yaml");
        let journal = Journal::start_at(state.path(), info(repo.path(), &manifest)).unwrap();
        journal.finish(None, true);

        let canonical = fs::canonicalize(repo.path()).unwrap();
        let runs = state
            .path()
            .join("repos")
            .join(repo_key(&canonical))
            .join("runs");
        let run_dir = fs::read_dir(runs).unwrap().next().unwrap().unwrap();
        let meta = read_meta(&run_dir.path()).unwrap();
        assert_eq!(meta.status, Status::Stopped);
        assert_eq!(meta.exit_code, None);
    }

    #[test]
    fn caller_supplied_run_id_names_the_run_dir() {
        let state = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let manifest = repo.path().join("pult.yaml");
        let mut i = info(repo.path(), &manifest);
        i.run_id = Some("desktop-supplied-id");
        let journal = Journal::start_at(state.path(), i).unwrap();
        journal.finish(Some(0), false);

        let canonical = fs::canonicalize(repo.path()).unwrap();
        let run_dir = state
            .path()
            .join("repos")
            .join(repo_key(&canonical))
            .join("runs")
            .join("desktop-supplied-id");
        assert!(run_dir.join("meta.json").exists());
    }

    #[test]
    fn prune_keeps_newest_per_command_and_spares_live_running() {
        let state = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let manifest = repo.path().join("pult.yaml");

        // Five finished runs of one command… (the sleeps guarantee distinct
        // `started_at` millisecond stamps, so newest-first is unambiguous)
        for _ in 0..5 {
            let journal = Journal::start_at(state.path(), info(repo.path(), &manifest)).unwrap();
            journal.finish(Some(0), false);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        // …one still-running (this test process is its live writer)…
        let live = Journal::start_at(state.path(), info(repo.path(), &manifest)).unwrap();
        // …and one run of a different command, which a scoped prune must not touch.
        let mut other = info(repo.path(), &manifest);
        other.command_id = "status";
        let other_journal = Journal::start_at(state.path(), other).unwrap();
        other_journal.finish(Some(0), false);

        let canonical = fs::canonicalize(repo.path()).unwrap();
        let runs_root = state
            .path()
            .join("repos")
            .join(repo_key(&canonical))
            .join("runs");
        let count = || fs::read_dir(&runs_root).unwrap().count();
        assert_eq!(count(), 7);

        // 6 "deploy" runs, newest first = [live, finished×5]; keep 2 → the
        // 4 oldest finished go. The "status" run is out of scope.
        let removed = prune(&runs_root, 2, Some("deploy"));
        assert_eq!(removed, 4);
        assert_eq!(count(), 3);

        // keep 0 would drop everything — except a running run whose writer
        // (this test process) is alive. Only the remaining finished deploy goes.
        let removed = prune(&runs_root, 0, Some("deploy"));
        assert_eq!(removed, 1);
        assert_eq!(count(), 2);
        let statuses: Vec<Status> = fs::read_dir(&runs_root)
            .unwrap()
            .flatten()
            .filter_map(|e| read_meta(&e.path()))
            .map(|m| m.status)
            .collect();
        assert!(statuses.contains(&Status::Running));
        live.finish(Some(0), false);
    }

    #[test]
    fn unreadable_run_dirs_are_skipped_not_errors() {
        let state = tempfile::tempdir().unwrap();
        let runs_root = state.path().join("runs");
        fs::create_dir_all(runs_root.join("garbage-no-meta")).unwrap();
        fs::create_dir_all(runs_root.join("garbage-bad-meta")).unwrap();
        fs::write(runs_root.join("garbage-bad-meta/meta.json"), "not json").unwrap();
        assert_eq!(prune(&runs_root, 1, None), 0);
    }
}
