//! The machine-readable failure surface: under `--json`, a failure pult
//! controls itself ends in exactly one JSON error document on stdout, in the
//! envelope shared by every Lonic tool — while the prose line on stderr stays
//! exactly what it was, so a consumer matching that prose keeps working.
//!
//! ```json
//! {"lonic_json": 1, "tool": "pult", "schema": 1, "command": "list", "ok": false,
//!  "error": {"code": "no_manifest", "message": "no pult.yaml found …"}}
//! ```
//!
//! A consumer classifies a failure by `error.code`, never by the exit status:
//! pult hands a command's own exit code straight through, so every number
//! except 0, 1, 2, 130 and 128+n may belong to whatever pult ran. That is
//! why every code here exits 1 except `usage` (2) and `cancelled` (130).
//!
//! Success documents are unchanged (bare, schema 1) — wrapping them in the
//! envelope is an incompatible change and waits for a schema bump.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::prompt;

/// The version of the envelope every Lonic tool shares.
const LONIC_JSON: u32 = 1;
const TOOL: &str = "pult";
/// The version of pult's own payloads — the `schema` its JSON documents carry.
const SCHEMA: u32 = 1;

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
pub fn coded(code: ErrorCode, err: impl Into<anyhow::Error>) -> anyhow::Error {
    let inner = err.into();
    let code = existing_code(&inner).unwrap_or(code);
    anyhow::Error::new(Coded { code, inner })
}

/// The code an error already carries: a prompt's own cancellation sentinel,
/// or the outermost [`Coded`] in its chain (which, by [`coded`], already
/// holds the innermost code attached).
fn existing_code(err: &anyhow::Error) -> Option<ErrorCode> {
    err.chain().find_map(|e| {
        if e.is::<prompt::Cancelled>() || e.is::<prompt::Dismissed>() {
            Some(ErrorCode::Cancelled)
        } else {
            e.downcast_ref::<Coded>().map(|c| c.code)
        }
    })
}

/// How a failure is classified: its attached code, else `generic`.
pub fn classify(err: &anyhow::Error) -> ErrorCode {
    existing_code(err).unwrap_or(ErrorCode::Generic)
}

/// A usage error with pult's own text — printed to stderr as-is (no
/// `pult: error:` prefix, the way pult has always printed its usage lines),
/// exit 2.
pub fn usage(message: impl Into<String>) -> anyhow::Error {
    coded(ErrorCode::Usage, anyhow::anyhow!(message.into()))
}

/// The invocation's machine mode, decided once from the raw arguments
/// before anything else runs: a failure can happen before (or inside) the
/// parse that would otherwise say whether `--json` was asked for.
struct Mode {
    json: bool,
    command: Option<String>,
}

static MODE: OnceLock<Mode> = OnceLock::new();

/// Set once, while a `--json` stream (`runs tail --json`) has written to
/// stdout: an error document after part of a stream would not be the one
/// document stdout carries, so none is written.
static STDOUT_COMMITTED: AtomicBool = AtomicBool::new(false);

/// Record the machine mode for this process from its arguments (argv[0]
/// excluded). Called once, first thing.
pub fn init(args: &[String]) {
    let _ = MODE.set(Mode {
        json: wants_json(args),
        command: command_name(args),
    });
}

/// Mark stdout as carrying part of a `--json` stream.
pub fn commit_stdout() {
    STDOUT_COMMITTED.store(true, Ordering::Relaxed);
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
/// anything else. The first argument that is not an option names the
/// subcommand; `--run-id` is the one global option that takes a value.
fn command_name(args: &[String]) -> Option<String> {
    let args: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .take_while(|a| *a != "--")
        .collect();
    let mut positionals = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i] {
            "--run-id" => i += 1,
            a if a.starts_with('-') => {}
            a => positionals.push(a),
        }
        i += 1;
    }
    match positionals.as_slice() {
        ["doctor", ..] => Some("doctor".to_string()),
        ["runs", sub @ ("list" | "tail"), ..] => Some(format!("runs {sub}")),
        [] if args.contains(&"--list") => Some("list".to_string()),
        _ => None,
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

/// Print the error document, when this invocation is in JSON mode and has
/// not already committed stdout to a stream.
fn emit(code: ErrorCode, message: &str, details: Option<serde_json::Value>) {
    let Some(mode) = MODE.get() else { return };
    if !mode.json || STDOUT_COMMITTED.load(Ordering::Relaxed) {
        return;
    }
    println!(
        "{}",
        document(mode.command.as_deref(), code, message, details)
    );
}

/// Report a failure that reached `main()` and return its exit status: the
/// prose line on stderr, then (under `--json`) the error document on stdout.
/// A cancellation prints no prose, as before.
pub fn report(err: &anyhow::Error) -> i32 {
    let code = classify(err);
    let message = format!("{err:#}");
    match code {
        ErrorCode::Cancelled => {}
        ErrorCode::Usage => eprintln!("{message}"),
        _ => eprintln!("pult: error: {message}"),
    }
    emit(code, &message, None);
    code.exit_code()
}

/// Parse `argv` (argv[0] included) with `cli`. A parse failure in JSON mode
/// prints clap's own text to stderr as usual, then a `usage` document whose
/// `details.usage` keeps that text, and exits 2. Help and version are not
/// failures and are left to clap, as is everything outside JSON mode.
pub fn get_matches<I, T>(cli: clap::Command, argv: I) -> clap::ArgMatches
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    let err = match cli.try_get_matches_from(argv) {
        Ok(matches) => return matches,
        Err(err) => err,
    };
    let json = MODE.get().is_some_and(|m| m.json);
    if !json
        || matches!(
            err.kind(),
            clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
        )
    {
        err.exit();
    }
    let text = err.render().to_string();
    let _ = err.print();
    let first = text.lines().next().unwrap_or_default();
    emit(
        ErrorCode::Usage,
        first.strip_prefix("error: ").unwrap_or(first),
        Some(serde_json::json!({ "usage": text.trim_end() })),
    );
    std::process::exit(ErrorCode::Usage.exit_code());
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

    /// The prose line must not change when a code is attached — consumers
    /// still match it. Reddens if `Coded`'s Display or `source` stop
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
        assert_eq!(doc["schema"], 1);
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
}
