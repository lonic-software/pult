//! `pult runs` — the CLI reader surface over the run journal (journal.rs):
//! `list` (this repo's run history), `tail` (one run's event stream, with
//! `--follow` for a live run), `prune` (apply retention now). Thin clients
//! should prefer this over touching the journal layout directly, though the
//! layout itself is contract too (see the run-journal spec).
//!
//! Reader rules honored here: never write inside a run dir, skip anything
//! unreadable, and derive "crashed" (meta says running, writer is dead)
//! rather than expecting the journal to say it.

use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::journal::{self, Meta, Status};
use crate::resolver::Resolved;

/// Poll cadence for `tail --follow` — the fs-notification-free fallback is
/// the only mode the CLI needs; 150ms is well under human latency.
const FOLLOW_POLL: Duration = Duration::from_millis(150);

pub fn run_cli(resolved: &Resolved, matches: &clap::ArgMatches) -> Result<i32> {
    match matches.subcommand() {
        Some(("list", sub)) => list(resolved, sub.get_flag("json")),
        Some(("tail", sub)) => {
            let run_id = sub.get_one::<String>("run_id").expect("clap: required");
            let follow = sub.get_flag("follow");
            if sub.get_flag("json") {
                // A live stream cannot be one document.
                if follow {
                    return Err(crate::machine::usage(
                        "--json reads a run as one document and can't --follow it — \
                         use --jsonl --follow for the live event stream",
                    ));
                }
                return tail_document(resolved, run_id);
            }
            tail(resolved, run_id, follow, sub.get_flag("jsonl"))
        }
        Some(("prune", _)) => prune(resolved),
        _ => Err(crate::machine::usage(
            "usage: pult runs <list [--json] | tail <RUN_ID> [--follow] [--json | --jsonl] | prune>",
        )),
    }
}

/// This repo's `runs/` root inside the journal state dir. The dir may not
/// exist yet (no journaled run so far) — callers treat that as an empty
/// history, not an error.
fn runs_root(resolved: &Resolved) -> Result<PathBuf> {
    let state = journal::state_dir()
        .context("no journal state dir (PULT_STATE_DIR unset and no home directory)")?;
    let canonical = std::fs::canonicalize(&resolved.dir).unwrap_or_else(|_| resolved.dir.clone());
    Ok(state
        .join("repos")
        .join(journal::repo_key(&canonical))
        .join("runs"))
}

/// A meta plus its reader-derived liveness — `crashed` = the journal says
/// running but the writing process is gone.
fn load_all(resolved: &Resolved) -> Result<Vec<(Meta, bool)>> {
    let root = runs_root(resolved)?;
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Ok(Vec::new());
    };
    let mut runs: Vec<(Meta, bool)> = entries
        .flatten()
        .filter_map(|e| journal::read_meta(&e.path()))
        .map(|meta| {
            let crashed = meta.status == Status::Running && !journal::writer_alive(&meta);
            (meta, crashed)
        })
        .collect();
    // RFC3339 UTC sorts lexically — newest first.
    runs.sort_by(|a, b| b.0.started_at.cmp(&a.0.started_at));
    Ok(runs)
}

fn display_status(meta: &Meta, crashed: bool) -> &'static str {
    if crashed {
        return "crashed";
    }
    match meta.status {
        Status::Running => "running",
        Status::Stopped => "stopped",
        Status::Exited => match meta.exit_code {
            Some(0) => "ok",
            _ => "failed",
        },
    }
}

fn list(resolved: &Resolved, json: bool) -> Result<i32> {
    let runs = load_all(resolved)?;
    if json {
        // Each entry is the meta document verbatim plus the derived
        // `crashed` flag — additive, so the meta schema stays the contract.
        let docs: Vec<serde_json::Value> = runs
            .iter()
            .map(|(meta, crashed)| {
                let mut doc = serde_json::to_value(meta).expect("meta serializes");
                doc["crashed"] = serde_json::json!(crashed);
                doc
            })
            .collect();
        let doc = crate::machine::success("runs list", serde_json::json!({ "runs": docs }));
        println!("{}", serde_json::to_string_pretty(&doc)?);
        return Ok(0);
    }
    if runs.is_empty() {
        println!("no journaled runs for {}", resolved.dir.display());
        return Ok(0);
    }
    let id_width = runs
        .iter()
        .map(|(m, _)| m.command_id.len())
        .max()
        .unwrap_or(0);
    for (meta, crashed) in &runs {
        let exit = match meta.exit_code {
            Some(code) => format!("exit {code}"),
            None => String::new(),
        };
        println!(
            "{}  {:id_width$}  {:7}  {}  {}",
            meta.started_at,
            meta.command_id,
            display_status(meta, *crashed),
            meta.run_id,
            exit,
        );
    }
    Ok(0)
}

/// One run's directory inside `runs_root`, given a caller-supplied run id.
/// `Path::join` is unsafe for an unvalidated id — an absolute operand
/// discards `runs_root` outright, and `../` escapes it — so this is the one
/// seam every reader must route a `run_id` through rather than joining
/// directly. Rejects with the same message `tail` uses for a merely-unknown
/// id: a distinguishable error here would tell a traversal attempt it found
/// a real path instead of a missing run.
fn run_dir_for(runs_root: &std::path::Path, run_id: &str) -> Result<PathBuf> {
    if !journal::valid_run_id(run_id) {
        bail!("no journaled run `{run_id}` for this repo (see `pult runs list`)");
    }
    Ok(runs_root.join(run_id))
}

/// One run's `events.jsonl`, which must exist.
fn events_path(resolved: &Resolved, run_id: &str) -> Result<(PathBuf, PathBuf)> {
    let run_dir = run_dir_for(&runs_root(resolved)?, run_id)?;
    let events_path = run_dir.join("events.jsonl");
    if !events_path.exists() {
        bail!("no journaled run `{run_id}` for this repo (see `pult runs list`)");
    }
    Ok((run_dir, events_path))
}

/// `runs tail <ID> --json`: the events journaled so far, as one document.
fn tail_document(resolved: &Resolved, run_id: &str) -> Result<i32> {
    let (_, events_path) = events_path(resolved, run_id)?;
    let raw = std::fs::read(&events_path)
        .with_context(|| format!("failed to read {}", events_path.display()))?;
    let (events, skipped) = collect_events(&raw);
    let doc = crate::machine::success(
        "runs tail",
        serde_json::json!({ "run_id": run_id, "events": events, "skipped": skipped }),
    );
    println!("{}", serde_json::to_string_pretty(&doc)?);
    Ok(0)
}

/// Split journal bytes into the events they hold and a count of the complete
/// lines that are not one: every newline-terminated line that is a JSON
/// object is an event, in order; any other complete line is counted in
/// `skipped`, so a damaged journal says so instead of losing lines silently.
/// A final line with no newline is not yet written (the journal's own rule),
/// so it is neither.
fn collect_events(raw: &[u8]) -> (Vec<serde_json::Value>, usize) {
    let mut events = Vec::new();
    let mut skipped = 0;
    let Some(last) = raw.iter().rposition(|b| *b == b'\n') else {
        return (events, skipped);
    };
    for line in raw[..last].split(|b| *b == b'\n') {
        match serde_json::from_slice::<serde_json::Value>(line) {
            Ok(v @ serde_json::Value::Object(_)) => events.push(v),
            _ => skipped += 1,
        }
    }
    (events, skipped)
}

fn tail(resolved: &Resolved, run_id: &str, follow: bool, jsonl: bool) -> Result<i32> {
    let (run_dir, events_path) = events_path(resolved, run_id)?;

    let mut file = std::fs::File::open(&events_path)
        .with_context(|| format!("failed to open {}", events_path.display()))?;
    let mut offset: u64 = 0;
    // Once a line is out, stdout is a JSONL stream: a later failure must not
    // append an error document to it.
    let mut stdout = crate::machine::StreamStdout::new();

    follow_events(&mut file, &mut offset, follow, jsonl, &mut stdout, || {
        journal::read_meta(&run_dir)
    })
    .map(|()| 0)
}

/// Drive one run's events file to completion: drain what's there, and (in
/// `--follow` mode) poll `observe` — normally `journal::read_meta` — until
/// it reports the run is over. `observe` is a parameter rather than a
/// direct call so a test can make the writer's exit-then-meta-flip race
/// land deterministically instead of depending on real thread timing.
///
/// The terminal case always drains once more before returning: `finish`
/// appends the `exit` event *then* flips meta, so anything written in the
/// gap between a drain pass and the `observe` call that follows it —
/// always including a same-final-tick `exit` event — would otherwise never
/// be read. Draining again after observing "over" (whether a terminal
/// status or a dead writer) closes that gap.
fn follow_events(
    file: &mut std::fs::File,
    offset: &mut u64,
    follow: bool,
    jsonl: bool,
    out: &mut impl Write,
    mut observe: impl FnMut() -> Option<Meta>,
) -> Result<()> {
    loop {
        drain_events(file, offset, jsonl, out)?;

        if !follow {
            return Ok(());
        }
        // Follow until the run is over: a terminal status in meta, or a
        // dead writer (crash) — either way nothing more will be appended
        // once that's observed.
        match observe() {
            Some(meta) if meta.status == Status::Running && journal::writer_alive(&meta) => {
                std::thread::sleep(FOLLOW_POLL);
            }
            _ => {
                drain_events(file, offset, jsonl, out)?;
                return Ok(());
            }
        }
    }
}

/// One pass over whatever has been appended to `file` since `*offset`,
/// rendering each complete record to `out`. A torn final line (no trailing
/// newline yet) is "not yet written": rewind to its start and pick it up
/// whole on a later pass.
fn drain_events(
    file: &mut std::fs::File,
    offset: &mut u64,
    jsonl: bool,
    out: &mut impl Write,
) -> Result<()> {
    file.seek(SeekFrom::Start(*offset))?;
    let mut reader = BufReader::new(&*file);
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            break;
        }
        if !line.ends_with('\n') {
            break;
        }
        *offset += n as u64;
        let trimmed = line.trim_end();
        if jsonl {
            writeln!(out, "{trimmed}")?;
        } else if let Some(rendered) = render_event(trimmed) {
            writeln!(out, "{rendered}")?;
        }
        out.flush().ok();
    }
    Ok(())
}

/// Text-mode rendering of one journal event line — a compact human echo of
/// the run, not a replay (streams are labeled, protocol events prefixed).
/// `None` = unparseable or unknown kind, skipped per the reader rules.
fn render_event(line: &str) -> Option<String> {
    let doc: serde_json::Value = serde_json::from_str(line).ok()?;
    match doc["kind"].as_str()? {
        "line" => {
            let text = doc["text"].as_str()?;
            Some(match doc["stream"].as_str() {
                Some("stderr") => format!("! {text}"),
                _ => format!("  {text}"),
            })
        }
        "step" => Some(format!(
            "· step {}/{} {}",
            doc["k"],
            doc["n"],
            doc["name"].as_str().unwrap_or(""),
        )),
        "progress" => match doc["pct"].as_u64() {
            Some(pct) => Some(format!("· progress {pct}%")),
            None => Some("· progress ?".to_string()),
        },
        "status" => Some(format!("· {}", doc["text"].as_str().unwrap_or(""))),
        "exit" => {
            let stopped = doc["stopped"].as_bool().unwrap_or(false);
            Some(match (stopped, doc["code"].as_i64()) {
                (true, _) => "■ stopped".to_string(),
                (false, Some(0)) => "✓ exit 0".to_string(),
                (false, Some(code)) => format!("✗ exit {code}"),
                (false, None) => "✗ exited abnormally".to_string(),
            })
        }
        _ => None,
    }
}

fn prune(resolved: &Resolved) -> Result<i32> {
    let root = runs_root(resolved)?;
    let removed = journal::prune(&root, journal::keep_limit(), None);
    println!("pruned {removed} run(s)");
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// H1: a path-escaping run id must be rejected before it is ever joined
    /// onto `runs_root` — `../x` and an absolute path would otherwise let
    /// `pult runs tail` read a file outside the run journal entirely. If
    /// the `valid_run_id` check inside `run_dir_for` is reverted, this test
    /// goes red: the traversal/absolute/slash/empty/overlong ids below would
    /// each start returning `Ok` (a joined, escaping path) instead of `Err`.
    #[test]
    fn run_dir_for_rejects_path_escaping_ids() {
        let root = tempfile::tempdir().unwrap();
        for bad in ["../x", "/tmp/evil", "a/b", "", &"x".repeat(65)] {
            let err = run_dir_for(root.path(), bad).unwrap_err();
            assert!(
                err.to_string().contains("no journaled run"),
                "id {bad:?}: {err}"
            );
        }
    }

    fn terminal_meta() -> Meta {
        Meta {
            schema: 1,
            run_id: "test-run".to_string(),
            repo_dir: PathBuf::from("/tmp/repo"),
            manifest: PathBuf::from("/tmp/repo/pult.yaml"),
            command_id: "deploy".to_string(),
            command_title: "Deploy".to_string(),
            params: indexmap::IndexMap::new(),
            origin: "cli".to_string(),
            interactive: false,
            pult_version: "0.0.0".to_string(),
            pid: 1,
            pgid: None,
            started_at: "2026-01-01T00:00:00.000Z".to_string(),
            status: Status::Exited,
            exit_code: Some(0),
            ended_at: Some("2026-01-01T00:00:01.000Z".to_string()),
        }
    }

    /// M1: `finish` appends the `exit` event *then* flips meta to a
    /// terminal status — so a reader that drains, then reads meta, then
    /// stops on seeing "over" can miss anything written in that gap,
    /// always including the `exit` event itself when the writer's `finish`
    /// races the reader's check. `observe` (normally `journal::read_meta`)
    /// is a parameter here specifically so that race can be reproduced
    /// deterministically instead of depending on real thread timing: the
    /// closure below appends the straggler line at the exact moment the
    /// loop asks "is it over yet", landing it in the gap on purpose.
    ///
    /// Goes red if `follow_events`'s second `drain_events` call (after the
    /// terminal match arm) is reverted: the appended exit line would then
    /// never be read, and this assertion would fail.
    #[test]
    fn follow_events_drains_once_more_after_observing_terminal_state() {
        let dir = tempfile::tempdir().unwrap();
        let events_path = dir.path().join("events.jsonl");
        let line1 = r#"{"ts":1,"kind":"line","stream":"stdout","text":"hello"}"#;
        let exit_line = r#"{"ts":2,"kind":"exit","code":0,"stopped":false}"#;
        std::fs::write(&events_path, format!("{line1}\n")).unwrap();

        let mut file = std::fs::File::open(&events_path).unwrap();
        let mut offset = 0u64;
        let mut out: Vec<u8> = Vec::new();

        let meta = terminal_meta();
        let mut appended = false;
        let observe = || {
            if !appended {
                appended = true;
                let mut f = std::fs::OpenOptions::new()
                    .append(true)
                    .open(&events_path)
                    .unwrap();
                writeln!(f, "{exit_line}").unwrap();
            }
            Some(meta.clone())
        };

        follow_events(&mut file, &mut offset, true, true, &mut out, observe).unwrap();

        let rendered = String::from_utf8(out).unwrap();
        assert!(
            rendered.contains(r#""kind":"exit""#),
            "final drain must pick up the exit event appended during \
             the terminal observation: {rendered}"
        );
    }

    #[test]
    fn run_dir_for_accepts_well_formed_ids() {
        let root = tempfile::tempdir().unwrap();
        for ok in [
            "550e8400-e29b-41d4-a716-446655440000",
            "desktop-supplied-id",
        ] {
            assert_eq!(run_dir_for(root.path(), ok).unwrap(), root.path().join(ok));
        }
    }

    /// Every complete line is either an event or counted; the torn tail is
    /// neither. Reddens if `skipped` stops counting a damaged line, if a
    /// non-object line becomes an event, or if the torn tail is read.
    #[test]
    fn collect_events_counts_what_it_cannot_read_and_waits_for_a_torn_tail() {
        let raw = b"{\"kind\":\"line\"}\nnot json\n\n[1]\n{\"kind\":\"exit\"}\n{\"kind\":\"li";
        let (events, skipped) = collect_events(raw);
        assert_eq!(
            events,
            [
                serde_json::json!({"kind": "line"}),
                serde_json::json!({"kind": "exit"})
            ]
        );
        assert_eq!(skipped, 3);
        assert_eq!(collect_events(b"{\"kind\":\"li"), (Vec::new(), 0));
        assert_eq!(collect_events(b""), (Vec::new(), 0));
    }

    #[test]
    fn render_event_covers_every_kind_and_skips_unknown() {
        assert_eq!(
            render_event(r#"{"ts":1,"kind":"line","stream":"stdout","text":"hi"}"#).unwrap(),
            "  hi"
        );
        assert_eq!(
            render_event(r#"{"ts":1,"kind":"line","stream":"stderr","text":"bad"}"#).unwrap(),
            "! bad"
        );
        assert_eq!(
            render_event(r#"{"ts":1,"kind":"step","k":2,"n":3,"name":"push"}"#).unwrap(),
            "· step 2/3 push"
        );
        assert_eq!(
            render_event(r#"{"ts":1,"kind":"progress","pct":50,"text":null}"#).unwrap(),
            "· progress 50%"
        );
        assert_eq!(
            render_event(r#"{"ts":1,"kind":"progress","pct":null,"text":null}"#).unwrap(),
            "· progress ?"
        );
        assert_eq!(
            render_event(r#"{"ts":1,"kind":"exit","code":0,"stopped":false}"#).unwrap(),
            "✓ exit 0"
        );
        assert_eq!(
            render_event(r#"{"ts":1,"kind":"exit","code":null,"stopped":true}"#).unwrap(),
            "■ stopped"
        );
        assert!(render_event(r#"{"ts":1,"kind":"hologram"}"#).is_none());
        assert!(render_event("not json").is_none());
    }
}
