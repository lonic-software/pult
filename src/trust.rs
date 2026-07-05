use std::collections::HashMap;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::prompt;

/// Trust-on-first-use for discovered manifests (see spec §7): a pult.yaml is a
/// list of things to *execute*, so the first time we see one — and any time it
/// or anything it includes changes — the user must approve it before anything
/// runs. The hash covers the resolved whole (root + every include).
pub fn ensure_trusted(
    path: &Path,
    resolved_hash: &str,
    includes: &[String],
    assume_yes: bool,
    about_to_run: Option<&str>,
) -> Result<()> {
    let store_path = store_path()?;
    let mut store = load_store(&store_path)?;
    let key = path.to_string_lossy().into_owned();

    let previous = store.get(&key);
    if previous.map(String::as_str) == Some(resolved_hash) {
        return Ok(());
    }
    if assume_yes {
        store.insert(key, resolved_hash.to_string());
        return save_store(&store_path, &store);
    }
    if !std::io::stdin().is_terminal() {
        bail!(
            "manifest {} is not trusted; run `pult` interactively once to review it, or pass --trust",
            path.display()
        );
    }

    let question = build_question(path, previous.is_some(), includes, about_to_run);
    let yes = prompt::confirm(&question)?;
    if !yes {
        bail!("manifest not trusted — nothing was run");
    }
    store.insert(key, resolved_hash.to_string());
    save_store(&store_path, &store)
}

/// The trust question shown before anything runs. It always names what changed
/// (or that this is first contact) and every include; when a command is about
/// to run, it also shows the **composed script** so trust is granted while
/// looking at the actual command, not just a source name. Pure so the exact
/// wording is testable without a TTY.
fn build_question(
    path: &Path,
    changed: bool,
    includes: &[String],
    about_to_run: Option<&str>,
) -> String {
    let mut question = if changed {
        format!(
            "The manifest {} (or something it includes) has CHANGED since you trusted it.",
            path.display()
        )
    } else {
        format!("Trust the manifest {}?", path.display())
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
            false,
            &[],
            Some("./bin/install '<flavor>'"),
        );
        assert!(q.contains("Trust the manifest github.com/lonic/forklift@v1.2.0?"));
        assert!(q.contains("About to run:"), "got: {q}");
        assert!(q.contains("./bin/install '<flavor>'"), "got: {q}");
        assert!(q.contains("Trust?"));
    }

    #[test]
    fn question_lists_includes_and_flags_a_change() {
        let q = build_question(
            Path::new("/repo/pult.yaml"),
            true,
            &["github.com/opskit/aws-common@v1.4.2 (commit 8a6e6fd4a4)".to_string()],
            None,
        );
        assert!(q.contains("has CHANGED"), "got: {q}");
        assert!(q.contains("It includes:"), "got: {q}");
        assert!(q.contains("aws-common@v1.4.2"), "got: {q}");
        assert!(!q.contains("About to run:"), "no command → no preview: {q}");
    }
}
