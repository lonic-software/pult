use std::collections::HashMap;

use anyhow::Result;

use crate::exec;
use crate::prompt;
use crate::resolver::Resolved;

/// Bare `pult`: the guided flow (spec §9 Phase 1). A command menu, then one
/// prompt per param — sequential, so each picker can shell out with every
/// earlier answer already known.
pub fn run(resolved: &Resolved, assume_trusted: bool, print: bool) -> Result<i32> {
    println!("◆  {} · pult", resolved.name);
    let labels: Vec<String> = resolved
        .commands
        .iter()
        .map(|c| match &c.origin {
            Some(src) => format!("{}  ({})  ← {src}", c.title, c.id),
            None => format!("{}  ({})", c.title, c.id),
        })
        .collect();
    let index = prompt::select_index("What do you want to do?", labels)?;
    let cmd = &resolved.commands[index];
    exec::execute(resolved, cmd, &HashMap::new(), assume_trusted, print)
}
