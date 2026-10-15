//! The machine-readable surface: under `--json`, pult writes exactly one JSON
//! document on stdout, in the envelope shared by every Lonic tool. A success
//! carries its payload in `data`; a failure pult controls itself carries an
//! `error`, and the document replaces the prose line pult prints on stderr
//! without `--json`. When the document cannot be written (stdout is closed),
//! the prose line is printed instead, so a failure never goes unreported.
//!
//! ```json
//! {"lonic_json": 1, "tool": "pult", "schema": 2, "command": "list", "ok": true,
//!  "data": {"pult_version": "…", "name": "…", "commands": […], …}}
//! {"lonic_json": 1, "tool": "pult", "schema": 2, "command": "list", "ok": false,
//!  "error": {"code": "no_manifest", "message": "no pult.yaml found …"}}
//! ```
//!
//! `ok` says whether pult produced its result, not whether that result is
//! good news: `doctor` with failing checks exits 1 and still says `ok: true`.
//! Help and version are results too: under `--json` their text is a success
//! document's `data.message`, exit 0.
//!
//! A consumer classifies a failure by `error.code`, never by the exit status:
//! pult hands a command's own exit code straight through, so every number
//! except 0, 1, 2, 130 and 128+n may belong to whatever pult ran. That is
//! why every code here exits 1 except `usage` (2) and `cancelled` (130).

use std::sync::OnceLock;

use crate::prompt;

/// The version of the envelope every Lonic tool shares.
const LONIC_JSON: u32 = 1;
const TOOL: &str = "pult";
/// The version of pult's own payloads — the `schema` its JSON documents
/// carry. Bumped only on an incompatible change; a new param kind is not one
/// (see docs/reference.md on unknown kinds). The run journal versions its
/// on-disk records with its own counter (journal.rs).
pub const SCHEMA: u32 = 2;

/// Pult's error-code registry. A code never changes meaning within one
/// `schema`; new codes may be added.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    /// The command line could not be parsed, or names no known subcommand.
    Usage,
    /// No manifest was found from the current directory upward, and there is
    /// no user manifest.
    NoManifest,
    /// The manifest, or something it includes, is not valid or does not resolve.
    InvalidManifest,
    /// The manifest is not trusted at its current content, and nothing granted it.
    TrustChanged,
    /// A git module could not be fetched.
    FetchFailed,
    /// The user cancelled at one of pult's own prompts.
    Cancelled,
    /// Any failure not otherwise classified.
    Generic,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::Usage => "usage",
            ErrorCode::NoManifest => "no_manifest",
            ErrorCode::InvalidManifest => "invalid_manifest",
            ErrorCode::TrustChanged => "trust_changed",
            ErrorCode::FetchFailed => "fetch_failed",
            ErrorCode::Cancelled => "cancelled",
            ErrorCode::Generic => "generic",
        }
    }

    pub fn exit_code(self) -> i32 {
        match self {
            ErrorCode::Usage => 2,
            ErrorCode::Cancelled => 130,
            _ => 1,
        }
    }
}

/// An error carrying its [`ErrorCode`]. It displays exactly as the error it
/// wraps, and continues that error's source chain, so `{e:#}` — the prose
/// line on stderr — is byte-identical with or without the code attached.
#[derive(Debug)]
pub struct Coded {
    code: ErrorCode,
    /// The error document's `details`, when the failure has structured facts
    /// a consumer acts on (a trust refusal's hashes).
    details: Option<serde_json::Value>,
    inner: anyhow::Error,
}

impl std::fmt::Display for Coded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.inner, f)
    }
}

impl std::error::Error for Coded {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.inner.source()
    }
}

/// Attach `code` to `err` — unless `err` already carries one, which then
/// wins: a code is attached where the failure is best understood, so a fetch
/// failure inside manifest resolution stays `fetch_failed` when the
/// resolution boundary later marks everything it returns `invalid_manifest`.
/// An error that already carries a code is returned as it is, so a prompt's
/// sentinel stays reachable by `downcast_ref` at the top of the error.
pub fn coded(code: ErrorCode, err: impl Into<anyhow::Error>) -> anyhow::Error {
    attach(code, None, err.into())
}

/// [`coded`], with the `details` the error document carries under `--json`.
pub fn coded_with_details(
    code: ErrorCode,
    details: serde_json::Value,
    err: impl Into<anyhow::Error>,
) -> anyhow::Error {
    attach(code, Some(details), err.into())
}

fn attach(
    code: ErrorCode,
    details: Option<serde_json::Value>,
    inner: anyhow::Error,
) -> anyhow::Error {
    if existing_code(&inner).is_some() {
        return inner;
    }
    anyhow::Error::new(Coded {
        code,
        details,
        inner,
    })
}

/// The code an error already carries: a prompt's own cancellation sentinel,
/// or the outermost [`Coded`] in its chain (by [`coded`], the only one).
fn existing_code(err: &anyhow::Error) -> Option<ErrorCode> {
    err.chain().find_map(|e| {
        if e.is::<prompt::Cancelled>() || e.is::<prompt::Dismissed>() {
            Some(ErrorCode::Cancelled)
        } else {
            e.downcast_ref::<Coded>().map(|c| c.code)
        }
    })
}

/// The `details` attached with the error's code, if any.
fn details(err: &anyhow::Error) -> Option<serde_json::Value> {
    err.chain()
        .find_map(|e| e.downcast_ref::<Coded>())
        .and_then(|c| c.details.clone())
}

/// How a failure is classified: its attached code, else `generic`.
pub fn classify(err: &anyhow::Error) -> ErrorCode {
    existing_code(err).unwrap_or(ErrorCode::Generic)
}

/// A usage error with pult's own text, exit 2. Without `--json` it is printed
/// to stderr as-is (no `pult: error:` prefix, the way pult has always printed
/// its usage lines); under `--json` it is the document's `message`.
pub fn usage(message: impl Into<String>) -> anyhow::Error {
    coded(ErrorCode::Usage, anyhow::anyhow!(message.into()))
}

/// The invocation's machine mode, decided once from the raw arguments
/// before anything else runs: a failure can happen before (or inside) the
/// parse that would otherwise say whether `--json` was asked for.
struct Mode {
    json: bool,
    command: Option<String>,
    /// The `command` of a help or version document: the surface whose help
    /// was asked for, which is not `command` when the flag comes first
    /// (`pult --help doctor` is the top-level help).
    help_surface: Option<String>,
}

static MODE: OnceLock<Mode> = OnceLock::new();

/// Record the machine mode for this process from its arguments (argv[0]
/// excluded). Called once, first thing.
pub fn init(args: &[String]) {
    let _ = MODE.set(Mode {
        json: wants_json(args),
        command: command_name(args),
        help_surface: help_surface(args),
    });
}

/// Whether the arguments ask for JSON: a bare `--json` token before any
/// `--`. Only the exact token counts, so a value that merely contains it
/// (`--run-id=--json`) does not.
fn wants_json(args: &[String]) -> bool {
    args.iter()
        .take_while(|a| a.as_str() != "--")
        .any(|a| a == "--json")
}

/// The machine surface the arguments name, for the document's `command`:
/// `doctor`, `runs list`, `runs tail`, or `list` for `--list`; `None` for
/// anything else.
fn command_name(args: &[String]) -> Option<String> {
    Scan::of(args, false).surface(true)
}

/// The machine surface a help or version flag answers for: the one named
/// before the first such flag, where `--list` names nothing, since
/// `--list --help` is the top-level help.
fn help_surface(args: &[String]) -> Option<String> {
    Scan::of(args, true).surface(false)
}

/// The help or version request pult answers itself when no manifest is
/// found: only a top-level one, a flag before any subcommand. A subcommand's
/// help needs the manifest that declares it.
pub fn top_level_ask(args: &[String]) -> Option<Ask> {
    let scan = Scan::of(args, true);
    scan.ask.filter(|_| scan.positionals.is_empty())
}

/// A help or version flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ask {
    Help,
    Version,
}

/// The arguments before any `--`, read the way the top-level parser reads
/// them: the first argument that is not an option names the subcommand, and
/// `--run-id` is the one global option that takes a value.
struct Scan<'a> {
    positionals: Vec<&'a str>,
    list: bool,
    /// The first help or version flag, when the scan stops at it.
    ask: Option<Ask>,
}

impl<'a> Scan<'a> {
    /// With `stop_at_ask`, the scan ends at the first help or version flag,
    /// the one clap acts on.
    fn of(args: &'a [String], stop_at_ask: bool) -> Scan<'a> {
        let mut scan = Scan {
            positionals: Vec::new(),
            list: false,
            ask: None,
        };
        let mut args = args.iter().map(String::as_str);
        while let Some(arg) = args.next() {
            match arg {
                "--" => break,
                "--run-id" => {
                    // clap refuses a value that starts with `-`, so the line
                    // is a usage error there and no help or version answers.
                    let value = args.next();
                    if stop_at_ask && value.is_none_or(|v| v.starts_with('-')) {
                        break;
                    }
                }
                "--list" => scan.list = true,
                a if a.starts_with('-') => {
                    if stop_at_ask && let Some(ask) = asks(a) {
                        scan.ask = Some(ask);
                        break;
                    }
                }
                a => scan.positionals.push(a),
            }
        }
        scan
    }

    fn surface(&self, list: bool) -> Option<String> {
        match self.positionals.as_slice() {
            ["doctor", ..] => Some("doctor".to_string()),
            ["runs", sub @ ("list" | "tail"), ..] => Some(format!("runs {sub}")),
            [] if list && self.list => Some("list".to_string()),
            _ => None,
        }
    }
}

/// `--help`, `--version`, or whichever of `h` and `V` comes first in a
/// short-flag cluster, as clap acts on the first.
fn asks(arg: &str) -> Option<Ask> {
    match arg.strip_prefix("--") {
        Some("help") => Some(Ask::Help),
        Some("version") => Some(Ask::Version),
        Some(_) => None,
        None => arg.strip_prefix('-')?.chars().find_map(|c| match c {
            'h' => Some(Ask::Help),
            'V' => Some(Ask::Version),
            _ => None,
        }),
    }
}

/// The error document for a failure. `message` is the whole error chain,
/// the same text the prose line carries.
fn document(
    command: Option<&str>,
    code: ErrorCode,
    message: &str,
    details: Option<serde_json::Value>,
) -> serde_json::Value {
    let mut error = serde_json::json!({ "code": code.as_str(), "message": message });
    if let Some(details) = details {
        error["details"] = details;
    }
    serde_json::json!({
        "lonic_json": LONIC_JSON,
        "tool": TOOL,
        "schema": SCHEMA,
        "command": command,
        "ok": false,
        "error": error,
    })
}

/// The success document for `command`, carrying `data`.
pub fn success(command: &str, data: serde_json::Value) -> serde_json::Value {
    succeeded(Some(command), data)
}

fn succeeded(command: Option<&str>, data: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "lonic_json": LONIC_JSON,
        "tool": TOOL,
        "schema": SCHEMA,
        "command": command,
        "ok": true,
        "data": data,
    })
}

/// The success document answering a help or version request with `text`.
fn help_document(command: Option<&str>, text: &str) -> serde_json::Value {
    succeeded(command, serde_json::json!({ "message": text.trim_end() }))
}

/// Write the error document, if this invocation is in JSON mode and stdout
/// can still carry one; returns whether it was written.
fn emit(code: ErrorCode, message: &str, details: Option<serde_json::Value>) -> bool {
    let Some(mode) = MODE.get().filter(|m| m.json) else {
        return false;
    };
    write_document(&document(mode.command.as_deref(), code, message, details))
}

/// Answer a help or version request with `text` on stdout: under `--json`
/// the success document carrying it, `command` being `surface`, otherwise
/// the text itself. Both belong on stdout, so neither falls back to the
/// other, and a write that fails (the reader left) is ignored, as clap's own
/// help ignores it.
pub fn emit_help(surface: Option<&str>, text: &str) {
    use std::io::Write;
    let json = MODE.get().is_some_and(|m| m.json);
    let mut out = std::io::stdout().lock();
    let _ = if json {
        writeln!(out, "{}", help_document(surface, text))
    } else {
        writeln!(out, "{}", text.trim_end())
    }
    .and_then(|()| out.flush());
}

fn write_document(doc: &serde_json::Value) -> bool {
    // Not `println!`: with stdout closed it panics, turning the failure's
    // exit status into 101. A document nobody can read falls back to prose
    // on stderr, even one cut short when the reader left mid-write.
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    writeln!(out, "{doc}").and_then(|()| out.flush()).is_ok()
}

/// Report a failure that reached `main()` and return its exit status: under
/// `--json` the error document on stdout, otherwise the prose line on
/// stderr. A cancellation prints no prose.
pub fn report(err: &anyhow::Error) -> i32 {
    let code = classify(err);
    let message = format!("{err:#}");
    if !emit(code, &message, details(err)) {
        match code {
            ErrorCode::Cancelled => {}
            ErrorCode::Usage => eprintln!("{message}"),
            _ => eprintln!("pult: error: {message}"),
        }
    }
    code.exit_code()
}

fn is_ask(err: &clap::Error) -> bool {
    matches!(
        err.kind(),
        clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
    )
}

/// Parse `argv` (argv[0] included) with `cli`. In JSON mode, help and version
/// are a success document whose `data.message` is clap's text, exit 0, and
/// any other parse failure is a `usage` document whose `details.usage` keeps
/// clap's own text, in place of printing that text to stderr, exit 2.
/// Outside JSON mode, clap prints and exits as it always does.
pub fn get_matches<I, T>(mut cli: clap::Command, argv: I) -> clap::ArgMatches
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    let argv: Vec<std::ffi::OsString> = argv.into_iter().map(Into::into).collect();
    let err = match cli.try_get_matches_from_mut(argv.iter().cloned()) {
        Ok(matches) => return matches,
        Err(err) => err,
    };
    let Some(mode) = MODE.get().filter(|m| m.json) else {
        err.exit();
    };
    // `--json` is pult's own mode switch, and a surface without a `--json`
    // of its own refuses it as an unknown argument before clap reaches a help
    // or version flag later on the line. So that the answer does not depend
    // on the order, a help or version request the line holds once `--json`
    // is set aside is answered; any other failure is reported as it was.
    let err = if is_ask(&err) {
        err
    } else {
        let mut separated = false;
        let without_json = argv.iter().filter(|a| {
            separated |= a.to_str() == Some("--");
            separated || a.to_str() != Some("--json")
        });
        match cli.try_get_matches_from_mut(without_json.cloned()) {
            Err(ask) if is_ask(&ask) => ask,
            _ => err,
        }
    };
    let text = err.render().to_string();
    if is_ask(&err) {
        emit_help(mode.help_surface.as_deref(), &text);
        std::process::exit(0);
    }
    if !emit(
        ErrorCode::Usage,
        &clap_message(&text),
        Some(serde_json::json!({ "usage": text.trim_end() })),
    ) {
        let _ = err.print();
    }
    std::process::exit(ErrorCode::Usage.exit_code());
}

/// The failure itself from clap's rendered error: the paragraph before the
/// usage and help hints, without its `error: ` prefix. It can span lines
/// (`the following required arguments were not provided:\n  <RUN_ID>`).
fn clap_message(text: &str) -> String {
    let head = text.split("\n\n").next().unwrap_or_default().trim_end();
    head.strip_prefix("error: ").unwrap_or(head).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn only_a_bare_json_token_before_the_separator_asks_for_json() {
        assert!(wants_json(&args("--list --json")));
        assert!(wants_json(&args("doctor --json")));
        assert!(!wants_json(&args("--list")));
        assert!(!wants_json(&args("deploy -- --json")));
        assert!(!wants_json(&args("--run-id=--json --list")));
        assert!(!wants_json(&args("--jsonx --list")));
    }

    #[test]
    fn command_name_names_the_machine_surface() {
        assert_eq!(
            command_name(&args("--list --json")).as_deref(),
            Some("list")
        );
        assert_eq!(
            command_name(&args("--trust --list --json")).as_deref(),
            Some("list")
        );
        assert_eq!(
            command_name(&args("doctor --json")).as_deref(),
            Some("doctor")
        );
        assert_eq!(
            command_name(&args("--trust doctor --json")).as_deref(),
            Some("doctor")
        );
        assert_eq!(
            command_name(&args("--run-id abc doctor --json")).as_deref(),
            Some("doctor")
        );
        assert_eq!(
            command_name(&args("runs list --json")).as_deref(),
            Some("runs list")
        );
        assert_eq!(
            command_name(&args("runs tail X --json")).as_deref(),
            Some("runs tail")
        );
        assert_eq!(command_name(&args("runs lst --json")), None);
        assert_eq!(command_name(&args("deploy --list --json")), None);
        assert_eq!(command_name(&args("--json")), None);
    }

    #[test]
    fn a_help_or_version_flag_answers_for_the_surface_before_it() {
        let surface = |s: &str| help_surface(&args(s));
        assert_eq!(surface("doctor --help --json").as_deref(), Some("doctor"));
        assert_eq!(surface("--run-id abc doctor -h").as_deref(), Some("doctor"));
        assert_eq!(surface("runs tail --help").as_deref(), Some("runs tail"));
        assert_eq!(surface("--help doctor --json"), None);
        assert_eq!(surface("-hV doctor"), None);
        assert_eq!(surface("--list --help --json"), None);
        assert_eq!(surface("--version --json"), None);
        assert_eq!(surface("hello --help --json"), None);
    }

    #[test]
    fn only_a_top_level_flag_is_asked_without_a_manifest_and_the_first_wins() {
        let ask = |s: &str| top_level_ask(&args(s));
        assert_eq!(ask("--help --json"), Some(Ask::Help));
        assert_eq!(ask("--json -V"), Some(Ask::Version));
        assert_eq!(ask("--help --version"), Some(Ask::Help));
        assert_eq!(ask("--version --help"), Some(Ask::Version));
        assert_eq!(ask("-qhV"), Some(Ask::Help));
        assert_eq!(ask("-Vh"), Some(Ask::Version));
        assert_eq!(ask("--list --help"), Some(Ask::Help));
        assert_eq!(ask("--run-id abc --help"), Some(Ask::Help));
        // clap refuses these as a missing `--run-id` value, so nothing answers.
        assert_eq!(ask("--run-id -x --help"), None);
        assert_eq!(ask("--run-id --help"), None);
        assert_eq!(ask("doctor --help"), None);
        assert_eq!(ask("hello --json -- -h"), None);
        assert_eq!(ask("--json -- --version"), None);
        assert_eq!(ask("--run-id -h"), None);
        assert_eq!(ask("--jsonh --helpx"), None);
    }

    /// The prose line and the document's `message` must not change when a
    /// code is attached. Reddens if `Coded`'s Display or `source` stop
    /// delegating to the wrapped error.
    #[test]
    fn a_coded_error_prints_exactly_as_the_error_it_wraps() {
        use anyhow::Context;
        let plain: anyhow::Result<()> = Err(anyhow::anyhow!("root cause"))
            .context("middle")
            .context("top");
        let plain = format!("{:#}", plain.unwrap_err());
        let wrapped = coded(
            ErrorCode::InvalidManifest,
            Err::<(), _>(anyhow::anyhow!("root cause"))
                .context("middle")
                .unwrap_err(),
        )
        .context("top");
        assert_eq!(format!("{wrapped:#}"), plain);
        assert_eq!(plain, "top: middle: root cause");
    }

    /// Innermost wins: a fetch failure stays `fetch_failed` through a
    /// boundary that marks everything `invalid_manifest`, and through
    /// uncoded context layered between them. Reddens if `coded` lets the
    /// outer code override, or `classify` stops looking past the top.
    #[test]
    fn the_innermost_code_wins_through_context_layers() {
        use anyhow::Context;
        let fetch = coded(ErrorCode::FetchFailed, anyhow::anyhow!("git clone failed"));
        let through_context = Err::<(), _>(fetch)
            .context("resolving include")
            .unwrap_err();
        let at_boundary = coded(ErrorCode::InvalidManifest, through_context);
        assert_eq!(classify(&at_boundary), ErrorCode::FetchFailed);

        let uncoded = coded(ErrorCode::InvalidManifest, anyhow::anyhow!("bad include"));
        assert_eq!(classify(&uncoded), ErrorCode::InvalidManifest);
        assert_eq!(classify(&anyhow::anyhow!("anything")), ErrorCode::Generic);
    }

    #[test]
    fn a_prompt_cancellation_classifies_as_cancelled_even_when_coded() {
        let err = anyhow::Error::new(prompt::Cancelled);
        assert_eq!(classify(&err), ErrorCode::Cancelled);
        let err = coded(ErrorCode::TrustChanged, prompt::Dismissed);
        assert_eq!(classify(&err), ErrorCode::Cancelled);
        assert_eq!(ErrorCode::Cancelled.exit_code(), 130);
    }

    #[test]
    fn a_clap_message_keeps_every_line_of_the_failure_but_not_the_hints() {
        let text = "error: the following required arguments were not provided:\n  <RUN_ID>\n\n\
                    Usage: pult runs tail <RUN_ID>\n\nFor more information, try '--help'.\n";
        assert_eq!(
            clap_message(text),
            "the following required arguments were not provided:\n  <RUN_ID>"
        );
    }

    /// Reddens if `coded` wraps an error that already carries a code: the
    /// prompt sentinel would no longer be the error's own type, and the
    /// guided flow's Esc-steps-back check (`downcast_ref::<Dismissed>`) breaks.
    #[test]
    fn coding_a_prompt_sentinel_leaves_it_downcastable() {
        let err = coded(ErrorCode::InvalidManifest, prompt::Dismissed);
        assert!(err.downcast_ref::<prompt::Dismissed>().is_some());
    }

    #[test]
    fn the_document_carries_the_shared_envelope_header_in_order() {
        let doc = document(
            Some("list"),
            ErrorCode::NoManifest,
            "no pult.yaml found",
            None,
        );
        let keys: Vec<&str> = doc
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            ["lonic_json", "tool", "schema", "command", "ok", "error"]
        );
        assert_eq!(doc["lonic_json"], 1);
        assert_eq!(doc["tool"], "pult");
        assert_eq!(doc["schema"], 2);
        assert_eq!(doc["ok"], false);
        assert_eq!(doc["error"]["code"], "no_manifest");
        assert!(doc["error"].get("details").is_none());
        let doc = document(
            None,
            ErrorCode::Usage,
            "x",
            Some(serde_json::json!({"usage": "u"})),
        );
        assert_eq!(doc["command"], serde_json::Value::Null);
        assert_eq!(doc["error"]["details"]["usage"], "u");
    }

    #[test]
    fn a_success_document_carries_the_same_header_and_its_data() {
        let doc = success("runs list", serde_json::json!({"runs": []}));
        let keys: Vec<&str> = doc
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            ["lonic_json", "tool", "schema", "command", "ok", "data"]
        );
        assert_eq!(doc["schema"], 2);
        assert_eq!(doc["command"], "runs list");
        assert_eq!(doc["ok"], true);
        assert_eq!(doc["data"]["runs"], serde_json::json!([]));
    }
}
