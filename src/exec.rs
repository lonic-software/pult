use std::collections::HashMap;
use std::path::Path;

use anyhow::{Result, bail};
use indexmap::IndexMap;

use crate::manifest::ParamKind;
use crate::resolver::{Resolved, ResolvedCommand, ResolvedRun};
use crate::{compile, interp, options, prompt, runner, trust};

/// Execute one command: fill its params (from provided values or interactive
/// prompts, in declared order — which is what makes dependent `from:` sources
/// work), build the final script, and hand off to the runner (or print it).
///
/// This is the single choke point through which anything from a manifest gets
/// executed, so the trust check lives here. For an ephemeral `pult x` source the
/// trust prompt also shows the **composed command about to run** — honest there
/// because the trust unit is that one invocation. A real manifest's trust covers
/// every command it declares, so showing one would misrepresent the scope; its
/// prompt stays the file/include summary. Either way `--print` prints that same
/// composed command with no trust gate — a side-effect-free dry run, since the
/// preview is built without running any module code (dynamic option sources stay
/// unresolved, shown as `<name>` metavars).
pub fn execute(
    resolved: &Resolved,
    cmd: &ResolvedCommand,
    provided: &HashMap<String, String>,
    assume_trusted: bool,
    print: bool,
) -> Result<i32> {
    if print {
        // Dry run: compose without executing anything (provided values
        // concrete, the rest as `<name>`), print, and stop — no trust needed.
        let preview = compose(cmd, &fill(cmd, provided, None)?)?;
        println!("{preview}");
        return Ok(0);
    }

    // Only an ephemeral source shows its command in the trust prompt (see above).
    let about_to_run = if resolved.ephemeral {
        Some(compose(cmd, &fill(cmd, provided, None)?)?)
    } else {
        None
    };
    trust::ensure_trusted(
        &resolved.path,
        &resolved.trust_hash,
        &resolved.include_summary,
        assume_trusted,
        about_to_run.as_deref(),
    )?;

    // Trusted now — fill for real (prompting for and resolving any values not
    // given on the command line) and run.
    let values = fill(cmd, provided, Some(&resolved.run_dir))?;
    match &cmd.run {
        ResolvedRun::Script(template) => {
            runner::run_sh(&interp::interpolate(template, &values)?, &resolved.run_dir)
        }
        ResolvedRun::Steps(_) => {
            runner::run_bash(&compile::compile(cmd, &values)?, &cmd.id, &resolved.run_dir)
        }
    }
}

/// Fill a command's params in declared order. `run_dir: None` = **preview**:
/// never prompt or run a dynamic option source (both would execute code from a
/// possibly-untrusted module); unprovided params become `<name>` metavars.
/// `Some(dir)` = **live**: prompt for anything not provided, running dynamic
/// pickers against `dir` — reached only after the trust gate.
fn fill(
    cmd: &ResolvedCommand,
    provided: &HashMap<String, String>,
    run_dir: Option<&Path>,
) -> Result<IndexMap<String, String>> {
    let mut values: IndexMap<String, String> = IndexMap::new();
    for (name, def) in &cmd.params {
        let value = match (provided.get(name), def.kind()) {
            (Some(v), ParamKind::Pick(pick)) => {
                // Validate against static options; dynamic sources accept any
                // value so direct invocation stays fast and scriptable.
                if let Some(opts) = &pick.options
                    && !opts.contains(v)
                {
                    bail!(
                        "invalid value `{v}` for `{name}` (expected one of: {})",
                        opts.join(", ")
                    );
                }
                v.clone()
            }
            (Some(v), ParamKind::Input(_)) => v.clone(),
            // Preview: show the slot instead of prompting or shelling out.
            (None, _) if run_dir.is_none() => format!("<{name}>"),
            (None, ParamKind::Pick(pick)) => {
                let opts = options::resolve_pick(pick, &values, run_dir.unwrap())?;
                prompt::select(&format!("{name}?"), opts)?
            }
            (None, ParamKind::Input(input)) => {
                prompt::text(&format!("{name}?"), input.default.as_deref())?
            }
            (_, ParamKind::Use(_)) => unreachable!("resolver inlines every use: param"),
        };
        values.insert(name.clone(), value);
    }
    Ok(values)
}

/// Render the final command text for `values`: a plain `run:` interpolates to a
/// single line; a step list compiles to its bash script.
fn compose(cmd: &ResolvedCommand, values: &IndexMap<String, String>) -> Result<String> {
    match &cmd.run {
        ResolvedRun::Script(template) => interp::interpolate(template, values),
        ResolvedRun::Steps(_) => compile::compile(cmd, values),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{manifest, resolver};

    #[test]
    fn print_is_a_trustfree_dry_run_that_never_runs_option_sources() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("ran");
        // A dynamic picker whose source would create a marker file if executed —
        // running it during a dry run is exactly the "inspect before trust" hole.
        let src = format!(
            "version: 1\nname: demo\ncommands:\n  - id: go\n    title: Go\n    \
             params:\n      p: {{ pick: {{ from: \"touch '{}'; echo x\" }} }}\n    \
             run: \"echo ran {{p}}\"\n",
            marker.display()
        );
        std::fs::write(dir.path().join("pult.yaml"), &src).unwrap();
        let resolved =
            resolver::resolve(manifest::load(&dir.path().join("pult.yaml")).unwrap()).unwrap();

        // print + no values + untrusted: must not prompt, must not run the option
        // source, must not consult the trust store — a pure, side-effect-free
        // preview of what the (as-yet-untrusted) command would run.
        let code = execute(
            &resolved,
            &resolved.commands[0],
            &HashMap::new(),
            false,
            true,
        )
        .unwrap();
        assert_eq!(code, 0);
        assert!(
            !marker.exists(),
            "option source ran during a --print dry run"
        );
    }
}
