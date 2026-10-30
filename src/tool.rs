//! `pult tool <BIN> [COMMAND [ARGS…]] [--print]` — run a program's commands
//! from that program's own `self schema` document, with no manifest anywhere.
//!
//! BIN is resolved once to an absolute path and spawned twice from it: once
//! for `BIN self schema --json`, whose envelope carries the document, and
//! once for the run itself. A document is a manifest in a narrow subset of
//! the vocabulary ([`check_subset`]), and each of its runs is an argument
//! tail with a fixed grammar (see `resolver::resolve_tool`), so every run is
//! BIN invoked with arguments — never a shell, never another program.

use std::ffi::OsStr;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Arg, ArgAction};
use serde::Deserialize;
use serde_json::value::RawValue;

use crate::machine::{self, ErrorCode};
use crate::manifest::{
    self, CommandDef, FlagDef, FullOption, InputDef, Manifest, OptionDef, ParamDef, PickDef,
    RepeatDef, RunSpec,
};
use crate::resolver::{self, Resolved};
use crate::{exec, flow};

pub fn run_cli(rest: &[String]) -> Result<i32> {
    let cli = phase_one();
    let (head, tail) = crate::x::split_at_command(&cli, rest);
    let matches = machine::get_matches(
        cli,
        std::iter::once("pult tool".to_string()).chain(head.iter().cloned()),
    );
    let bin = matches.get_one::<String>("bin").expect("required");
    let command = matches.get_one::<String>("command").map(String::as_str);
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    run(
        bin,
        command,
        tail,
        &cwd,
        std::env::var_os("PATH").as_deref(),
        matches.get_flag("print"),
        std::io::stdin().is_terminal(),
    )
}

/// Phase one's parser: `pult tool`'s own arguments up to COMMAND.
fn phase_one() -> clap::Command {
    clap::Command::new("pult tool")
        .bin_name("pult tool")
        .about("Run a program's commands from its own `self schema` document")
        .arg(Arg::new("bin").required(true).value_name("BIN").help(
            "The program: a path (absolute, or relative to here), or a name looked up on PATH",
        ))
        .arg(
            Arg::new("command")
                .value_name("COMMAND")
                .help("Which of the program's commands to run; omitted opens the guided menu"),
        )
        // Only documents the tail: `split_at_command` keeps it out of this
        // parse, and `bind_tail` binds it once the command is known.
        .arg(Arg::new("args").value_name("ARGS").num_args(0..).help(
            "The command's arguments, as the document declares them; \
             `--help` here is the command's help",
        ))
        .arg(print_arg())
}

fn print_arg() -> Arg {
    Arg::new("print")
        .long("print")
        .action(ArgAction::SetTrue)
        .help("Print the composed command instead of running it (still runs BIN once to read its document)")
}

/// Bind the command's raw arguments: its params, plus `--print`, and
/// nothing else — clap refuses anything more (`--trust`, `--var`) as usage.
fn bind_tail(
    bin: &str,
    cmd: &resolver::ResolvedCommand,
    tail: &[String],
) -> (
    std::collections::HashMap<String, crate::value::Value>,
    clap::ArgMatches,
) {
    let matches = machine::get_matches(tail_cli(bin, cmd), tail.iter().cloned());
    (crate::bind::provided(cmd, &matches), matches)
}

fn tail_cli(bin: &str, cmd: &resolver::ResolvedCommand) -> clap::Command {
    clap::Command::new(format!("pult tool {bin} {}", cmd.id))
        .no_binary_name(true)
        .about(cmd.title.clone())
        .args(crate::bind::args(cmd))
        .arg(print_arg())
}

/// The whole flow. `path_var` is the PATH a bare BIN is looked up on.
pub fn run(
    bin: &str,
    command: Option<&str>,
    tail: &[String],
    cwd: &Path,
    path_var: Option<&OsStr>,
    print: bool,
    interactive: bool,
) -> Result<i32> {
    let program = resolve_bin(bin, cwd, path_var)?;
    let resolved =
        load(&program, cwd).map_err(|e| machine::coded(ErrorCode::InvalidManifest, e))?;
    eprintln!("◆  {}", program.display());

    match command {
        Some(id) => {
            let cmd = resolved
                .commands
                .iter()
                .find(|c| c.id == id)
                .ok_or_else(|| {
                    anyhow!(
                        "`{bin}` has no command `{id}` — it offers: {}",
                        command_ids(&resolved)
                    )
                })?;
            let (provided, matches) = bind_tail(bin, cmd, tail);
            exec::execute(
                &resolved,
                cmd,
                &provided,
                exec::Prompting::Direct,
                &crate::trust::Grant::Ask,
                print || matches.get_flag("print"),
                None,
            )
        }
        None if interactive => flow::run(&resolved, &crate::trust::Grant::Ask, print, None),
        None => {
            eprintln!("specify a command:  pult tool {bin} <command> [args…]");
            eprintln!("commands:");
            for c in &resolved.commands {
                eprintln!("  {}  {}", c.id, c.title);
            }
            Ok(2)
        }
    }
}

fn command_ids(resolved: &Resolved) -> String {
    resolved
        .commands
        .iter()
        .map(|c| c.id.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// BIN as the absolute path every spawn uses. With a path separator it is
/// made absolute against `cwd` without resolving symlinks — a tool may
/// dispatch on the name it was invoked by. A bare name is the first
/// executable file on PATH (absolute entries only; PATHEXT on Windows).
/// Nothing found is a usage error.
fn resolve_bin(bin: &str, cwd: &Path, path_var: Option<&OsStr>) -> Result<PathBuf> {
    let has_separator = bin.contains('/') || (cfg!(windows) && bin.contains('\\'));
    if has_separator {
        let path = std::path::absolute(cwd.join(bin))
            .with_context(|| format!("failed to resolve `{bin}`"))?;
        if is_executable(&path) {
            return Ok(path);
        }
        return Err(machine::usage(format!(
            "pult tool: `{}` is not an executable file",
            path.display()
        )));
    }
    if !bin.is_empty() {
        for dir in path_var.map(std::env::split_paths).into_iter().flatten() {
            if !dir.is_absolute() {
                continue;
            }
            for name in candidate_names(bin) {
                let path = dir.join(&name);
                if is_executable(&path) {
                    return Ok(path);
                }
            }
        }
    }
    Err(machine::usage(format!(
        "pult tool: no executable `{bin}` on PATH"
    )))
}

/// The file names a bare BIN may have: itself, and on Windows itself with
/// each PATHEXT extension.
fn candidate_names(bin: &str) -> Vec<String> {
    let mut names = vec![bin.to_string()];
    if cfg!(windows) {
        names.extend(pathext().iter().map(|e| format!("{bin}{e}")));
    }
    names
}

fn pathext() -> Vec<String> {
    std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string())
        .split(';')
        .filter(|e| !e.is_empty())
        .map(str::to_string)
        .collect()
}

/// Windows runs a file only by its extension, so an extensionless file
/// (a POSIX shell script beside `name.exe`, say) is not a program there.
fn has_pathext(path: &Path, pathext: &[String]) -> bool {
    path.extension().is_some_and(|ext| {
        pathext.iter().any(|e| {
            e.strip_prefix('.')
                .is_some_and(|e| ext.eq_ignore_ascii_case(e))
        })
    })
}

fn is_executable(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    if cfg!(windows) {
        return has_pathext(path, &pathext());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    true
}

/// Fetch, parse, subset-check and resolve `program`'s document.
fn load(program: &Path, cwd: &Path) -> Result<Resolved> {
    let what = format!("`{} self schema --json`", program.display());
    let output = Command::new(program)
        .args(["self", "schema", "--json"])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("could not run {what}"))?;
    let document = document(&output.stdout, &output.stderr, program)?;
    check_subset(&document)
        .with_context(|| format!("{what} declares something pult tool refuses"))?;
    resolver::resolve_tool(document, program, cwd)
        .with_context(|| format!("{what} printed a document pult cannot use"))
}

/// The envelope fields `pult tool` reads; everything else (`tool`, `schema`,
/// `command`, …) is the tool's own business. `data` is kept as the exact
/// bytes the tool wrote: never through `serde_json::Value`, whose map keeps
/// the last of two equal keys and would hide a repeat from the parse.
#[derive(Deserialize)]
struct Envelope<'a> {
    lonic_json: Option<serde_json::Number>,
    #[serde(default)]
    ok: bool,
    #[serde(borrow)]
    data: Option<&'a RawValue>,
    error: Option<ErrorBody>,
}

#[derive(Deserialize)]
struct ErrorBody {
    message: Option<String>,
}

/// The manifest inside a `self schema --json` envelope. Every failure names
/// the program and carries what it said: its `error.message`, or else the
/// end of its stderr.
fn document(stdout: &[u8], stderr: &[u8], program: &Path) -> Result<Manifest> {
    let what = format!("`{} self schema --json`", program.display());
    let envelope: Envelope = serde_json::from_slice(stdout).map_err(|e| {
        anyhow!(
            "{what} did not print one JSON document ({e}){}",
            stderr_tail(stderr)
        )
    })?;
    match &envelope.lonic_json {
        Some(n) if n.as_u64() == Some(1) => {}
        Some(n) => bail!("{what} printed lonic_json {n}, but pult reads only lonic_json 1"),
        None => bail!("{what} printed no lonic_json, so it is not a Lonic envelope"),
    }
    if !envelope.ok {
        let message = envelope.error.and_then(|e| e.message);
        bail!(
            "{what} failed: {}",
            message.unwrap_or_else(|| format!("no message{}", stderr_tail(stderr)))
        );
    }
    let data = envelope
        .data
        .with_context(|| format!("{what} printed ok:true with no data"))?;
    manifest::parse_json(
        data.get(),
        Path::new(&format!("{} self schema", program.display())),
    )
}

/// The last few lines of a failing tool's stderr, for the error message.
fn stderr_tail(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let lines: Vec<&str> = text.trim_end().lines().collect();
    if lines.is_empty() {
        return String::new();
    }
    format!(": {}", lines[lines.len().saturating_sub(5)..].join("\n"))
}

/// A tool document is an allowlisted subset of the manifest vocabulary:
/// `version`, `name`, `description`, `commands`; per command `id`, `title`,
/// `category`, `description`, `interactive`, `params` and a one-line `run`;
/// per param `input`, `flag`, `repeat`, or `pick` with static `options`, and
/// an optional `description`.
/// Every struct is destructured in full, with no `..`, so a field the
/// vocabulary grows fails to compile here until it is placed on one side.
fn check_subset(m: &Manifest) -> Result<()> {
    let Manifest {
        version: _,
        name: _,
        description: _,
        includes,
        registries,
        vars,
        params,
        steps,
        commands,
    } = m;
    let top = [
        ("includes", !includes.is_empty()),
        ("registries", registries.is_some()),
        ("vars", !vars.is_empty()),
        ("params", !params.is_empty()),
        ("steps", !steps.is_empty()),
    ];
    if let Some((field, _)) = top.iter().find(|(_, set)| *set) {
        bail!("`{field}:` is outside what a tool document may declare");
    }
    for cmd in commands {
        let CommandDef {
            id,
            title: _,
            params,
            run,
            check,
            interactive: _,
            category: _,
            description: _,
        } = cmd;
        let refuse = |field: &str| {
            anyhow!("command `{id}`: `{field}` is outside what a tool document may declare")
        };
        if check.is_some() {
            return Err(refuse("check"));
        }
        match run {
            RunSpec::Script(_) => {}
            RunSpec::List(_) => return Err(refuse("a step-list run")),
        }
        for (name, def) in params {
            let ParamDef {
                pick,
                input,
                flag,
                repeat,
                use_,
                description: _,
            } = def;
            if use_.is_some() {
                return Err(refuse(&format!("{name}.use")));
            }
            if let Some(PickDef { options, from }) = pick {
                if from.is_some() {
                    return Err(refuse(&format!("{name}.pick.from")));
                }
                for option in options.iter().flatten() {
                    match option {
                        OptionDef::Plain(_) => {}
                        OptionDef::Full(FullOption {
                            value: _,
                            description: _,
                        }) => {}
                    }
                }
            }
            if let Some(InputDef {
                default: _,
                secret: _,
            }) = input
            {}
            if let Some(FlagDef { arg: _ }) = flag {}
            if let Some(RepeatDef {
                arg: _,
                min: _,
                max: _,
                secret: _,
            }) = repeat
            {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolver::ResolvedRun;

    /// `forklift self schema --json`, verbatim; provenance beside it.
    const FIXTURE: &str = include_str!("../tests/fixtures/forklift-self-schema.json");

    fn from_envelope(stdout: &str, program: &Path) -> Result<Resolved> {
        let document = document(stdout.as_bytes(), b"", program)?;
        check_subset(&document)?;
        resolver::resolve_tool(document, program, Path::new("/work"))
    }

    fn forklift() -> Resolved {
        from_envelope(FIXTURE, Path::new("/opt/forklift")).unwrap()
    }

    /// The argv `pult tool BIN <id> <tail…>` spawns its program with.
    fn composed(resolved: &Resolved, id: &str, tail: &[&str]) -> Vec<String> {
        let cmd = resolved.commands.iter().find(|c| c.id == id).unwrap();
        let matches = tail_cli("forklift", cmd)
            .try_get_matches_from(tail)
            .unwrap_or_else(|e| panic!("{id} {tail:?}: {e}"));
        let provided = crate::bind::provided(cmd, &matches);
        let values = exec::fill(cmd, &provided, None, exec::Prompting::Direct).unwrap();
        let ResolvedRun::Argv(words) = &cmd.run else {
            panic!("{id}: not an argv run");
        };
        exec::argv(cmd, words, &values, &values)
    }

    #[test]
    fn the_banner_shows_the_shape_that_ran_with_a_secret_masked() {
        let stdout = doc(
            r#"{"id":"c","title":"T","run":"c {token}","params":{"token":{"repeat":{"arg":"--token","secret":true}}}}"#,
        );
        let resolved = from_envelope(&stdout, Path::new("/opt/t")).unwrap();
        let cmd = &resolved.commands[0];
        let ResolvedRun::Argv(words) = &cmd.run else {
            panic!("not an argv run");
        };
        let list = |item: &str| {
            [(
                "token".to_string(),
                crate::value::Value::List(vec![item.to_string()]),
            )]
            .into_iter()
            .collect()
        };
        let (values, shown) = (list("-s3cr3t"), list("••••••"));
        assert_eq!(
            exec::argv(cmd, words, &values, &values),
            ["c", "--token=-s3cr3t"]
        );
        assert_eq!(
            exec::argv(cmd, words, &values, &shown),
            ["c", "--token=••••••"]
        );
    }

    #[test]
    fn a_tool_document_may_describe_its_params() {
        let stdout = doc(
            r#"{"id":"c","title":"T","run":"c -- {p}","params":{"p":{"input":{},"description":"What p is"}}}"#,
        );
        let resolved = from_envelope(&stdout, Path::new("/opt/t")).unwrap();
        assert_eq!(
            resolved.commands[0].params["p"].description.as_deref(),
            Some("What p is")
        );
    }

    #[test]
    fn windows_runs_only_a_file_with_a_pathext_extension() {
        let pathext = [".COM".to_string(), ".EXE".to_string()];
        assert!(has_pathext(Path::new("forklift.exe"), &pathext));
        assert!(has_pathext(Path::new("forklift.EXE"), &pathext));
        assert!(!has_pathext(Path::new("forklift"), &pathext));
        assert!(!has_pathext(Path::new("forklift.sh"), &pathext));
        assert!(!has_pathext(Path::new("forklift.exe"), &[]));
    }

    fn envelope(data: &str) -> String {
        format!(
            r#"{{"lonic_json":1,"tool":"t","schema":7,"command":"self","ok":true,"data":{data}}}"#
        )
    }

    fn doc(commands: &str) -> String {
        envelope(&format!(r#"{{"version":1,"commands":[{commands}]}}"#))
    }

    fn refusal(stdout: &str) -> String {
        format!(
            "{:#}",
            from_envelope(stdout, Path::new("/opt/t")).unwrap_err()
        )
    }

    #[test]
    fn every_forklift_command_parses_into_the_tool_subset() {
        let resolved = forklift();
        assert_eq!(resolved.commands.len(), 100);
        assert_eq!(resolved.name, "forklift");
        assert!(
            resolved
                .commands
                .iter()
                .all(|c| matches!(c.run, ResolvedRun::Argv(_)))
        );
    }

    #[test]
    fn clone_keeps_the_emitted_param_order() {
        let resolved = forklift();
        let clone = resolved.commands.iter().find(|c| c.id == "clone").unwrap();
        let names: Vec<&str> = clone.params.keys().map(String::as_str).collect();
        assert_eq!(
            names,
            [
                "pallet",
                "token",
                "only",
                "cache",
                "expect",
                "url",
                "directory"
            ]
        );
    }

    #[test]
    fn forklift_params_carry_their_help_as_their_description() {
        let resolved = forklift();
        for cmd in &resolved.commands {
            for (key, param) in &cmd.params {
                assert!(param.description.is_some(), "{} {key}", cmd.id);
            }
        }
        let diff = resolved.commands.iter().find(|c| c.id == "diff").unwrap();
        assert_eq!(
            diff.params["staged"].description.as_deref(),
            Some(
                "Compare the inventory against the pallet head (what \"forklift stack\" would record)"
            )
        );
    }

    #[test]
    fn forklift_commands_compose_to_these_argvs() {
        let resolved = forklift();
        let table: &[(&str, &[&str], &[&str])] = &[
            (
                "diff",
                &["--staged", "--", "--staged", "b"],
                &["diff", "--staged", "--", "--staged", "b"],
            ),
            ("diff", &[], &["diff", "--"]),
            (
                "profile-create",
                &["--id", "u-1", "bob"],
                &["profile", "create", "--id", "u-1", "--", "bob"],
            ),
            // A dash-leading value under a `--` option becomes one argument.
            ("query", &["--grep=-x"], &["query", "--grep=-x", "--"]),
            ("query", &["--grep", "x"], &["query", "--grep", "x", "--"]),
            (
                "clone",
                &["--only", "a", "--only=-b", "https://h/w", "dir"],
                &[
                    "clone",
                    "--only",
                    "a",
                    "--only=-b",
                    "--",
                    "https://h/w",
                    "dir",
                ],
            ),
            // A command inside a family nested in another.
            (
                "self-alias-install",
                &["fl2"],
                &["self", "alias", "install", "--", "fl2"],
            ),
            // Recorded text is an option, and a destructive command acts only when confirmed.
            ("stack", &["--message", "x"], &["stack", "--message", "x"]),
            (
                "scope-prune",
                &["--confirm", "docs"],
                &["scope", "prune", "--confirm", "--", "docs"],
            ),
            (
                "restore",
                &["--confirm", "a"],
                &["restore", "--confirm", "--", "a"],
            ),
            ("version", &[], &["version"]),
        ];
        let mut wrong = Vec::new();
        for (id, tail, want) in table {
            let got = composed(&resolved, id, tail);
            if got != *want {
                wrong.push(format!("{id} {tail:?}: got {got:?}"));
            }
        }
        assert!(wrong.is_empty(), "{wrong:#?}");
    }

    /// BIN never enters interpolated text: whatever its path holds, the
    /// program is exactly it and the arguments are unchanged.
    #[test]
    fn a_hostile_bin_path_is_spawned_verbatim() {
        let bin = Path::new("/tmp/a b/{diff}/'q'/$(x)/forklift");
        let resolved = from_envelope(FIXTURE, bin).unwrap();
        assert_eq!(resolved.path, bin);
        let args = composed(&resolved, "diff", &["--staged", "--", "x"]);
        let command = crate::runner::argv_command(&resolved.path, &args, Path::new("/"));
        assert_eq!(command.get_program(), bin.as_os_str());
        let spawned: Vec<&OsStr> = command.get_args().collect();
        assert_eq!(spawned, ["diff", "--staged", "--", "x"]);
    }

    #[test]
    fn an_envelope_that_cannot_carry_a_document_is_refused_naming_why() {
        let cases: &[(&str, &[u8], &str)] = &[
            (
                "",
                b"error: unrecognized subcommand 'self'\n",
                "unrecognized subcommand",
            ),
            ("{} {}", b"", "did not print one JSON document"),
            (
                r#"{"lonic_json":2,"ok":true,"data":{}}"#,
                b"",
                "lonic_json 2",
            ),
            (r#"{"ok":true,"data":{}}"#, b"", "no lonic_json"),
            (
                r#"{"lonic_json":1,"ok":false,"error":{"code":"x","message":"no warehouse"}}"#,
                b"",
                "failed: no warehouse",
            ),
            (
                r#"{"lonic_json":1,"ok":false}"#,
                b"boom\n",
                "failed: no message: boom",
            ),
            (r#"{"lonic_json":1,"ok":true}"#, b"", "ok:true with no data"),
            (
                r#"{"lonic_json":1,"ok":true,"data":{"version":1},"data":{"version":1}}"#,
                b"",
                "duplicate field `data`",
            ),
        ];
        for (stdout, stderr, want) in cases {
            let err = document(stdout.as_bytes(), stderr, Path::new("/opt/t"))
                .err()
                .map(|e| format!("{e:#}"))
                .unwrap_or_default();
            assert!(err.contains(want), "{stdout}: {err}");
            assert!(
                err.contains("/opt/t self schema --json") || err.contains("/opt/t self schema"),
                "{err}"
            );
        }
    }

    /// The document is read from its exact bytes, by a JSON parser, version
    /// probe first.
    #[test]
    fn the_document_is_parsed_as_json_from_its_own_bytes() {
        let cmd = r#"{"id":"c","title":"T","run":"c"}"#;
        // A repeated param key is refused: a detour through
        // `serde_json::Value` would keep the last one silently.
        let repeated = doc(
            r#"{"id":"c","title":"T","run":"c {k}","params":{"k":{"input":{}},"k":{"flag":{}}}}"#,
        );
        assert!(refusal(&repeated).contains(r#"duplicate key "k""#));
        // A surrogate-pair escape is valid JSON (YAML's parser refuses it).
        let emoji = doc(r#"{"id":"c","title":"😀","run":"c"}"#);
        let resolved = from_envelope(&emoji, Path::new("/opt/t")).unwrap();
        assert_eq!(resolved.commands[0].title, "😀");
        // A newer document says to upgrade, not that a field is unknown.
        let newer = envelope(&format!(
            r#"{{"version":{},"later":true,"commands":[{cmd}]}}"#,
            manifest::SUPPORTED_VERSION + 1
        ));
        assert!(
            refusal(&newer).contains("upgrade pult"),
            "{}",
            refusal(&newer)
        );
    }

    #[test]
    fn a_document_outside_the_subset_is_refused_naming_the_field() {
        let c = r#"{"id":"c","title":"T","run":"c"}"#;
        let top = |extra: &str| envelope(&format!(r#"{{"version":1,{extra},"commands":[{c}]}}"#));
        let cases = [
            (top(r#""includes":[{"source":"./m"}]"#), "`includes:`"),
            (top(r#""vars":{"v":{"required":true}}"#), "`vars:`"),
            (top(r#""params":{"p":{"input":{}}}"#), "`params:`"),
            (top(r#""steps":{"s":"echo"}"#), "`steps:`"),
            (
                doc(r#"{"id":"c","title":"T","run":"c","check":"true"}"#),
                "command `c`: `check`",
            ),
            (
                doc(r#"{"id":"c","title":"T","run":["c"]}"#),
                "command `c`: `a step-list run`",
            ),
            (
                doc(
                    r#"{"id":"c","title":"T","run":"c {p}","params":{"p":{"pick":{"from":"ls"}}}}"#,
                ),
                "command `c`: `p.pick.from`",
            ),
        ];
        for (stdout, want) in cases {
            let err = refusal(&stdout);
            assert!(err.contains(want), "want {want}: {err}");
        }
    }

    #[test]
    fn a_run_outside_the_grammar_is_refused() {
        const GRAMMAR: &str = "outside a tool run's grammar";
        // An undeclared placeholder and an empty run are refused before the
        // grammar, as in any manifest.
        let runs = [
            ("c; rm -rf x", GRAMMAR),
            ("c 'q'", GRAMMAR),
            ("c \"q\"", GRAMMAR),
            ("c  d", GRAMMAR),
            ("c ", GRAMMAR),
            ("c\nd", GRAMMAR),
            ("c --flag", GRAMMAR),
            ("c --x={p}", GRAMMAR),
            ("c {p}x", GRAMMAR),
            ("c $HOME", GRAMMAR),
            ("c {nope}", "not a declared param"),
            ("", "has an empty run"),
        ];
        for (run, want) in runs {
            let stdout = doc(&format!(
                r#"{{"id":"c","title":"T","run":{},"params":{{"p":{{"input":{{}}}}}}}}"#,
                serde_json::to_string(run).unwrap()
            ));
            let err = refusal(&stdout);
            assert!(err.contains(want), "{run:?}: {err}");
        }
        let fine = doc(
            r#"{"id":"c","title":"T","run":"c sub.v1 a_b -- {p}","params":{"p":{"input":{}}}}"#,
        );
        from_envelope(&fine, Path::new("/opt/t")).unwrap();
    }

    /// Pult's reserved ids bind only pult's own subcommands; under
    /// `pult tool BIN <id>` nothing competes with a tool's `trust`.
    #[test]
    fn a_tool_may_name_a_command_pult_reserves() {
        let stdout = doc(r#"{"id":"trust","title":"T","run":"trust"}"#);
        let resolved = from_envelope(&stdout, Path::new("/opt/t")).unwrap();
        assert_eq!(resolved.commands[0].id, "trust");
        // A param named after one of pult's own options still collides.
        let print =
            doc(r#"{"id":"c","title":"T","run":"c {print}","params":{"print":{"flag":{}}}}"#);
        assert!(refusal(&print).contains("reserved name"));
    }

    #[test]
    fn bin_resolves_by_path_or_on_path() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("tool-x");
        std::fs::write(&exe, "").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        // By path, relative to the cwd, made absolute without resolving.
        let got = resolve_bin("./tool-x", dir.path(), None).unwrap();
        assert_eq!(
            got,
            std::path::absolute(dir.path().join("./tool-x")).unwrap()
        );
        // Bare, on PATH; a relative entry is skipped.
        let path = std::env::join_paths([Path::new("rel"), dir.path()]).unwrap();
        let got = resolve_bin("tool-x", Path::new("/"), Some(&path)).unwrap();
        assert_eq!(got, exe);
        let relative_only = std::env::join_paths([Path::new(".")]).unwrap();
        let err = resolve_bin("tool-x", dir.path(), Some(&relative_only)).unwrap_err();
        assert_eq!(machine::classify(&err), ErrorCode::Usage);
        // Not found, or not a file: usage.
        let err = resolve_bin("./missing", dir.path(), None).unwrap_err();
        assert_eq!(machine::classify(&err), ErrorCode::Usage);
        let err = resolve_bin("./", dir.path(), None).unwrap_err();
        assert_eq!(machine::classify(&err), ErrorCode::Usage);
    }

    #[cfg(unix)]
    #[test]
    fn a_file_without_an_exec_bit_is_not_a_bin() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("plain"), "").unwrap();
        let err = resolve_bin("./plain", dir.path(), None).unwrap_err();
        assert!(format!("{err}").contains("not an executable file"), "{err}");
    }
}
