use std::collections::HashMap;

use anyhow::Result;

use crate::exec;
use crate::label;
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
pub fn run(resolved: &Resolved, assume_trusted: bool, print: bool, run_id: Option<&str>) -> Result<i32> {
    println!("◆  {} · pult", resolved.name);
    let groups = group_commands(&resolved.commands);
    let width = label::width();

    let cmd = if groups.len() <= 1 {
        let index = prompt::select_index(
            "What do you want to do?",
            command_labels(&resolved.commands, width),
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
            let labels = command_labels_ref(members, width);
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
    exec::execute(resolved, cmd, &HashMap::new(), assume_trusted, print, run_id)
}

fn command_labels(cmds: &[ResolvedCommand], width: usize) -> Vec<String> {
    cmds.iter().map(|c| label_for(c, width)).collect()
}

fn command_labels_ref(cmds: &[&ResolvedCommand], width: usize) -> Vec<String> {
    cmds.iter().map(|c| label_for(c, width)).collect()
}

fn label_for(c: &ResolvedCommand, width: usize) -> String {
    menu_label(
        &c.title,
        c.description.as_deref(),
        &c.id,
        c.origin.as_deref(),
        width,
    )
}

/// Compose a menu label: `Title — description  (id)  ← src`. The `←src`
/// origin suffix and the `— description` segment are both optional (origin
/// when there's no source repo, description when the command has none). A
/// thin wrapper over the shared [`label::compose`] — see that function for
/// the truncation contract (description absorbs all truncation, char-boundary
/// safe, dropped entirely rather than a lone `…`).
fn menu_label(
    title: &str,
    desc: Option<&str>,
    id: &str,
    origin: Option<&str>,
    width: usize,
) -> String {
    let tail = match origin {
        Some(src) => format!("  ({id})  ← {src}"),
        None => format!("  ({id})"),
    };
    label::compose(title, desc, &tail, width)
}

#[cfg(test)]
mod tests {
    use super::menu_label;

    #[test]
    fn no_description_matches_historical_format() {
        assert_eq!(
            menu_label("Show status", None, "status", None, 100),
            "Show status  (status)"
        );
        assert_eq!(
            menu_label("Show status", None, "status", Some("aws"), 100),
            "Show status  (status)  ← aws"
        );
    }

    #[test]
    fn description_fits_shows_in_full() {
        assert_eq!(
            menu_label(
                "Show status",
                Some("Prints the current deploy status"),
                "status",
                Some("aws"),
                100
            ),
            "Show status — Prints the current deploy status  (status)  ← aws"
        );
    }

    #[test]
    fn empty_description_is_treated_as_absent() {
        assert_eq!(
            menu_label("Show status", Some(""), "status", None, 100),
            "Show status  (status)"
        );
    }

    #[test]
    fn description_is_truncated_to_fit_width() {
        let label = menu_label(
            "Show status",
            Some("A very long description that will not fit in the available width at all"),
            "status",
            Some("aws"),
            50,
        );
        assert!(label.chars().count() <= 50);
        assert!(label.contains('…'), "label was: {label:?}");
        assert!(label.contains("(status)"), "label was: {label:?}");
        assert!(label.contains("← aws"), "label was: {label:?}");
        assert!(label.starts_with("Show status — "), "label was: {label:?}");
    }

    #[test]
    fn tiny_width_omits_description_entirely_and_leaves_base_whole() {
        let base = "Show status  (status)  ← aws";
        // Width smaller than even the description-less base label.
        let label = menu_label(
            "Show status",
            Some("Some description"),
            "status",
            Some("aws"),
            5,
        );
        assert_eq!(label, base);
        assert!(!label.contains('…'));
    }

    #[test]
    fn truncation_is_char_boundary_safe_with_multibyte_chars() {
        // Description built with multi-byte chars (é) and an emoji right
        // around where truncation is expected to land.
        let desc = "café ".repeat(10) + "🎉 tail";
        let label = menu_label("Title", Some(&desc), "id", Some("src"), 40);
        assert!(label.chars().count() <= 40);
        assert!(label.contains('…'));
        assert!(label.contains("(id)"));
        assert!(label.contains("← src"));
    }
}
