use anyhow::{Result, bail};
use indexmap::IndexMap;

use crate::interp;
use crate::resolver::{ResolvedCommand, ResolvedEntry, ResolvedRun, ResolvedSeg};

/// Compile a step-list run into one bash script: named steps become shell
/// functions, the run list becomes the calls, declared outputs get runtime
/// assertions, `exports:` becomes renames. Param values are interpolated
/// leniently into user-authored fragments only — generated glue never passes
/// through interpolation.
pub fn compile(cmd: &ResolvedCommand, values: &IndexMap<String, String>) -> Result<String> {
    let ResolvedRun::Steps(entries) = &cmd.run else {
        bail!("command `{}` has a plain run, nothing to compile", cmd.id);
    };

    // (fn_name, body) — deduped on identical body, suffixed on name clashes.
    let mut fns: Vec<(String, String)> = Vec::new();
    let mut body = String::new();

    for entry in entries {
        match entry {
            ResolvedEntry::Inline(s) => {
                body.push_str(&interp::interpolate_lenient(s, values));
                push_newline(&mut body);
            }
            ResolvedEntry::Call(call) => {
                let script = interp::interpolate_lenient(&call.script, values);
                let fn_name = intern(&mut fns, &call.name, script);
                body.push_str(&fn_name);
                body.push('\n');
                for out in &call.outputs {
                    body.push_str(&assertion(&call.name, out));
                }
                for (from, to) in &call.exports {
                    body.push_str(&format!("{to}=\"${from}\"; unset {from}\n"));
                }
            }
            ResolvedEntry::Pipe(segs) => {
                let parts: Vec<String> = segs
                    .iter()
                    .map(|seg| match seg {
                        ResolvedSeg::Inline(s) => interp::interpolate_lenient(s, values),
                        ResolvedSeg::Call { name, script } => {
                            let script = interp::interpolate_lenient(script, values);
                            intern(&mut fns, name, script)
                        }
                    })
                    .collect();
                body.push_str(&parts.join(" | "));
                body.push('\n');
            }
        }
    }

    let mut out = String::from("set -euo pipefail\n");
    for (fn_name, script) in &fns {
        out.push_str(&format!("\n{fn_name}() {{\n{}\n}}\n", script.trim_end()));
    }
    out.push('\n');
    out.push_str(&body);
    Ok(out)
}

fn push_newline(body: &mut String) {
    if !body.ends_with('\n') {
        body.push('\n');
    }
}

fn assertion(step: &str, output: &str) -> String {
    format!(
        "[ \"${{{output}+x}}\" ] || {{ echo \"pult: step {step} did not set declared output {output}\" >&2; exit 1; }}\n"
    )
}

/// Register a step function; identical (name, body) is reused, a name clash
/// with a different body (same step, different `with:`) gets a suffix.
fn intern(fns: &mut Vec<(String, String)>, step_name: &str, script: String) -> String {
    let base = mangle(step_name);
    let mut candidate = base.clone();
    let mut n = 1;
    loop {
        match fns.iter().find(|(name, _)| *name == candidate) {
            Some((_, existing)) if *existing == script => return candidate,
            Some(_) => {
                n += 1;
                candidate = format!("{base}_{n}");
            }
            None => {
                fns.push((candidate.clone(), script));
                return candidate;
            }
        }
    }
}

/// `aws:resolve-task` → `aws__resolve_task`
fn mangle(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 2);
    if name.starts_with(|c: char| c.is_ascii_digit()) {
        out.push('_');
    }
    for c in name.chars() {
        match c {
            ':' => out.push_str("__"),
            c if c.is_ascii_alphanumeric() || c == '_' => out.push(c),
            _ => out.push('_'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolver::ResolvedCall;

    fn vals(pairs: &[(&str, &str)]) -> IndexMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn command(entries: Vec<ResolvedEntry>) -> ResolvedCommand {
        ResolvedCommand {
            id: "t".into(),
            title: "T".into(),
            params: IndexMap::new(),
            run: ResolvedRun::Steps(entries),
            origin: None,
        }
    }

    #[test]
    fn compiles_calls_assertions_and_exports() {
        let cmd = command(vec![
            ResolvedEntry::Call(ResolvedCall {
                name: "aws:resolve-task".into(),
                script: "TASK=$(find-task {env})".into(),
                outputs: vec!["TASK".into()],
                exports: [("TASK".to_string(), "BACKEND_TASK".to_string())]
                    .into_iter()
                    .collect(),
            }),
            ResolvedEntry::Inline("echo \"$BACKEND_TASK\"".into()),
        ]);
        let script = compile(&cmd, &vals(&[("env", "dev")])).unwrap();
        assert!(script.starts_with("set -euo pipefail\n"), "got: {script}");
        assert!(
            script.contains("aws__resolve_task() {\nTASK=$(find-task dev)\n}"),
            "got: {script}"
        );
        assert!(script.contains("[ \"${TASK+x}\" ] || { echo \"pult: step aws:resolve-task did not set declared output TASK\" >&2; exit 1; }"), "got: {script}");
        assert!(
            script.contains("BACKEND_TASK=\"$TASK\"; unset TASK"),
            "got: {script}"
        );
        // order: function call before assertion before rename before inline
        let call_pos = script.rfind("aws__resolve_task\n").unwrap();
        let assert_pos = script.find("[ \"${TASK+x}\"").unwrap();
        let rename_pos = script.find("BACKEND_TASK=").unwrap();
        let echo_pos = script.find("echo \"$BACKEND_TASK\"").unwrap();
        assert!(call_pos < assert_pos && assert_pos < rename_pos && rename_pos < echo_pos);
    }

    #[test]
    fn compiles_pipes_from_functions_and_inline() {
        let cmd = command(vec![ResolvedEntry::Pipe(vec![
            ResolvedSeg::Call {
                name: "list".into(),
                script: "printf 'a\\nb\\n'".into(),
            },
            ResolvedSeg::Inline("tr a-z A-Z".into()),
        ])]);
        let script = compile(&cmd, &vals(&[])).unwrap();
        assert!(
            script.contains("list() {\nprintf 'a\\nb\\n'\n}"),
            "got: {script}"
        );
        assert!(script.contains("list | tr a-z A-Z"), "got: {script}");
    }

    #[test]
    fn same_step_different_binding_gets_distinct_function() {
        let call = |script: &str| {
            ResolvedEntry::Call(ResolvedCall {
                name: "greet".into(),
                script: script.into(),
                outputs: vec![],
                exports: IndexMap::new(),
            })
        };
        let cmd = command(vec![call("echo one"), call("echo two"), call("echo one")]);
        let script = compile(&cmd, &vals(&[])).unwrap();
        assert!(script.contains("greet() {\necho one\n}"), "got: {script}");
        assert!(script.contains("greet_2() {\necho two\n}"), "got: {script}");
        assert_eq!(
            script.matches("greet() {").count(),
            1,
            "identical body reused"
        );
    }

    #[test]
    fn generated_glue_survives_shelly_step_bodies() {
        let cmd = command(vec![ResolvedEntry::Call(ResolvedCall {
            name: "gnarly".into(),
            script: "X=$(ps aux | awk '{print $1}'); [ \"${X:-}\" ] && OUT=\"$X\"".into(),
            outputs: vec!["OUT".into()],
            exports: IndexMap::new(),
        })]);
        let script = compile(&cmd, &vals(&[("env", "dev")])).unwrap();
        assert!(script.contains("awk '{print $1}'"), "got: {script}");
        assert!(script.contains("${X:-}"), "got: {script}");
    }
}
