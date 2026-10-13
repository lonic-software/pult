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
fn data(out: &Output, command: &str, exit: i32) -> serde_json::Value {
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
    assert_eq!(doc["command"], command);
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
