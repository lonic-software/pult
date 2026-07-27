use std::path::Path;

use anyhow::{Context, Result, bail};
use indexmap::IndexMap;

use crate::interp;
use crate::manifest::PickDef;
use crate::mask::Masker;

/// Bundles all three of `resolve_pick`'s value/redaction inputs so a call
/// site can't silently swap two of them — see the field docs and
/// `resolve_pick` below for why that matters.
///
/// All three live here, not just `display_values` and `masker`: an earlier
/// revision bundled only those two on the theory that bundling made a swap a
/// type error, but that was false — `values` stayed a separate, adjacent,
/// same-typed argument next to `redaction`, so `resolve_pick(pick, &values,
/// &redaction, dir)` and `resolve_pick(pick, &display_values, &Redaction {
/// display_values: &values, .. }, dir)` both still compiled, and the second
/// one inverts the fix (pult would execute the masked command and display
/// the real one). Folding `values` in here too makes that swap require named
/// fields — `Redaction { values: &display_values, display_values: &values,
/// masker }` — which is visible at the call site rather than a silent
/// positional transposition. That's the property this type actually
/// provides now; see `resolve_pick` for how each field is used.
pub struct Redaction<'a> {
    /// The real param values — used to build the command that actually
    /// executes.
    pub values: &'a IndexMap<String, String>,
    /// The `redact()`-masked param values — secrets replaced with the fixed,
    /// human-facing `••••••` (issue #7's other layer, `exec.rs`'s `redact`).
    /// Used to build the *display* copy of the interpolated `from:` command
    /// that every error string in this module embeds.
    pub display_values: &'a IndexMap<String, String>,
    /// The run's throwaway `Masker`, built from the same values (see
    /// `exec.rs`'s `fill` call site for why it's throwaway and per-call).
    /// Applied to both `display` (on top of `redact` — catches a secret's
    /// value surfacing through a *different*, non-secret param that
    /// name-based `redact` can't see) and the child's raw stderr (the only
    /// pass that applies there at all — stderr isn't a template, so `redact`
    /// has nothing to substitute into).
    pub masker: &'a Masker,
}

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
    redaction: &Redaction<'_>,
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
    // Interpolate twice: `cmd` (real values) is what actually runs; `display`
    // is what every error string below embeds instead, and gets both of this
    // module's redaction passes — `redact` first (by name: a secret in its
    // own declared slot becomes `••••••`, no length floor, but it only ever
    // looks at params flagged `secret:` and only in their own placeholder),
    // then `redaction.masker` on top (by value: catches that same secret's
    // raw value resurfacing through a *different*, non-secret param — e.g. a
    // URL built from it — which `redact` structurally cannot see, being
    // name-based). Neither pass alone covers both leak shapes, so both run.
    // `redact` clones the values map, so `redaction.values` and
    // `redaction.display_values` have identical key sets, and STRICT-mode
    // `interpolate`'s only error (`unknown parameter`) embeds the raw
    // template, never a value — so this second interpolation can't fail
    // differently from the first, and its own error text can't leak either.
    let cmd = interp::interpolate(from, redaction.values)?;
    let display = redaction
        .masker
        .apply(&interp::interpolate(from, redaction.display_values)?)
        .into_owned();
    let output = crate::shell::sh()?
        .arg("-c")
        .arg(&cmd)
        .current_dir(dir)
        .output()
        .with_context(|| format!("failed to run option source `{display}`"))?;
    if !output.status.success() {
        // `display` above already carries both of this module's passes. The
        // child's stderr is arbitrary text, not a template, so only the
        // value-based pass applies to it — there is no placeholder for
        // `redact` to substitute into. Two markers can legitimately appear
        // in one rendered string: `••••••` (`redact`, name-based, no length
        // floor) inside `display`, and `<redacted>` (`masker`, value-based,
        // `MIN_LEN` floor — see mask.rs's doc for why that floor exists) in
        // either half. That's the existing two-layer doctrine (see mask.rs's
        // doc comments) working as designed — don't unify them into one
        // marker or one pass.
        //
        // Two residual gaps in the value-based half specifically, both to
        // document rather than chase (neither is fixable without real cost
        // — see each):
        //   - `MIN_LEN`: a secret under 6 characters, cross-leaked into a
        //     differently-named param, isn't caught (`redact` doesn't
        //     recognize it there by name; `masker` won't touch anything
        //     that short). Fixing it would mean giving `redact` a floor
        //     too, or removing `masker`'s — don't.
        //   - Quote-mangling: `interp::interpolate` shell-quotes every
        //     substituted value, and `shell_quote` (interp.rs) rewrites a
        //     `'` inside a quoted value to `'\''`. A secret containing a
        //     single quote, cross-leaked into a non-secret param, is no
        //     longer a contiguous copy of itself by the time it's embedded
        //     here — the needle `masker` built from the *unquoted* secret
        //     can't match text that's since been split by an escape
        //     sequence, so it leaks through this exact `display` route,
        //     mangled but present. Fixing it would mean `masker` tracking
        //     every secret's *quoted* form too (which quoting applies
        //     depends on every other char in the value, so this is a
        //     moving target, not a fixed second needle) — not attempted
        //     here.
        // Mask *before* trimming, not after: `Masker`'s needle is the raw
        // secret value, trailing whitespace and all (a pasted credential
        // picks up trailing whitespace constantly), and the child's own
        // trailing newline sits right after it in the captured stderr.
        // Trimming first would strip exactly the whitespace the needle
        // needs to match, so `apply` would find nothing — the #12 failure
        // mode again, for a very ordinary input.
        let raw_stderr = String::from_utf8_lossy(&output.stderr);
        let masked_stderr = redaction.masker.apply(&raw_stderr);
        let stderr = masked_stderr.trim_end();
        bail!(
            "option source `{display}` failed ({}):\n{}",
            output.status,
            stderr
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut options = Vec::new();
    for line in stdout.lines() {
        if let Some(opt) = parse_dynamic_line(line, &display)? {
            options.push(opt);
        }
    }
    if options.is_empty() {
        bail!("option source `{display}` produced no options");
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

    /// A no-op `Redaction` for tests that aren't exercising secret handling:
    /// the display map is the same map as the real one (nothing to redact),
    /// and the masker has no needles loaded.
    fn no_redaction<'a>(values: &'a IndexMap<String, String>, masker: &'a Masker) -> Redaction<'a> {
        Redaction {
            values,
            display_values: values,
            masker,
        }
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
        let vals = IndexMap::new();
        let masker = Masker::new(Vec::<String>::new());
        let opts = resolve_pick(&pick, &no_redaction(&vals, &masker), Path::new(".")).unwrap();
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
        let vals = IndexMap::new();
        let masker = Masker::new(Vec::<String>::new());
        let opts = resolve_pick(&pick, &no_redaction(&vals, &masker), Path::new(".")).unwrap();
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
        let vals = IndexMap::new();
        let masker = Masker::new(Vec::<String>::new());
        let opts = resolve_pick(&pick, &no_redaction(&vals, &masker), Path::new(".")).unwrap();
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
        let vals = IndexMap::new();
        let masker = Masker::new(Vec::<String>::new());
        let opts = resolve_pick(&pick, &no_redaction(&vals, &masker), Path::new(".")).unwrap();
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
        let vals = IndexMap::new();
        let masker = Masker::new(Vec::<String>::new());
        let opts = resolve_pick(&pick, &no_redaction(&vals, &masker), Path::new(".")).unwrap();
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
        let vals = IndexMap::new();
        let masker = Masker::new(Vec::<String>::new());
        let err = resolve_pick(&pick, &no_redaction(&vals, &masker), Path::new("."))
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
        let masker = Masker::new(Vec::<String>::new());
        let opts = resolve_pick(&pick, &no_redaction(&vals, &masker), Path::new(".")).unwrap();
        assert_eq!(values(&opts), ["dev-customer"]);
    }

    /// Kept unchanged (empty masker, no redaction) from before this PR — it
    /// now doubles as the guard that masking never *suppresses* stderr text.
    /// A masker with no needles must never remove "boom" from the error;
    /// only an actual secret match should ever replace text here.
    #[test]
    fn failing_source_reports_stderr() {
        let pick = PickDef {
            options: None,
            from: Some("echo boom >&2; exit 3".into()),
        };
        let vals = IndexMap::new();
        let masker = Masker::new(Vec::<String>::new());
        let err = resolve_pick(&pick, &no_redaction(&vals, &masker), Path::new("."))
            .unwrap_err()
            .to_string();
        assert!(err.contains("boom"), "got: {err}");
    }

    /// Adversarial-review finding (round 3): `Masker`'s needle is the secret
    /// value exactly as stored, trailing whitespace included — a pasted
    /// credential picks up trailing whitespace constantly. Trimming the
    /// child's stderr *before* masking (the bug this reddens against) strips
    /// exactly the whitespace the needle requires, so `apply` matches
    /// nothing and the secret prints in cleartext — the #12 failure mode
    /// again, for an ordinary input. Masking must run on the untrimmed
    /// stderr; only the *result* gets trimmed. Redden by reordering back to
    /// `masker.apply(raw_stderr.trim_end())`.
    #[test]
    fn stderr_masks_a_secret_with_trailing_whitespace() {
        let secret_core = "hunter2-super-secret";
        let secret = format!("{secret_core} "); // trailing space, deliberately
        let pick = PickDef {
            options: None,
            from: Some(format!("printf 'tok={secret}\\n' >&2; exit 1")),
        };
        let vals = IndexMap::new();
        let masker = Masker::new([secret]);
        let redaction = Redaction {
            values: &vals,
            display_values: &vals,
            masker: &masker,
        };
        let err = resolve_pick(&pick, &redaction, Path::new("."))
            .unwrap_err()
            .to_string();
        assert!(!err.contains(secret_core), "leaked: {err}");
        assert!(err.contains("<redacted>"), "got: {err}");
    }

    /// Complement to `exec.rs`'s `fill`-level regression test: pins the
    /// *success* direction of the two-map split. `from:` embeds the real
    /// token in the command that actually runs (the display map only
    /// affects error text), so the returned option value must be the real
    /// one, never `••••••` — proving execution didn't accidentally pick up
    /// the masked/display command instead of the real one.
    #[test]
    fn execution_uses_the_real_map_not_the_display_map() {
        let pick = PickDef {
            options: None,
            from: Some("echo {token}".into()),
        };
        let mut real = IndexMap::new();
        real.insert("token".to_string(), "hunter2-secret".to_string());
        let mut display = IndexMap::new();
        display.insert("token".to_string(), "••••••".to_string());
        let masker = Masker::new(Vec::<String>::new());
        let redaction = Redaction {
            values: &real,
            display_values: &display,
            masker: &masker,
        };
        let opts = resolve_pick(&pick, &redaction, Path::new(".")).unwrap();
        assert_eq!(values(&opts), ["hunter2-secret"]);
    }
}
