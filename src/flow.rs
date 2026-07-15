use std::collections::HashMap;

use anyhow::Result;

use crate::exec;
use crate::prompt;
use crate::resolver::{Resolved, ResolvedCommand, group_commands};

/// Bare `pult`: the guided flow (spec §9 Phase 1). A command menu, then one
/// prompt per param — sequential, so each picker can shell out with every
/// earlier answer already known.
///
/// When commands span more than one display group (`category:`, else include
/// origin, else the implicit "local" group — see
/// [`crate::resolver::group_commands`]), the menu is two-stage: pick a group,
/// then a command within it. Esc at the inner "which command?" select steps
/// back to the group list rather than exiting; Ctrl-C at the inner select
/// still aborts immediately, same as everywhere else. Esc or Ctrl-C at the
/// group level exits as usual (exit 130). A single group behaves exactly as
/// the historical flat list.
pub fn run(resolved: &Resolved, assume_trusted: bool, print: bool) -> Result<i32> {
    println!("◆  {} · pult", resolved.name);
    let groups = group_commands(&resolved.commands);

    let cmd = if groups.len() <= 1 {
        let index = prompt::select_index(
            "What do you want to do?",
            command_labels(&resolved.commands),
        )?;
        &resolved.commands[index]
    } else {
        loop {
            let group_labels: Vec<String> = groups
                .iter()
                .map(|(label, cmds)| format!("{label}  ({})", cmds.len()))
                .collect();
            let gi = prompt::select_index("What do you want to do?", group_labels)?;
            let members = &groups[gi].1;
            let labels = command_labels_ref(members);
            match prompt::select_index("Which command?", labels) {
                Ok(ci) => break members[ci],
                // Esc: back to the group list. Ctrl-C (`Cancelled`) falls
                // through to the propagating arm below — it must abort, not
                // step back a menu level.
                Err(e) if e.downcast_ref::<prompt::Dismissed>().is_some() => continue,
                Err(e) => return Err(e),
            }
        }
    };
    exec::execute(resolved, cmd, &HashMap::new(), assume_trusted, print)
}

fn command_labels(cmds: &[ResolvedCommand]) -> Vec<String> {
    cmds.iter().map(label_for).collect()
}

fn command_labels_ref(cmds: &[&ResolvedCommand]) -> Vec<String> {
    cmds.iter().map(|c| label_for(c)).collect()
}

fn label_for(c: &ResolvedCommand) -> String {
    match &c.origin {
        Some(src) => format!("{}  ({})  ← {src}", c.title, c.id),
        None => format!("{}  ({})", c.title, c.id),
    }
}
