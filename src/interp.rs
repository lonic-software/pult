use anyhow::{Context, Result, bail};
use indexmap::IndexMap;

/// Substitute `{name}` placeholders with shell-quoted values — STRICT mode,
/// for single-line templates (plain `run:` strings, `pick.from` sources):
/// unknown placeholders and unclosed braces are errors; `{{`/`}}` escape.
pub fn interpolate(template: &str, values: &IndexMap<String, String>) -> Result<String> {
    let mut out = String::with_capacity(template.len());
    walk(template, |part| {
        match part {
            Part::Literal(s) => out.push_str(s),
            Part::Placeholder(name) => {
                let v = values
                    .get(name)
                    .with_context(|| format!("unknown parameter `{{{name}}}` in `{template}`"))?;
                out.push_str(&shell_quote(v));
            }
        }
        Ok(())
    })?;
    Ok(out)
}

/// The placeholder names a STRICT template references, in order of appearance.
pub fn placeholders(template: &str) -> Result<Vec<String>> {
    let mut names = Vec::new();
    walk(template, |part| {
        if let Part::Placeholder(name) = part {
            names.push(name.to_string());
        }
        Ok(())
    })?;
    Ok(names)
}

enum Part<'a> {
    Literal(&'a str),
    Placeholder(&'a str),
}

fn walk<'a>(template: &'a str, mut f: impl FnMut(Part<'a>) -> Result<()>) -> Result<()> {
    let bytes = template.as_bytes();
    let mut i = 0;
    let mut lit_start = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'{' if bytes.get(i + 1) == Some(&b'{') => {
                f(Part::Literal(&template[lit_start..i + 1]))?;
                i += 2;
                lit_start = i;
            }
            b'}' if bytes.get(i + 1) == Some(&b'}') => {
                f(Part::Literal(&template[lit_start..i + 1]))?;
                i += 2;
                lit_start = i;
            }
            b'{' => {
                f(Part::Literal(&template[lit_start..i]))?;
                let end = template[i + 1..]
                    .find('}')
                    .map(|off| i + 1 + off)
                    .with_context(|| format!("unclosed `{{` in template `{template}`"))?;
                let name = &template[i + 1..end];
                if name.is_empty() {
                    bail!("empty `{{}}` placeholder in template `{template}`");
                }
                f(Part::Placeholder(name))?;
                i = end + 1;
                lit_start = i;
            }
            _ => i += 1,
        }
    }
    f(Part::Literal(&template[lit_start..]))?;
    Ok(())
}

/// Can this brace content be a pult placeholder / var name?
fn is_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
}

/// Scan a SCRIPT (lenient domain) and call `f(name, start, end)` for each
/// candidate placeholder: `{name}` where name is name-shaped and the brace is
/// not preceded by `$` (that would be shell `${…}` syntax). `start..end` spans
/// the braces inclusive. Everything else — awk blocks, brace groups, shell
/// expansions — is not a placeholder.
fn scan_script(script: &str, mut f: impl FnMut(&str, usize, usize)) {
    let bytes = script.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{'
            && (i == 0 || bytes[i - 1] != b'$')
            && let Some(off) = script[i + 1..].find('}')
        {
            let end = i + 1 + off;
            let name = &script[i + 1..end];
            if is_name(name) {
                f(name, i, end + 1);
                i = end + 1;
                continue;
            }
        }
        i += 1;
    }
}

/// LENIENT interpolation for step scripts and inline run-list fragments:
/// substitutes `{name}` (shell-quoted) only when `name` is a known value;
/// everything else — `${TASK+x}`, `awk '{print $1}'`, unmatched braces —
/// passes through untouched. Never errors.
pub fn interpolate_lenient(script: &str, values: &IndexMap<String, String>) -> String {
    let mut out = String::with_capacity(script.len());
    let mut last = 0;
    scan_script(script, |name, start, end| {
        if let Some(v) = values.get(name) {
            out.push_str(&script[last..start]);
            out.push_str(&shell_quote(v));
            last = end;
        }
    });
    out.push_str(&script[last..]);
    out
}

/// Candidate placeholder names in a script (lenient scan), deduplicated.
pub fn scan_placeholders(script: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    scan_script(script, |name, _, _| {
        if !names.iter().any(|n| n == name) {
            names.push(name.to_string());
        }
    });
    names
}

/// Rename placeholders in a script per `map` (for `with:` rebinding): `{k}`
/// becomes the mapped RAW text (which may itself contain placeholders, to be
/// resolved at run time). Unmapped placeholders are left as-is.
pub fn rename_placeholders(script: &str, map: &IndexMap<String, String>) -> String {
    let mut out = String::with_capacity(script.len());
    let mut last = 0;
    scan_script(script, |name, start, end| {
        if let Some(replacement) = map.get(name) {
            out.push_str(&script[last..start]);
            out.push_str(replacement);
            last = end;
        }
    });
    out.push_str(&script[last..]);
    out
}

/// Load-time `${var}` substitution (module vars + `module.dir`): replaces
/// `${name}` only when `name` is declared; any other `$…` is shell and passes
/// through untouched. Values are inserted raw — they are compose-time
/// constants authored in the manifest, not runtime data.
pub fn substitute_vars(template: &str, vars: &IndexMap<String, String>) -> String {
    let bytes = template.as_bytes();
    let mut out = String::with_capacity(template.len());
    let mut i = 0;
    let mut last = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'$'
            && bytes[i + 1] == b'{'
            && let Some(off) = template[i + 2..].find('}')
        {
            let end = i + 2 + off;
            let name = &template[i + 2..end];
            if let Some(v) = vars.get(name) {
                out.push_str(&template[last..i]);
                out.push_str(v);
                i = end + 1;
                last = i;
                continue;
            }
        }
        i += 1;
    }
    out.push_str(&template[last..]);
    out
}

/// Quote a value for use in `sh -c`. Values that are plainly safe pass
/// through unquoted so command lines stay readable.
pub fn shell_quote(s: &str) -> String {
    let safe = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-./:@%+=,".contains(c));
    if safe {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vals(pairs: &[(&str, &str)]) -> IndexMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn substitutes_in_order() {
        let out = interpolate(
            "./impl shell {customer} {env}",
            &vals(&[("customer", "demo-leeds"), ("env", "dev")]),
        )
        .unwrap();
        assert_eq!(out, "./impl shell demo-leeds dev");
    }

    #[test]
    fn quotes_unsafe_values() {
        let out = interpolate("echo {msg}", &vals(&[("msg", "hi; rm -rf /")])).unwrap();
        assert_eq!(out, r"echo 'hi; rm -rf /'");
    }

    #[test]
    fn double_braces_are_literal() {
        let out = interpolate("echo {{literal}} {x}", &vals(&[("x", "v")])).unwrap();
        assert_eq!(out, "echo {literal} v");
    }

    #[test]
    fn unknown_placeholder_errors_in_strict_mode() {
        let err = interpolate("echo {nope}", &vals(&[]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("nope"));
    }

    #[test]
    fn unclosed_brace_errors_in_strict_mode() {
        assert!(interpolate("echo {oops", &vals(&[])).is_err());
    }

    #[test]
    fn lists_placeholders() {
        let names = placeholders("a {x} b {{skip}} {y}").unwrap();
        assert_eq!(names, ["x", "y"]);
    }

    // ── lenient mode ──

    #[test]
    fn lenient_substitutes_known_params_only() {
        let out = interpolate_lenient("deploy {env} {unknown}", &vals(&[("env", "dev")]));
        assert_eq!(out, "deploy dev {unknown}");
    }

    #[test]
    fn lenient_leaves_shell_expansions_alone() {
        let script = r#"[ "${TASK+x}" ] && echo "${TASK}""#;
        let out = interpolate_lenient(script, &vals(&[("TASK", "nope")]));
        assert_eq!(out, script, "shell ${{…}} must never be treated as a param");
    }

    #[test]
    fn lenient_leaves_awk_and_groups_alone() {
        let script = "ps aux | awk '{print $1}' | while read u; do { echo $u; }; done";
        let out = interpolate_lenient(script, &vals(&[("env", "dev")]));
        assert_eq!(out, script);
    }

    #[test]
    fn lenient_quotes_substituted_values() {
        let out = interpolate_lenient("echo {msg}", &vals(&[("msg", "a b")]));
        assert_eq!(out, "echo 'a b'");
    }

    #[test]
    fn scan_finds_candidates_not_shell() {
        let names = scan_placeholders("x {env} ${HOME} {print $1} {env} {svc}");
        assert_eq!(names, ["env", "svc"]);
    }

    #[test]
    fn rename_rebinds_and_leaves_rest() {
        let out = rename_placeholders(
            "login {env} keep {other}",
            &vals(&[("env", "{target_env}")]),
        );
        assert_eq!(out, "login {target_env} keep {other}");
    }

    // ── ${var} substitution ──

    #[test]
    fn vars_substitute_declared_only() {
        let out = substitute_vars(
            "cluster ${prefix}-{env}; [ \"${TASK+x}\" ]; ${module.dir}/bin/x",
            &vals(&[("prefix", "dirconn"), ("module.dir", "/mods/aws")]),
        );
        assert_eq!(
            out,
            "cluster dirconn-{env}; [ \"${TASK+x}\" ]; /mods/aws/bin/x"
        );
    }

    #[test]
    fn vars_leave_plain_shell_dollars() {
        let out = substitute_vars("echo $HOME ${undeclared}", &vals(&[]));
        assert_eq!(out, "echo $HOME ${undeclared}");
    }
}
