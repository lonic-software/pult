//! A trust grant binds to the hash a human was shown: the refusal and the
//! `--list --json` document carry the full trust hash, `--trust=<hash>`
//! accepts the manifest only at exactly that hash, and a bare `--trust`
//! still accepts whatever is current but says which hash it recorded. Each
//! case drives the real binary with stdin closed, so nothing can prompt.

use std::process::{Command, Output, Stdio};

const MANIFEST: &str = "version: 1\ncommands:\n  - id: hello\n    title: Hello\n    run: echo hi\n";
const EDITED: &str = "version: 1\ncommands:\n  - id: hello\n    title: Hello\n    run: echo bye\n";

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
        env.write_manifest(MANIFEST);
        env
    }

    fn write_manifest(&self, manifest: &str) {
        std::fs::write(self.repo.path().join("pult.yaml"), manifest).unwrap();
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

    /// The `--list --json` document, which never needs trust.
    fn list(&self) -> serde_json::Value {
        let out = self.pult(&["--list", "--json"]);
        assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
        serde_json::from_slice(&out.stdout).unwrap()
    }
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The `trust_changed` refusal's `details`, after checking the exit status
/// and the code.
fn refusal_details(out: &Output) -> serde_json::Value {
    assert_eq!(out.status.code(), Some(1), "stderr: {}", stderr(out));
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("stdout is not one JSON document ({e}): {}", stdout(out)));
    assert_eq!(doc["error"]["code"], "trust_changed", "{doc}");
    doc["error"]["details"].clone()
}

fn is_full_hash(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Every 64-hex-character word in `text`, in order.
fn hashes_in(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_ascii_hexdigit())
        .filter(|w| is_full_hash(w))
        .map(String::from)
        .collect()
}

#[test]
fn a_first_contact_refusal_carries_the_full_hash_that_list_reports() {
    let env = Env::new();
    let listed = env.list();
    let hash = listed["trust_hash"]
        .as_str()
        .expect("trust_hash is a string");
    assert!(is_full_hash(hash), "{listed}");
    assert_eq!(listed["trusted"], false);

    let out = env.pult(&["doctor", "--json"]);
    let details = refusal_details(&out);
    let manifest = listed["manifest"].as_str().unwrap();
    assert_eq!(
        details,
        serde_json::json!({
            "subject": manifest,
            "current": hash,
            "what_changed": "first_contact",
        })
    );
    let keys: Vec<_> = details.as_object().unwrap().keys().cloned().collect();
    assert_eq!(keys, ["subject", "current", "what_changed"]);
    // The prose names the same full hash, for a human reading a CI log.
    assert_eq!(hashes_in(&stderr(&out)), [hash, hash], "{}", stderr(&out));
}

#[test]
fn trust_naming_the_current_hash_accepts_it_without_a_warning() {
    let env = Env::new();
    let hash = env.list()["trust_hash"].as_str().unwrap().to_string();

    let out = env.pult(&[&format!("--trust={hash}"), "--list", "--json"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(stderr(&out), "", "a bound grant has nothing to warn about");
    assert_eq!(env.list()["trusted"], true);

    let out = env.pult(&["hello"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(stdout(&out).contains("hi"), "{}", stdout(&out));
}

#[test]
fn a_changed_manifest_refuses_naming_both_hashes() {
    let env = Env::new();
    let before = env.list()["trust_hash"].as_str().unwrap().to_string();
    let out = env.pult(&[&format!("--trust={before}"), "--list"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));

    env.write_manifest(EDITED);
    let after = env.list()["trust_hash"].as_str().unwrap().to_string();
    assert_ne!(before, after);

    let details = refusal_details(&env.pult(&["doctor", "--json"]));
    assert_eq!(details["current"], after.as_str());
    assert_eq!(details["previous"], before.as_str());
    assert_eq!(details["what_changed"], "changed");
}

/// The grant binds to what was reviewed: a hash that is no longer current is
/// refused, the store keeps the old hash, and the refusal names the new one.
#[test]
fn trust_naming_a_stale_hash_is_refused_and_records_nothing() {
    let env = Env::new();
    let before = env.list()["trust_hash"].as_str().unwrap().to_string();
    env.pult(&[&format!("--trust={before}"), "--list"]);
    env.write_manifest(EDITED);
    let after = env.list()["trust_hash"].as_str().unwrap().to_string();

    let out = env.pult(&[&format!("--trust={before}"), "--list", "--json"]);
    let details = refusal_details(&out);
    assert_eq!(details["what_changed"], "unexpected");
    assert_eq!(details["current"], after.as_str());
    assert_eq!(details["previous"], before.as_str());

    assert_eq!(env.list()["trusted"], false);
    let out = env.pult(&[&format!("--trust={before}"), "hello"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(!stdout(&out).contains("bye"), "the edited command ran");
}

/// A named hash is a pin, not only a grant: one that does not match is
/// refused even when the manifest is already trusted at its current hash.
#[test]
fn trust_naming_the_wrong_hash_is_refused_even_when_already_trusted() {
    let env = Env::new();
    env.pult(&["--trust", "--list"]);
    assert_eq!(env.list()["trusted"], true);

    let wrong = "0".repeat(64);
    let details = refusal_details(&env.pult(&[&format!("--trust={wrong}"), "doctor", "--json"]));
    assert_eq!(details["what_changed"], "unexpected");
    assert_eq!(details["previous"], details["current"]);
}

#[test]
fn a_bare_trust_still_accepts_and_warns_with_the_hash_it_recorded() {
    let env = Env::new();
    let hash = env.list()["trust_hash"].as_str().unwrap().to_string();

    let out = env.pult(&["--trust", "--list", "--json"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let warning = stderr(&out);
    assert!(warning.starts_with("pult: warning: "), "{warning}");
    assert_eq!(hashes_in(&warning), [hash.clone(), hash], "{warning}");
    assert_eq!(env.list()["trusted"], true);

    // Already trusted at this hash: nothing is accepted, so nothing to warn.
    let out = env.pult(&["--trust", "--list"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(stderr(&out), "");
}

/// The value needs `=`: a bare `--trust` before a command never takes the
/// command as its hash, and an empty value is a usage error.
#[test]
fn the_trust_value_needs_an_equals_sign() {
    let env = Env::new();
    let out = env.pult(&["--trust", "hello"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(stdout(&out).contains("hi"), "{}", stdout(&out));

    let out = env.pult(&["--trust=", "--list"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
}

#[test]
fn pult_x_binds_its_trust_the_same_way() {
    let env = Env::new();
    let module = env.repo.path().join("mod");
    std::fs::create_dir_all(&module).unwrap();
    std::fs::write(
        module.join("pult.module.yaml"),
        "version: 1\nname: demo-mod\ncommands:\n  - { id: hello, title: Say hi, run: \"echo from-mod\" }\n",
    )
    .unwrap();

    let out = env.pult(&["x", "./mod", "hello"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let named = hashes_in(&stderr(&out));
    assert_eq!(
        named.len(),
        2,
        "the refusal names the hash: {}",
        stderr(&out)
    );
    let hash = named[0].clone();

    let wrong = "f".repeat(64);
    let out = env.pult(&["x", "./mod", "hello", &format!("--trust={wrong}")]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(
        !stdout(&out).contains("from-mod"),
        "a wrong hash ran the module"
    );
    assert!(stderr(&out).contains(&hash), "{}", stderr(&out));

    let out = env.pult(&["x", "./mod", "hello", &format!("--trust={hash}")]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(stdout(&out).contains("from-mod"), "{}", stdout(&out));
    assert!(!stderr(&out).contains("warning"), "{}", stderr(&out));
}

#[test]
fn pult_x_bare_trust_warns_with_the_hash_it_recorded() {
    let env = Env::new();
    let module = env.repo.path().join("mod");
    std::fs::create_dir_all(&module).unwrap();
    std::fs::write(
        module.join("pult.module.yaml"),
        "version: 1\nname: demo-mod\ncommands:\n  - { id: hello, title: Say hi, run: \"echo from-mod\" }\n",
    )
    .unwrap();
    let refused = env.pult(&["x", "./mod", "hello"]);
    let hash = hashes_in(&stderr(&refused))[0].clone();

    let out = env.pult(&["x", "./mod", "hello", "--trust"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(stdout(&out).contains("from-mod"), "{}", stdout(&out));
    assert_eq!(
        hashes_in(&stderr(&out)),
        [hash.clone(), hash],
        "{}",
        stderr(&out)
    );
}
