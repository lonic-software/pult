use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

/// Run a plain command line via `sh -c` with inherited stdio, so interactive
/// sessions (e.g. `aws ecs execute-command`) get a working PTY — the reason
/// the guided flow avoids a full-screen render loop (spec §9).
///
/// `display` is the line shown in the banner and any error — the caller passes
/// a secret-redacted rendering, so `cmdline` itself is never written anywhere.
pub fn run_sh(cmdline: &str, display: &str, dir: &Path) -> Result<i32> {
    eprintln!("└  running: {display}");
    execute(
        Command::new("sh").arg("-c").arg(cmdline).current_dir(dir),
        display,
    )
}

/// Run a compiled step script via bash (the composed scripts rely on
/// `set -o pipefail`). Same inherited stdio.
pub fn run_bash(script: &str, label: &str, dir: &Path) -> Result<i32> {
    eprintln!("└  running: {label} (compiled steps — `--print` shows the script)");
    execute(
        Command::new("bash").arg("-c").arg(script).current_dir(dir),
        label,
    )
}

fn execute(command: &mut Command, what: &str) -> Result<i32> {
    let status = command
        .status()
        .with_context(|| format!("failed to run `{what}`"))?;
    Ok(exit_code(status))
}

#[cfg(unix)]
fn exit_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .or_else(|| status.signal().map(|s| 128 + s))
        .unwrap_or(1)
}

#[cfg(not(unix))]
fn exit_code(status: std::process::ExitStatus) -> i32 {
    status.code().unwrap_or(1)
}
