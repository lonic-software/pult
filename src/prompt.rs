use std::io::IsTerminal;

use anyhow::{Result, bail};
use inquire::{Confirm, InquireError, Password, PasswordDisplayMode, Select, Text};

/// Sentinel error for Esc during a prompt (inquire's `OperationCanceled`).
/// Distinct from [`Cancelled`] (Ctrl-C) because callers may treat the two
/// differently: the guided flow's inner "which command?" select treats Esc
/// as "step back to the group list", not as an abort. `main()`'s top-level
/// handler still exits 130 for this, same as `Cancelled` — only the flow's
/// inner select gives it special meaning.
#[derive(Debug)]
pub struct Dismissed;

impl std::fmt::Display for Dismissed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "dismissed")
    }
}

impl std::error::Error for Dismissed {}

/// Sentinel error for Ctrl-C during a prompt (inquire's
/// `OperationInterrupted`). Always an abort — every caller, including the
/// guided flow's inner select, must let this propagate rather than
/// reinterpreting it as "go back a menu level". `main()` exits 130 for it.
#[derive(Debug)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "cancelled")
    }
}

impl std::error::Error for Cancelled {}

fn require_tty(what: &str) -> Result<()> {
    if !std::io::stdin().is_terminal() {
        bail!("{what}, and stdin is not a terminal — pass all values on the command line");
    }
    Ok(())
}

fn map_inquire<T>(result: Result<T, InquireError>) -> Result<T> {
    result.map_err(|e| match e {
        InquireError::OperationCanceled => anyhow::Error::new(Dismissed),
        InquireError::OperationInterrupted => anyhow::Error::new(Cancelled),
        other => anyhow::Error::new(other),
    })
}

pub fn select(message: &str, options: Vec<String>) -> Result<String> {
    require_tty(&format!("`{message}` needs an interactive choice"))?;
    map_inquire(Select::new(message, options).prompt())
}

/// Select by index, for menus whose display labels differ from their values.
pub fn select_index(message: &str, labels: Vec<String>) -> Result<usize> {
    require_tty(&format!("`{message}` needs an interactive choice"))?;
    map_inquire(Select::new(message, labels).raw_prompt()).map(|choice| choice.index)
}

pub fn text(message: &str, default: Option<&str>) -> Result<String> {
    require_tty(&format!("`{message}` needs interactive input"))?;
    let mut prompt = Text::new(message);
    if let Some(d) = default {
        prompt = prompt.with_default(d);
    }
    map_inquire(prompt.prompt())
}

/// Prompt for a `secret: true` input: masked while typing, never echoed into
/// scrollback. No confirmation round — these are pasted credentials, not new
/// passwords being chosen.
pub fn password(message: &str) -> Result<String> {
    require_tty(&format!("`{message}` needs interactive input"))?;
    map_inquire(
        Password::new(message)
            .with_display_mode(PasswordDisplayMode::Masked)
            .without_confirmation()
            .prompt(),
    )
}

pub fn confirm(message: &str) -> Result<bool> {
    require_tty("trusting a manifest needs interactive confirmation")?;
    map_inquire(Confirm::new(message).with_default(false).prompt())
}
