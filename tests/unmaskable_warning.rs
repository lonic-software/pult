//! End-to-end falsifier for a follow-up review finding on #12's fix: the
//! "secret too short to mask" warning (`exec::warn_unmaskable`) must fire
//! even when a failing `pick.from` aborts `fill` before `execute` ever
//! reaches its own post-`fill` masker/warning call. Before this fix, the
//! warning lived only in `execute`, *after* `fill` returns — so the exact
//! scenario it exists to flag (a too-short secret, a failing option source,
//! the secret echoed into stderr) aborted inside `fill` with zero warning.
//!
//! A unit test can't observe this: `warn_unmaskable` writes straight to the
//! process's real stderr, not a return value, so — the same reasoning as
//! `journal_masking.rs` — this drives the actual compiled binary rather than
//! calling `exec::fill`/`exec::execute` in-process.

use std::process::Command;

/// Under `mask::MIN_LEN` (6 characters) — deliberately, so the masker treats
/// it as unmaskable and this test exercises the exact gap the warning is
/// about, not a value long enough to be masked instead.
const SECRET: &str = "abcde";

#[test]
fn too_short_secret_is_warned_about_even_though_a_failing_pick_from_aborts_the_run() {
    let state = tempfile::tempdir().unwrap();
    // See `journal_masking.rs`'s identical comment: bound to a named local so
    // the directory isn't removed before the child process reads it.
    let trust_dir = tempfile::tempdir().unwrap();
    let trust_store = trust_dir.path().join("trust.json");
    let repo = tempfile::tempdir().unwrap();

    std::fs::write(
        repo.path().join("pult.yaml"),
        "version: 1\ncommands:\n  - id: deploy\n    title: Deploy\n    params:\n      \
         token: { input: { secret: true } }\n      \
         target: { pick: { from: \"echo tok={token} >&2; exit 1\" } }\n    \
         run: \"echo {target}\"\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_pult"))
        .current_dir(repo.path())
        .env("PULT_STATE_DIR", state.path())
        .env("PULT_TRUST_STORE", &trust_store)
        .args(["--trust", "deploy", SECRET])
        .output()
        .expect("failed to run the pult binary");

    assert!(
        !output.status.success(),
        "expected the failing pick.from source to abort the run: stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        stderr.contains("too short to mask"),
        "expected the unmaskable-secret warning even though `fill` aborted before \
         `execute`'s own post-`fill` warning ever runs: {stderr}"
    );
    // This value is *known* to leak (it's under `MIN_LEN`, so the masker has
    // no needle for it — a documented gap, not something this change fixes).
    // Asserting the leak too pins that this test actually exercises the
    // scenario the warning exists for, rather than one where nothing leaked
    // and the warning happened to fire anyway for an unrelated reason.
    assert!(
        stderr.contains(&format!("tok={SECRET}")),
        "expected the documented sub-MIN_LEN leak in stderr: {stderr}"
    );
}

/// Adversarial-review finding (round 3): the run-level warning was gated on
/// the claim that masking always happens somewhere at the run level once
/// `execute` reaches it — true for `ResolvedRun::Script` (the `running:`
/// banner is masked regardless of journaling), false for
/// `ResolvedRun::Steps` on any run that isn't `Journaling::Full` (its
/// "banner" is just the command id — see `run_bash`'s call site in
/// `exec.rs` — no masker ever touches it). Warning there would be exactly
/// the misleading noise #11's narrower gate existed to prevent: a run where
/// *nothing* is masked, but pult claims a secret was "too short to mask" as
/// if some masking pass had actually looked for it.
///
/// `PULT_JOURNAL=0` forces `Journaling::Off` even for this non-interactive,
/// non-ephemeral run, so the only way this run's masker ever gets applied to
/// anything is if the (wrong) old gate fires it anyway.
#[test]
fn steps_run_with_masking_off_is_not_warned_about_an_unmaskable_secret() {
    const SHORT_SECRET: &str = "abcd"; // 4 chars — under MIN_LEN

    let state = tempfile::tempdir().unwrap();
    let trust_dir = tempfile::tempdir().unwrap();
    let trust_store = trust_dir.path().join("trust.json");
    let repo = tempfile::tempdir().unwrap();

    std::fs::write(
        repo.path().join("pult.yaml"),
        "version: 1\ncommands:\n  - id: deploy\n    title: Deploy\n    params:\n      \
         token: { input: { secret: true } }\n    \
         run: [\"echo {token}\"]\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_pult"))
        .current_dir(repo.path())
        .env("PULT_STATE_DIR", state.path())
        .env("PULT_TRUST_STORE", &trust_store)
        .env("PULT_JOURNAL", "0")
        .args(["--trust", "deploy", SHORT_SECRET])
        .output()
        .expect("failed to run the pult binary");

    assert!(
        output.status.success(),
        "expected the steps run to succeed: stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !stderr.contains("too short to mask"),
        "warned about masking on a run where a `Steps` command with \
         journaling off never masks anything at the run level: {stderr}"
    );
}
