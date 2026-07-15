use anyhow::{Result, bail};
use indexmap::IndexMap;

use crate::interp;
use crate::resolver::{ResolvedCommand, ResolvedEntry, ResolvedRun, ResolvedSeg};

/// Compile a step-list run into one bash script: named steps become shell
/// functions, the run list becomes the calls, declared outputs get runtime
/// assertions, `exports:` becomes renames. Param values are interpolated
/// leniently into user-authored fragments only — generated glue never passes
/// through interpolation.
///
/// `events`: when true, inject a guarded `step k/n <label>` emission before
/// each top-level entry (see `step_guard`) — consumed only if a live run has
/// wired up `$PULT_EVENTS`, so it's a silent no-op elsewhere. `--print`
/// previews pass `false` to keep that output exactly the composed script.
pub fn compile(
    cmd: &ResolvedCommand,
    values: &IndexMap<String, String>,
    events: bool,
) -> Result<String> {
    let ResolvedRun::Steps(entries) = &cmd.run else {
        bail!("command `{}` has a plain run, nothing to compile", cmd.id);
    };

    // (fn_name, body) — deduped on identical body, suffixed on name clashes.
    let mut fns: Vec<(String, String)> = Vec::new();
    let mut body = String::new();
    let total = entries.len();

    for (i, entry) in entries.iter().enumerate() {
        if events {
            body.push_str(&step_guard(i + 1, total, &step_label(entry)));
        }
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

/// A guarded, no-op-when-unwired emission of the `step k/n <label>` event —
/// injected before every top-level entry when `events` is on. The guard
/// means the compiled script behaves identically whether or not anything is
/// listening on `$PULT_EVENTS`.
fn step_guard(k: usize, n: usize, label: &str) -> String {
    format!(
        "[ -n \"${{PULT_EVENTS:-}}\" ] && printf 'step %d/%d %s\\n' {k} {n} {} >&\"$PULT_EVENTS\" || true\n",
        interp::shell_quote(label)
    )
}

/// The step-event label for one top-level entry: a `Call`'s step name (as
/// the user knows it), or the first line of an `Inline`/`Pipe` fragment
/// (joined with ` | ` for a pipe), truncated to 40 chars. Also the source of
/// the `"steps"` field in `--list --json` (`main.rs`) — same labels there.
pub fn step_label(entry: &ResolvedEntry) -> String {
    match entry {
        ResolvedEntry::Call(call) => call.name.clone(),
        ResolvedEntry::Inline(s) => truncate40(first_line(s)),
        ResolvedEntry::Pipe(segs) => {
            let parts: Vec<&str> = segs
                .iter()
                .map(|seg| match seg {
                    ResolvedSeg::Inline(s) => first_line(s),
                    ResolvedSeg::Call { name, .. } => name.as_str(),
                })
                .collect();
            truncate40(&parts.join(" | "))
        }
    }
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("")
}

fn truncate40(s: &str) -> String {
    if s.chars().count() <= 40 {
        s.to_string()
    } else {
        s.chars().take(40).collect()
    }
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
            origin_name: None,
            check: None,
            interactive: false,
            category: None,
            description: None,
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
        let script = compile(&cmd, &vals(&[("env", "dev")]), false).unwrap();
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
        let script = compile(&cmd, &vals(&[]), false).unwrap();
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
        let script = compile(&cmd, &vals(&[]), false).unwrap();
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
        let script = compile(&cmd, &vals(&[("env", "dev")]), false).unwrap();
        assert!(script.contains("awk '{print $1}'"), "got: {script}");
        assert!(script.contains("${X:-}"), "got: {script}");
    }

    #[test]
    fn events_false_is_byte_identical_to_no_events_output() {
        let cmd = command(vec![
            ResolvedEntry::Call(ResolvedCall {
                name: "aws:resolve-task".into(),
                script: "TASK=$(find-task {env})".into(),
                outputs: vec!["TASK".into()],
                exports: IndexMap::new(),
            }),
            ResolvedEntry::Inline("echo done".into()),
        ]);
        let values = vals(&[("env", "dev")]);
        let without_flag = compile(&cmd, &values, false).unwrap();
        // `--print` and any other caller that never mentions `events` still
        // gets exactly this — the guard is opt-in, not a hidden default.
        assert!(!without_flag.contains("PULT_EVENTS"), "got: {without_flag}");
        assert!(!without_flag.contains("step 1/2"), "got: {without_flag}");
    }

    #[test]
    fn events_true_injects_guarded_step_lines() {
        let cmd = command(vec![
            ResolvedEntry::Call(ResolvedCall {
                name: "aws:resolve-task".into(),
                script: "TASK=$(find-task {env})".into(),
                outputs: vec!["TASK".into()],
                exports: IndexMap::new(),
            }),
            ResolvedEntry::Inline("echo done".into()),
        ]);
        let script = compile(&cmd, &vals(&[("env", "dev")]), true).unwrap();
        assert!(
            script.contains(
                "[ -n \"${PULT_EVENTS:-}\" ] && printf 'step %d/%d %s\\n' 1 2 aws:resolve-task >&\"$PULT_EVENTS\" || true"
            ),
            "got: {script}"
        );
        assert!(
            script.contains(
                "[ -n \"${PULT_EVENTS:-}\" ] && printf 'step %d/%d %s\\n' 2 2 'echo done' >&\"$PULT_EVENTS\" || true"
            ),
            "got: {script}"
        );
        // guard for step 1 precedes the function call, guard for step 2
        // precedes the inline echo
        let guard1 = script.find("step %d/%d %s\\n' 1 2").unwrap();
        let call_pos = script.rfind("aws__resolve_task\n").unwrap();
        let guard2 = script.find("step %d/%d %s\\n' 2 2").unwrap();
        let echo_pos = script.find("echo done").unwrap();
        assert!(guard1 < call_pos && call_pos < guard2 && guard2 < echo_pos);
    }

    #[test]
    fn events_true_truncates_multiline_inline_labels_to_first_line() {
        let long = "a".repeat(50);
        let cmd = command(vec![ResolvedEntry::Inline(format!("{long}\nsecond line"))]);
        let script = compile(&cmd, &vals(&[]), true).unwrap();
        // The label is the first line only, truncated to 40 chars — not the
        // full 50-char first line and not the second line. The inline body
        // itself (both lines) still runs unchanged; only the guard's label
        // is truncated.
        let expected_label = "a".repeat(40);
        assert!(
            script.contains(&format!(
                "printf 'step %d/%d %s\\n' 1 1 {expected_label} >&"
            )),
            "got: {script}"
        );
        assert!(script.contains(&long), "full inline body must still run");
        assert!(script.contains("second line"), "second line must still run");
    }

    #[test]
    fn events_true_pipe_label_joins_segments() {
        let cmd = command(vec![ResolvedEntry::Pipe(vec![
            ResolvedSeg::Call {
                name: "list".into(),
                script: "printf 'a\\nb\\n'".into(),
            },
            ResolvedSeg::Inline("tr a-z A-Z".into()),
        ])]);
        let script = compile(&cmd, &vals(&[]), true).unwrap();
        assert!(
            script.contains("printf 'step %d/%d %s\\n' 1 1 'list | tr a-z A-Z'"),
            "got: {script}"
        );
    }
}
