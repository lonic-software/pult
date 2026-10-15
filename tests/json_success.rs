//! Under `--json`, a success is one document in the shared envelope at
//! schema 2, its payload under `data`. Each case drives the real binary.

use std::process::{Command, Output, Stdio};

const MANIFEST: &str = "version: 1\ncommands:\n  - id: hello\n    title: Hello\n    check: \"false\"\n    run: echo hi\n";

struct Env {
    repo: tempfile::TempDir,
    state: tempfile::TempDir,
    trust: tempfile::TempDir,
}

impl Env {
    fn new() -> Env {
        let env = Env {
            repo: tempfile::tempdir().unwrap(),
            state: tempfile::tempdir().unwrap(),
            trust: tempfile::tempdir().unwrap(),
        };
        std::fs::write(env.repo.path().join("pult.yaml"), MANIFEST).unwrap();
        env
    }

    fn pult(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_pult"))
            .current_dir(self.repo.path())
            .env("PULT_STATE_DIR", self.state.path())
            .env("PULT_TRUST_STORE", self.trust.path().join("trust.json"))
            .env(
                "PULT_USER_MANIFEST",
                self.repo.path().join("no-such-user-manifest.yaml"),
            )
            .args(args)
            .stdin(Stdio::null())
            .output()
            .expect("failed to run the pult binary")
    }

    /// Journal one run of `hello` under the id `r1`.
    fn journal_a_run(&self) {
        let out = self.pult(&["--trust", "hello", "--run-id", "r1"]);
        assert_eq!(out.status.code(), Some(0), "{out:?}");
    }
}

/// The one success document on stdout, with its header checked; returns `data`.
fn data(out: &Output, command: impl Into<serde_json::Value>, exit: i32) -> serde_json::Value {
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(exit), "{out:?}");
    let doc: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("stdout is not one JSON document ({e}): {stdout}"));
    let keys: Vec<&str> = doc
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        ["lonic_json", "tool", "schema", "command", "ok", "data"]
    );
    assert_eq!(doc["lonic_json"], 1);
    assert_eq!(doc["tool"], "pult");
    assert_eq!(doc["schema"], 2);
    assert_eq!(doc["command"], command.into());
    assert_eq!(doc["ok"], true);
    doc["data"].clone()
}

#[test]
fn list_wraps_its_document_in_the_envelope() {
    let env = Env::new();
    let data = data(&env.pult(&["--list", "--json"]), "list", 0);
    assert!(data.get("schema").is_none(), "{data}");
    assert_eq!(data["commands"][0]["id"], "hello");
    assert_eq!(data["pult_version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(data["trust_hash"].as_str().unwrap().len(), 64);
}

/// A failing readiness check is a result: `ok: true`, exit 1.
#[test]
fn doctor_with_a_failing_check_is_ok_and_exits_1() {
    let env = Env::new();
    let data = data(&env.pult(&["--trust", "doctor", "--json"]), "doctor", 1);
    assert!(data.get("schema").is_none(), "{data}");
    assert_eq!(data["commands"][0]["ready"], false);
}

#[test]
fn runs_list_is_an_object_holding_the_runs() {
    let env = Env::new();
    assert_eq!(
        data(&env.pult(&["runs", "list", "--json"]), "runs list", 0)["runs"],
        serde_json::json!([])
    );
    env.journal_a_run();
    let data = data(&env.pult(&["runs", "list", "--json"]), "runs list", 0);
    assert_eq!(data["runs"][0]["run_id"], "r1");
    assert_eq!(data["runs"][0]["crashed"], false);
}

#[test]
fn runs_tail_json_is_one_document_and_jsonl_is_the_raw_stream() {
    let env = Env::new();
    env.journal_a_run();
    let data = data(&env.pult(&["runs", "tail", "r1", "--json"]), "runs tail", 0);
    assert_eq!(data["run_id"], "r1");
    assert_eq!(data["skipped"], 0);
    let events = data["events"].as_array().unwrap();
    assert!(events.iter().any(|e| e["kind"] == "exit"), "{data}");

    let out = env.pult(&["runs", "tail", "r1", "--jsonl"]);
    assert_eq!(out.status.code(), Some(0));
    let lines: Vec<serde_json::Value> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(&lines, events);
}

/// A stream cannot be one document.
#[test]
fn runs_tail_json_refuses_follow_as_a_usage_error() {
    let env = Env::new();
    env.journal_a_run();
    let out = env.pult(&["runs", "tail", "r1", "--json", "--follow"]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["schema"], 2);
    assert_eq!(doc["ok"], false);
    assert_eq!(doc["error"]["code"], "usage");
}

/// Help and version are results, not failures: under `--json` each is a
/// success document carrying the text plain mode prints, exit 0. `command`
/// names the machine surface whose help it is, and is null at the top level
/// (a help flag before the subcommand asks for the top-level help).
#[test]
fn help_and_version_under_json_are_success_documents() {
    let env = Env::new();
    let null = serde_json::Value::Null;
    let version = format!("pult {}", env!("CARGO_PKG_VERSION"));

    let plain = env.pult(&["--version"]);
    assert_eq!(String::from_utf8_lossy(&plain.stdout).trim_end(), version);
    assert_eq!(
        data(&env.pult(&["--version", "--json"]), null.clone(), 0)["message"],
        version
    );
    assert_eq!(
        data(&env.pult(&["--json", "-V"]), null.clone(), 0)["message"],
        version
    );

    let top = data(&env.pult(&["--help", "--json"]), null.clone(), 0);
    let top = top["message"].as_str().unwrap();
    assert!(top.contains("Usage: pult [OPTIONS]"), "{top}");
    assert!(top.contains("hello"), "{top}");
    assert!(!top.ends_with('\n'), "{top:?}");

    for (args, command, usage) in [
        (
            &["doctor", "--help", "--json"][..],
            "doctor",
            "Usage: pult doctor",
        ),
        (
            &["--trust", "doctor", "-h", "--json"][..],
            "doctor",
            "Usage: pult doctor",
        ),
        (
            &["--json", "doctor", "--help"][..],
            "doctor",
            "Usage: pult doctor",
        ),
        (
            &["--run-id", "abc", "doctor", "-h", "--json"][..],
            "doctor",
            "Usage: pult doctor",
        ),
        (
            &["runs", "list", "--help", "--json"][..],
            "runs list",
            "Usage: pult runs list",
        ),
        (
            &["runs", "tail", "--help", "--json"][..],
            "runs tail",
            "Usage: pult runs tail",
        ),
    ] {
        let help = data(&env.pult(args), command, 0);
        let help = help["message"].as_str().unwrap();
        assert!(help.contains(usage), "{args:?}: {help}");
    }
    for (args, usage) in [
        (&["--help", "doctor", "--json"][..], "Usage: pult [OPTIONS]"),
        (&["--list", "--help", "--json"][..], "Usage: pult [OPTIONS]"),
        (&["hello", "--help", "--json"][..], "Usage: pult hello"),
        (&["runs", "--help", "--json"][..], "Usage: pult runs"),
        (&["init", "--help", "--json"][..], "Usage: pult init"),
        (&["x", "--help", "--json"][..], "Usage: pult x"),
        // `--json` first: surfaces without a `--json` of their own still
        // answer the help rather than refusing `--json`.
        (&["hello", "--json", "--help"][..], "Usage: pult hello"),
        (&["runs", "--json", "--help"][..], "Usage: pult runs"),
        (&["init", "--json", "-h"][..], "Usage: pult init"),
        (&["x", "--json", "--help"][..], "Usage: pult x"),
        (&["--json", "--version"][..], "pult "),
    ] {
        let help = data(&env.pult(args), null.clone(), 0);
        let help = help["message"].as_str().unwrap();
        assert!(help.contains(usage), "{args:?}: {help}");
    }
}

/// The help and version pult answers itself when no manifest is found take
/// the same shape as clap's.
#[test]
fn help_and_version_without_a_manifest_are_success_documents_too() {
    let env = Env::new();
    std::fs::remove_file(env.repo.path().join("pult.yaml")).unwrap();
    let null = serde_json::Value::Null;
    let version = format!("pult {}", env!("CARGO_PKG_VERSION"));

    assert_eq!(
        data(&env.pult(&["--version", "--json"]), null.clone(), 0)["message"],
        version
    );
    let help = data(&env.pult(&["--help", "--json"]), null.clone(), 0);
    let help = help["message"].as_str().unwrap();
    assert!(help.contains("No manifest was found"), "{help}");

    let plain = env.pult(&["--help"]);
    assert_eq!(plain.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&plain.stdout).trim_end(), help);
}

/// Setting `--json` aside to find a help request must not turn a real
/// failure into a success: with no help or version on the line, or with an
/// unknown argument ahead of it, the line is still a usage error.
#[test]
fn a_failure_with_json_first_stays_a_usage_error() {
    let env = Env::new();
    for args in [
        &["hello", "--json"][..],
        &["hello", "--json", "--bogus", "--help"][..],
        &["init", "--json", "--bogus"][..],
    ] {
        let out = env.pult(args);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {out:?}");
        let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(doc["ok"], false, "{args:?}: {doc}");
        assert_eq!(doc["error"]["code"], "usage", "{args:?}: {doc}");
    }
}

/// With no manifest, only a top-level help or version is pult's to answer:
/// a subcommand's help, or a `-h` after `--`, is still `no_manifest`. The
/// first flag wins, as it does under clap.
#[test]
fn without_a_manifest_only_a_top_level_flag_is_answered_and_the_first_wins() {
    let env = Env::new();
    std::fs::remove_file(env.repo.path().join("pult.yaml")).unwrap();
    let null = serde_json::Value::Null;
    let help = data(
        &env.pult(&["--help", "--version", "--json"]),
        null.clone(),
        0,
    );
    assert!(
        help["message"]
            .as_str()
            .unwrap()
            .contains("No manifest was found"),
        "{help}"
    );
    let version = data(&env.pult(&["--json", "-Vh"]), null, 0);
    assert_eq!(
        version["message"],
        format!("pult {}", env!("CARGO_PKG_VERSION"))
    );

    for (args, command) in [
        (
            &["hello", "--json", "--", "-h"][..],
            serde_json::Value::Null,
        ),
        (
            &["doctor", "--help", "--json"][..],
            serde_json::json!("doctor"),
        ),
        (
            &["hello", "--version", "--json"][..],
            serde_json::Value::Null,
        ),
    ] {
        let out = env.pult(args);
        assert_eq!(out.status.code(), Some(1), "{args:?}: {out:?}");
        let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(doc["error"]["code"], "no_manifest", "{args:?}: {doc}");
        assert_eq!(doc["command"], command, "{args:?}: {doc}");
    }
}

/// A help or version answer nobody can read is not a panic (exit 101), in
/// plain mode or under `--json`, with or without a manifest. Stdout is a
/// pipe whose read end is closed before pult starts, so every write fails.
#[test]
fn help_and_version_to_a_closed_stdout_exit_0_without_a_panic() {
    let with = Env::new();
    let without = Env::new();
    std::fs::remove_file(without.repo.path().join("pult.yaml")).unwrap();
    for env in [&with, &without] {
        for args in [
            &["--help", "--json"][..],
            &["--version", "--json"][..],
            &["--help"][..],
            &["--version"][..],
        ] {
            let (reader, writer) = std::io::pipe().unwrap();
            drop(reader);
            let out = Command::new(env!("CARGO_BIN_EXE_pult"))
                .current_dir(env.repo.path())
                .env("PULT_STATE_DIR", env.state.path())
                .env("PULT_TRUST_STORE", env.trust.path().join("trust.json"))
                .env(
                    "PULT_USER_MANIFEST",
                    env.repo.path().join("no-such-user-manifest.yaml"),
                )
                .args(args)
                .stdin(Stdio::null())
                .stdout(writer)
                .output()
                .unwrap();
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert_eq!(out.status.code(), Some(0), "{args:?}: {stderr}");
            assert!(!stderr.contains("panicked"), "{args:?}: {stderr}");
        }
    }
}
