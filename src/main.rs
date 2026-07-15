mod add;
mod compile;
mod discovery;
mod doctor;
mod events;
mod exec;
mod fetch;
mod flow;
mod init;
mod interp;
mod manifest;
mod options;
mod prompt;
mod resolver;
mod runner;
mod selfupdate;
mod trust;
mod verify;
mod x;

use std::collections::HashMap;
use std::io::Read;

use anyhow::{Context, Result, bail};
use clap::{Arg, ArgAction};
use indexmap::IndexMap;

use discovery::Scope;
use manifest::{ParamDef, ParamKind};
use resolver::{PinInfo, Resolved, ResolvedRun};

fn main() {
    match run() {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            if e.downcast_ref::<prompt::Cancelled>().is_some() {
                std::process::exit(130);
            }
            eprintln!("pult: error: {e:#}");
            std::process::exit(1);
        }
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

fn run() -> Result<i32> {
    // `pult update` needs no manifest (the id is reserved, so no manifest
    // command can ever claim it) — handle it before discovery.
    let args: Vec<String> = std::env::args().skip(1).collect();
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
            _ => {
                eprintln!("usage: pult self schema");
                Ok(2)
            }
        };
    }

    let (loaded, scope) = match discovery::find_manifest() {
        Ok(found) => found,
        Err(e) => {
            // --version / --help should still work with no manifest around.
            let args: Vec<String> = std::env::args().skip(1).collect();
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
    let mut resolved = resolver::resolve(loaded)?;
    if scope == Scope::User {
        // Personal commands act on wherever you are; the manifest dir
        // (~/.config/pult) stays the base for the manifest's own includes.
        resolved.run_dir = std::env::current_dir().context("failed to read current directory")?;
    }

    let matches = build_cli(&resolved, scope).get_matches();
    let assume_trusted = matches.get_flag("trust");
    let print = matches.get_flag("print");
    let params_json = matches.get_flag("params-json");

    // `--params-json` only makes sense feeding a direct command invocation —
    // reject it up front for every other routing (bare flow, `--list`,
    // `doctor`, `includes`) rather than silently ignoring it.
    if params_json {
        let is_direct_command = !matches.get_flag("list")
            && matches!(matches.subcommand(), Some((id, _)) if id != "doctor" && id != "includes");
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
    if assume_trusted {
        trust::ensure_trusted(
            &resolved.path,
            &resolved.trust_hash,
            &resolved.include_summary,
            true,
            None,
        )?;
    }

    if matches.get_flag("list") {
        if matches.get_flag("json") {
            let trusted = trust::is_trusted(&resolved.path, &resolved.trust_hash)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&list_json(&resolved, trusted, scope))?
            );
        } else {
            print_list(&resolved);
        }
        return Ok(0);
    }

    match matches.subcommand() {
        Some(("doctor", sub)) => doctor::run(&resolved, assume_trusted, sub.get_flag("json")),
        Some(("includes", sub)) => match sub.subcommand() {
            Some(("verify", _)) => verify::run(&resolved),
            _ => {
                eprintln!("usage: pult includes <verify | add <SOURCE> [--prefix P] [--user]>");
                Ok(2)
            }
        },
        Some((id, sub)) => {
            let cmd = resolved
                .commands
                .iter()
                .find(|c| c.id == id)
                .expect("clap only accepts declared subcommands");
            let mut provided = HashMap::new();
            for name in cmd.params.keys() {
                if let Some(v) = sub.get_one::<String>(name.as_str()) {
                    provided.insert(name.clone(), v.clone());
                }
            }
            if params_json {
                let mut raw = String::new();
                std::io::stdin()
                    .read_to_string(&mut raw)
                    .context("failed to read stdin for --params-json")?;
                merge_stdin_params(&raw, &cmd.params, &mut provided)?;
            }
            exec::execute(&resolved, cmd, &provided, assume_trusted, print)
        }
        None => flow::run(&resolved, assume_trusted, print),
    }
}

/// Merge param values read from stdin (as a JSON object) into `provided`,
/// which is already populated from positional args. Kept as a pure function so
/// the parsing/merge rules are unit-testable without an actual stdin pipe.
///
/// Rules: stdin must be a JSON object whose values are all strings (anything
/// else is a clear error); every key must be a param this command declares
/// (typo safety — the error names the valid ones); and a param supplied both
/// positionally and via JSON is a conflict, not a silent override.
fn merge_stdin_params(
    raw: &str,
    declared: &IndexMap<String, ParamDef>,
    provided: &mut HashMap<String, String>,
) -> Result<()> {
    let value: serde_json::Value =
        serde_json::from_str(raw).context("--params-json: stdin is not valid JSON")?;
    let obj = value
        .as_object()
        .context("--params-json: stdin must be a JSON object of param name -> string value")?;
    for (key, v) in obj {
        if !declared.contains_key(key) {
            let known: Vec<&str> = declared.keys().map(String::as_str).collect();
            bail!(
                "--params-json: unknown param `{key}` — this command declares: {}",
                if known.is_empty() {
                    "(none)".to_string()
                } else {
                    known.join(", ")
                }
            );
        }
        let s = v.as_str().with_context(|| {
            format!("--params-json: param `{key}` must be a JSON string, got {v}")
        })?;
        if provided.contains_key(key) {
            bail!("param `{key}` was given both positionally and via --params-json — pick one");
        }
        provided.insert(key.clone(), s.to_string());
    }
    Ok(())
}

/// Build the CLI dynamically from the resolved manifest: every command becomes
/// a subcommand, every param a positional arg (optional — missing ones are
/// prompted for, so partial invocations degrade into a shorter guided flow).
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
               pult doctor [--json]         run every command's `check:` and report readiness\n  \
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
        .arg(
            Arg::new("trust")
                .long("trust")
                .global(true)
                .action(ArgAction::SetTrue)
                .help("Trust this repo's manifest without prompting (for CI)"),
        )
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
                .help("Read param values as a JSON object from stdin (keeps secrets out of argv)"),
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
        );

    for cmd in &resolved.commands {
        let mut sub = clap::Command::new(cmd.id.clone()).about(cmd.title.clone());
        for (index, (name, def)) in cmd.params.iter().enumerate() {
            let mut arg = Arg::new(name.clone())
                .index(index + 1)
                .required(false)
                .value_name(name.to_uppercase());
            arg = match def.kind() {
                ParamKind::Pick(pick) => match &pick.options {
                    Some(opts) => arg.help(format!("one of: {}", opts.join(", "))),
                    None => arg.help("picked from a dynamic option source if omitted"),
                },
                ParamKind::Input(input) if input.secret => {
                    arg.help("secret; prompted without echo if omitted")
                }
                ParamKind::Input(_) => arg.help("free text; prompted if omitted"),
                ParamKind::Use(_) => unreachable!("resolver inlines every use: param"),
            };
            sub = sub.arg(arg);
        }
        cli = cli.subcommand(sub);
    }
    cli
}

/// The `--list --json` document — the stable machine-readable surface for
/// agents and tooling. Schema 1; changes are additive only, breaking changes
/// bump `schema`. `trusted` is passed in so this stays a pure function of the
/// resolved manifest (the caller consults the trust store).
fn list_json(resolved: &Resolved, trusted: bool, scope: Scope) -> serde_json::Value {
    use serde_json::json;
    let includes: Vec<_> = resolved
        .pins
        .iter()
        .map(|pin| match pin {
            PinInfo::Local { source } => json!({ "source": source, "kind": "local" }),
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
                            json!({ "name": name, "kind": "pick", "options": options })
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
                    ParamKind::Use(_) => unreachable!("resolver inlines every use: param"),
                })
                .collect();
            // Step names a live run emits as `step k/n <name>` events (see
            // compile.rs / the PULT_EVENTS protocol) — same labels, so a
            // surface can render milestones without parsing the script.
            // `null` for a plain (string-form) `run:`, nothing to name.
            let steps = match &cmd.run {
                ResolvedRun::Steps(entries) => {
                    json!(entries.iter().map(compile::step_label).collect::<Vec<_>>())
                }
                ResolvedRun::Script(_) => serde_json::Value::Null,
            };
            json!({
                "id": cmd.id,
                "title": cmd.title,
                "origin": cmd.origin,
                "params": params,
                // Readiness probe (run it via `pult doctor`; null = none
                // declared) and the "needs a controlling terminal" contract —
                // non-terminal surfaces treat interactive commands as
                // terminal-only.
                "check": cmd.check,
                "interactive": cmd.interactive,
                "steps": steps,
            })
        })
        .collect();
    json!({
        "schema": 1,
        "pult_version": env!("CARGO_PKG_VERSION"),
        "name": resolved.name,
        "manifest": resolved.path,
        "dir": resolved.dir,
        "run_dir": resolved.run_dir,
        "scope": match scope { Scope::Repo => "repo", Scope::User => "user" },
        "trusted": trusted,
        "includes": includes,
        "commands": commands,
    })
}

fn print_list(resolved: &Resolved) {
    let width = resolved
        .commands
        .iter()
        .map(|c| c.id.len())
        .max()
        .unwrap_or(0);
    println!("{} · {}", resolved.name, resolved.path.display());
    for cmd in &resolved.commands {
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
        println!("{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
      env: { pick: { options: [dev, uat] } }
      customer: { pick: { from: "./bin/impl list --env {env}" } }
      note: { input: { default: "" } }
    run: "./bin/impl shell {customer} {env}"
    check: "command -v aws"
    interactive: true
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

        assert_eq!(doc["schema"], 1);
        assert_eq!(doc["name"], "demo");
        assert_eq!(doc["scope"], "repo");
        assert_eq!(doc["trusted"], false);
        assert_eq!(doc["run_dir"], doc["dir"]);
        let cmd = &doc["commands"][0];
        assert_eq!(cmd["id"], "shell");
        assert_eq!(cmd["origin"], serde_json::Value::Null);
        let params = cmd["params"].as_array().unwrap();
        assert_eq!(params[0]["kind"], "pick");
        assert_eq!(params[0]["options"][0], "dev");
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

    /// A command's declared params, for `merge_stdin_params` tests — going
    /// through a real manifest instead of hand-building `ParamDef`s keeps the
    /// fixture honest with what the resolver actually produces.
    fn declared_params(yaml: &str) -> IndexMap<String, ParamDef> {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("pult.yaml"), yaml).unwrap();
        let loaded = manifest::load(&dir.path().join("pult.yaml")).unwrap();
        let resolved = resolver::resolve(loaded).unwrap();
        resolved.commands[0].params.clone()
    }

    const TOKEN_CMD: &str = "version: 1\ncommands:\n  - id: c\n    title: C\n    params:\n      \
         token: { input: { secret: true } }\n      region: { input: { default: eu } }\n    run: \"true\"\n";

    #[test]
    fn merge_stdin_params_happy_path() {
        let declared = declared_params(TOKEN_CMD);
        let mut provided = HashMap::new();
        provided.insert("region".to_string(), "us".to_string());
        merge_stdin_params(r#"{"token":"hunter2"}"#, &declared, &mut provided).unwrap();
        assert_eq!(provided.get("token").map(String::as_str), Some("hunter2"));
        assert_eq!(provided.get("region").map(String::as_str), Some("us"));
    }

    #[test]
    fn merge_stdin_params_unknown_key_names_valid_params() {
        let declared = declared_params(TOKEN_CMD);
        let mut provided = HashMap::new();
        let err = merge_stdin_params(r#"{"toekn":"x"}"#, &declared, &mut provided).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("unknown param `toekn`"), "got: {msg}");
        assert!(
            msg.contains("token"),
            "should list valid params, got: {msg}"
        );
    }

    #[test]
    fn merge_stdin_params_conflict_with_positional_errors() {
        let declared = declared_params(TOKEN_CMD);
        let mut provided = HashMap::new();
        provided.insert("token".to_string(), "positional".to_string());
        let err =
            merge_stdin_params(r#"{"token":"hunter2"}"#, &declared, &mut provided).unwrap_err();
        assert!(
            format!("{err:#}").contains("both positionally and via --params-json"),
            "got: {err:#}"
        );
    }

    #[test]
    fn merge_stdin_params_non_string_value_errors() {
        let declared = declared_params(TOKEN_CMD);
        let mut provided = HashMap::new();
        let err = merge_stdin_params(r#"{"token": 123}"#, &declared, &mut provided).unwrap_err();
        assert!(
            format!("{err:#}").contains("must be a JSON string"),
            "got: {err:#}"
        );
    }

    #[test]
    fn merge_stdin_params_invalid_json_errors() {
        let declared = declared_params(TOKEN_CMD);
        let mut provided = HashMap::new();
        let err = merge_stdin_params("not json", &declared, &mut provided).unwrap_err();
        assert!(
            format!("{err:#}").contains("not valid JSON"),
            "got: {err:#}"
        );
    }

    #[test]
    fn merge_stdin_params_non_object_json_errors() {
        let declared = declared_params(TOKEN_CMD);
        let mut provided = HashMap::new();
        let err = merge_stdin_params(r#"["token"]"#, &declared, &mut provided).unwrap_err();
        assert!(
            format!("{err:#}").contains("must be a JSON object"),
            "got: {err:#}"
        );
    }
}
