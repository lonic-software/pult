use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use serde_json::json;

use crate::resolver::{Resolved, ResolvedCommand};
use crate::trust;

/// `pult doctor` — run every command's `check:` and report readiness before
/// anything is executed for real. Checks are manifest code, so the trust gate
/// applies exactly as it does for `run:`. Output is suppressed (a check
/// signals by exit code); exit 1 if any declared check failed.
pub fn run(resolved: &Resolved, assume_trusted: bool, json: bool) -> Result<i32> {
    trust::ensure_trusted(
        &resolved.path,
        &resolved.trust_hash,
        &resolved.include_summary,
        assume_trusted,
        None,
    )?;
    report(resolved, json)
}

/// One command's readiness probe. `exit_code: None` means no `check:` was
/// declared — there's nothing to run, which is not a failure.
struct Probe<'a> {
    cmd: &'a ResolvedCommand,
    exit_code: Option<i32>,
}

impl Probe<'_> {
    fn ready(&self) -> Option<bool> {
        self.exit_code.map(|c| c == 0)
    }
}

/// Run every command's declared `check:`, past the trust gate (separated so
/// tests can probe without touching the trust store).
fn probe_all(resolved: &Resolved) -> Result<Vec<Probe<'_>>> {
    resolved
        .commands
        .iter()
        .map(|cmd| {
            let exit_code = match &cmd.check {
                None => None,
                Some(check) => {
                    let status = Command::new("sh")
                        .arg("-c")
                        .arg(check)
                        .current_dir(&resolved.run_dir)
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status()
                        .with_context(|| format!("failed to run check for `{}`", cmd.id))?;
                    Some(status.code().unwrap_or(1))
                }
            };
            Ok(Probe { cmd, exit_code })
        })
        .collect()
}

/// The probe loop: compute every result once, then render as text or JSON.
/// Exit code semantics are identical either way — 1 if any declared check
/// failed, 0 otherwise.
fn report(resolved: &Resolved, want_json: bool) -> Result<i32> {
    let probes = probe_all(resolved)?;
    let failed = probes.iter().filter(|p| p.ready() == Some(false)).count();

    if want_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&to_json(resolved, &probes))?
        );
    } else {
        print_text(resolved, &probes);
    }
    Ok(if failed == 0 { 0 } else { 1 })
}

fn print_text(resolved: &Resolved, probes: &[Probe]) {
    let width = resolved
        .commands
        .iter()
        .map(|c| c.id.len())
        .max()
        .unwrap_or(0);
    println!("{} · {}", resolved.name, resolved.path.display());
    let checked = probes.iter().filter(|p| p.exit_code.is_some()).count();
    let failed = probes.iter().filter(|p| p.ready() == Some(false)).count();
    for p in probes {
        match p.exit_code {
            None => println!("  -  {:width$}  {}", p.cmd.id, p.cmd.title),
            Some(0) => println!("  ✓  {:width$}  {}", p.cmd.id, p.cmd.title),
            Some(code) => println!(
                "  ✗  {:width$}  {}  (check exited {code})",
                p.cmd.id, p.cmd.title
            ),
        }
    }
    if checked == 0 {
        println!("no command declares a `check:` — nothing to probe");
        return;
    }
    println!(
        "{}/{checked} checks passed{}",
        checked - failed,
        if checked < resolved.commands.len() {
            "  (-: no check declared)"
        } else {
            ""
        }
    );
}

/// The `pult doctor --json` document — schema 1, same additive-only contract
/// as `--list --json`.
fn to_json(resolved: &Resolved, probes: &[Probe]) -> serde_json::Value {
    let commands: Vec<_> = probes
        .iter()
        .map(|p| {
            json!({
                "id": p.cmd.id,
                "title": p.cmd.title,
                "check": p.cmd.check,
                "ready": p.ready(),
                "exit_code": p.exit_code,
            })
        })
        .collect();
    json!({
        "schema": 1,
        "name": resolved.name,
        "manifest": resolved.path,
        "commands": commands,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{manifest, resolver};

    // The tempdir must outlive the report: checks spawn with run_dir as cwd.
    fn resolved_from(yaml: &str) -> (tempfile::TempDir, Resolved) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("pult.yaml"), yaml).unwrap();
        let resolved =
            resolver::resolve(manifest::load(&dir.path().join("pult.yaml")).unwrap()).unwrap();
        (dir, resolved)
    }

    const TRIO: &str = "version: 1\ncommands:\n\
         \x20 - { id: ok, title: Ok, run: \"true\", check: \"true\" }\n\
         \x20 - { id: bad, title: Bad, run: \"true\", check: \"false\" }\n\
         \x20 - { id: none, title: None, run: \"true\" }\n";

    #[test]
    fn failing_check_yields_exit_1_passing_yields_0() {
        let (_d, failing) = resolved_from(TRIO);
        assert_eq!(report(&failing, false).unwrap(), 1);

        let (_d, passing) = resolved_from(
            "version: 1\ncommands:\n\
             \x20 - { id: ok, title: Ok, run: \"true\", check: \"true\" }\n",
        );
        assert_eq!(report(&passing, false).unwrap(), 0);
    }

    #[test]
    fn no_checks_declared_is_not_a_failure() {
        let (_d, none) =
            resolved_from("version: 1\ncommands:\n  - { id: a, title: A, run: \"true\" }\n");
        assert_eq!(report(&none, false).unwrap(), 0);
    }

    #[test]
    fn json_mode_reports_ready_and_exit_code_for_pass_fail_and_no_check() {
        let (_d, trio) = resolved_from(TRIO);
        let probes = probe_all(&trio).unwrap();
        let doc = to_json(&trio, &probes);

        assert_eq!(doc["schema"], 1);
        assert_eq!(doc["name"], trio.name);
        assert_eq!(doc["manifest"], trio.path.to_string_lossy().as_ref());

        let cmds = doc["commands"].as_array().unwrap();
        assert_eq!(cmds[0]["id"], "ok");
        assert_eq!(cmds[0]["check"], "true");
        assert_eq!(cmds[0]["ready"], true);
        assert_eq!(cmds[0]["exit_code"], 0);

        assert_eq!(cmds[1]["id"], "bad");
        assert_eq!(cmds[1]["ready"], false);
        assert_eq!(cmds[1]["exit_code"], 1);

        assert_eq!(cmds[2]["id"], "none");
        assert_eq!(cmds[2]["check"], serde_json::Value::Null);
        assert_eq!(cmds[2]["ready"], serde_json::Value::Null);
        assert_eq!(cmds[2]["exit_code"], serde_json::Value::Null);

        // exit code from report() matches: one failed check -> 1
        assert_eq!(report(&trio, true).unwrap(), 1);
    }
}
