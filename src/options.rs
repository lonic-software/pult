use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use indexmap::IndexMap;

use crate::interp;
use crate::manifest::PickDef;

/// A resolved pick option: the value handed to the command, and an optional
/// display-only description shown next to it in the interactive picker. The
/// value is never affected by the description — see `label::option_label`.
#[derive(Debug, Clone, PartialEq)]
pub struct PickOption {
    pub value: String,
    pub description: Option<String>,
}

/// Resolve a picker's options: either the static list, or the stdout lines of
/// its `from:` shell command (interpolated with the params answered so far).
pub fn resolve_pick(
    pick: &PickDef,
    values: &IndexMap<String, String>,
    dir: &Path,
) -> Result<Vec<PickOption>> {
    if let Some(options) = &pick.options {
        return Ok(options
            .iter()
            .map(|o| PickOption {
                value: o.value().to_string(),
                description: o.description().map(str::to_string),
            })
            .collect());
    }
    let from = pick
        .from
        .as_ref()
        .context("pick has neither options nor from (should be caught at load)")?;
    let cmd = interp::interpolate(from, values)?;
    let output = Command::new("sh")
        .arg("-c")
        .arg(&cmd)
        .current_dir(dir)
        .output()
        .with_context(|| format!("failed to run option source `{cmd}`"))?;
    if !output.status.success() {
        bail!(
            "option source `{cmd}` failed ({}):\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim_end()
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut options = Vec::new();
    for line in stdout.lines() {
        if let Some(opt) = parse_dynamic_line(line, &cmd)? {
            options.push(opt);
        }
    }
    if options.is_empty() {
        bail!("option source `{cmd}` produced no options");
    }
    Ok(options)
}

/// Parse one stdout line from a `from:` source (§3 of the design doc):
///
/// 0. Blank after trim → skip, *before* any tab handling — this must run
///    first so a whitespace-only line that happens to contain a tab
///    (`" \t "`) is silently skipped like any other blank line, not sent
///    into rule 2's hard error.
/// 1. No tab (and not blank) → `line.trim()` value-only, no description.
///    Byte-identical to the historical (pre-description) behavior.
/// 2. Contains a tab (and not blank) → split the *original* line at the
///    *first* tab: `value = left.trim()`, `description = right.trim()`
///    (later tabs stay inside the description verbatim).
///    - An empty value before the tab is a hard error naming the source
///      command — almost certainly `printf '%s\t%s\n' "$v" "$d"` with `$v`
///      empty/unset, the bug this catches at its source rather than
///      silently handing back an incomplete picker.
///    - An empty description collapses to `None`, so `printf 'a\t\n'` is
///      equivalent to `printf 'a\n'`.
fn parse_dynamic_line(line: &str, cmd: &str) -> Result<Option<PickOption>> {
    if line.trim().is_empty() {
        return Ok(None);
    }
    let Some(tab_idx) = line.find('\t') else {
        return Ok(Some(PickOption {
            value: line.trim().to_string(),
            description: None,
        }));
    };
    let (left, right) = line.split_at(tab_idx);
    let right = &right[1..]; // drop the tab itself
    let value = left.trim();
    if value.is_empty() {
        bail!("option source `{cmd}` emitted a line with an empty value before the tab");
    }
    let description = right.trim();
    Ok(Some(PickOption {
        value: value.to_string(),
        description: if description.is_empty() {
            None
        } else {
            Some(description.to_string())
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::OptionDef;

    fn values(opts: &[PickOption]) -> Vec<&str> {
        opts.iter().map(|o| o.value.as_str()).collect()
    }

    /// §7.2 — a scalar YAML option behaves identically to today: values as
    /// declared, all descriptions `None`.
    #[test]
    fn static_options_pass_through() {
        let pick = PickDef {
            options: Some(vec![
                OptionDef::Plain("dev".into()),
                OptionDef::Plain("uat".into()),
            ]),
            from: None,
        };
        let opts = resolve_pick(&pick, &IndexMap::new(), Path::new(".")).unwrap();
        assert_eq!(values(&opts), ["dev", "uat"]);
        assert!(opts.iter().all(|o| o.description.is_none()));
    }

    #[test]
    fn static_options_carry_their_description() {
        let pick = PickDef {
            options: Some(vec![
                OptionDef::Plain("dev".into()),
                OptionDef::Full(crate::manifest::FullOption {
                    value: "uat".into(),
                    description: Some("User acceptance".into()),
                }),
            ]),
            from: None,
        };
        let opts = resolve_pick(&pick, &IndexMap::new(), Path::new(".")).unwrap();
        assert_eq!(opts[0].description, None);
        assert_eq!(opts[1].description.as_deref(), Some("User acceptance"));
    }

    /// §7.3 — a value-only `from:` line is unchanged, including
    /// trim-and-skip-blank, even when the blank line contains a tab. Redden
    /// by requiring tabs, changing trimming, or handling the tab before the
    /// blank check (which would turn `" \t "` into the §7.6 hard error).
    #[test]
    fn from_reads_stdout_lines() {
        let pick = PickDef {
            options: None,
            from: Some("printf 'alpha\\n  beta  \\n\\n \\t \\n'".into()),
        };
        let opts = resolve_pick(&pick, &IndexMap::new(), Path::new(".")).unwrap();
        assert_eq!(values(&opts), ["alpha", "beta"]);
        assert!(opts.iter().all(|o| o.description.is_none()));
    }

    /// §7.4 — tab lines split at the first tab only; later tabs stay inside
    /// the description verbatim. Redden with `splitn(3)` / `rsplit`.
    #[test]
    fn tab_lines_split_at_the_first_tab_only() {
        let pick = PickDef {
            options: None,
            from: Some("printf 'a\\tb\\tc\\n'".into()),
        };
        let opts = resolve_pick(&pick, &IndexMap::new(), Path::new(".")).unwrap();
        assert_eq!(opts.len(), 1);
        assert_eq!(opts[0].value, "a");
        assert_eq!(opts[0].description.as_deref(), Some("b\tc"));
    }

    /// §7.5 — an empty description (nothing after the tab) is `None`, not
    /// `Some("")`.
    #[test]
    fn empty_description_after_tab_is_none() {
        let pick = PickDef {
            options: None,
            from: Some("printf 'a\\t\\n'".into()),
        };
        let opts = resolve_pick(&pick, &IndexMap::new(), Path::new(".")).unwrap();
        assert_eq!(opts[0].value, "a");
        assert_eq!(opts[0].description, None);
    }

    /// §7.6 — an empty value before a tab is a hard error naming the source
    /// command. Redden by silently skipping such lines instead.
    #[test]
    fn empty_value_before_tab_is_a_hard_error_naming_the_source() {
        let pick = PickDef {
            options: None,
            from: Some("printf '\\tdesc\\n'".into()),
        };
        let err = resolve_pick(&pick, &IndexMap::new(), Path::new("."))
            .unwrap_err()
            .to_string();
        assert!(err.contains("empty value before the tab"), "got: {err}");
        assert!(err.contains("printf"), "should name the source: {err}");
    }

    #[test]
    fn from_interpolates_earlier_answers() {
        let pick = PickDef {
            options: None,
            from: Some("echo {env}-customer".into()),
        };
        let mut vals = IndexMap::new();
        vals.insert("env".to_string(), "dev".to_string());
        let opts = resolve_pick(&pick, &vals, Path::new(".")).unwrap();
        assert_eq!(values(&opts), ["dev-customer"]);
    }

    #[test]
    fn failing_source_reports_stderr() {
        let pick = PickDef {
            options: None,
            from: Some("echo boom >&2; exit 3".into()),
        };
        let err = resolve_pick(&pick, &IndexMap::new(), Path::new("."))
            .unwrap_err()
            .to_string();
        assert!(err.contains("boom"), "got: {err}");
    }
}
