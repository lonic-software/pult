mod compile;
mod discovery;
mod exec;
mod fetch;
mod flow;
mod interp;
mod manifest;
mod options;
mod prompt;
mod resolver;
mod runner;
mod selfupdate;
mod trust;
mod verify;

use std::collections::HashMap;

use anyhow::Result;
use clap::{Arg, ArgAction};

use manifest::ParamKind;
use resolver::Resolved;

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

fn run() -> Result<i32> {
    // `pult update` needs no manifest (the id is reserved, so no manifest
    // command can ever claim it) — handle it before discovery.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("update") {
        let requested = args.get(1).filter(|a| !a.starts_with('-'));
        return selfupdate::run(requested.map(String::as_str));
    }

    let loaded = match discovery::find_manifest() {
        Ok(loaded) => loaded,
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
                     Run inside a repo containing a pult.yaml. Bare `pult` opens the\n\
                     guided flow; `pult <command> [values…]` runs directly.\n\
                     `pult update` updates pult itself to the latest release.\n\n\
                     No pult.yaml was found from the current directory upward."
                );
                return Ok(0);
            }
            return Err(e);
        }
    };
    let resolved = resolver::resolve(loaded)?;

    let matches = build_cli(&resolved).get_matches();
    let assume_trusted = matches.get_flag("trust");
    let print = matches.get_flag("print");

    // `--trust` is an explicit act — record it even when this invocation
    // doesn't execute anything (e.g. `pult --trust --list`).
    if assume_trusted {
        trust::ensure_trusted(
            &resolved.path,
            &resolved.trust_hash,
            &resolved.include_summary,
            true,
        )?;
    }

    if matches.get_flag("list") {
        print_list(&resolved);
        return Ok(0);
    }

    match matches.subcommand() {
        Some(("includes", sub)) => match sub.subcommand() {
            Some(("verify", _)) => verify::run(&resolved),
            _ => {
                eprintln!("usage: pult includes verify");
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
            exec::execute(&resolved, cmd, &provided, assume_trusted, print)
        }
        None => flow::run(&resolved, assume_trusted, print),
    }
}

/// Build the CLI dynamically from the resolved manifest: every command becomes
/// a subcommand, every param a positional arg (optional — missing ones are
/// prompted for, so partial invocations degrade into a shorter guided flow).
fn build_cli(resolved: &Resolved) -> clap::Command {
    let mut cli = clap::Command::new("pult")
        .version(env!("CARGO_PKG_VERSION"))
        .about(format!(
            "{} · commands from {}",
            resolved.name,
            resolved.path.display()
        ))
        .after_help("Run bare `pult` for the guided flow.")
        .arg(
            Arg::new("list")
                .long("list")
                .action(ArgAction::SetTrue)
                .help("List the commands this repo declares"),
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
        .subcommand(
            clap::Command::new("includes")
                .about("Include maintenance (the id `includes` is reserved)")
                .subcommand(
                    clap::Command::new("verify")
                        .about("Check every include still resolves and no git tag has moved"),
                ),
        )
        // Listed for help only — `update` is intercepted before clap runs so
        // it also works with no manifest around.
        .subcommand(
            clap::Command::new("update")
                .about("Update pult itself to the latest release (or a given version)")
                .arg(Arg::new("version").required(false).value_name("VERSION")),
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
                ParamKind::Input(_) => arg.help("free text; prompted if omitted"),
                ParamKind::Use(_) => unreachable!("resolver inlines every use: param"),
            };
            sub = sub.arg(arg);
        }
        cli = cli.subcommand(sub);
    }
    cli
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
        if let Some(origin) = &cmd.origin {
            line.push_str(&format!("  ← {origin}"));
        }
        println!("{line}");
    }
}
