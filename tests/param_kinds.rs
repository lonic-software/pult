//! The `flag` and `repeat` param kinds, driven through the real binary: how
//! each binds on the command line (`pult <command>` and `pult x` alike),
//! renders into the script, fills when absent, and which manifests the
//! resolver refuses.

use std::path::Path;
use std::process::{Command, Output, Stdio};

const MANIFEST: &str = r#"version: 1
commands:
  - id: go
    title: Go
    params:
      env: { pick: { options: [dev, prod] } }
      force: { flag: {} }
      dry: { flag: { arg: "-n" } }
      only: { repeat: { arg: "--only", max: 2 } }
      files: { repeat: { min: 1 } }
    run: "printf '%s|' {env} {force} {dry} {only} {files}"
"#;

struct Env {
    repo: tempfile::TempDir,
    state: tempfile::TempDir,
    trust: tempfile::TempDir,
}

impl Env {
    fn new(manifest: &str) -> Env {
        let env = Env {
            repo: tempfile::tempdir().unwrap(),
            state: tempfile::tempdir().unwrap(),
            trust: tempfile::tempdir().unwrap(),
        };
        std::fs::write(env.repo.path().join("pult.yaml"), manifest).unwrap();
        env
    }

    fn write(&self, rel: &str, content: &str) {
        let path = self.repo.path().join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn pult(&self, args: &[&str]) -> Output {
        self.pult_stdin(args, "")
    }

    fn pult_stdin(&self, args: &[&str], stdin: &str) -> Output {
        use std::io::Write;
        let mut child = Command::new(env!("CARGO_BIN_EXE_pult"))
            .current_dir(self.repo.path())
            .env("PULT_STATE_DIR", self.state.path())
            .env("PULT_TRUST_STORE", self.trust.path().join("trust.json"))
            .env(
                "PULT_USER_MANIFEST",
                self.repo.path().join("no-such-user-manifest.yaml"),
            )
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to run the pult binary");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The last line of stdout: `pult x` heads its output with the module it ran.
fn last_line(out: &Output) -> String {
    stdout(out).lines().last().unwrap_or_default().to_string()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn expect(out: &Output, code: i32) {
    assert_eq!(out.status.code(), Some(code), "{out:?}");
}

/// The error code of the one `--json` error document on stdout.
fn error_code(out: &Output) -> String {
    let doc: serde_json::Value = serde_json::from_str(stdout(out).trim())
        .unwrap_or_else(|e| panic!("not one JSON document ({e}): {out:?}"));
    doc["error"]["code"].as_str().unwrap().to_string()
}

#[test]
fn flags_and_repeats_render_as_separate_quoted_words() {
    let env = Env::new(MANIFEST);
    let out = env.pult(&[
        "go", "dev", "--force", "--only", "a", "--only", "b c", "f1", "f 2", "--print",
    ]);
    expect(&out, 0);
    assert_eq!(
        stdout(&out).trim(),
        "printf '%s|' dev --force  --only a --only 'b c' f1 'f 2'"
    );
}

/// Direct mode never prompts for what it can default: an absent flag is
/// off and an absent optional repeat is empty. stdin is not a terminal, so a
/// prompt would fail the run.
#[test]
fn direct_mode_defaults_an_absent_flag_and_an_optional_repeat() {
    let env = Env::new(MANIFEST);
    let out = env.pult(&["--trust", "go", "dev", "f1"]);
    expect(&out, 0);
    assert_eq!(stdout(&out), "dev|f1|");
}

#[test]
fn a_required_repeat_without_a_terminal_names_the_param() {
    let env = Env::new(MANIFEST);
    let out = env.pult(&["--trust", "go", "dev"]);
    expect(&out, 1);
    assert!(
        stderr(&out).contains("`files` needs at least 1 value(s)"),
        "{out:?}"
    );
}

/// Exit 2, pult's usage exit. A manifest command takes no `--json`, so the
/// message is the evidence the bound itself refused.
#[test]
fn a_count_outside_the_bounds_is_a_usage_error() {
    let env = Env::new(MANIFEST);
    let over = env.pult(&[
        "go", "dev", "--only", "a", "--only", "b", "--only", "c", "f1", "--print",
    ]);
    expect(&over, 2);
    assert!(
        stderr(&over).contains("`only` takes at most 2 value(s), but 3 were given"),
        "{over:?}"
    );

    let under = env.pult_stdin(
        &["go", "--params-json", "--print"],
        r#"{"env": "dev", "files": []}"#,
    );
    expect(&under, 2);
    assert!(
        stderr(&under).contains("`files` needs at least 1 value(s), but 0 were given"),
        "{under:?}"
    );
}

#[test]
fn params_json_is_typed_by_kind() {
    let env = Env::new(MANIFEST);
    let wrong = env.pult_stdin(
        &["go", "--params-json", "--print"],
        r#"{"env": "dev", "force": "yes", "files": ["x"]}"#,
    );
    expect(&wrong, 2);
    assert!(
        stderr(&wrong).contains("`force` must be a boolean"),
        "{wrong:?}"
    );

    let right = env.pult_stdin(
        &["go", "--params-json", "--print"],
        r#"{"env": "dev", "force": true, "only": ["o"], "files": ["x", "y z"]}"#,
    );
    expect(&right, 0);
    assert_eq!(
        stdout(&right).trim(),
        "printf '%s|' dev --force  --only o x 'y z'"
    );
}

#[test]
fn list_json_describes_flags_and_repeats() {
    let env = Env::new(MANIFEST);
    let out = env.pult(&["--list", "--json"]);
    expect(&out, 0);
    let doc: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    let params = &doc["data"]["commands"][0]["params"];
    assert_eq!(
        params[1],
        serde_json::json!({"name": "force", "kind": "flag", "arg": "--force"})
    );
    assert_eq!(
        params[2],
        serde_json::json!({"name": "dry", "kind": "flag", "arg": "-n"})
    );
    assert_eq!(
        params[3],
        serde_json::json!({"name": "only", "kind": "repeat", "arg": "--only",
                           "min": 0, "max": 2, "secret": false})
    );
    assert_eq!(
        params[4],
        serde_json::json!({"name": "files", "kind": "repeat", "arg": null,
                           "min": 1, "max": null, "secret": false})
    );
}

/// The journal stays strings: a flag as `true`/`false`, a repeat as JSON
/// array text, a secret repeat redacted whole.
#[test]
fn the_journal_records_typed_values_as_strings() {
    let env = Env::new(
        r#"version: 1
commands:
  - id: go
    title: Go
    params:
      force: { flag: {} }
      keys: { repeat: { arg: "--key", secret: true } }
      files: { repeat: {} }
    run: "true {force} {keys} {files}"
"#,
    );
    let out = env.pult(&[
        "--trust",
        "go",
        "--keys",
        "hunter2-super-secret",
        "a",
        "b c",
        "--run-id",
        "r1",
    ]);
    expect(&out, 0);
    let meta = find(env.state.path(), "meta.json").expect("no meta.json journaled");
    let meta: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(meta).unwrap()).unwrap();
    assert_eq!(
        meta["params"],
        serde_json::json!({"force": "false", "keys": "<redacted>", "files": "[\"a\",\"b c\"]"})
    );
}

fn find(dir: &Path, name: &str) -> Option<std::path::PathBuf> {
    for entry in std::fs::read_dir(dir).ok()? {
        let path = entry.ok()?.path();
        if path.is_dir() {
            if let Some(found) = find(&path, name) {
                return Some(found);
            }
        } else if path.file_name().is_some_and(|n| n == name) {
            return Some(path);
        }
    }
    None
}

/// Every manifest the resolver must refuse, each as `invalid_manifest`.
#[test]
fn the_resolver_refuses_bindings_that_cannot_work() {
    let cases = [
        ("a reserved name", "print: { flag: {} }", "reserved"),
        (
            "two positional repeats",
            "a: { repeat: {} }\n      b: { repeat: {} }",
            "without an `arg`",
        ),
        (
            "a positional repeat before a positional",
            "a: { repeat: {} }\n      b: { input: {} }",
            "last",
        ),
        (
            "from: naming a flag",
            "f: { flag: {} }\n      p: { pick: { from: \"echo {f}\" } }",
            "only pick and input",
        ),
        ("max below min", "a: { repeat: { min: 2, max: 1 } }", "max"),
        ("an empty arg", "a: { flag: { arg: \"\" } }", "arg"),
    ];
    for (what, params, needle) in cases {
        let env = Env::new(&format!(
            "version: 1\ncommands:\n  - id: c\n    title: C\n    params:\n      {params}\n    run: \"true\"\n"
        ));
        let out = env.pult(&["--list", "--json"]);
        assert_eq!(out.status.code(), Some(1), "{what}: {out:?}");
        assert_eq!(error_code(&out), "invalid_manifest", "{what}");
        let doc: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
        let message = doc["error"]["message"].as_str().unwrap();
        assert!(message.contains(needle), "{what}: {message}");
    }
}

/// A flag's default `arg` is its key in the file that declares it: an
/// include prefix and a `use:` rename change what the command line calls
/// it, never what the tool receives.
#[test]
fn a_flag_default_arg_survives_a_prefixed_include_and_a_rename() {
    let env = Env::new(
        r#"version: 1
includes:
  - source: ./mod
    prefix: m
commands:
  - id: go
    title: Go
    params:
      yes: { use: m:force }
    run: "echo {yes}"
"#,
    );
    env.write(
        "mod/pult.module.yaml",
        "version: 1\nparams:\n  force: { flag: {} }\ncommands: []\n",
    );
    let out = env.pult(&["go", "--yes", "--print"]);
    expect(&out, 0);
    assert_eq!(stdout(&out).trim(), "echo --force");
}

#[test]
fn pult_x_binds_a_command_exactly_as_pult_does() {
    let env = Env::new(MANIFEST);
    env.write("mod/pult.module.yaml", MANIFEST);

    let out = env.pult(&["x", "./mod", "go", "dev", "--dry", "f1", "--print"]);
    expect(&out, 0);
    assert_eq!(last_line(&out), "printf '%s|' dev  -n  f1");

    // `pult x`'s own switches bind before the command too.
    let out = env.pult(&["x", "./mod", "--print", "go", "dev", "f1"]);
    expect(&out, 0);
    assert_eq!(last_line(&out), "printf '%s|' dev    f1");

    // An argument the command doesn't take is refused, never dropped.
    let out = env.pult(&["x", "./mod", "go", "dev", "--nope", "--print"]);
    expect(&out, 2);

    let out = env.pult(&["x", "./mod", "go", "dev", "f1", "--var", "a=b"]);
    expect(&out, 2);
    assert!(
        stderr(&out).contains("--var must come before the command"),
        "{out:?}"
    );

    // Wherever it falls among the command's arguments.
    let out = env.pult(&["x", "./mod", "go", "--var", "a=b", "dev", "f1"]);
    expect(&out, 2);
    assert!(
        stderr(&out).contains("--var must come before the command"),
        "{out:?}"
    );

    // `--` reaches the command's binding, so a dash-led value binds the same.
    let direct = env.pult(&["go", "--print", "--", "dev", "-f1"]);
    expect(&direct, 0);
    let out = env.pult(&["x", "./mod", "--print", "go", "--", "dev", "-f1"]);
    expect(&out, 0);
    assert_eq!(last_line(&out), stdout(&direct).trim());

    let out = env.pult(&["x", "./mod", "go", "--help"]);
    expect(&out, 0);
    assert!(stdout(&out).contains("--only <ONLY>"), "{out:?}");

    // Before the command, `--help` is `pult x`'s own.
    let out = env.pult(&["x", "--help"]);
    expect(&out, 0);
    assert!(stdout(&out).contains("<SOURCE>"), "{out:?}");
}
