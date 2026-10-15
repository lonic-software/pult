//! `pult tool` end to end, against examples/argv_echo.rs: a program whose
//! self-schema document declares one command and which prints the argv it
//! received. Runs on windows-latest too (see .github/workflows/ci.yml), which
//! is the point — argv must reach the program byte for byte with no shell in
//! between, under Windows' command-line quoting as much as anywhere.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn pult() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_pult"))
}

/// Cargo builds examples beside the test binaries: `target/<profile>/examples`.
fn argv_echo() -> PathBuf {
    let exe = format!("argv_echo{}", std::env::consts::EXE_SUFFIX);
    let path = pult().parent().unwrap().join("examples").join(exe);
    assert!(path.is_file(), "{} is not built", path.display());
    path
}

fn tool(bin: &Path, args: &[&str]) -> Output {
    Command::new(pult())
        .arg("tool")
        .arg(bin)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

/// The argv the echo program printed, after its own name.
fn echoed(out: &Output) -> Vec<String> {
    assert!(
        out.status.success(),
        "{:?}: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("{e}: {:?}", String::from_utf8_lossy(&out.stdout)))
}

#[test]
fn tool_argv_reaches_the_program_byte_for_byte() {
    let words = [
        "/x", "a b", "--x", "\"q\"", r"C:\y\", "-v", "", "$HOME", "*",
    ];
    let mut args = vec!["echo", "--"];
    args.extend(words);
    let mut want = vec!["echo", "--"];
    want.extend(words);
    assert_eq!(echoed(&tool(&argv_echo(), &args)), want);
}

#[test]
fn tool_a_dash_led_value_rides_inside_its_flag() {
    // Two words, `--grep -x`, would hand the program `-x` as a flag of its
    // own; the tool-only rule joins them so the value stays a value.
    let out = tool(&argv_echo(), &["echo", "--loud", "--grep=-x", "--", "i"]);
    assert_eq!(echoed(&out), ["echo", "--loud", "--grep=-x", "--", "i"]);
    let out = tool(&argv_echo(), &["echo", "--grep", "plain"]);
    assert_eq!(echoed(&out), ["echo", "--grep", "plain", "--"]);
}

#[test]
fn tool_runs_a_program_under_a_hostile_path() {
    // Spaces, quotes and shell metacharacters in BIN itself: it is spawned
    // directly, never through a shell, so none of them is interpreted.
    let dir = tempfile::tempdir().unwrap();
    let hostile = dir.path().join("a b'$x;&(c)");
    std::fs::create_dir(&hostile).unwrap();
    let bin = hostile.join(argv_echo().file_name().unwrap());
    std::fs::copy(argv_echo(), &bin).unwrap();
    assert_eq!(
        echoed(&tool(&bin, &["echo", "--", "z"])),
        ["echo", "--", "z"]
    );
}

#[test]
fn tool_stdout_carries_only_the_program_output() {
    let out = tool(&argv_echo(), &["echo", "--", "only"]);
    let stdout = String::from_utf8(out.stdout.clone()).unwrap();
    assert_eq!(stdout.trim_end(), r#"["echo","--","only"]"#);
    // Pult's own lines went to stderr instead.
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("◆"), "{stderr}");
}

#[test]
fn tool_print_composes_without_running_the_command() {
    let out = tool(&argv_echo(), &["echo", "--print", "--", "a b"]);
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("echo -- "), "{stdout}");
    assert!(!stdout.contains('['), "the command ran: {stdout}");
}

#[test]
fn tool_refuses_an_unknown_command() {
    let out = tool(&argv_echo(), &["nope"]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("no command `nope`"), "{stderr}");
    assert!(stderr.contains("echo"), "{stderr}");
}

#[test]
fn tool_without_a_command_and_without_a_terminal_is_usage() {
    let out = tool(&argv_echo(), &[]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
}

/// The fixture against a live Forklift: set `PULT_TEST_FORKLIFT` to a
/// `forklift` binary. Drift here means the fixture needs refreshing — see
/// tests/fixtures/forklift-self-schema.md.
#[test]
fn tool_live_forklift_matches_the_fixture() {
    let Some(forklift) = std::env::var_os("PULT_TEST_FORKLIFT") else {
        eprintln!("skipped: PULT_TEST_FORKLIFT is not set");
        return;
    };
    let forklift = PathBuf::from(forklift);
    let live = Command::new(&forklift)
        .args(["self-schema", "--json"])
        .output()
        .unwrap();
    assert!(live.status.success(), "{live:?}");
    let live: serde_json::Value = serde_json::from_slice(&live.stdout).unwrap();
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/forklift-self-schema.json")).unwrap();
    assert!(
        live["data"] == fixture["data"],
        "{} describes itself differently from tests/fixtures/forklift-self-schema.json; \
         refresh the fixture as tests/fixtures/forklift-self-schema.md describes",
        forklift.display()
    );
    let out = tool(&forklift, &["version"]);
    assert!(out.status.success(), "{out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Forklift version"), "{stdout}");
}
