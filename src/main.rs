mod add;
mod bind;
mod compile;
mod discovery;
mod doctor;
mod events;
mod exec;
mod fetch;
mod flow;
mod init;
mod interp;
mod journal;
mod label;
mod machine;
mod manifest;
mod mask;
mod options;
mod prompt;
mod requirements;
mod resolver;
mod runner;
mod runs;
mod selfupdate;
mod shell;
mod trust;
mod value;
mod verify;
mod x;

use std::io::Read;

use anyhow::{Context, Result, bail};
use clap::{Arg, ArgAction};

use discovery::Scope;
use manifest::{OptionDef, ParamKind};
use resolver::{PinInfo, Resolved, ResolvedRun};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    machine::init(&args);
    match run(&args) {
        Ok(code) => std::process::exit(code),
        Err(e) => std::process::exit(machine::report(&e)),
    }
}

/// Authoring guide, pinned to this binary's version so the docs an author
/// reads match the pult they have.
pub(crate) fn docs_url() -> String {
    format!(
        "https://github.com/lonic-software/pult/blob/v{}/docs/authoring.md",
        env!("CARGO_PKG_VERSION")
    )
}

/// The manifest JSON Schema for this version — what `pult init` points the
/// editor modeline at. Version-pinned so the schema matches the binary.
pub(crate) fn schema_url() -> String {
    format!(
        "https://raw.githubusercontent.com/lonic-software/pult/v{}/pult.schema.json",
        env!("CARGO_PKG_VERSION")
    )
}

fn run(args: &[String]) -> Result<i32> {
    // `pult update` needs no manifest (the id is reserved, so no manifest
    // command can ever claim it) — handle it before discovery.
    if args.first().map(String::as_str) == Some("update") {
        let requested = args.get(1).filter(|a| !a.starts_with('-'));
        return selfupdate::run(requested.map(String::as_str));
    }
    // `includes add` is intercepted too: with `--user` it must work even
    // where no manifest exists yet (it can create the user manifest).
    if args.first().map(String::as_str) == Some("includes")
        && args.get(1).map(String::as_str) == Some("add")
    {
        return add::run_cli(&args[2..]);
    }
    // `pult x` — ephemeral execution from a module source. Intercepted here
    // because its whole point is running where no local manifest exists (the id
    // is reserved, so no manifest command can ever claim it).
    if args.first().map(String::as_str) == Some("x") {
        return x::run_cli(&args[1..]);
    }
    // As is `init` — its whole point is running where no manifest exists.
    if args.first().map(String::as_str) == Some("init") {
        return init::run_cli(&args[1..]);
    }
    // `pult self schema` prints the manifest JSON Schema (compiled in, so it
    // always matches this binary and works offline). No manifest needed.
    if args.first().map(String::as_str) == Some("self") {
        return match args.get(1).map(String::as_str) {
            Some("schema") => {
                print!("{}", include_str!("../pult.schema.json"));
                Ok(0)
            }
            _ => Err(machine::usage("usage: pult self schema")),
        };
    }

    let (loaded, scope) = match discovery::find_manifest() {
        Ok(found) => found,
        Err(e) => {
            // --version / --help should still work with no manifest around.
            if args.iter().any(|a| a == "-V" || a == "--version") {
                println!("pult {}", env!("CARGO_PKG_VERSION"));
                return Ok(0);
            }
            if args.iter().any(|a| a == "-h" || a == "--help") {
                println!(
                    "pult — manifest-driven ops launcher\n\n\
                     Run inside a repo containing a pult.yaml, or create one:\n  \
                       pult init          starter manifest in this directory\n  \
                       pult init --user   your personal manifest (~/.config/pult/)\n\n\
                     Bare `pult` opens the guided flow; `pult <command> [values…]`\n\
                     runs directly. `pult update` updates pult itself.\n\n\
                     No manifest was found (current directory upward, then the user\n\
                     manifest).\n\n\
                     Authoring guide: {}",
                    docs_url()
                );
                return Ok(0);
            }
            return Err(e);
        }
    };
    let mut resolved = resolver::resolve(loaded)
        .map_err(|e| machine::coded(machine::ErrorCode::InvalidManifest, e))?;
    if scope == Scope::User {
        // Personal commands act on wherever you are; the manifest dir
        // (~/.config/pult) stays the base for the manifest's own includes.
        resolved.run_dir = std::env::current_dir().context("failed to read current directory")?;
    }

    let matches = machine::get_matches(build_cli(&resolved, scope), std::env::args_os());
    let grant = trust::Grant::from_matches(&matches);
    let print = matches.get_flag("print");
    let params_json = matches.get_flag("params-json");

    // `--params-json` only makes sense feeding a direct command invocation —
    // reject it up front for every other routing (bare flow, `--list`,
    // `doctor`, `includes`, `runs`) rather than silently ignoring it.
    if params_json {
        let is_direct_command = !matches.get_flag("list")
            && matches!(matches.subcommand(), Some((id, _)) if id != "doctor" && id != "includes" && id != "runs");
        if !is_direct_command {
            bail!(
                "--params-json only applies to a direct command invocation \
                 (`pult <command> [values…]`), not to --list, doctor, includes, \
                 or the guided flow"
            );
        }
    }

    // `--trust` is an explicit act — record it even when this invocation
    // doesn't execute anything (e.g. `pult --trust --list`).
    if grant != trust::Grant::Ask {
        trust::ensure_trusted(
            &resolved.path,
            &resolved.trust_hash,
            &resolved.include_summary,
            &grant,
            None,
        )?;
    }

    if matches.get_flag("list") {
        if matches.get_flag("json") {
            let trusted = trust::is_trusted(&resolved.path, &resolved.trust_hash)?;
            let doc = machine::success("list", list_json(&resolved, trusted, scope));
            println!("{}", serde_json::to_string_pretty(&doc)?);
        } else {
            print_list(&resolved);
        }
        return Ok(0);
    }

    let run_id = matches.get_one::<String>("run-id").map(String::as_str);
    validate_run_id_arg(run_id)?;

    match matches.subcommand() {
        Some(("doctor", sub)) => doctor::run(&resolved, &grant, sub.get_flag("json")),
        Some(("runs", sub)) => runs::run_cli(&resolved, sub),
        Some(("includes", sub)) => match sub.subcommand() {
            Some(("verify", _)) => verify::run(&resolved),
            _ => Err(machine::usage(
                "usage: pult includes <verify | add <SOURCE> [--prefix P] [--user]>",
            )),
        },
        Some((id, sub)) => {
            let cmd = resolve_manifest_command(&resolved.commands, id)?;
            let mut provided = bind::provided(cmd, sub);
            if params_json {
                use std::io::IsTerminal;
                if std::io::stdin().is_terminal() {
                    bail!(
                        "--params-json expects a JSON object on stdin — pipe it \
                         (echo '{{...}}' | pult …) or redirect a file"
                    );
                }
                let mut raw = String::new();
                std::io::stdin()
                    .read_to_string(&mut raw)
                    .context("failed to read stdin for --params-json")?;
                bind::merge_params_json(&raw, &cmd.params, &mut provided)?;
            }
            exec::execute(
                &resolved,
                cmd,
                &provided,
                exec::Prompting::Direct,
                &grant,
                print,
                run_id,
            )
        }
        None => flow::run(&resolved, &grant, print, run_id),
    }
}

/// H1: an explicit `--run-id` is arg validation, not a journaling failure —
/// it fails the invocation before anything runs, loudly, rather than
/// degrading to a warning the way an in-flight journaling fault does. This
/// is also defense in depth: `journal::start_at` re-checks the same id
/// (invalid → journaling disabled for that run) in case some future caller
/// reaches it without going through here.
fn validate_run_id_arg(run_id: Option<&str>) -> Result<()> {
    if let Some(id) = run_id {
        anyhow::ensure!(
            journal::valid_run_id(id),
            "--run-id `{id}` is invalid: must be 1-64 ASCII alphanumeric/`-` characters"
        );
    }
    Ok(())
}

/// Resolve a clap-accepted subcommand id against the manifest's own declared
/// commands. `id` normally reaches this arm because it names a real manifest
/// command — but it can also be one of pult's own reserved ids (`update`,
/// `x`, `init`, `self`, …), which are ordinarily intercepted in `run()`
/// *before* clap parses anything, and only when the id is argv[1] itself.
/// A leading global flag (`pult --trust update`, `pult --params-json update`)
/// makes clap parse the reserved id as a generic subcommand instead, so it
/// lands here with no matching manifest command — the case that used to
/// `.expect()` and panic. Returning a clear error instead points the user at
/// the fix (put the reserved id first) rather than crashing.
fn resolve_manifest_command<'a>(
    commands: &'a [resolver::ResolvedCommand],
    id: &str,
) -> Result<&'a resolver::ResolvedCommand> {
    commands.iter().find(|c| c.id == id).ok_or_else(|| {
        anyhow::anyhow!(
            "`{id}` is pult's own subcommand — put it first: `pult {id} …` \
             (global flags go after it)"
        )
    })
}

/// Build the CLI dynamically from the resolved manifest: every command becomes
/// a subcommand, its params bound by `bind::args` (all optional — a missing
/// one is filled by `exec::fill`, so partial invocations degrade into a
/// shorter guided flow).
///
/// Help keeps the two surfaces apart: the subcommand list is purely what the
/// manifest declares (the product); pult's own subcommands are hidden from it
/// and documented in a separate section below.
fn build_cli(resolved: &Resolved, scope: Scope) -> clap::Command {
    let source = match scope {
        Scope::Repo => format!("commands from {}", resolved.path.display()),
        Scope::User => format!("your user manifest · {}", resolved.path.display()),
    };
    let mut cli = clap::Command::new("pult")
        .version(env!("CARGO_PKG_VERSION"))
        .about(format!("{} · {source}", resolved.name))
        .disable_help_subcommand(true)
        .after_help(format!(
            "pult itself:\n  \
               pult update [VERSION]        self-update to the latest (or given) release\n  \
               pult init [--user]           scaffold a starter manifest\n  \
               pult x <SOURCE> [COMMAND]    run a command straight from a module source (no manifest)\n  \
               pult includes add <SOURCE>   pin a module and add it to a manifest (--user)\n  \
               pult includes verify         check every pin still resolves and no tag moved\n  \
               pult doctor [--json]         run every `check:` and requirement, report readiness\n  \
               pult runs list [--json]      this repo's run history (the run journal)\n  \
               pult runs tail <ID> [--follow]  one run's event stream (--json / --jsonl for tooling)\n  \
               pult self schema             print the manifest JSON Schema (editors/CI)\n  \
               pult --list [--json]         what this manifest declares (--json for tooling)\n\n\
             Run bare `pult` for the guided flow.  Authoring guide: {}",
            docs_url()
        ))
        .arg(
            Arg::new("list")
                .long("list")
                .action(ArgAction::SetTrue)
                .help("List the commands this repo declares"),
        )
        .arg(
            Arg::new("json")
                .long("json")
                .requires("list")
                .action(ArgAction::SetTrue)
                .help("With --list: machine-readable JSON (stable schema, for tooling/agents)"),
        )
        .arg(trust::arg("this repo's manifest").global(true))
        .arg(
            Arg::new("print")
                .long("print")
                .global(true)
                .action(ArgAction::SetTrue)
                .help("Print the composed script instead of running it"),
        )
        .arg(
            Arg::new("params-json")
                .long("params-json")
                .global(true)
                .action(ArgAction::SetTrue)
                .help(
                    "Read param values as a JSON object from stdin (keeps them out of \
                     pult's argv and shell history)",
                ),
        )
        .arg(
            Arg::new("run-id")
                .long("run-id")
                .global(true)
                .value_name("ID")
                .help(
                    "Journal this run under a caller-supplied id (for wrapping surfaces \
                     like the desktop app; defaults to a generated UUIDv7)",
                ),
        )
        // Engine subcommands are hidden from the Commands list (that list is
        // the manifest's surface) and documented in after_help instead.
        .subcommand(
            clap::Command::new("includes")
                .hide(true)
                .about("Include maintenance (the id `includes` is reserved)")
                .subcommand(
                    clap::Command::new("verify")
                        .about("Check every include still resolves and no git tag has moved"),
                )
                // `includes add` is intercepted before clap runs (main.rs);
                // registered here so `pult includes --help` stays truthful.
                .subcommand(
                    clap::Command::new("add")
                        .about("Pin a module source and add it to a manifest's includes"),
                ),
        )
        // `update` is intercepted before clap runs so it also works with no
        // manifest around; registered so `pult update --help` still resolves.
        .subcommand(
            clap::Command::new("update")
                .hide(true)
                .about("Update pult itself to the latest release (or a given version)")
                .arg(Arg::new("version").required(false).value_name("VERSION")),
        )
        // `x` is intercepted before clap runs (it must work with no manifest);
        // registered so it's not mistaken for a manifest command and stays
        // hidden from the manifest's Commands list.
        .subcommand(
            clap::Command::new("x")
                .hide(true)
                .about("Run a command from a module source without adding it to a manifest"),
        )
        .subcommand(
            clap::Command::new("doctor")
                .hide(true)
                .about("Run every command's `check:` readiness probe and report the results")
                .arg(
                    Arg::new("json")
                        .long("json")
                        .action(ArgAction::SetTrue)
                        .help("Machine-readable JSON (stable schema, for tooling/agents)"),
                ),
        )
        // The run journal's reader surface (the id `runs` is reserved) —
        // see src/runs.rs and the run-journal spec.
        .subcommand(
            clap::Command::new("runs")
                .hide(true)
                .about("This repo's run history — the run journal")
                .subcommand(
                    clap::Command::new("list")
                        .about("List journaled runs, newest first")
                        .arg(
                            Arg::new("json")
                                .long("json")
                                .action(ArgAction::SetTrue)
                                .help("Machine-readable JSON (meta + derived `crashed` flag)"),
                        ),
                )
                .subcommand(
                    clap::Command::new("tail")
                        .about("Print one run's journaled event stream")
                        .arg(Arg::new("run_id").required(true).value_name("RUN_ID"))
                        .arg(
                            Arg::new("follow")
                                .long("follow")
                                .action(ArgAction::SetTrue)
                                .help("Keep following a live run until it ends"),
                        )
                        .arg(
                            Arg::new("json")
                                .long("json")
                                .action(ArgAction::SetTrue)
                                .conflicts_with("jsonl")
                                .help("The events so far as one JSON document (for tooling)"),
                        )
                        .arg(
                            Arg::new("jsonl")
                                .long("jsonl")
                                .action(ArgAction::SetTrue)
                                .help("Raw events.jsonl lines, one per line (works with --follow)"),
                        ),
                )
                .subcommand(
                    clap::Command::new("prune")
                        .about("Apply journal retention now (PULT_RUNS_KEEP, default 20 per command)"),
                ),
        );

    for cmd in &resolved.commands {
        let mut sub = clap::Command::new(cmd.id.clone()).about(cmd.title.clone());
        if let Some(description) = &cmd.description {
            sub = sub.long_about(format!("{}\n\n{description}", cmd.title));
        }
        sub = sub.args(bind::args(cmd));
        cli = cli.subcommand(sub);
    }
    cli
}

/// The `--list --json` document's `data` — the stable machine-readable
/// surface for agents and tooling, carried in the envelope at
/// `machine::SCHEMA`. Changes within a schema are additive only. `trusted`
/// is passed in so this stays a pure function of the resolved manifest (the
/// caller consults the trust store).
///
/// A static pick's `params[].option_details` is
/// `[{"value", "description"}, ...]`, parallel to `options` (same order,
/// same length, present iff `options` is). Dynamic picks have neither key —
/// `[{"value", "description"}, ...]`, parallel to `options` (same order,
/// same length, present iff `options` is). Dynamic picks have neither key —
/// the option set is unknowable at list time (the source is never run, the
/// same no-side-effects guarantee as the preview path).
fn list_json(resolved: &Resolved, trusted: bool, scope: Scope) -> serde_json::Value {
    use serde_json::json;
    let includes: Vec<_> = resolved
        .pins
        .iter()
        .zip(&resolved.include_names)
        .map(|(pin, name)| match pin {
            PinInfo::Local { source } => {
                json!({ "source": source, "kind": "local", "name": name })
            }
            PinInfo::Git {
                source,
                url,
                rev,
                rev_kind,
                resolved_sha,
            } => json!({
                "source": source,
                "kind": "git",
                "url": url,
                "rev": rev,
                "rev_kind": rev_kind,
                "resolved_sha": resolved_sha,
                "name": name,
            }),
        })
        .collect();
    let commands: Vec<_> = resolved
        .commands
        .iter()
        .map(|cmd| {
            let params: Vec<_> = cmd
                .params
                .iter()
                .map(|(name, def)| match def.kind() {
                    ParamKind::Pick(pick) => match (&pick.options, &pick.from) {
                        (Some(options), _) => {
                            let values: Vec<&str> = options.iter().map(OptionDef::value).collect();
                            // `option_details` is present iff `options` is —
                            // same order, same length; old consumers ignore
                            // it, new ones get value + display description.
                            let details: Vec<_> = options
                                .iter()
                                .map(|o| json!({ "value": o.value(), "description": o.description() }))
                                .collect();
                            json!({
                                "name": name,
                                "kind": "pick",
                                "options": values,
                                "option_details": details,
                            })
                        }
                        (None, Some(from)) => json!({
                            "name": name,
                            "kind": "pick",
                            "source": from,
                            // params whose values the source interpolates —
                            // an invoker must supply these first
                            "depends_on": interp::placeholders(from).unwrap_or_default(),
                        }),
                        (None, None) => unreachable!("validated at load: options or from"),
                    },
                    ParamKind::Input(input) => {
                        json!({
                            "name": name,
                            "kind": "input",
                            "default": input.default,
                            // UIs render secret inputs as password fields and
                            // must never echo or persist their values.
                            "secret": input.secret,
                        })
                    }
                    ParamKind::Flag(flag) => json!({
                        "name": name,
                        "kind": "flag",
                        // The word the tool receives when the flag is set.
                        "arg": flag.arg,
                    }),
                    ParamKind::Repeat(repeat) => json!({
                        "name": name,
                        "kind": "repeat",
                        // null: a positional repeat, taking the trailing values.
                        "arg": repeat.arg,
                        "min": repeat.min,
                        "max": repeat.max,
                        "secret": repeat.secret,
                    }),
                    ParamKind::Use(_) => unreachable!("resolver inlines every use: param"),
                })
                .collect();
            // Step names a live run emits as `step k/n <name>` events (see
            // compile.rs / the PULT_EVENTS protocol) — same labels, so a
            // surface can render milestones without parsing the script.
            // `null` for a plain (string-form) `run:`, nothing to name.
            let steps = match &cmd.run {
                ResolvedRun::Steps(entries) => {
                    json!(
                        entries
                            .iter()
                            .enumerate()
                            .map(|(i, entry)| compile::step_label(i + 1, entry))
                            .collect::<Vec<_>>()
                    )
                }
                ResolvedRun::Script(_) => serde_json::Value::Null,
            };
            json!({
                "id": cmd.id,
                "title": cmd.title,
                "origin": cmd.origin,
                // Raw declared value (not the computed display group — see
                // `ResolvedCommand::group_label` / `--list` grouping); `null`
                // when the author set none.
                "category": cmd.category,
                // One or two sentences explaining what the command does
                // (shown by `--help` and UIs); `null` when the author set
                // none.
                "description": cmd.description,
                "params": params,
                // Readiness probe (run it via `pult doctor`; null = none
                // declared) and the "needs a controlling terminal" contract —
                // non-terminal surfaces treat interactive commands as
                // terminal-only.
                "check": cmd.check,
                "interactive": cmd.interactive,
                "steps": steps,
                // Effective requirement names, each a key of `requirements`
                // below — the command's own and its modules' (see
                // `ResolvedCommand::requires`).
                "requires": cmd.requires,
            })
        })
        .collect();
    json!({
        "pult_version": env!("CARGO_PKG_VERSION"),
        "name": resolved.name,
        "manifest": resolved.path,
        "dir": resolved.dir,
        "run_dir": resolved.run_dir,
        "scope": match scope { Scope::Repo => "repo", Scope::User => "user" },
        "trusted": trusted,
        "trust_hash": resolved.trust_hash,
        "includes": includes,
        "commands": commands,
        // Definitions only: listing runs no manifest code, so no check runs
        // here — `pult doctor` reports readiness.
        "requirements": resolved
            .requirements
            .values()
            .map(|req| json!({
                "name": req.name,
                "defined_in": req.defined_in,
                "check": req.check,
                "shadowed": req.shadowed,
            }))
            .collect::<Vec<_>>(),
    })
}

fn command_line(cmd: &resolver::ResolvedCommand, width: usize) -> String {
    let params: Vec<&str> = cmd.params.keys().map(String::as_str).collect();
    let mut line = format!(
        "  {:width$}  {}{}",
        cmd.id,
        cmd.title,
        if params.is_empty() {
            String::new()
        } else {
            format!("  <{}>", params.join("> <"))
        }
    );
    if cmd.interactive {
        line.push_str("  (interactive)");
    }
    if let Some(origin) = &cmd.origin {
        line.push_str(&format!("  ← {origin}"));
    }
    line
}

/// `pult --list` (text mode): one line per command, flat — the format
/// scripts parse. Grouping (`category:`, else include origin, else the
/// implicit "local" group — see `resolver::group_commands`) applies to the
/// guided flow, the future palette, and `--list --json` (`category` plus
/// each command's `name`/origin fields); this text surface stays flat so it
/// doesn't break existing consumers.
fn print_list(resolved: &Resolved) {
    let width = resolved
        .commands
        .iter()
        .map(|c| c.id.len())
        .max()
        .unwrap_or(0);
    println!("{} · {}", resolved.name, resolved.path.display());
    for cmd in &resolved.commands {
        println!("{}", command_line(cmd, width));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// H1: an explicit `--run-id` fails the invocation immediately when it
    /// isn't a safe path component — before discovery, before anything
    /// runs. Goes red if `validate_run_id_arg`'s check is reverted: each of
    /// these would then return `Ok(())` and the traversal would only be
    /// caught later (or not at all, per the other H1 tests).
    #[test]
    fn validate_run_id_arg_rejects_traversal_and_malformed_ids() {
        for bad in ["../x", "/tmp/evil", "a/b", "", &"x".repeat(65)] {
            let err = validate_run_id_arg(Some(bad)).unwrap_err();
            assert!(err.to_string().contains("--run-id"), "id {bad:?}: {err}");
        }
    }

    #[test]
    fn validate_run_id_arg_accepts_well_formed_ids_and_absence() {
        assert!(validate_run_id_arg(None).is_ok());
        assert!(validate_run_id_arg(Some("550e8400-e29b-41d4-a716-446655440000")).is_ok());
        assert!(validate_run_id_arg(Some("desktop-supplied-id")).is_ok());
    }

    #[test]
    fn list_json_exposes_params_and_dependencies() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("pult.yaml"),
            r#"
version: 1
name: demo
steps:
  restore-db: |
    ./bin/restore
  run-migrations: |
    ./bin/migrate
commands:
  - id: shell
    title: Open a shell
    params:
      env: { pick: { options: [dev, { value: uat, description: "User acceptance" }] } }
      customer: { pick: { from: "./bin/impl list --env {env}" } }
      note: { input: { default: "" } }
    run: "./bin/impl shell {customer} {env}"
    check: "command -v aws"
    interactive: true
    category: Ops
    description: Opens an interactive shell for the given customer and environment.
  - id: import
    title: Import data
    params:
      token: { input: { secret: true } }
    run: "./bin/impl import {token}"
  - id: deploy
    title: Deploy
    run:
      - use: restore-db
      - use: run-migrations
      - "echo done"
"#,
        )
        .unwrap();
        let loaded = manifest::load(&dir.path().join("pult.yaml")).unwrap();
        let resolved = resolver::resolve(loaded).unwrap();
        let doc = list_json(&resolved, false, Scope::Repo);

        // The envelope carries the schema; the payload does not repeat it.
        assert!(doc.get("schema").is_none());
        assert_eq!(doc["name"], "demo");
        assert_eq!(doc["scope"], "repo");
        assert_eq!(doc["trusted"], false);
        assert_eq!(doc["run_dir"], doc["dir"]);
        let cmd = &doc["commands"][0];
        assert_eq!(cmd["id"], "shell");
        assert_eq!(cmd["origin"], serde_json::Value::Null);
        assert_eq!(cmd["category"], "Ops");
        assert_eq!(
            cmd["description"],
            "Opens an interactive shell for the given customer and environment."
        );
        let params = cmd["params"].as_array().unwrap();
        assert_eq!(params[0]["kind"], "pick");
        assert_eq!(params[0]["options"][0], "dev");
        assert_eq!(params[0]["options"][1], "uat");
        // §7.8 — "options" stays plain strings; "option_details" is a
        // parallel array, same order/length, carrying the descriptions.
        assert!(params[0]["options"][0].is_string());
        assert_eq!(params[0]["option_details"][0]["value"], "dev");
        assert_eq!(
            params[0]["option_details"][0]["description"],
            serde_json::Value::Null
        );
        assert_eq!(params[0]["option_details"][1]["value"], "uat");
        assert_eq!(
            params[0]["option_details"][1]["description"],
            "User acceptance"
        );
        assert_eq!(
            params[0]["option_details"][1]["value"], params[0]["options"][1],
            "option_details[i].value must equal options[i]"
        );
        // Dynamic picks get neither key — the option set is unknowable at
        // list time (the source is never run).
        assert!(params[1].get("options").is_none());
        assert!(params[1].get("option_details").is_none());
        assert_eq!(params[1]["depends_on"][0], "env");
        assert_eq!(params[2]["kind"], "input");
        assert_eq!(params[2]["default"], "");
        assert_eq!(params[2]["secret"], false);
        // The surfaces a non-terminal UI keys off: readiness probe, the
        // needs-a-terminal contract, and password-field inputs.
        assert_eq!(cmd["check"], "command -v aws");
        assert_eq!(cmd["interactive"], true);
        // String-form `run:` has no steps to name.
        assert_eq!(cmd["steps"], serde_json::Value::Null);
        let import = &doc["commands"][1];
        assert_eq!(import["check"], serde_json::Value::Null);
        assert_eq!(import["category"], serde_json::Value::Null);
        assert_eq!(import["description"], serde_json::Value::Null);
        assert_eq!(import["interactive"], false);
        assert_eq!(import["params"][0]["secret"], true);
        assert_eq!(import["steps"], serde_json::Value::Null);
        // List-form `run:` exposes its step labels — same names a live run
        // emits as `step k/n <name>` events.
        let deploy = &doc["commands"][2];
        assert_eq!(
            deploy["steps"],
            serde_json::json!(["restore-db", "run-migrations", "echo done"])
        );
    }

    #[test]
    fn list_json_includes_module_name() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("mods/aws")).unwrap();
        std::fs::write(
            dir.path().join("mods/aws/pult.module.yaml"),
            "version: 1\nname: AWS Tooling\ncommands:\n  - { id: shell, title: Shell, run: \"true\" }\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("mods/plain")).unwrap();
        std::fs::write(
            dir.path().join("mods/plain/pult.module.yaml"),
            "version: 1\ncommands:\n  - { id: c, title: C, run: \"true\" }\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("pult.yaml"),
            r#"
version: 1
name: demo
includes:
  - source: ./mods/aws
    prefix: aws
  - source: ./mods/plain
    prefix: plain
commands:
  - id: local
    title: Local
    run: "true"
"#,
        )
        .unwrap();
        let loaded = manifest::load(&dir.path().join("pult.yaml")).unwrap();
        let resolved = resolver::resolve(loaded).unwrap();
        let doc = list_json(&resolved, false, Scope::Repo);

        let includes = doc["includes"].as_array().unwrap();
        assert_eq!(includes.len(), 2);
        assert_eq!(includes[0]["source"], "./mods/aws");
        assert_eq!(includes[0]["name"], "AWS Tooling");
        assert_eq!(includes[1]["source"], "./mods/plain");
        assert_eq!(includes[1]["name"], serde_json::Value::Null);
    }

    #[test]
    fn resolve_manifest_command_finds_a_declared_command() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("pult.yaml"),
            "version: 1\ncommands:\n  - { id: shell, title: Shell, run: \"true\" }\n",
        )
        .unwrap();
        let loaded = manifest::load(&dir.path().join("pult.yaml")).unwrap();
        let resolved = resolver::resolve(loaded).unwrap();
        let cmd = resolve_manifest_command(&resolved.commands, "shell").unwrap();
        assert_eq!(cmd.id, "shell");
    }

    /// The routing-panic regression (fix: reserved ids like `update` only get
    /// intercepted before clap runs when they're argv[1]; a leading global
    /// flag — `pult --trust update` — makes clap parse it as a generic
    /// subcommand instead, landing here with no manifest command to match).
    #[test]
    fn resolve_manifest_command_errors_clearly_for_an_unmatched_reserved_id() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("pult.yaml"),
            "version: 1\ncommands:\n  - { id: shell, title: Shell, run: \"true\" }\n",
        )
        .unwrap();
        let loaded = manifest::load(&dir.path().join("pult.yaml")).unwrap();
        let resolved = resolver::resolve(loaded).unwrap();
        let err = resolve_manifest_command(&resolved.commands, "update").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("pult's own subcommand"), "got: {msg}");
        assert!(msg.contains("pult update …"), "got: {msg}");
    }
}
