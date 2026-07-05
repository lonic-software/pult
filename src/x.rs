use std::collections::HashMap;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Arg, ArgAction};
use indexmap::IndexMap;

use crate::fetch::{self, Source};
use crate::manifest::{self, IncludeDef, Loaded, Manifest};
use crate::resolver::{self, PinInfo, Resolved};
use crate::{exec, flow};

/// `pult x <source> [command] [values…]` — ephemeral execution: run a command
/// straight from a module source without pinning it into any manifest (the npx
/// of pult). The source is pinned exactly as an include is (a bare git source
/// resolves to its latest version tag), fetched into the immutable cache, and
/// gated by the same trust-on-first-use prompt — so "no config" never means
/// "unseen code". Intercepted before discovery (like `init`/`update`) because
/// the whole point is running where no local manifest exists yet.
pub fn run_cli(rest: &[String]) -> Result<i32> {
    let matches = clap::Command::new("pult x")
        .bin_name("pult x")
        .about("Run a command from a module source without adding it to a manifest")
        .arg(Arg::new("source").required(true).value_name("SOURCE").help(
            "./path, host.tld/org/repo[//sub][@pin], or git::<url>[//sub][@pin] — \
                     a git source without a pin resolves to its latest version tag",
        ))
        .arg(
            Arg::new("command")
                .value_name("COMMAND")
                .help("Which of the module's commands to run; omitted opens the guided menu"),
        )
        .arg(
            Arg::new("values")
                .value_name("VALUES")
                .num_args(0..)
                .help("Positional values for the command's params, in declared order"),
        )
        .arg(
            Arg::new("var")
                .long("var")
                .value_name("NAME=VALUE")
                .action(ArgAction::Append)
                .help("Bind a module var (repeatable) — for modules that declare `vars:`"),
        )
        .arg(
            Arg::new("trust")
                .long("trust")
                .action(ArgAction::SetTrue)
                .help("Trust the module without prompting (for CI)"),
        )
        .arg(
            Arg::new("print")
                .long("print")
                .action(ArgAction::SetTrue)
                .help("Print the composed script instead of running it"),
        )
        .get_matches_from(std::iter::once("pult x".to_string()).chain(rest.iter().cloned()));

    let source = matches.get_one::<String>("source").expect("required");
    let command = matches.get_one::<String>("command").map(String::as_str);
    let values: Vec<String> = matches
        .get_many::<String>("values")
        .map(|vs| vs.cloned().collect())
        .unwrap_or_default();
    let vars = parse_vars(&matches)?;
    let assume_trusted = matches.get_flag("trust");
    let print = matches.get_flag("print");

    // Interactivity is decided once, here, and threaded down — the core never
    // sniffs the terminal itself (so the test suite can't turn interactive).
    let interactive = std::io::stdin().is_terminal();
    let cwd = std::env::current_dir().context("failed to read current directory")?;

    run(
        source,
        command,
        &values,
        &vars,
        &cwd,
        assume_trusted,
        print,
        interactive,
        None,
    )
}

/// `--var name=value` → pairs; a missing `=` is a usage error, not a silent drop.
fn parse_vars(matches: &clap::ArgMatches) -> Result<Vec<(String, String)>> {
    matches
        .get_many::<String>("var")
        .into_iter()
        .flatten()
        .map(|s| {
            s.split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .ok_or_else(|| anyhow!("--var expects NAME=VALUE, got `{s}`"))
        })
        .collect()
}

/// The whole flow: pin + fetch the source into a throwaway single-include
/// manifest, resolve it (so `module.dir`, var defaults, and shipped executables
/// all work exactly as they would in an include), then dispatch — a named
/// command runs directly, no command opens the guided menu. `cache_root: None`
/// = the default module cache; tests pass an explicit root.
#[allow(clippy::too_many_arguments)]
pub fn run(
    source: &str,
    command: Option<&str>,
    values: &[String],
    vars: &[(String, String)],
    cwd: &Path,
    assume_trusted: bool,
    print: bool,
    interactive: bool,
    cache_root: Option<&Path>,
) -> Result<i32> {
    let resolved = resolve_ephemeral(source, cwd, vars, cache_root)?;

    match &resolved.pins[0] {
        PinInfo::Git {
            source,
            resolved_sha,
            ..
        } => println!("◆  {source}  ·  commit {}", short(resolved_sha)),
        PinInfo::Local { source } => println!("◆  {source}  ·  local module"),
    }

    match command {
        Some(id) => {
            let cmd = resolved
                .commands
                .iter()
                .find(|c| c.id == id)
                .ok_or_else(|| {
                    anyhow!(
                        "`{source}` has no command `{id}` — it offers: {}",
                        command_ids(&resolved)
                    )
                })?;
            if values.len() > cmd.params.len() {
                bail!(
                    "command `{id}` takes at most {} value(s), but {} were given",
                    cmd.params.len(),
                    values.len()
                );
            }
            // Positional values bind to params in declared order; anything not
            // supplied is prompted for by the executor (same as `pult <cmd>`).
            let mut provided = HashMap::new();
            for (i, value) in values.iter().enumerate() {
                let (name, _) = cmd.params.get_index(i).expect("bounds checked above");
                provided.insert(name.clone(), value.clone());
            }
            exec::execute(&resolved, cmd, &provided, assume_trusted, print)
        }
        None if interactive => flow::run(&resolved, assume_trusted, print),
        None => {
            eprintln!("specify a command:  pult x {source} <command> [values…]");
            eprintln!("commands:");
            for c in &resolved.commands {
                eprintln!("  {}  {}", c.id, c.title);
            }
            Ok(2)
        }
    }
}

/// Pin the source, wrap it in a synthetic one-include root manifest, and resolve
/// that — reusing every include guarantee (immutable pin, `module.dir`, var
/// binding, tree-hash trust) instead of re-implementing them. The returned
/// `Resolved` is retargeted for ephemeral use: the trust identity is the source
/// itself (a pinned git source is globally unique; a local module is keyed by
/// its canonical path, so the same tree trusts once regardless of cwd), commands
/// run in the invocation directory, and the include summary is dropped because
/// the module *is* the thing being trusted, not an include of it.
fn resolve_ephemeral(
    source: &str,
    cwd: &Path,
    vars: &[(String, String)],
    cache_root: Option<&Path>,
) -> Result<Resolved> {
    let pinned_source = match fetch::parse_source_lenient(source)? {
        // A local module resolves relative to the invocation directory.
        Source::Local => source.to_string(),
        Source::Git(git_src) => {
            if git_src.rev.is_empty() {
                let tag = fetch::latest_version_tag(&git_src.url)?.with_context(|| {
                    format!(
                        "no version tags on {} — pin explicitly with `@<tag|sha>`",
                        git_src.url
                    )
                })?;
                println!("·  {} → latest version tag {tag}", git_src.url);
                format!("{source}@{tag}")
            } else {
                source.to_string()
            }
        }
    };

    let mut var_bindings: IndexMap<String, String> = IndexMap::new();
    for (k, v) in vars {
        var_bindings.insert(k.clone(), v.clone());
    }
    let root = Manifest {
        version: manifest::SUPPORTED_VERSION,
        name: None,
        description: None,
        includes: vec![IncludeDef {
            source: pinned_source.clone(),
            vars: var_bindings,
            prefix: None,
            sha256: None,
        }],
        registries: None,
        vars: IndexMap::new(),
        params: IndexMap::new(),
        steps: IndexMap::new(),
        commands: Vec::new(),
    };
    let loaded = Loaded {
        manifest: root,
        // Never touched on disk — `dir` is the base local includes resolve
        // against; `path`/`raw` feed resolution and are overridden below.
        path: cwd.join("<pult x>"),
        dir: cwd.to_path_buf(),
        raw: format!("pult x {pinned_source}\n"),
    };

    let mut resolved = resolver::resolve_with(loaded, cache_root)
        .with_context(|| format!("could not load `{source}`"))?;

    resolved.path = match &resolved.pins[0] {
        PinInfo::Git { source, .. } => PathBuf::from(source),
        PinInfo::Local { .. } => cwd
            .join(source)
            .canonicalize()
            .unwrap_or_else(|_| cwd.join(source)),
    };
    resolved.name = pinned_source;
    resolved.run_dir = cwd.to_path_buf();
    resolved.include_summary.clear();
    // A single ephemeral source: let the trust prompt show the command about to
    // run (the trust unit is exactly this invocation).
    resolved.ephemeral = true;
    Ok(resolved)
}

fn command_ids(resolved: &Resolved) -> String {
    resolved
        .commands
        .iter()
        .map(|c| c.id.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

fn short(sha: &str) -> &str {
    &sha[..10.min(sha.len())]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A local directory module with one no-param command and one that takes a
    /// value. Returns the tempdir (kept alive) and the module path `./mod`.
    fn local_module() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("mod")).unwrap();
        std::fs::write(
            dir.path().join("mod/pult.module.yaml"),
            "version: 1\nname: demo-mod\ncommands:\n  \
             - { id: hello, title: Say hi, run: \"echo hi\" }\n  \
             - id: greet\n    title: Greet\n    params:\n      who: { input: { default: world } }\n    run: \"echo hi {who}\"\n",
        )
        .unwrap();
        dir
    }

    #[test]
    fn resolves_local_module_and_keys_trust_by_canonical_path() {
        let dir = local_module();
        let resolved = resolve_ephemeral("./mod", dir.path(), &[], None).unwrap();

        let ids: Vec<_> = resolved.commands.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["hello", "greet"]);
        // Ephemeral commands act on the invocation directory.
        assert_eq!(resolved.run_dir, dir.path());
        // Trust identity is the module's canonical path, not the synthetic root.
        let expected = dir.path().join("mod").canonicalize().unwrap();
        assert_eq!(resolved.path, expected);
        // The module is what's trusted — no phantom "includes" line.
        assert!(resolved.include_summary.is_empty());
        // Ephemeral: the trust prompt will show the command about to run.
        assert!(resolved.ephemeral);
    }

    #[test]
    fn unknown_command_lists_what_is_offered() {
        let dir = local_module();
        let err = run(
            "./mod",
            Some("nope"),
            &[],
            &[],
            dir.path(),
            true,
            false,
            false,
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("no command `nope`"), "got: {err}");
        assert!(err.contains("hello, greet"), "got: {err}");
    }

    #[test]
    fn too_many_values_is_rejected_before_execution() {
        let dir = local_module();
        // `hello` takes no params, so even one value is too many — and this must
        // fail loudly rather than silently drop the extra.
        let err = run(
            "./mod",
            Some("hello"),
            &["surplus".to_string()],
            &[],
            dir.path(),
            true,
            false,
            false,
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("at most 0 value"), "got: {err}");
    }

    #[test]
    fn unrecognized_source_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let err = resolve_ephemeral("not-a-source", dir.path(), &[], None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not recognized"), "got: {err}");
    }

    fn parse_pairs(args: &[&str]) -> Result<Vec<(String, String)>> {
        let m = clap::Command::new("t")
            .arg(
                Arg::new("var")
                    .long("var")
                    .action(ArgAction::Append)
                    .num_args(1),
            )
            .get_matches_from(std::iter::once("t").chain(args.iter().copied()));
        parse_vars(&m)
    }

    #[test]
    fn var_parsing_splits_on_first_equals() {
        let ok = parse_pairs(&["--var", "region=eu-west-1", "--var", "note=a=b"]).unwrap();
        assert_eq!(
            ok,
            vec![
                ("region".to_string(), "eu-west-1".to_string()),
                ("note".to_string(), "a=b".to_string()),
            ]
        );
        let err = parse_pairs(&["--var", "novalue"]).unwrap_err().to_string();
        assert!(err.contains("NAME=VALUE"), "got: {err}");
    }

    // ── end to end against a local git "remote" (mirrors add.rs) ──

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
    fn bare_git_source_pins_latest_tag_for_trust() {
        let remote = tempfile::tempdir().unwrap();
        std::fs::write(
            remote.path().join("module.yaml"),
            "version: 1\nname: mod\ncommands:\n  - { id: hi, title: Hi, run: \"echo hi\" }\n",
        )
        .unwrap();
        git_cmd(&["init", "-q"], remote.path());
        git_cmd(&["add", "-A"], remote.path());
        git_cmd(
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-qm",
                "one",
            ],
            remote.path(),
        );
        git_cmd(&["tag", "v0.1.0"], remote.path());
        git_cmd(&["tag", "v0.2.0"], remote.path());

        let cache = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let source = format!("git::{}", remote.path().display());

        let resolved = resolve_ephemeral(&source, cwd.path(), &[], Some(cache.path())).unwrap();
        // Unpinned → resolves to the highest version tag; the trust key carries
        // that pin, so bumping the remote's latest tag re-prompts.
        assert_eq!(resolved.name, format!("{source}@v0.2.0"));
        assert_eq!(resolved.path, PathBuf::from(format!("{source}@v0.2.0")));
        assert_eq!(resolved.commands[0].id, "hi");
    }
}
