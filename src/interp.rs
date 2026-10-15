use anyhow::{Context, Result, bail};
use indexmap::IndexMap;

use crate::value::Words;

/// Substitute `{name}` placeholders with `words` — each value already
/// rendered as shell text (see `value::words`), inserted as is. STRICT mode,
/// for single-line templates (plain `run:` strings, `pick.from` sources):
/// unknown placeholders and unclosed braces are errors; `{{`/`}}` escape.
pub fn interpolate(template: &str, words: &Words) -> Result<String> {
    let mut out = String::with_capacity(template.len());
    walk(template, |part| {
        match part {
            Part::Literal(s) => out.push_str(s),
            Part::Placeholder(name) => {
                let v = words
                    .get(name)
                    .with_context(|| format!("unknown parameter `{{{name}}}` in `{template}`"))?;
                out.push_str(v);
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
/// substitutes `{name}` with its rendered `words` entry only when `name` is a
/// known value; everything else — `${TASK+x}`, `awk '{print $1}'`, unmatched
/// braces — passes through untouched. Never errors.
pub fn interpolate_lenient(script: &str, words: &Words) -> String {
    let mut out = String::with_capacity(script.len());
    let mut last = 0;
    scan_script(script, |name, start, end| {
        if let Some(v) = words.get(name) {
            out.push_str(&script[last..start]);
            out.push_str(v);
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
/// through untouched. `vars` are inserted raw — they are compose-time
/// constants authored in the manifest, and may deliberately be shell.
///
/// `paths` (`module.dir`) are data, not shell: a module cached under
/// `C:\Users\Máté Kolonics\…` must still reach its `bin/` as one word.
/// With `shell` set, each is quoted for the shell context it lands in
/// ([`shell_contexts`]), so `${module.dir}/bin/x`, `"${module.dir}/bin/x"`
/// and a heredoc line all keep the path intact. Without it (prose, option
/// values, `with:`-free value fields) the path goes in as is. One pass, so
/// nothing substituted is ever rescanned.
pub fn substitute_vars(
    template: &str,
    vars: &IndexMap<String, String>,
    paths: &IndexMap<String, String>,
    shell: bool,
) -> String {
    let contexts = if shell {
        shell_contexts(template)
    } else {
        Vec::new()
    };
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
            let value = match (vars.get(name), paths.get(name)) {
                (Some(v), _) => Some(v.clone()),
                (None, Some(p)) if shell => Some(quote_for(p, contexts[i])),
                (None, Some(p)) => Some(p.clone()),
                (None, None) => None,
            };
            if let Some(v) = value {
                out.push_str(&template[last..i]);
                out.push_str(&v);
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

/// The shell quoting in effect at a byte of a script.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellContext {
    /// Unquoted (top level, inside `$(…)` or backticks).
    Bare,
    /// Inside `"…"`.
    Double,
    /// Inside `'…'`.
    Single,
    /// Body of a heredoc with an unquoted delimiter: expansions happen, `"`
    /// is ordinary.
    Heredoc,
    /// A comment or a quoted-delimiter heredoc body: nothing is interpreted.
    Literal,
}

/// Insert `value` so the shell reads it back verbatim in context `ctx`.
fn quote_for(value: &str, ctx: ShellContext) -> String {
    let escape = |specials: &str| {
        let mut out = String::with_capacity(value.len());
        for c in value.chars() {
            if specials.contains(c) {
                out.push('\\');
            }
            out.push(c);
        }
        out
    };
    match ctx {
        ShellContext::Bare => shell_quote(value),
        ShellContext::Double => escape("\\\"$`"),
        ShellContext::Heredoc => escape("\\$`"),
        ShellContext::Single => value.replace('\'', r"'\''"),
        ShellContext::Literal => value.to_string(),
    }
}

/// The [`ShellContext`] at every byte of `script` (plus one past the end), as
/// sh would read it. A lexer, not a parser: it tracks exactly what decides
/// how a spliced word must be quoted — quotes, `\` escapes, `$(…)` and
/// backtick nesting, `#` comments and heredoc bodies — and nothing else. An
/// apostrophe in a comment or heredoc must not flip the rest of the script
/// into "single-quoted", which is why those two are tracked at all.
pub fn shell_contexts(script: &str) -> Vec<ShellContext> {
    #[derive(Clone, Copy, PartialEq)]
    enum Frame {
        /// Unquoted; `parens` counts `(` open inside a `$(…)`.
        Bare {
            parens: u32,
        },
        Double,
        Backtick,
    }
    struct Heredoc {
        delim: String,
        strip_tabs: bool,
        expand: bool,
    }

    let b = script.as_bytes();
    let n = b.len();
    let mut ctx = vec![ShellContext::Bare; n + 1];
    let mut stack = vec![Frame::Bare { parens: 0 }];
    let mut pending: Vec<Heredoc> = Vec::new();
    let mut i = 0;
    while i < n {
        let top = *stack.last().expect("root frame is never popped");
        let c = b[i];
        if top == Frame::Double {
            ctx[i] = ShellContext::Double;
            match c {
                b'\\' => {
                    if i + 1 < n {
                        ctx[i + 1] = ShellContext::Double;
                    }
                    i += 2;
                    continue;
                }
                b'"' => {
                    stack.pop();
                }
                b'`' => stack.push(Frame::Backtick),
                b'$' if b.get(i + 1) == Some(&b'(') => {
                    stack.push(Frame::Bare { parens: 0 });
                    i += 2;
                    continue;
                }
                _ => {}
            }
            i += 1;
            continue;
        }

        // Unquoted: the root, a `$(…)`, or backticks.
        ctx[i] = ShellContext::Bare;
        match c {
            b'\\' => {
                i += 2;
                continue;
            }
            b'\'' => {
                let close = script[i + 1..].find('\'').map_or(n, |o| i + 1 + o);
                ctx[i + 1..close].fill(ShellContext::Single);
                i = close + 1;
                continue;
            }
            b'"' => stack.push(Frame::Double),
            b'`' if top == Frame::Backtick => {
                stack.pop();
            }
            b'`' => stack.push(Frame::Backtick),
            b'$' if b.get(i + 1) == Some(&b'(') => {
                stack.push(Frame::Bare { parens: 0 });
                i += 2;
                continue;
            }
            b'(' if stack.len() > 1 => {
                if let Some(Frame::Bare { parens }) = stack.last_mut() {
                    *parens += 1;
                }
            }
            b')' if stack.len() > 1 => match stack.last_mut() {
                Some(Frame::Bare { parens: 0 }) => {
                    stack.pop();
                }
                Some(Frame::Bare { parens }) => *parens -= 1,
                _ => {}
            },
            b'#' if i == 0 || b" \t\n;&|(".contains(&b[i - 1]) => {
                let end = script[i..].find('\n').map_or(n, |o| i + o);
                ctx[i..end].fill(ShellContext::Literal);
                i = end;
                continue;
            }
            b'<' if b.get(i + 1) == Some(&b'<') && b.get(i + 2) != Some(&b'<') => {
                let mut j = i + 2;
                let strip_tabs = b.get(j) == Some(&b'-');
                if strip_tabs {
                    j += 1;
                }
                while j < n && (b[j] == b' ' || b[j] == b'\t') {
                    j += 1;
                }
                let start = j;
                while j < n && !b" \t\n;&|<>()".contains(&b[j]) {
                    j += 1;
                }
                let word = &script[start..j];
                if !word.is_empty() {
                    pending.push(Heredoc {
                        delim: word.replace(['\'', '"', '\\'], ""),
                        strip_tabs,
                        expand: !word.contains(['\'', '"', '\\']),
                    });
                }
                i = j;
                continue;
            }
            b'\n' if !pending.is_empty() => {
                i += 1;
                // Each pending heredoc's body follows, in order, line by line.
                for doc in pending.drain(..) {
                    while i < n {
                        let end = script[i..].find('\n').map_or(n, |o| i + o);
                        let line = &script[i..end];
                        let line = if doc.strip_tabs {
                            line.trim_start_matches('\t')
                        } else {
                            line
                        };
                        if line == doc.delim {
                            ctx[i..end].fill(ShellContext::Literal);
                            i = end + 1;
                            break;
                        }
                        ctx[i..end].fill(if doc.expand {
                            ShellContext::Heredoc
                        } else {
                            ShellContext::Literal
                        });
                        i = end + 1;
                    }
                }
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    ctx
}

/// `path` as a shell on Windows wants it: `canonicalize`'s verbatim prefix
/// (`\\?\C:\…`, `\\?\UNC\server\…`) dropped and `\` turned into `/`, giving
/// `C:/Users/…` — which Git for Windows's `sh` executes as is, and which
/// can't be mangled by a stray `\` escape. Pure and string-based so it's
/// testable on every host; only called on Windows.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn windows_shell_path(path: &str) -> String {
    let path = if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else {
        path.strip_prefix(r"\\?\").unwrap_or(path).to_string()
    };
    path.replace('\\', "/")
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

    use crate::value::text_words as words;

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
            &words(&[("customer", "demo-leeds"), ("env", "dev")]),
        )
        .unwrap();
        assert_eq!(out, "./impl shell demo-leeds dev");
    }

    #[test]
    fn quotes_unsafe_values() {
        let out = interpolate("echo {msg}", &words(&[("msg", "hi; rm -rf /")])).unwrap();
        assert_eq!(out, r"echo 'hi; rm -rf /'");
    }

    #[test]
    fn double_braces_are_literal() {
        let out = interpolate("echo {{literal}} {x}", &words(&[("x", "v")])).unwrap();
        assert_eq!(out, "echo {literal} v");
    }

    #[test]
    fn unknown_placeholder_errors_in_strict_mode() {
        let err = interpolate("echo {nope}", &words(&[]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("nope"));
    }

    #[test]
    fn unclosed_brace_errors_in_strict_mode() {
        assert!(interpolate("echo {oops", &words(&[])).is_err());
    }

    #[test]
    fn lists_placeholders() {
        let names = placeholders("a {x} b {{skip}} {y}").unwrap();
        assert_eq!(names, ["x", "y"]);
    }

    // ── lenient mode ──

    #[test]
    fn lenient_substitutes_known_params_only() {
        let out = interpolate_lenient("deploy {env} {unknown}", &words(&[("env", "dev")]));
        assert_eq!(out, "deploy dev {unknown}");
    }

    #[test]
    fn lenient_leaves_shell_expansions_alone() {
        let script = r#"[ "${TASK+x}" ] && echo "${TASK}""#;
        let out = interpolate_lenient(script, &words(&[("TASK", "nope")]));
        assert_eq!(out, script, "shell ${{…}} must never be treated as a param");
    }

    #[test]
    fn lenient_leaves_awk_and_groups_alone() {
        let script = "ps aux | awk '{print $1}' | while read u; do { echo $u; }; done";
        let out = interpolate_lenient(script, &words(&[("env", "dev")]));
        assert_eq!(out, script);
    }

    #[test]
    fn lenient_quotes_substituted_values() {
        let out = interpolate_lenient("echo {msg}", &words(&[("msg", "a b")]));
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
            &vals(&[("prefix", "dirconn")]),
            &vals(&[("module.dir", "/mods/aws")]),
            true,
        );
        assert_eq!(
            out,
            "cluster dirconn-{env}; [ \"${TASK+x}\" ]; /mods/aws/bin/x"
        );
    }

    #[test]
    fn vars_leave_plain_shell_dollars() {
        let out = substitute_vars("echo $HOME ${undeclared}", &vals(&[]), &vals(&[]), true);
        assert_eq!(out, "echo $HOME ${undeclared}");
    }
    fn dir(script: &str, path: &str) -> String {
        substitute_vars(script, &vals(&[]), &vals(&[("module.dir", path)]), true)
    }

    #[test]
    fn module_dir_with_a_space_stays_one_word_bare() {
        let out = dir("${module.dir}/bin/words --all", "/Users/Máté Kolonics/m");
        assert_eq!(out, "'/Users/Máté Kolonics/m'/bin/words --all");
    }

    #[test]
    fn module_dir_inside_double_quotes_is_escaped_not_requoted() {
        let out = dir(
            r#"x="$(cat "${module.dir}/who.txt")" "${module.dir}""#,
            r#"/a b/$x"#,
        );
        assert_eq!(out, r#"x="$(cat "/a b/\$x/who.txt")" "/a b/\$x""#);
    }

    #[test]
    fn module_dir_inside_single_quotes_closes_and_reopens() {
        assert_eq!(dir("echo '${module.dir}'", "/it's"), r"echo '/it'\''s'");
    }

    #[test]
    fn module_dir_in_command_substitution_is_bare() {
        let out = dir("T=$(${module.dir}/bin/x (y))\n${module.dir}", "/a b");
        assert_eq!(out, "T=$('/a b'/bin/x (y))\n'/a b'");
    }

    #[test]
    fn apostrophes_in_comments_and_heredocs_dont_flip_quoting() {
        let script = "# don't\ncat <<EOF\nit's ${module.dir}\nEOF\ncat <<'X'\n${module.dir}\nX\n${module.dir}/b";
        assert_eq!(
            dir(script, "/a b"),
            "# don't\ncat <<EOF\nit's /a b\nEOF\ncat <<'X'\n/a b\nX\n'/a b'/b"
        );
    }

    #[test]
    fn safe_module_dir_is_unchanged() {
        assert_eq!(dir("${module.dir}/bin/x", "/mods/aws"), "/mods/aws/bin/x");
    }

    #[test]
    fn module_dir_is_raw_outside_shell_fields() {
        let out = substitute_vars(
            "in ${module.dir}",
            &vals(&[]),
            &vals(&[("module.dir", "/a b")]),
            false,
        );
        assert_eq!(out, "in /a b");
    }

    #[test]
    fn windows_paths_lose_the_verbatim_prefix_and_backslashes() {
        assert_eq!(
            windows_shell_path(r"\\?\C:\Users\Máté Kolonics\.cache\pult"),
            "C:/Users/Máté Kolonics/.cache/pult"
        );
        assert_eq!(windows_shell_path(r"\\?\UNC\srv\share\m"), "//srv/share/m");
        assert_eq!(windows_shell_path(r"D:\m"), "D:/m");
    }
}
