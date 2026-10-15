//! Under `--json`, a failure ends in exactly one error document on stdout,
//! classified by `error.code`, in place of the prose line pult prints on
//! stderr without `--json`. Each case drives the real binary, because most of these
//! failures happen before (or inside) the argument parse that would
//! otherwise tell pult whether JSON was asked for.

use std::process::{Command, Output, Stdio};

const MANIFEST: &str = "version: 1\ncommands:\n  - id: hello\n    title: Hello\n    run: echo hi\n";

struct Env {
    repo: tempfile::TempDir,
    state: tempfile::TempDir,
    trust: tempfile::TempDir,
    cache: tempfile::TempDir,
}

impl Env {
    fn new(manifest: Option<&str>) -> Env {
        let env = Env {
            repo: tempfile::tempdir().unwrap(),
            state: tempfile::tempdir().unwrap(),
            trust: tempfile::tempdir().unwrap(),
            cache: tempfile::tempdir().unwrap(),
        };
        if let Some(m) = manifest {
            std::fs::write(env.repo.path().join("pult.yaml"), m).unwrap();
        }
        env
    }

    fn pult(&self, args: &[&str]) -> Output {
        self.command(args)
            .output()
            .expect("failed to run the pult binary")
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_pult"));
        cmd.current_dir(self.repo.path())
            .env("PULT_STATE_DIR", self.state.path())
            .env("PULT_TRUST_STORE", self.trust.path().join("trust.json"))
            .env("PULT_CACHE_DIR", self.cache.path())
            .env(
                "PULT_USER_MANIFEST",
                self.repo.path().join("no-such-user-manifest.yaml"),
            )
            .args(args)
            .stdin(Stdio::null())
            .stderr(Stdio::piped());
        cmd
    }
}

/// The one error document on stdout, checked for the shared envelope header
/// and the code; returns the document.
fn error_doc(out: &Output, command: Option<&str>, code: &str, exit: i32) -> serde_json::Value {
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(exit),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    let doc: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("stdout is not one JSON document ({e}): {stdout}"));
    // The document replaces the prose line: nothing of pult's own on stderr.
    assert!(stderr.is_empty(), "{stderr}");
    assert_eq!(doc["lonic_json"], 1, "{doc}");
    assert_eq!(doc["tool"], "pult", "{doc}");
    assert_eq!(doc["schema"], 2, "{doc}");
    assert_eq!(doc["ok"], false, "{doc}");
    assert_eq!(
        doc["command"],
        command.map_or(serde_json::Value::Null, Into::into),
        "{doc}"
    );
    assert_eq!(doc["error"]["code"], code, "{doc}");
    doc
}

/// The prose the document replaced: the same invocation without `--json`
/// prints `prose(message)` on stderr, with stdout empty and the same exit
/// status as the document's run.
fn assert_prose_without_json(
    env: &Env,
    args: &[&str],
    doc: &serde_json::Value,
    exit: i32,
    prose: impl Fn(&str) -> String,
) {
    let message = doc["error"]["message"].as_str().unwrap();
    let without: Vec<&str> = args.iter().copied().filter(|a| *a != "--json").collect();
    let without = env.pult(&without);
    assert_eq!(
        String::from_utf8_lossy(&without.stderr).trim_end(),
        prose(message)
    );
    assert_eq!(without.status.code(), Some(exit));
    assert!(
        without.stdout.is_empty(),
        "{}",
        String::from_utf8_lossy(&without.stdout)
    );
}

fn pult_error(message: &str) -> String {
    format!("pult: error: {message}")
}

#[test]
fn no_manifest_is_classified_on_every_machine_surface() {
    let env = Env::new(None);
    for (args, command) in [
        (&["--list", "--json"][..], "list"),
        (&["doctor", "--json"][..], "doctor"),
        (&["runs", "list", "--json"][..], "runs list"),
        (&["runs", "tail", "r1", "--json"][..], "runs tail"),
    ] {
        let out = env.pult(args);
        let doc = error_doc(&out, Some(command), "no_manifest", 1);
        let message = doc["error"]["message"].as_str().unwrap();
        assert!(message.starts_with("no pult.yaml found"), "{message}");
        // `message` carries the same text as the prose line it replaces.
        assert_prose_without_json(&env, args, &doc, 1, pult_error);
    }
}

#[test]
fn an_invalid_manifest_is_classified_whether_it_fails_to_parse_or_to_resolve() {
    // Fails in `manifest::load` (unsupported version).
    let env = Env::new(Some("version: 9\n"));
    let doc = error_doc(
        &env.pult(&["--list", "--json"]),
        Some("list"),
        "invalid_manifest",
        1,
    );
    assert_prose_without_json(&env, &["--list", "--json"], &doc, 1, pult_error);

    // Parses, but fails in resolution (an include that does not exist).
    let env = Env::new(Some(
        "version: 1\nincludes:\n  - source: ./missing.yaml\ncommands: []\n",
    ));
    let doc = error_doc(
        &env.pult(&["doctor", "--json"]),
        Some("doctor"),
        "invalid_manifest",
        1,
    );
    assert_prose_without_json(&env, &["doctor", "--json"], &doc, 1, pult_error);
}

/// A fetch failure is raised inside manifest resolution, whose boundary marks
/// everything `invalid_manifest` — the more specific code must survive it.
#[test]
fn a_git_module_that_cannot_be_fetched_is_fetch_failed_not_invalid_manifest() {
    let missing = tempfile::tempdir().unwrap();
    let url = missing.path().join("no-such-repo");
    let env = Env::new(Some(&format!(
        "version: 1\nincludes:\n  - source: git::file://{}@v1.0.0\ncommands: []\n",
        url.display()
    )));
    let out = env.pult(&["--list", "--json"]);
    let doc = error_doc(&out, Some("list"), "fetch_failed", 1);
    assert!(
        doc["error"]["message"]
            .as_str()
            .unwrap()
            .contains("failed to fetch git module"),
        "{doc}"
    );
    assert_prose_without_json(&env, &["--list", "--json"], &doc, 1, pult_error);
}

#[test]
fn an_untrusted_manifest_without_a_terminal_is_trust_changed() {
    let env = Env::new(Some(MANIFEST));
    let out = env.pult(&["doctor", "--json"]);
    let doc = error_doc(&out, Some("doctor"), "trust_changed", 1);
    assert!(
        doc["error"]["message"]
            .as_str()
            .unwrap()
            .contains("is not trusted"),
        "{doc}"
    );
    assert_prose_without_json(&env, &["doctor", "--json"], &doc, 1, pult_error);
}

#[test]
fn a_command_line_clap_rejects_is_a_usage_document_with_clap_text() {
    let env = Env::new(Some(MANIFEST));
    let out = env.pult(&["--list", "--json", "--bogus"]);
    let doc = error_doc(&out, Some("list"), "usage", 2);
    let usage = doc["error"]["details"]["usage"].as_str().unwrap();
    assert!(usage.contains("--bogus"), "{doc}");
    // The document keeps all of clap's own text, which is what it prints
    // without `--json` (its usage line echoes the arguments it was given).
    assert_prose_without_json(&env, &["--list", "--json", "--bogus"], &doc, 2, |_| {
        usage.replace("pult --list --json", "pult --list")
    });
    assert_eq!(
        doc["error"]["message"],
        "unexpected argument '--bogus' found"
    );

    // A failure clap describes over several lines keeps all of them.
    let out = env.pult(&["runs", "tail", "--json"]);
    let doc = error_doc(&out, Some("runs tail"), "usage", 2);
    let message = doc["error"]["message"].as_str().unwrap();
    assert!(message.contains("<RUN_ID>"), "{doc}");

    // Help is not a failure: no document, clap's help on stdout, exit 0.
    let out = env.pult(&["doctor", "--json", "--help"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(!String::from_utf8_lossy(&out.stdout).contains("lonic_json"));
}

#[test]
fn pults_own_usage_lines_are_usage_documents_in_place_of_their_prose() {
    let env = Env::new(Some(MANIFEST));
    let out = env.pult(&["self", "--json"]);
    // `self` names no machine surface.
    let doc = error_doc(&out, None, "usage", 2);
    assert_eq!(doc["error"]["message"], "usage: pult self schema");
    // Without `--json` it is printed bare, as pult always has — no
    // `pult: error:` prefix.
    assert_prose_without_json(&env, &["self", "--json"], &doc, 2, str::to_string);
}

#[test]
fn an_unknown_run_is_generic() {
    let env = Env::new(Some(MANIFEST));
    let out = env.pult(&["runs", "tail", "nope", "--json"]);
    let doc = error_doc(&out, Some("runs tail"), "generic", 1);
    assert_prose_without_json(
        &env,
        &["runs", "tail", "nope", "--json"],
        &doc,
        1,
        pult_error,
    );
}

/// A `--json` after `--` belongs to the command, not to pult.
#[test]
fn json_after_the_separator_does_not_ask_for_a_document() {
    let env = Env::new(None);
    let out = env.pult(&["--list", "--", "--json"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        out.stdout.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
}

/// With stdout closed, the document cannot be written; the failure still
/// exits with its own status rather than a panic (exit 101), and is reported
/// as the prose line instead.
#[test]
fn a_closed_stdout_does_not_turn_the_failure_into_a_panic() {
    let env = Env::new(None);
    let mut child = env.command(&["--list", "--json"]);
    let out = child.stdout(Stdio::piped()).spawn().unwrap();
    // Close our end of the pipe before pult writes to it.
    let mut out = out;
    drop(out.stdout.take());
    let out = out.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(!stderr.contains("panicked"), "{stderr}");
    assert!(
        stderr.starts_with("pult: error: no pult.yaml found"),
        "{stderr}"
    );
}

/// A damaged module cache is a fetch problem, not an authoring one.
#[test]
fn a_corrupt_module_cache_entry_is_fetch_failed() {
    let env = Env::new(Some(MANIFEST));
    let source = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        assert!(
            Command::new("git")
                .current_dir(source.path())
                .args(args)
                .output()
                .unwrap()
                .status
                .success()
        );
    };
    std::fs::write(
        source.path().join("pult.module.yaml"),
        "version: 1\ncommands: []\n",
    )
    .unwrap();
    git(&["init", "-q"]);
    git(&["add", "."]);
    git(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@t",
        "commit",
        "-qm",
        "m",
    ]);
    git(&["tag", "v1.0.0"]);
    std::fs::write(
        env.repo.path().join("pult.yaml"),
        format!(
            "version: 1\nincludes:\n  - source: git::file://{}@v1.0.0\n{}",
            source.path().display(),
            MANIFEST.trim_start_matches("version: 1\n")
        ),
    )
    .unwrap();
    // Warm the cache, then damage every entry's metadata.
    let warm = env.pult(&["--list", "--json"]);
    assert_eq!(
        warm.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&warm.stderr)
    );
    for entry in std::fs::read_dir(env.cache.path()).unwrap() {
        let meta = entry.unwrap().path().join("meta.json");
        if meta.exists() {
            std::fs::write(meta, "{").unwrap();
        }
    }
    error_doc(
        &env.pult(&["--list", "--json"]),
        Some("list"),
        "fetch_failed",
        1,
    );
}
