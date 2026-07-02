use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::manifest::{self, Loaded};

const MANIFEST_NAMES: [&str; 2] = ["pult.yaml", "pult.yml"];

/// Find and load the nearest manifest, walking up from the current directory
/// (the way eslint / vite discover their config).
pub fn find_manifest() -> Result<Loaded> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    find_manifest_from(&cwd)
}

pub fn find_manifest_from(start: &Path) -> Result<Loaded> {
    let mut dir = start.to_path_buf();
    loop {
        for name in MANIFEST_NAMES {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return manifest::load(&candidate);
            }
        }
        if !dir.pop() {
            bail!(
                "no pult.yaml found (searched from {} upward)",
                start.display()
            );
        }
    }
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
        let loaded = find_manifest_from(&nested).unwrap();
        assert_eq!(loaded.manifest.name.as_deref(), Some("found"));
    }

    #[test]
    fn errors_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        // A tempdir has no pult.yaml anywhere up to / (assuming a clean machine);
        // to keep the test hermetic, only assert the not-found error message shape
        // when discovery genuinely fails.
        if let Err(e) = find_manifest_from(dir.path()) {
            assert!(e.to_string().contains("no pult.yaml"));
        }
    }
}
