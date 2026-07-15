use std::path::Path;

use anyhow::{Context, Result, bail};
use clap::ArgAction;

use crate::discovery;

/// `pult init [--user]` — scaffold a starter manifest. Intercepted before
/// discovery (like `update`): its whole point is running where nothing
/// exists yet.
pub fn run_cli(rest: &[String]) -> Result<i32> {
    let matches = clap::Command::new("pult init")
        .bin_name("pult init")
        .about("Create a starter pult.yaml in the current directory")
        .arg(
            clap::Arg::new("user")
                .long("user")
                .action(ArgAction::SetTrue)
                .help("Create your user manifest (~/.config/pult/pult.yaml) instead"),
        )
        .get_matches_from(std::iter::once("pult init".to_string()).chain(rest.iter().cloned()));

    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let target = if matches.get_flag("user") {
        discovery::user_manifest_path().context("could not determine the user manifest location")?
    } else {
        cwd.join("pult.yaml")
    };
    let name = if matches.get_flag("user") {
        "personal".to_string()
    } else {
        cwd.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "my-project".to_string())
    };
    init_at(&target, &name)?;

    println!("✓  created {}", target.display());
    // A nested manifest is legitimate (monorepo subproject), but shadowing
    // should never be a surprise.
    if !matches.get_flag("user")
        && let Some(parent) = cwd.parent()
        && let Ok((shadowed, discovery::Scope::Repo)) = discovery::find_manifest_from(parent, None)
    {
        println!(
            "   note: this shadows {} for everything under {}",
            shadowed.path.display(),
            cwd.display()
        );
    }
    println!("   next: `pult` for the guided flow, `pult --list`, then edit away");
    println!("   tip: the file has a schema modeline — open it in an editor with the");
    println!("        YAML extension for completion and inline validation as you type");
    Ok(0)
}

/// Write the starter manifest — refusing to overwrite is the only guard a
/// scaffolder needs. The template must always load and resolve (tested).
pub fn init_at(target: &Path, name: &str) -> Result<()> {
    if target.exists() {
        bail!("{} already exists — nothing was written", target.display());
    }
    let sibling = target.with_extension("yml");
    if sibling.exists() {
        bail!("{} already exists — nothing was written", sibling.display());
    }
    if let Some(dir) = target.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("failed to create {}", dir.display()))?;
    }
    std::fs::write(target, template(name))
        .with_context(|| format!("failed to write {}", target.display()))?;
    Ok(())
}

fn template(name: &str) -> String {
    // The modeline gives editors running the YAML language server (VS Code's
    // Red Hat extension, yaml-language-server anywhere) live completion,
    // typo-flagging, and hover docs on this file. Both URLs are version-pinned
    // so they match this binary.
    format!(
        r#"# yaml-language-server: $schema={schema}
# pult manifest — authoring guide: {docs}
version: 1
name: {name}

commands:
  - id: hello
    title: Hello
    description: Say hello, greeting {{name}} by name.
    params:
      # input: free text · pick: a fixed list, or a shell command whose
      # stdout lines become the options ({{earlier-param}} may be referenced)
      name: {{ input: {{ default: world }} }}
      # env: {{ pick: {{ options: [dev, uat, prod] }} }}
    run: "echo hello {{name}}"

  # Multi-step commands compile to one fail-fast bash script — see
  # `pult <command> --print`. Real logic belongs in scripts the yaml calls.
  # - id: deploy
  #   title: Deploy
  #   run:
  #     - "./bin/build"
  #     - "./bin/deploy"

# Shared command sets from git, pinned to a tag — or let pult write this:
#   pult includes add github.com/org/repo[//dir] --prefix aws
# includes:
#   - source: github.com/org/ops-modules//aws@v1.0.0
#     prefix: aws
"#,
        schema = crate::schema_url(),
        docs = crate::docs_url(),
        name = name,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{manifest, resolver};

    #[test]
    fn template_loads_and_resolves() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("pult.yaml");
        init_at(&target, "demo").unwrap();
        let resolved = resolver::resolve(manifest::load(&target).unwrap()).unwrap();
        assert_eq!(resolved.name, "demo");
        assert_eq!(resolved.commands[0].id, "hello");
    }

    #[test]
    fn refuses_existing_manifest_either_spelling() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("pult.yaml");
        std::fs::write(dir.path().join("pult.yml"), "version: 1\n").unwrap();
        let err = init_at(&target, "x").unwrap_err().to_string();
        assert!(err.contains("already exists"), "got: {err}");

        std::fs::remove_file(dir.path().join("pult.yml")).unwrap();
        init_at(&target, "x").unwrap();
        let err = init_at(&target, "x").unwrap_err().to_string();
        assert!(err.contains("already exists"), "got: {err}");
    }

    #[test]
    fn creates_parent_dirs_for_user_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("config/pult/pult.yaml");
        init_at(&target, "personal").unwrap();
        assert!(target.is_file());
    }
}
