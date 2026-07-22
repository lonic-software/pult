//! Shared label-composition primitive — `head — description<tail>`, with the
//! description absorbing all truncation. A dependency-free leaf module so
//! both the guided flow's command menu (`flow::menu_label`) and pick option
//! labels (`option_label`) can share it without either module depending on
//! the other (see the design doc's §4 rationale: `flow` already depends on
//! `exec`, so putting this in `flow` would make `exec` import `flow` to use
//! it — inverting the natural layering).

use crate::options::PickOption;

/// Fallback terminal width when it can't be detected (not a tty, env issue).
const FALLBACK_WIDTH: usize = 100;

/// Columns to reserve for inquire's prompt chrome (the rendered cursor arrow
/// and padding it adds to each option line) when sizing labels.
const INQUIRE_CHROME_MARGIN: usize = 4;

/// Terminal width to size labels against, minus inquire's chrome margin.
pub fn width() -> usize {
    let cols = crossterm::terminal::size()
        .map(|(cols, _)| cols as usize)
        .unwrap_or(FALLBACK_WIDTH);
    cols.saturating_sub(INQUIRE_CHROME_MARGIN)
}

/// Compose a label: `head — description<tail>`, where `tail` is a
/// pre-formatted, always-whole suffix (e.g. `"  (id)  ← src"`, or `""`).
///
/// The label must fit on one line within `width` columns. The description
/// absorbs all truncation — ellipsized with a single trailing `…` — while
/// `head` and `tail` always survive whole. If there isn't even room for a
/// single truncated description character, the description is dropped
/// entirely rather than emitting a lone `…`. If `width` is too small even for
/// the description-less label, that label is returned untouched and left for
/// the terminal to wrap.
pub fn compose(head: &str, desc: Option<&str>, tail: &str, width: usize) -> String {
    let base = format!("{head}{tail}");

    let desc = desc.filter(|d| !d.is_empty());
    let Some(desc) = desc else {
        return base;
    };

    if base.chars().count() > width {
        return base;
    }

    const SEP: &str = " — ";
    let full = format!("{head}{SEP}{desc}{tail}");
    if full.chars().count() <= width {
        return full;
    }

    let non_desc_len = head.chars().count() + SEP.chars().count() + tail.chars().count();
    if non_desc_len >= width {
        return base;
    }
    let avail = width - non_desc_len;
    // Need room for at least one desc char plus the ellipsis; otherwise omit
    // the description entirely rather than emit a lone `…`.
    if avail < 2 {
        return base;
    }
    let desc_chars = avail - 1;
    let truncated: String = desc.chars().take(desc_chars).collect();
    format!("{head}{SEP}{truncated}…{tail}")
}

/// `value — description`, one line, description ellipsized to fit — the
/// label rendered for one pick option in the interactive picker.
pub fn option_label(o: &PickOption, width: usize) -> String {
    compose(&o.value, o.description.as_deref(), "", width)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn option_label_truncates_a_multibyte_description_at_a_small_width() {
        let o = PickOption {
            value: "uat".to_string(),
            description: Some("café ".repeat(10) + "🎉 tail"),
        };
        let label = option_label(&o, 20);
        assert!(label.chars().count() <= 20);
        assert!(label.contains('…'));
        assert!(label.starts_with("uat — "));
    }
}
