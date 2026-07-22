use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use indexmap::IndexMap;

use crate::interp;
use crate::manifest::PickDef;

/// Resolve a picker's options: either the static list, or the stdout lines of
/// its `from:` shell command (interpolated with the params answered so far).
pub fn resolve_pick(
    pick: &PickDef,
    values: &IndexMap<String, String>,
    dir: &Path,
) -> Result<Vec<String>> {
    if let Some(options) = &pick.options {
        return Ok(options.iter().map(|o| o.value().to_string()).collect());
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
    let options: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    if options.is_empty() {
        bail!("option source `{cmd}` produced no options");
    }
    Ok(options)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::OptionDef;

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
        assert_eq!(opts, ["dev", "uat"]);
    }

    #[test]
    fn from_reads_stdout_lines() {
        let pick = PickDef {
            options: None,
            from: Some("printf 'alpha\\n  beta  \\n\\n'".into()),
        };
        let opts = resolve_pick(&pick, &IndexMap::new(), Path::new(".")).unwrap();
        assert_eq!(opts, ["alpha", "beta"]);
    }

    #[test]
    fn from_interpolates_earlier_answers() {
        let pick = PickDef {
            options: None,
            from: Some("echo {env}-customer".into()),
        };
        let mut values = IndexMap::new();
        values.insert("env".to_string(), "dev".to_string());
        let opts = resolve_pick(&pick, &values, Path::new(".")).unwrap();
        assert_eq!(opts, ["dev-customer"]);
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
