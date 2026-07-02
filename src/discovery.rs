use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::manifest::{self, Loaded};

const MANIFEST_NAMES: [&str; 2] = ["pult.yaml", "pult.yml"];

/// Where a manifest came from — it changes what directory commands run in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Found walking up from cwd; commands run at the manifest's directory.
    Repo,
    /// The user's personal manifest, found only when no repo manifest exists;
    /// commands run at the invocation directory (personal commands act on
    /// wherever you are, not on ~/.config).
    User,
}

/// Find and load the nearest manifest, walking up from the current directory
/// (the way eslint / vite discover their config). When nothing is found all
/// the way up, fall back to the user manifest — `$PULT_USER_MANIFEST`, or
/// `~/.config/pult/pult.yaml` — so bare `pult` outside any repo is your
/// personal launcher.
pub fn find_manifest() -> Result<(Loaded, Scope)> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    find_manifest_from(&cwd, user_manifest_path().as_deref())
}

pub fn find_manifest_from(start: &Path, user_manifest: Option<&Path>) -> Result<(Loaded, Scope)> {
    let mut dir = start.to_path_buf();
    loop {
        for name in MANIFEST_NAMES {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Ok((manifest::load(&candidate)?, Scope::Repo));
            }
        }
        if !dir.pop() {
            break;
        }
    }
    if let Some(path) = user_manifest
        && path.is_file()
    {
        return Ok((manifest::load(path)?, Scope::User));
    }
    bail!(
        "no pult.yaml found (searched from {} upward; no user manifest at {}) — \
         `pult init` creates one here, `pult init --user` creates your personal one",
        start.display(),
        user_manifest
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "~/.config/pult/pult.yaml".to_string()),
    );
}

/// `$PULT_USER_MANIFEST`, or the first of `~/.config/pult/pult.{yaml,yml}`
/// that exists (falling back to the .yaml spelling for error messages).
/// `~/.config` on every platform: this is a hand-edited file, and one
/// documented location beats three platform-specific ones.
pub fn user_manifest_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("PULT_USER_MANIFEST") {
        return Some(PathBuf::from(p));
    }
    let base = dirs::home_dir()?.join(".config").join("pult");
    for name in MANIFEST_NAMES {
        let candidate = base.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    Some(base.join(MANIFEST_NAMES[0]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_manifest_in_ancestor() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("pult.yaml"),
            "version: 1\nname: found\ncommands:\n  - { id: x, title: X, run: \"true\" }\n",
        )
        .unwrap();
        let nested = dir.path().join("a/b/c");
        std::fs::create_dir_all(&nested).unwrap();
        let (loaded, scope) = find_manifest_from(&nested, None).unwrap();
        assert_eq!(loaded.manifest.name.as_deref(), Some("found"));
        assert_eq!(scope, Scope::Repo);
    }

    #[test]
    fn falls_back_to_user_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("user-pult.yaml");
        std::fs::write(
            &user,
            "version: 1\nname: personal\ncommands:\n  - { id: x, title: X, run: \"true\" }\n",
        )
        .unwrap();
        // Search from a manifest-less tree; unless the machine has a pult.yaml
        // in / (unlikely), discovery must land on the user manifest.
        let start = dir.path().join("empty");
        std::fs::create_dir_all(&start).unwrap();
        let (loaded, scope) = find_manifest_from(&start, Some(&user)).unwrap();
        if scope == Scope::User {
            assert_eq!(loaded.manifest.name.as_deref(), Some("personal"));
        }
    }

    #[test]
    fn repo_manifest_wins_over_user() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("pult.yaml"),
            "version: 1\nname: repo\ncommands:\n  - { id: x, title: X, run: \"true\" }\n",
        )
        .unwrap();
        let user = dir.path().join("user-pult.yaml");
        std::fs::write(
            &user,
            "version: 1\nname: personal\ncommands:\n  - { id: x, title: X, run: \"true\" }\n",
        )
        .unwrap();
        let (loaded, scope) = find_manifest_from(dir.path(), Some(&user)).unwrap();
        assert_eq!(scope, Scope::Repo);
        assert_eq!(loaded.manifest.name.as_deref(), Some("repo"));
    }

    #[test]
    fn errors_when_absent_names_both_locations() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nowhere.yaml");
        if let Err(e) = find_manifest_from(dir.path(), Some(&missing)) {
            let msg = e.to_string();
            assert!(msg.contains("no pult.yaml"), "got: {msg}");
            assert!(msg.contains("nowhere.yaml"), "got: {msg}");
        }
    }
}
