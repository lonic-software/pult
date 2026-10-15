//! `requires:` end to end: the gate's place in a run (after trust, before
//! params), which definition wins, and what `pult doctor` reports. Each check
//! that matters writes its own marker file, so no marker can satisfy another
//! test's assertion.

use std::process::{Command, Output, Stdio};

struct Env {
    repo: tempfile::TempDir,
    state: tempfile::TempDir,
    trust: tempfile::TempDir,
}

impl Env {
    fn new(manifest: &str, files: &[(&str, &str)]) -> Env {
        let env = Env {
            repo: tempfile::tempdir().unwrap(),
            state: tempfile::tempdir().unwrap(),
            trust: tempfile::tempdir().unwrap(),
        };
        std::fs::write(env.repo.path().join("pult.yaml"), manifest).unwrap();
        for (rel, content) in files {
            let path = env.repo.path().join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
        env
    }

    /// Run pult with stdin not a terminal.
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

    fn marker(&self, name: &str) -> bool {
        self.repo.path().join(name).exists()
    }
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn assert_missing(out: &Output, names: &str) {
    assert_eq!(out.status.code(), Some(1), "{}", stderr(out));
    let err = stderr(out);
    assert!(
        err.contains("needs tools that are missing:") && err.contains(names),
        "{err}"
    );
}

/// T1: a picker runs inside `fill`, and the gate comes first — so a missing
/// tool is reported as missing, not as the picker's own failure.
#[test]
fn the_gate_runs_before_option_sources() {
    let env = Env::new(
        "version: 2\nrequires:\n  nope: { check: \"exit 1\" }\n  here: { check: \"true\" }\n\
         commands:\n\
         \x20 - id: missing\n    title: M\n    requires: [nope]\n\
         \x20   params: { env: { pick: { from: \"touch picker-missing; echo dev\" } } }\n\
         \x20   run: echo {env}\n\
         \x20 - id: present\n    title: P\n    requires: [here]\n\
         \x20   params: { env: { pick: { from: \"touch picker-present; echo dev\" } } }\n\
         \x20   run: echo {env}\n",
        &[],
    );
    let out = env.pult(&["--trust", "missing"]);
    assert_missing(&out, "nope");
    assert!(
        !env.marker("picker-missing"),
        "the picker ran before the gate"
    );

    // Paired: with the requirement met, the same picker does run (and the
    // run then stops at the prompt, since stdin is not a terminal).
    let out = env.pult(&["--trust", "present"]);
    assert_ne!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(env.marker("picker-present"), "{}", stderr(&out));
}

/// T2: a check is manifest code, so it runs only once the manifest is trusted.
#[test]
fn the_gate_runs_after_trust() {
    let env = Env::new(
        "version: 2\nrequires:\n  probe: { check: \"touch check-ran\" }\n\
         commands:\n  - { id: c, title: C, requires: [probe], run: \"true\" }\n",
        &[],
    );
    let out = env.pult(&["c"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(stderr(&out).contains("trust"), "{}", stderr(&out));
    assert!(
        !env.marker("check-ran"),
        "the check ran on an untrusted manifest"
    );

    // Paired: trusted, the check runs and passes.
    let out = env.pult(&["--trust", "c"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(env.marker("check-ran"));
}

/// T5: the root's definition wins over an include's. Both checks pass, so
/// only their markers say which one ran.
#[test]
fn the_root_definition_is_the_one_that_runs() {
    let env = Env::new(
        "version: 2\nincludes:\n  - { source: ./m }\n\
         requires:\n  node: { check: \"touch root-check\" }\n\
         commands:\n  - { id: c, title: C, requires: [node], run: \"true\" }\n",
        &[(
            "m/pult.module.yaml",
            "version: 2\nrequires:\n  node: { check: \"touch module-check\" }\ncommands: []\n",
        )],
    );
    let out = env.pult(&["--trust", "c"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(env.marker("root-check"));
    assert!(!env.marker("module-check"), "the shadowed definition ran");
}

/// T6: a root command that uses a module's step needs the module's tools.
#[test]
fn a_module_step_brings_its_requirements() {
    let env = Env::new(
        "version: 2\nincludes:\n  - { source: ./m, prefix: t }\ncommands:\n\
         \x20 - { id: uses, title: U, run: [ { use: \"t:greet\" } ] }\n\
         \x20 - { id: plain, title: P, run: \"echo plain-ran\" }\n",
        &[(
            "m/pult.module.yaml",
            "version: 2\nrequires:\n  nope: { check: \"exit 1\" }\n\
             steps:\n  greet: echo greet-ran\ncommands: []\n",
        )],
    );
    let out = env.pult(&["--trust", "uses"]);
    assert_missing(&out, "nope");
    assert!(!String::from_utf8_lossy(&out.stdout).contains("greet-ran"));

    // Paired: a root command taking nothing from the module runs.
    let out = env.pult(&["--trust", "plain"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(String::from_utf8_lossy(&out.stdout).contains("plain-ran"));
}

#[test]
fn doctor_reports_every_definition_and_fails_on_a_missing_one() {
    let env = Env::new(
        "version: 2\nincludes:\n  - { source: ./m }\n\
         requires:\n  here: { check: \"true\" }\n\
         commands:\n  - { id: c, title: C, requires: [here, gone], run: \"true\" }\n",
        &[(
            "m/pult.module.yaml",
            "version: 2\nrequires:\n  gone: { check: \"exit 7\" }\n  here: { check: \"false\" }\n\
             commands: []\n",
        )],
    );
    let out = env.pult(&["--trust", "doctor", "--json"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let reqs = &doc["data"]["requirements"];
    assert_eq!(reqs[0]["name"], "here");
    assert_eq!(reqs[0]["defined_in"], serde_json::Value::Null);
    assert_eq!(reqs[0]["ready"], true);
    assert_eq!(reqs[0]["shadowed"], serde_json::json!(["./m"]));
    assert_eq!(reqs[0]["needed_by"], serde_json::json!(["c"]));
    assert_eq!(reqs[1]["name"], "gone");
    assert_eq!(reqs[1]["defined_in"], "./m");
    assert_eq!(reqs[1]["ready"], false);
    assert_eq!(reqs[1]["exit_code"], 7);
    assert_eq!(reqs[1]["resolved_path"], serde_json::Value::Null);
    assert_eq!(reqs[1]["install"], serde_json::Value::Null);
    assert_eq!(reqs.as_array().unwrap().len(), 2);
    // The command's own row says it would not run, and why.
    let cmd = &doc["data"]["commands"][0];
    assert_eq!(cmd["ready"], false);
    assert_eq!(cmd["exit_code"], serde_json::Value::Null);
    assert_eq!(cmd["missing"], serde_json::json!(["gone"]));

    // Listing runs no check, and reports the same definitions.
    let out = env.pult(&["--list", "--json"]);
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        doc["data"]["commands"][0]["requires"],
        serde_json::json!(["here", "gone"])
    );
    assert_eq!(doc["data"]["requirements"][1]["check"], "exit 7");
}

/// A v1 file using `requires:` is refused with the version message, not run.
#[test]
fn a_v1_file_using_requires_is_refused() {
    let env = Env::new(
        "version: 1\nrequires:\n  node: {}\ncommands:\n  - { id: c, title: C, run: \"true\" }\n",
        &[],
    );
    let out = env.pult(&["--trust", "c"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("`requires:` needs `version: 2`"),
        "{}",
        stderr(&out)
    );
}

/// A command whose own check passes but whose tool is missing is not ready;
/// a missing tool no command needs is reported but does not fail doctor.
#[test]
fn doctor_readiness_is_whether_each_command_would_run() {
    let env = Env::new(
        "version: 2\nrequires:\n  aws: { check: \"exit 1\" }\n  later: { check: \"exit 1\" }\n\
         commands:\n  - { id: deploy, title: D, check: \"true\", requires: [aws], run: \"true\" }\n",
        &[],
    );
    let out = env.pult(&["--trust", "doctor", "--json"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["data"]["commands"][0]["exit_code"], 0);
    assert_eq!(doc["data"]["commands"][0]["ready"], false);

    let env = Env::new(
        "version: 2\nrequires:\n  later: { check: \"exit 1\" }\n\
         commands:\n  - { id: c, title: C, check: \"true\", run: \"true\" }\n",
        &[],
    );
    let out = env.pult(&["--trust", "doctor", "--json"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["data"]["requirements"][0]["ready"], false);
    assert_eq!(
        doc["data"]["requirements"][0]["needed_by"],
        serde_json::json!([])
    );
}
