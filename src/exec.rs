use std::collections::HashMap;

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
/// executed, so the trust check lives here.
pub fn execute(
    resolved: &Resolved,
    cmd: &ResolvedCommand,
    provided: &HashMap<String, String>,
    assume_trusted: bool,
    print: bool,
) -> Result<i32> {
    trust::ensure_trusted(
        &resolved.path,
        &resolved.trust_hash,
        &resolved.include_summary,
        assume_trusted,
    )?;

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
            (None, ParamKind::Pick(pick)) => {
                let opts = options::resolve_pick(pick, &values, &resolved.run_dir)?;
                prompt::select(&format!("{name}?"), opts)?
            }
            (None, ParamKind::Input(input)) => {
                prompt::text(&format!("{name}?"), input.default.as_deref())?
            }
            (_, ParamKind::Use(_)) => unreachable!("resolver inlines every use: param"),
        };
        values.insert(name.clone(), value);
    }

    match &cmd.run {
        ResolvedRun::Script(template) => {
            let cmdline = interp::interpolate(template, &values)?;
            if print {
                println!("{cmdline}");
                return Ok(0);
            }
            runner::run_sh(&cmdline, &resolved.run_dir)
        }
        ResolvedRun::Steps(_) => {
            let script = compile::compile(cmd, &values)?;
            if print {
                println!("{script}");
                return Ok(0);
            }
            runner::run_bash(&script, &cmd.id, &resolved.run_dir)
        }
    }
}
