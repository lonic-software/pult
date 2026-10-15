//! `requires:` — the tools a command needs, checked before it runs.
//!
//! A check is manifest code, so everything here runs only past the trust
//! gate: `exec::execute` calls [`gate`] after `trust::ensure_trusted`, and
//! `pult doctor` calls [`probe`] after its own.

use std::path::Path;
use std::process::Stdio;

use anyhow::{Context, Result, anyhow};
use serde_json::json;

use crate::machine::{self, ErrorCode};
use crate::resolver::{Requirement, Resolved, ResolvedCommand};
use crate::shell;

/// Run a check script — a requirement's or a command's `check:` — in
/// `run_dir` with no stdin and its output discarded; its exit code (0 =
/// ready, and a signal counts as 1).
pub fn run_check(script: &str, run_dir: &Path) -> Result<i32> {
    let status = shell::sh_script(script)?
        .command
        .current_dir(run_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    Ok(status.code().unwrap_or(1))
}

/// Run one requirement's check; its exit code (0 = present).
pub fn probe(req: &Requirement, run_dir: &Path) -> Result<i32> {
    run_check(&req.check, run_dir)
        .with_context(|| format!("failed to run the check for requirement `{}`", req.name))
}

/// What `command -v <name>` finds on the inherited `PATH`, or `None`.
pub fn resolved_path(req: &Requirement, run_dir: &Path) -> Result<Option<String>> {
    let script = format!("command -v {}", crate::interp::shell_quote(&req.name));
    let out = shell::sh_script(&script)?
        .command
        .current_dir(run_dir)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .with_context(|| format!("failed to look up `{}` on PATH", req.name))?;
    let found = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Ok((out.status.success() && !found.is_empty()).then_some(found))
}

/// The requirements `cmd` needs, in its effective order.
pub fn of<'a>(resolved: &'a Resolved, cmd: &ResolvedCommand) -> Vec<&'a Requirement> {
    cmd.requires
        .iter()
        .map(|name| {
            resolved
                .requirements
                .get(name)
                .expect("the resolver validated every requirement name")
        })
        .collect()
}

/// The gate before a run: check every requirement in `reqs`, and fail with
/// `missing_requirements` naming each one whose check did not pass.
/// `subject` is the command the requirements were gathered for.
pub fn gate(subject: &str, reqs: &[&Requirement], run_dir: &Path) -> Result<()> {
    let mut missing = Vec::new();
    for req in reqs {
        let code = probe(req, run_dir)?;
        if code != 0 {
            missing.push((*req, code, resolved_path(req, run_dir)?));
        }
    }
    if missing.is_empty() {
        return Ok(());
    }
    let listed: Vec<String> = missing
        .iter()
        .map(|(req, code, _)| format!("{} (`{}` exited {code})", req.name, req.check))
        .collect();
    let details = json!({
        "command": subject,
        "missing": missing
            .iter()
            .map(|(req, _, path)| json!({
                "name": req.name,
                "resolved_path": path,
                "install": null,
            }))
            .collect::<Vec<_>>(),
    });
    Err(machine::coded_with_details(
        ErrorCode::MissingRequirements,
        details,
        anyhow!(
            "`{subject}` needs tools that are missing: {} — install them, then run again",
            listed.join(", ")
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(name: &str, check: &str) -> Requirement {
        Requirement {
            name: name.to_string(),
            check: check.to_string(),
            defined_in: None,
            shadowed: Vec::new(),
        }
    }

    #[test]
    fn a_failed_check_is_missing_requirements_naming_only_the_failures() {
        let dir = tempfile::tempdir().unwrap();
        let (gone, here) = (req("pult-no-such-tool", "exit 4"), req("sh", "true"));
        let err = gate("deploy", &[&here, &gone], dir.path()).unwrap_err();
        assert_eq!(machine::classify(&err), ErrorCode::MissingRequirements);
        assert_eq!(
            ErrorCode::MissingRequirements.as_str(),
            "missing_requirements"
        );
        assert_eq!(
            machine::details(&err),
            Some(json!({
                "command": "deploy",
                "missing": [{ "name": "pult-no-such-tool", "resolved_path": null, "install": null }],
            }))
        );
        assert!(format!("{err:#}").contains("pult-no-such-tool (`exit 4` exited 4)"));

        // Paired: every check passing lets the run through.
        gate("deploy", &[&here], dir.path()).unwrap();
    }

    #[test]
    fn resolved_path_reports_what_command_v_finds() {
        let dir = tempfile::tempdir().unwrap();
        let found = resolved_path(&req("sh", "false"), dir.path()).unwrap();
        assert!(found.is_some_and(|p| p.ends_with("sh")));
        assert_eq!(
            resolved_path(&req("pult-no-such-tool", "false"), dir.path()).unwrap(),
            None
        );
    }
}
