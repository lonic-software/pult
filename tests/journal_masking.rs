//! End-to-end wiring falsifier for issue #7 (and its scope-correcting
//! comment): drives the real, compiled `pult` binary so that a regression
//! where `exec.rs` never builds — or never passes — a `Masker` at all would
//! still redden a test. `journal.rs`'s and `runner.rs`'s own unit tests
//! construct their own `Masker` directly (see `append_locked_masks_...` and
//! `tee_masks_secret_in_...`), so neither of those can catch `exec.rs`
//! itself failing to wire one in — only running the actual binary,
//! unmodified, can.
//!
//! Journaling is unix-only for now (M4 — see `journal.rs`'s `Journal::start`),
//! so this whole file is unix-only too.
#![cfg(unix)]

use std::process::Command;

/// A manifest with one `secret: true` param, supplied non-interactively
/// (positionally) and echoed back by `run:` — the same shape the issue was
/// filed against. `--trust` bypasses the trust prompt so this runs
/// unattended; `--run-id` gives the run a deterministic directory name
/// instead of a generated UUID, so the test can find it without depending on
/// `journal::repo_key`'s hashing.
#[test]
fn secret_param_never_reaches_events_jsonl_in_plaintext() {
    const SECRET: &str = "sekrit-token-0007";
    const RUN_ID: &str = "wiring-falsifier-run";

    let state = tempfile::tempdir().unwrap();
    // Bound to a named local (like `state`/`repo`) so the directory isn't
    // deleted until the test ends — a `TempDir` dropped after `.path()` is
    // taken (as a bare `tempfile::tempdir().unwrap().path().join(...)`
    // expression would) removes its directory at the end of that statement,
    // before the child process ever runs, leaving `PULT_TRUST_STORE`
    // pointing at nothing.
    let trust_dir = tempfile::tempdir().unwrap();
    let trust_store = trust_dir.path().join("trust.json");
    let repo = tempfile::tempdir().unwrap();

    std::fs::write(
        repo.path().join("pult.yaml"),
        "version: 1\ncommands:\n  - id: login\n    title: Login\n    params:\n      \
         token: { input: { secret: true } }\n    run: \"echo {token}\"\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_pult"))
        .current_dir(repo.path())
        // On the child's env only — never `std::env::set_var` in-process,
        // which would race any other test running in parallel and is
        // `unsafe` under edition 2024.
        .env("PULT_STATE_DIR", state.path())
        .env("PULT_TRUST_STORE", &trust_store)
        .args(["--trust", "--run-id", RUN_ID, "login", SECRET])
        .output()
        .expect("failed to run the pult binary");

    assert!(
        output.status.success(),
        "pult exited non-zero (code {:?})\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    // Find the run's events.jsonl by run-id under the state dir, without
    // reimplementing `journal::repo_key`'s hashing: a fresh state dir has
    // exactly one repo entry.
    let repos_dir = state.path().join("repos");
    let mut events_path = None;
    for entry in std::fs::read_dir(&repos_dir).expect("state dir has no repos/") {
        let candidate = entry
            .unwrap()
            .path()
            .join("runs")
            .join(RUN_ID)
            .join("events.jsonl");
        if candidate.exists() {
            events_path = Some(candidate);
            break;
        }
    }
    let events_path =
        events_path.expect("no run directory for --run-id wiring-falsifier-run under state dir");
    let events = std::fs::read_to_string(&events_path).unwrap();

    assert!(
        !events.contains(SECRET),
        "plaintext secret leaked into events.jsonl: {events}"
    );
    assert!(
        events.contains("<redacted>"),
        "expected the masked marker in events.jsonl: {events}"
    );
}
