use std::collections::HashMap;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::prompt;

/// What an invocation grants through `--trust`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Grant {
    /// No `--trust`: a manifest not trusted at its current hash is asked
    /// about, or refused without a terminal.
    Ask,
    /// A bare `--trust`: accepts whatever is current when it runs, and warns
    /// with the hash it recorded. Kept for CI and pult-desktop's trust button;
    /// it binds to nothing a human reviewed.
    Current,
    /// `--trust=<hash>`: accepts the manifest only at exactly this hash, and
    /// refuses at any other — even one already trusted.
    Hash(String),
}

impl Grant {
    /// The grant the `--trust` argument from [`arg`] carries.
    pub fn from_matches(matches: &clap::ArgMatches) -> Grant {
        match matches.get_one::<String>("trust") {
            Some(hash) => Grant::Hash(hash.clone()),
            None if matches.contains_id("trust") => Grant::Current,
            None => Grant::Ask,
        }
    }
}

/// The `--trust[=<hash>]` argument, shared by `pult` and `pult x`. The value
/// needs `=`, so a bare `--trust` never swallows the word after it.
pub fn arg(subject: &str) -> clap::Arg {
    clap::Arg::new("trust")
        .long("trust")
        .value_name("HASH")
        .num_args(0..=1)
        .require_equals(true)
        .value_parser(parse_hash)
        .help(format!(
            "Trust {subject} without prompting: --trust=<HASH> only at that trust hash; \
             a bare --trust at whatever it is now (for CI; warns)"
        ))
}

/// A `--trust=<hash>` value: a full trust hash, 64 hex characters. Uppercase
/// is accepted and lowered, so a pasted hash compares equal; anything else is
/// a usage error rather than a refusal that reads as if the manifest moved.
fn parse_hash(value: &str) -> std::result::Result<String, String> {
    if value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(value.to_ascii_lowercase())
    } else {
        Err(
            "expected a full trust hash: 64 hex characters, as `pult --list --json` \
             reports in `trust_hash`"
                .to_string(),
        )
    }
}

/// Why trust is being asked for, or was refused — the error document's
/// `details.what_changed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WhatChanged {
    /// Nothing was ever trusted under this subject.
    FirstContact,
    /// A different hash was trusted under this subject.
    Changed,
    /// `--trust=<hash>` named a hash that is not the current one.
    Unexpected,
}

impl WhatChanged {
    fn as_str(self) -> &'static str {
        match self {
            WhatChanged::FirstContact => "first_contact",
            WhatChanged::Changed => "changed",
            WhatChanged::Unexpected => "unexpected",
        }
    }
}

/// A `trust_changed` refusal, carrying the hash a later `--trust=<hash>` can
/// name: `{subject, current, previous?, what_changed}`.
fn refusal(
    subject: &str,
    current: &str,
    previous: Option<&str>,
    what_changed: WhatChanged,
    message: String,
) -> anyhow::Error {
    let mut details = serde_json::json!({ "subject": subject, "current": current });
    if let Some(previous) = previous {
        details["previous"] = previous.into();
    }
    details["what_changed"] = what_changed.as_str().into();
    crate::machine::coded_with_details(
        crate::machine::ErrorCode::TrustChanged,
        details,
        anyhow::anyhow!(message),
    )
}

/// Trust-on-first-use for discovered manifests (see spec §7): a pult.yaml is a
/// list of things to *execute*, so the first time we see one — and any time it
/// or anything it includes changes — the user must approve it before anything
/// runs. The hash covers the resolved whole (root + every include).
pub fn ensure_trusted(
    path: &Path,
    resolved_hash: &str,
    includes: &[String],
    grant: &Grant,
    about_to_run: Option<&str>,
) -> Result<()> {
    let store_path = store_path()?;
    let mut store = load_store(&store_path)?;
    let key = path.to_string_lossy().into_owned();
    let previous = store.get(&key).cloned();

    if let Grant::Hash(named) = grant
        && named != resolved_hash
    {
        return Err(refusal(
            &key,
            resolved_hash,
            previous.as_deref(),
            WhatChanged::Unexpected,
            format!(
                "--trust={named} does not match manifest {}, whose trust hash is \
                 {resolved_hash}; review the manifest as it is now before trusting that hash",
                path.display()
            ),
        ));
    }
    if previous.as_deref() == Some(resolved_hash) {
        return Ok(());
    }
    let what_changed = if previous.is_some() {
        WhatChanged::Changed
    } else {
        WhatChanged::FirstContact
    };
    match grant {
        Grant::Hash(_) => {}
        Grant::Current => eprintln!(
            "pult: warning: a bare --trust accepted manifest {} at whatever it is now; \
             recorded trust hash {resolved_hash}. Pass --trust={resolved_hash} to accept \
             only the content you reviewed.",
            path.display()
        ),
        Grant::Ask => {
            if !std::io::stdin().is_terminal() {
                let state = match what_changed {
                    WhatChanged::Changed => "it has CHANGED since you trusted it",
                    _ => "first contact",
                };
                return Err(refusal(
                    &key,
                    resolved_hash,
                    previous.as_deref(),
                    what_changed,
                    format!(
                        "manifest {} is not trusted ({state}); its trust hash is {resolved_hash}. \
                         Run this once interactively to review it, or pass \
                         --trust={resolved_hash} once you have reviewed it",
                        path.display()
                    ),
                ));
            }
            let question = build_question(
                path,
                resolved_hash,
                previous.as_deref(),
                includes,
                about_to_run,
            );
            if !prompt::confirm(&question)? {
                return Err(refusal(
                    &key,
                    resolved_hash,
                    previous.as_deref(),
                    what_changed,
                    "manifest not trusted — nothing was run".to_string(),
                ));
            }
        }
    }
    store.insert(key, resolved_hash.to_string());
    save_store(&store_path, &store)
}

/// The trust question shown before anything runs. It always names what changed
/// (or that this is first contact), the full trust hash being accepted — the
/// value a later `--trust=<hash>` names — and every include; when a command is
/// about to run, it also shows the **composed script** so trust is granted while
/// looking at the actual command, not just a source name. Pure so the exact
/// wording is testable without a TTY.
fn build_question(
    path: &Path,
    current: &str,
    previous: Option<&str>,
    includes: &[String],
    about_to_run: Option<&str>,
) -> String {
    let mut question = match previous {
        Some(previous) => format!(
            "The manifest {} (or something it includes) has CHANGED since you trusted it.\n  \
             Trust hash: {current}\n  Was:        {previous}",
            path.display()
        ),
        None => format!(
            "First contact: the manifest {} is not trusted yet.\n  Trust hash: {current}",
            path.display()
        ),
    };
    if !includes.is_empty() {
        question.push_str("\n  It includes:");
        for source in includes {
            question.push_str(&format!("\n    · {source}"));
        }
    }
    if let Some(script) = about_to_run {
        question.push_str("\n  About to run:");
        for line in script.lines() {
            question.push_str(&format!("\n    {line}"));
        }
    }
    question.push_str("\n Commands in these files will be executed. Trust?");
    question
}

/// Read-only query: is this manifest currently trusted at exactly this hash?
/// Used by `--list --json` so tooling can tell whether invoking would prompt.
pub fn is_trusted(path: &Path, resolved_hash: &str) -> Result<bool> {
    let store = load_store(&store_path()?)?;
    let key = path.to_string_lossy();
    Ok(store.get(key.as_ref()).map(String::as_str) == Some(resolved_hash))
}

/// Overridable via PULT_TRUST_STORE (used by tests and CI).
fn store_path() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("PULT_TRUST_STORE") {
        return Ok(PathBuf::from(p));
    }
    let base = dirs::config_dir().context("could not determine the user config directory")?;
    Ok(base.join("pult").join("trust.json"))
}

fn load_store(path: &Path) -> Result<HashMap<String, String>> {
    match std::fs::read_to_string(path) {
        Ok(s) => serde_json::from_str(&s)
            .with_context(|| format!("corrupt trust store {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(HashMap::new()),
        Err(e) => Err(e).with_context(|| format!("failed to read {}", path.display())),
    }
}

fn save_store(path: &Path, store: &HashMap<String, String>) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(store)?;
    std::fs::write(path, json).with_context(|| format!("failed to write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Full-length hashes, distinct from each other: the question must show
    // every character of the one being accepted, never a prefix.
    const CURRENT: &str = "7f3c1e0a9b2d4c6e8f0a1b3c5d7e9f1a2b4c6d8e0f1a3b5c7d9e1f2a4b6c8d0e";
    const PREVIOUS: &str = "0d8c6b4a2f1e9d7c5b3a1f0e8d6c4b2a1f9e7d5c3b1a0f8e6c4d2b9a0e1c3f7a";

    #[test]
    fn store_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("trust.json");
        let mut store = HashMap::new();
        store.insert("/repo/pult.yaml".to_string(), "abc123".to_string());
        save_store(&path, &store).unwrap();
        let loaded = load_store(&path).unwrap();
        assert_eq!(loaded, store);
    }

    #[test]
    fn missing_store_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let loaded = load_store(&dir.path().join("absent.json")).unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn question_shows_the_command_about_to_run() {
        let q = build_question(
            Path::new("github.com/lonic/forklift@v1.2.0"),
            CURRENT,
            None,
            &[],
            Some("./bin/install '<flavor>'"),
        );
        assert!(q.contains("First contact: the manifest github.com/lonic/forklift@v1.2.0"));
        assert!(q.contains(&format!("Trust hash: {CURRENT}")), "got: {q}");
        assert!(
            !q.contains("Was:"),
            "first contact has no previous hash: {q}"
        );
        assert!(q.contains("About to run:"), "got: {q}");
        assert!(q.contains("./bin/install '<flavor>'"), "got: {q}");
        assert!(q.contains("Trust?"));
    }

    #[test]
    fn question_lists_includes_and_flags_a_change() {
        let q = build_question(
            Path::new("/repo/pult.yaml"),
            CURRENT,
            Some(PREVIOUS),
            &["github.com/opskit/aws-common@v1.4.2 (commit 8a6e6fd4a4)".to_string()],
            None,
        );
        assert!(q.contains("has CHANGED"), "got: {q}");
        assert!(q.contains(&format!("Trust hash: {CURRENT}")), "got: {q}");
        assert!(q.contains(&format!("Was:        {PREVIOUS}")), "got: {q}");
        assert!(q.contains("It includes:"), "got: {q}");
        assert!(q.contains("aws-common@v1.4.2"), "got: {q}");
        assert!(!q.contains("About to run:"), "no command → no preview: {q}");
    }
}
