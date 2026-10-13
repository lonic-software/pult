//! End-to-end falsifier for a defect found (not specified) while implementing
//! #6 step 1: `build_cli` gives a `dynamic-inputs` param no positional `Arg`
//! (nothing to bind — the whole point is a set of keys unknown until run
//! time), but the real dispatch loop in `main.rs`'s `run()` used to ask
//! `clap::ArgMatches` for every declared param's value by name regardless,
//! including one that was never registered as an `Arg`. `clap_builder`
//! 4.6.0's `get_one` → `try_get_one` → `verify_arg` treats an id outside
//! `valid_args` as `MatchesError::UnknownArgument`, and `get_one` panics on
//! any `Err` — but only `#[cfg(debug_assertions)]`, i.e. every dev-profile
//! build (`cargo test`, `cargo run`), never `--release`. A unit test can't
//! observe a `panic!` unwinding out of `main()` the way the process's exit
//! code and stderr can, so this drives the actual compiled binary — the
//! same reasoning as `journal_masking.rs`/`unmaskable_warning.rs`.
//!
//! This is a companion to the in-process falsifiers in `src/main.rs`
//! (`dynamic_inputs_param_leaves_later_positionals_gapless`, which pins the
//! `build_cli` half) and `src/exec.rs` (`dynamic_inputs_param_is_refused_at_live_fill_*`,
//! which pin `fill`'s refusal) — neither of those goes through `run()`'s own
//! dispatch loop, so neither would have caught this.

use std::process::Command;

#[test]
fn dynamic_inputs_command_fails_cleanly_through_the_real_cli_dispatch() {
    let state = tempfile::tempdir().unwrap();
    let trust_dir = tempfile::tempdir().unwrap();
    let trust_store = trust_dir.path().join("trust.json");
    let repo = tempfile::tempdir().unwrap();

    // `site_secrets` sits in the middle — declared last would never reach
    // the dispatch loop's per-param lookup for a *later* param, so it
    // wouldn't exercise the same code path a real gap-in-the-middle manifest
    // does. `env` and `region` each get a real positional; `site_secrets`
    // gets none.
    std::fs::write(
        repo.path().join("pult.yaml"),
        "version: 1\ncommands:\n  - id: go\n    title: Go\n    params:\n      \
         env: { pick: { options: [dev, prod] } }\n      \
         site_secrets: { dynamic-inputs: { from: \"./discover\" } }\n      \
         region: { pick: { options: [eu, us] } }\n    run: \"true\"\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_pult"))
        .current_dir(repo.path())
        .env("PULT_STATE_DIR", state.path())
        .env("PULT_TRUST_STORE", &trust_store)
        .args(["--trust", "go", "dev", "eu"])
        .output()
        .expect("failed to run the pult binary");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "dynamic-inputs is not implemented yet, so this run must fail: \
         stdout: {stdout}\nstderr: {stderr}"
    );
    // The real failure mode this test guards: without the dispatch loop's
    // eligible-params filter, this panics (`clap_builder`'s
    // `MatchesError::unwrap`) rather than returning `fill`'s clean error —
    // a Rust panic prints "panicked at" and a backtrace hint to stderr, and
    // exits 101, not pult's own `eprintln!("pult: error: {e:#}")` path.
    assert!(
        !stderr.contains("panicked at"),
        "the CLI dispatch loop panicked instead of erroring cleanly: {stderr}"
    );
    assert!(
        stderr.contains("dynamic-inputs` is not implemented yet"),
        "expected fill's clean refusal, got: {stderr}"
    );
}
