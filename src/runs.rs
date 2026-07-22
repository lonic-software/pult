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
        Some(("tail", sub)) => tail(
            resolved,
            sub.get_one::<String>("run_id").expect("clap: required"),
            sub.get_flag("follow"),
            sub.get_flag("json"),
        ),
        Some(("prune", _)) => prune(resolved),
        _ => {
            eprintln!(
                "usage: pult runs <list [--json] | tail <RUN_ID> [--follow] [--json] | prune>"
            );
            Ok(2)
        }
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
        println!("{}", serde_json::to_string_pretty(&docs)?);
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

fn tail(resolved: &Resolved, run_id: &str, follow: bool, json: bool) -> Result<i32> {
    let run_dir = run_dir_for(&runs_root(resolved)?, run_id)?;
    let events_path = run_dir.join("events.jsonl");
    if !events_path.exists() {
        bail!("no journaled run `{run_id}` for this repo (see `pult runs list`)");
    }

    let mut file = std::fs::File::open(&events_path)
        .with_context(|| format!("failed to open {}", events_path.display()))?;
    let mut offset: u64 = 0;
    let mut stdout = std::io::stdout();

    loop {
        // Read whatever has been appended since the last pass. A torn final
        // line (no trailing newline yet) is "not yet written": rewind to
        // its start and pick it up whole on a later pass.
        file.seek(SeekFrom::Start(offset))?;
        let mut reader = BufReader::new(&file);
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
            offset += n as u64;
            let trimmed = line.trim_end();
            if json {
                writeln!(stdout, "{trimmed}")?;
            } else if let Some(rendered) = render_event(trimmed) {
                writeln!(stdout, "{rendered}")?;
            }
            stdout.flush().ok();
        }

        if !follow {
            return Ok(0);
        }
        // Follow until the run is over: a terminal status in meta, or a
        // dead writer (crash) — either way nothing more will be appended.
        match journal::read_meta(&run_dir) {
            Some(meta) if meta.status == Status::Running && journal::writer_alive(&meta) => {
                std::thread::sleep(FOLLOW_POLL);
            }
            _ => {
                // One last drain pass already happened above; done.
                return Ok(0);
            }
        }
    }
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
