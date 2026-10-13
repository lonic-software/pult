//! Who emits `step` on a `PULT_EVENTS` channel, and how a child's `progress`
//! lands in the journal — driven through the real `pult` binary, since the
//! decision is made in `exec.rs` before compiling and no unit test reaches
//! that site.
//!
//! Journaling and the events channel are unix-only (M4), so this file is too.
#![cfg(unix)]

use std::path::Path;
use std::process::{Command, Output};

/// Three top-level entries, so the compiled script has three `step` guards;
/// the second entry reports 60% of its own work on the channel.
const MANIFEST: &str = r#"version: 1
commands:
  - id: build
    title: Build
    run:
      - "echo one"
      - '[ -n "${PULT_EVENTS:-}" ] && printf "progress 60 half\n" >&"$PULT_EVENTS" || true'
      - "echo three"
"#;

/// How the run's `PULT_EVENTS` is set up by whoever starts pult.
enum Upstream<'a> {
    /// Unset: pult may own the channel.
    None,
    /// fd 5, opened onto this file by a parent shell — the shape of a parent
    /// that owns `PULT_EVENTS` and reads it itself.
    Channel(&'a Path),
    /// A value that is not an fd number.
    Invalid(&'a str),
    /// The same, on a run with journaling off and no terminal, so pult
    /// creates no channel of its own to replace the value with.
    InvalidUnjournaled(&'a str),
}

fn run_pult(repo: &Path, state: &Path, trust: &Path, upstream: Upstream) -> Output {
    std::fs::write(repo.join("pult.yaml"), MANIFEST).unwrap();
    let pult = env!("CARGO_BIN_EXE_pult");
    let journal_off = matches!(upstream, Upstream::InvalidUnjournaled(_));
    let mut command = match upstream {
        Upstream::Channel(file) => {
            let mut c = Command::new("sh");
            c.args(["-c", r#"exec 5>"$0"; exec "$@""#])
                .arg(file)
                .arg(pult)
                .env("PULT_EVENTS", "5");
            c
        }
        Upstream::Invalid(value) | Upstream::InvalidUnjournaled(value) => {
            let mut c = Command::new(pult);
            c.env("PULT_EVENTS", value);
            c
        }
        Upstream::None => {
            let mut c = Command::new(pult);
            c.env_remove("PULT_EVENTS");
            c
        }
    };
    command
        .current_dir(repo)
        .env("PULT_STATE_DIR", state)
        .env("PULT_TRUST_STORE", trust)
        // Journaling is on by default; an exported `PULT_JOURNAL=0` must not
        // turn these tests into a missing-journal failure.
        .env_remove("PULT_JOURNAL");
    if journal_off {
        command.env("PULT_JOURNAL", "0");
    }
    let output = command
        .args(["--trust", "--run-id", "events-run", "build"])
        .output()
        .expect("failed to run the pult binary");
    assert!(
        output.status.success(),
        "pult exited non-zero (code {:?})\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    output
}

/// The run's `events.jsonl`, found by run id under a fresh state dir (which
/// has exactly one repo entry).
fn journal_records(state: &Path) -> Vec<serde_json::Value> {
    let repos = std::fs::read_dir(state.join("repos")).expect("state dir has no repos/");
    for entry in repos {
        let path = entry.unwrap().path().join("runs/events-run/events.jsonl");
        if path.exists() {
            return std::fs::read_to_string(path)
                .unwrap()
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect();
        }
    }
    panic!(
        "no events.jsonl for run events-run under {}",
        state.display()
    );
}

/// Under a parent that owns the channel, pult writes no `step` of its own
/// onto it: the parent's file holds the child's `progress` line and nothing
/// else. Goes red if `exec.rs` compiles with step guards regardless of an
/// inherited channel (the file then starts with `step 1/3 echo one`).
#[test]
fn an_inherited_channel_gets_no_step_lines_from_pult() {
    let state = tempfile::tempdir().unwrap();
    let trust = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    let events_file = out.path().join("events.txt");

    run_pult(
        repo.path(),
        state.path(),
        &trust.path().join("trust.json"),
        Upstream::Channel(&events_file),
    );

    // The child's own line arrived, so the channel was live and inherited.
    assert_eq!(
        std::fs::read_to_string(&events_file).unwrap(),
        "progress 60 half\n"
    );
}

/// When pult owns the channel (no inherited `PULT_EVENTS`), the steps are
/// injected, and the child's 60% of step 2 of 3 is journaled raw as `pct`
/// and banded as `run_pct` = (100+60)/3 = 53 — a value distinct from the raw
/// 60, the step's start 33 and the unscaled 93. Goes red if step injection is
/// turned off unconditionally (no `step` records, and `run_pct` falls back to
/// the raw 60), or if the journal stops banding.
#[test]
fn an_owned_channel_journals_steps_and_banded_progress() {
    let state = tempfile::tempdir().unwrap();
    let trust = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();

    run_pult(
        repo.path(),
        state.path(),
        &trust.path().join("trust.json"),
        Upstream::None,
    );

    let records = journal_records(state.path());
    let steps: Vec<(u64, u64)> = records
        .iter()
        .filter(|r| r["kind"] == "step")
        .map(|r| (r["k"].as_u64().unwrap(), r["n"].as_u64().unwrap()))
        .collect();
    assert_eq!(steps, [(1, 3), (2, 3), (3, 3)]);

    let progress: Vec<&serde_json::Value> =
        records.iter().filter(|r| r["kind"] == "progress").collect();
    assert_eq!(progress.len(), 1, "records: {records:?}");
    assert_eq!(progress[0]["pct"], 60);
    assert_eq!(progress[0]["run_pct"], 53);
}

/// An invalid inherited value is not someone else's channel: pult warns,
/// strips it, owns the channel itself and injects its steps, all from one
/// reading of `PULT_EVENTS`. Goes red if the compile-time choice treats any
/// set value as inherited (no `step` records, while the runner still claims
/// the channel). The strip is pinned by the test below.
#[test]
fn an_invalid_inherited_value_is_stripped_and_pult_owns_the_channel() {
    let state = tempfile::tempdir().unwrap();
    let trust = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();

    let output = run_pult(
        repo.path(),
        state.path(),
        &trust.path().join("trust.json"),
        Upstream::Invalid("events.log"),
    );

    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("pult: ignoring invalid PULT_EVENTS=events.log"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let records = journal_records(state.path());
    let steps = records.iter().filter(|r| r["kind"] == "step").count();
    assert_eq!(steps, 3, "records: {records:?}");
    let progress = records.iter().find(|r| r["kind"] == "progress").unwrap();
    assert_eq!(progress["run_pct"], 53);
}

/// With no channel of its own to replace it (journaling off, no terminal),
/// only the strip keeps an invalid value from the script: forwarded, the
/// injected guards would each write `step` lines into a file named
/// `events.log` in the run directory. Goes red if the runner warns but stops
/// removing the value from the child's environment.
#[test]
fn an_invalid_inherited_value_never_reaches_the_script() {
    let state = tempfile::tempdir().unwrap();
    let trust = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();

    let output = run_pult(
        repo.path(),
        state.path(),
        &trust.path().join("trust.json"),
        Upstream::InvalidUnjournaled("events.log"),
    );

    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("pult: ignoring invalid PULT_EVENTS=events.log"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    // The run did execute its steps, with nowhere to send events.
    assert!(String::from_utf8_lossy(&output.stdout).contains("three"));
    assert!(!repo.path().join("events.log").exists());
}
