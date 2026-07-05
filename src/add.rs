use std::io::IsTerminal;
use std::path::Path;

use anyhow::{Context, Result, bail};
use clap::{Arg, ArgAction};

use crate::fetch::{self, Source};
use crate::{discovery, manifest, prompt, resolver};

/// `pult includes add <source> [--prefix P] [--user]` — pin a module source
/// and append it to a manifest's includes. Intercepted before discovery (like
/// `update`), so `--user` works even where no manifest exists yet.
pub fn run_cli(rest: &[String]) -> Result<i32> {
    let matches = clap::Command::new("pult includes add")
        .bin_name("pult includes add")
        .about("Pin a module source and add it to a manifest's includes")
        .arg(Arg::new("source").required(true).value_name("SOURCE").help(
            "./path, host.tld/org/repo[//sub][@pin], or git::<url>[//sub][@pin] — \
                     a git source without a pin resolves to its latest version tag",
        ))
        .arg(
            Arg::new("prefix")
                .long("prefix")
                .value_name("PREFIX")
                .help("Namespace the module's exports as PREFIX:<name>"),
        )
        .arg(
            Arg::new("user")
                .long("user")
                .action(ArgAction::SetTrue)
                .help(
                    "Add to your user manifest (~/.config/pult/pult.yaml), creating it if needed",
                ),
        )
        .get_matches_from(
            std::iter::once("pult includes add".to_string()).chain(rest.iter().cloned()),
        );

    let source = matches.get_one::<String>("source").expect("required");
    let prefix = matches.get_one::<String>("prefix").map(String::as_str);
    let target = if matches.get_flag("user") {
        discovery::user_manifest_path().context("could not determine the user manifest location")?
    } else {
        let (loaded, _scope) = discovery::find_manifest()
            .context("nothing to add to — pass --user to target your user manifest")?;
        loaded.path
    };
    // Interactivity is decided HERE, once, and passed down — the core must
    // never sniff the terminal itself (under `cargo test` from a terminal,
    // stdin IS a TTY, and a library function that prompts on its own turns
    // the test suite interactive).
    let interactive = std::io::stdin().is_terminal();
    add_include(&target, source, prefix, None, interactive)?;
    Ok(0)
}

/// The whole flow: pin → fetch → summarize → (when `interactive`: prompt for
/// required vars and confirm) → write → verify the manifest still resolves,
/// rolling back if it doesn't. `cache_root: None` = the default module cache.
pub fn add_include(
    target: &Path,
    source: &str,
    prefix: Option<&str>,
    cache_root: Option<&Path>,
    interactive: bool,
) -> Result<()> {
    let target_dir = target
        .parent()
        .context("target manifest has no parent directory")?;

    // Pin the source and locate the module yaml for the summary.
    let (pinned_source, module_file) = match fetch::parse_source_lenient(source)? {
        Source::Local => {
            let t = target_dir.join(source);
            let file = if t.is_dir() {
                fetch::module_file_in(&t)
            } else {
                t
            };
            if !file.is_file() {
                bail!(
                    "no module at `{source}` (relative to {}) — expected {}",
                    target_dir.display(),
                    file.display()
                );
            }
            (source.to_string(), file)
        }
        Source::Git(mut git_src) => {
            if git_src.rev.is_empty() {
                let tag = fetch::latest_version_tag(&git_src.url)?.with_context(|| {
                    format!(
                        "no version tags found on {} — pin explicitly with `@<tag|sha>`",
                        git_src.url
                    )
                })?;
                println!("· latest version tag of {}: {tag}", git_src.url);
                git_src.display = format!("{source}@{tag}");
                git_src.rev = tag;
            }
            let default_cache;
            let root = match cache_root {
                Some(p) => p,
                None => {
                    default_cache = fetch::default_cache_root()?;
                    &default_cache
                }
            };
            let (checkout, _meta) = fetch::ensure_fetched(&git_src, root)?;
            let file = match &git_src.subpath {
                Some(p) if p.ends_with(".yaml") || p.ends_with(".yml") => checkout.join(p),
                Some(p) => fetch::module_file_in(&checkout.join(p)),
                None => fetch::module_file_in(&checkout),
            };
            (git_src.display.clone(), file)
        }
    };
    let module_raw = std::fs::read_to_string(&module_file)
        .with_context(|| format!("failed to read {}", module_file.display()))?;
    let module = manifest::parse(&module_raw, &module_file)?;

    // Refuse a second include of the same base source.
    let original = if target.is_file() {
        Some(
            std::fs::read_to_string(target)
                .with_context(|| format!("failed to read {}", target.display()))?,
        )
    } else {
        None
    };
    if let Some(text) = &original {
        let existing = manifest::parse(text, target)
            .with_context(|| format!("{} is not a valid manifest", target.display()))?;
        for inc in &existing.includes {
            if strip_pin(&inc.source) == strip_pin(&pinned_source) {
                bail!(
                    "already included as `{}` — edit that pin to upgrade instead",
                    inc.source
                );
            }
        }
    }

    // What this include brings.
    println!(
        "◆  {}{}",
        module.name.as_deref().unwrap_or("(unnamed module)"),
        module
            .description
            .as_deref()
            .map(|d| format!(" — {d}"))
            .unwrap_or_default()
    );
    let shown = |name: &str| match prefix {
        Some(p) => format!("{p}:{name}"),
        None => name.to_string(),
    };
    for cmd in &module.commands {
        println!("     command  {}  {}", shown(&cmd.id), cmd.title);
    }
    if !module.params.is_empty() || !module.steps.is_empty() {
        println!(
            "     blocks   {} named param(s), {} step(s)",
            module.params.len(),
            module.steps.len()
        );
    }

    // Required vars must be bound at the include site.
    let mut vars: Vec<(String, String)> = Vec::new();
    for (name, def) in &module.vars {
        if !def.required {
            continue;
        }
        if !interactive {
            bail!(
                "module requires var `{name}` — run interactively, or add the include by hand \
                 with a `vars:` binding"
            );
        }
        let message = match &def.description {
            Some(d) => format!("{name}? ({d})"),
            None => format!("{name}?"),
        };
        vars.push((name.clone(), prompt::text(&message, None)?));
    }

    println!("{}", render_entry("  ", &pinned_source, prefix, &vars));
    if interactive && !prompt::confirm(&format!("Add this include to {}?", target.display()))? {
        bail!("nothing was written");
    }

    let new_text = match &original {
        Some(text) => insert_include(text, &pinned_source, prefix, &vars)?,
        None => format!(
            "version: 1\nname: personal\n\nincludes:\n{}",
            render_entry("  ", &pinned_source, prefix, &vars)
        ),
    };
    if original.is_none() {
        std::fs::create_dir_all(target_dir)
            .with_context(|| format!("failed to create {}", target_dir.display()))?;
    }
    std::fs::write(target, &new_text)
        .with_context(|| format!("failed to write {}", target.display()))?;

    // The edit was textual — prove the result still loads and resolves, and
    // roll back if it doesn't (a broken manifest must never be left behind).
    let check = manifest::load(target).and_then(|l| resolver::resolve_with(l, cache_root));
    if let Err(e) = check {
        match &original {
            Some(text) => std::fs::write(target, text)?,
            None => {
                let _ = std::fs::remove_file(target);
            }
        }
        return Err(e).with_context(|| {
            format!(
                "{} would not resolve with the new include — rolled back",
                target.display()
            )
        });
    }

    println!("✓  added `{pinned_source}` to {}", target.display());
    println!("   the next run re-shows the trust prompt (the manifest changed)");
    Ok(())
}

/// `host/repo//sub@pin` → `host/repo//sub`; local paths pass through.
fn strip_pin(source: &str) -> &str {
    match source.rfind('@') {
        Some(i) if !source[i + 1..].contains('/') => &source[..i],
        _ => source,
    }
}

fn render_entry(
    indent: &str,
    source: &str,
    prefix: Option<&str>,
    vars: &[(String, String)],
) -> String {
    let mut out = format!("{indent}- source: {source}\n");
    if let Some(p) = prefix {
        out.push_str(&format!("{indent}  prefix: {p}\n"));
    }
    if !vars.is_empty() {
        let bindings: Vec<String> = vars
            .iter()
            .map(|(k, v)| format!("{k}: \"{}\"", v.replace('\\', "\\\\").replace('"', "\\\"")))
            .collect();
        out.push_str(&format!("{indent}  vars: {{ {} }}\n", bindings.join(", ")));
    }
    out
}

/// Append the entry into an existing manifest textually — comments and
/// formatting survive. New entries go first in the `includes:` block (any
/// duplicate-name collision errors at resolve either way); the indentation of
/// the existing first item is copied so mixed-indent blocks can't happen.
fn insert_include(
    text: &str,
    source: &str,
    prefix: Option<&str>,
    vars: &[(String, String)],
) -> Result<String> {
    let lines: Vec<&str> = text.lines().collect();
    let Some(key_idx) = lines.iter().position(|l| l.starts_with("includes:")) else {
        // No includes block — top-level key order is free in YAML, so append.
        let mut out = text.to_string();
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("\nincludes:\n");
        out.push_str(&render_entry("  ", source, prefix, vars));
        return Ok(out);
    };
    let after_key = lines[key_idx]["includes:".len()..].trim();
    if !after_key.is_empty() && !after_key.starts_with('#') {
        bail!(
            "`includes:` uses an inline value — add this entry by hand:\n{}",
            render_entry("  ", source, prefix, vars)
        );
    }
    // First list item before the next top-level key decides the indentation.
    let mut insert_at = key_idx + 1;
    let mut indent = "  ".to_string();
    for (offset, line) in lines[key_idx + 1..].iter().enumerate() {
        let trimmed = line.trim_start();
        if !line.starts_with([' ', '\t']) && !trimmed.is_empty() {
            break; // next top-level key — the block had no items
        }
        if trimmed.starts_with("- ") {
            insert_at = key_idx + 1 + offset;
            indent = line[..line.len() - trimmed.len()].to_string();
            break;
        }
    }
    let mut out = String::new();
    for (i, line) in lines.iter().enumerate() {
        if i == insert_at {
            out.push_str(&render_entry(&indent, source, prefix, vars));
        }
        out.push_str(line);
        out.push('\n');
    }
    if insert_at == lines.len() {
        out.push_str(&render_entry(&indent, source, prefix, vars));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_pins_but_not_ssh_at() {
        assert_eq!(strip_pin("github.com/o/r//sub@v1"), "github.com/o/r//sub");
        assert_eq!(strip_pin("./tools"), "./tools");
        assert_eq!(
            strip_pin("git::ssh://git@corp/ops.git"),
            "git::ssh://git@corp/ops.git"
        );
    }

    #[test]
    fn inserts_before_first_item_copying_indent() {
        let text = "version: 1\nincludes:\n    - source: ./old\n      prefix: o\ncommands: []\n";
        let out = insert_include(text, "github.com/o/r@v1", Some("g"), &[]).unwrap();
        assert_eq!(
            out,
            "version: 1\nincludes:\n    - source: github.com/o/r@v1\n      prefix: g\n    - source: ./old\n      prefix: o\ncommands: []\n"
        );
    }

    #[test]
    fn appends_block_when_no_includes_key() {
        let text = "version: 1\nname: x\ncommands:\n  - { id: c, title: C, run: \"true\" }\n";
        let out = insert_include(text, "./tools", None, &[]).unwrap();
        assert!(
            out.ends_with("\nincludes:\n  - source: ./tools\n"),
            "got:\n{out}"
        );
        assert!(out.starts_with(text), "original text must be untouched");
    }

    #[test]
    fn empty_includes_block_gets_default_indent() {
        let text = "includes:\ncommands:\n  - { id: c, title: C, run: \"true\" }\n";
        let out = insert_include(text, "./tools", None, &[]).unwrap();
        assert!(
            out.starts_with("includes:\n  - source: ./tools\ncommands:"),
            "got:\n{out}"
        );
    }

    #[test]
    fn inline_includes_value_is_refused() {
        let err = insert_include("includes: []\ncommands: []\n", "./t", None, &[]).unwrap_err();
        assert!(err.to_string().contains("by hand"), "got: {err}");
    }

    #[test]
    fn renders_vars_quoted() {
        let entry = render_entry("  ", "./t", Some("p"), &[("a".into(), "x \"y\"".into())]);
        assert_eq!(
            entry,
            "  - source: ./t\n    prefix: p\n    vars: { a: \"x \\\"y\\\"\" }\n"
        );
    }

    // ── end to end against a local git "remote" ──

    fn git_cmd(args: &[&str], cwd: &Path) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn add_pins_latest_tag_and_verifies() {
        let remote = tempfile::tempdir().unwrap();
        std::fs::write(
            remote.path().join("pult.module.yaml"),
            "version: 1\nname: mod\ncommands:\n  - { id: hi, title: Hi, run: \"echo hi\" }\n",
        )
        .unwrap();
        git_cmd(&["init", "-q"], remote.path());
        git_cmd(&["add", "-A"], remote.path());
        let commit = [
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-qm",
            "one",
        ];
        git_cmd(&commit, remote.path());
        git_cmd(&["tag", "v0.1.0"], remote.path());
        git_cmd(&["tag", "v0.2.0"], remote.path());

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("pult.yaml");
        std::fs::write(
            &target,
            "version: 1\ncommands:\n  - { id: local, title: L, run: \"true\" }\n",
        )
        .unwrap();
        let cache = tempfile::tempdir().unwrap();
        let source = format!("git::{}", remote.path().display());

        // interactive: false — tests must never prompt, even when cargo test
        // runs from a real terminal (e.g. under `pult check`/`pult release`)
        add_include(&target, &source, Some("m"), Some(cache.path()), false).unwrap();
        let text = std::fs::read_to_string(&target).unwrap();
        assert!(
            text.contains(&format!("- source: {source}@v0.2.0")),
            "got:\n{text}"
        );
        assert!(text.contains("prefix: m"), "got:\n{text}");

        // idempotence: the same base source is refused
        let err = add_include(
            &target,
            &format!("{source}@v0.1.0"),
            None,
            Some(cache.path()),
            false,
        )
        .unwrap_err();
        assert!(err.to_string().contains("already included"), "got: {err}");
    }

    #[test]
    fn add_rolls_back_when_resolution_fails() {
        // A module whose only export collides with the root's command id.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("mod")).unwrap();
        std::fs::write(
            dir.path().join("mod/pult.module.yaml"),
            "version: 1\ncommands:\n  - { id: local, title: Clash, run: \"true\" }\n",
        )
        .unwrap();
        let target = dir.path().join("pult.yaml");
        let before = "version: 1\ncommands:\n  - { id: local, title: L, run: \"true\" }\n";
        std::fs::write(&target, before).unwrap();

        let err = add_include(&target, "./mod", None, None, false).unwrap_err();
        assert!(format!("{err:#}").contains("rolled back"), "got: {err:#}");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), before);
    }
}
