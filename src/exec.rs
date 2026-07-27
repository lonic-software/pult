use std::collections::HashMap;
use std::path::Path;

use anyhow::{Result, bail};
use indexmap::IndexMap;

use crate::journal;
use crate::label;
use crate::manifest::{ParamDef, ParamKind};
use crate::mask::{self, Masker};
use crate::resolver::{Resolved, ResolvedCommand, ResolvedRun};
use crate::runner::Journaling;
use crate::{compile, interp, options, prompt, runner, shell, trust};

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
    run_id: Option<&str>,
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

    // Resolve the shell this run will need before the journal below is
    // opened, not after: `run_sh`/`run_bash` call `shell::sh()`/`bash()`
    // internally too, but by then a journal may already be open, and their
    // `?` returns out of `execute` before ever reaching `runner::execute`'s
    // `journal.finish(...)` calls — the `Journal` (no `Drop` impl) would
    // just be silently dropped, orphaning its `meta.json` at
    // `status: "running"` forever. Only reachable on Windows (unix
    // resolution is infallible) and dormant until journaling reaches
    // non-unix targets (#8), but that's exactly what this PR anticipates.
    // Resolving here instead closes the window by construction: a failure
    // now means no journal was ever opened to orphan. `shell::sh()`/
    // `bash()` memoize their result, so the resolution `run_sh`/`run_bash`
    // repeat below is guaranteed to replay this same (already-succeeded)
    // outcome, never a fresh OS call that could disagree.
    match &cmd.run {
        ResolvedRun::Script(_) => {
            shell::sh()?;
        }
        ResolvedRun::Steps(_) => {
            shell::bash()?;
        }
    }

    // Open the run journal (see journal.rs). Ephemeral `pult x` sources are
    // skipped — no durable repo identity to journal under. The journal's
    // param audit record gets `<redacted>` for secrets (the spec's literal,
    // machine-recognizable marker — distinct from the `••••••` display mask
    // below, which is a human-facing rendering).
    let journaling = if resolved.ephemeral {
        Journaling::Off
    } else {
        // Post-`fill`, so an interactively prompted secret (not just one
        // supplied on the command line) is covered — `fill` above already
        // ran before this point.
        let (masker, unmaskable) = build_masker(cmd, &values);
        let journal = journal::Journal::start(journal::StartInfo {
            repo_dir: &resolved.dir,
            manifest: &resolved.path,
            command_id: &cmd.id,
            command_title: &cmd.title,
            params: journal_params(cmd, &values),
            interactive: cmd.interactive,
            run_id,
            masker: masker.clone(),
        });
        match journal {
            Some(j) if cmd.interactive => Journaling::MetaOnly(j),
            Some(j) => {
                // Warn only here: masking only ever happens on the `Full`
                // path (non-interactive, successfully journaled). Warning
                // on `MetaOnly` (interactive — the child owns stdio, pult
                // never sees output to mask) or `Off`/no-journal
                // (`PULT_JOURNAL=0`, no state dir, non-unix) would tell the
                // user a secret is "too short to mask" on a run where
                // nothing is ever masked regardless of its length.
                warn_unmaskable(&unmaskable);
                Journaling::Full(j, masker)
            }
            None => Journaling::Off,
        }
    };

    match &cmd.run {
        ResolvedRun::Script(template) => {
            // The `running:` banner (and any error message) shows a line with
            // secret values masked — the real one exists only in the child's argv.
            let display = interp::interpolate(template, &redact(cmd, &values))?;
            runner::run_sh(
                &interp::interpolate(template, &values)?,
                &display,
                &resolved.run_dir,
                journaling,
            )
        }
        ResolvedRun::Steps(_) => runner::run_bash(
            &compile::compile(cmd, &values, true)?,
            &cmd.id,
            &resolved.run_dir,
            journaling,
        ),
    }
}

/// Whether `def` is a `secret: true` input — the one condition
/// `journal_params`, `redact`, and `build_masker` all need. Unified here so
/// issue #6 (masking values discovered mid-run, not just supplied up front)
/// only has to extend this one place instead of three.
fn is_secret(def: &ParamDef) -> bool {
    matches!(def.kind(), ParamKind::Input(input) if input.secret)
}

/// `values` for the journal's audit record: secrets as the spec's literal
/// `"<redacted>"` — the value itself never reaches the journal module.
fn journal_params(
    cmd: &ResolvedCommand,
    values: &IndexMap<String, String>,
) -> IndexMap<String, String> {
    let mut out = values.clone();
    for (name, def) in &cmd.params {
        if is_secret(def)
            && let Some(v) = out.get_mut(name)
        {
            *v = "<redacted>".to_string();
        }
    }
    out
}

/// `values` with every `secret: true` param replaced by a fixed mask (fixed so
/// the mask doesn't leak the value's length).
fn redact(cmd: &ResolvedCommand, values: &IndexMap<String, String>) -> IndexMap<String, String> {
    let mut out = values.clone();
    for (name, def) in &cmd.params {
        if is_secret(def)
            && let Some(v) = out.get_mut(name)
        {
            *v = "••••••".to_string();
        }
    }
    out
}

/// Build the run's `Masker` from post-`fill` secret values, returning
/// alongside it the name of every secret `Masker::from_named` reports as
/// producing no maskable segment at all. Deliberately does *not* decide for
/// itself which values are "too short" — that would be exactly the drift
/// this fixes (`build_masker` used to run its own whole-value length check,
/// a different rule from what `Masker::from_named` actually splits and
/// filters per line; a multi-line value whose every individual line was too
/// short passed the whole-value check while masking nothing). One rule,
/// derived from what construction actually did; the caller (`execute`,
/// above) decides *whether* to warn from it.
fn build_masker(cmd: &ResolvedCommand, values: &IndexMap<String, String>) -> (Masker, Vec<String>) {
    let secrets: Vec<(String, String)> = cmd
        .params
        .iter()
        .filter(|(_, def)| is_secret(def))
        .filter_map(|(name, _)| values.get(name).map(|v| (name.clone(), v.clone())))
        .collect();
    Masker::from_named(secrets)
}

/// Cheap, honest, no knob (see `mask::MIN_LEN`'s doc comment for why there's
/// a floor at all): one line per secret `build_masker` reports as unmaskable.
fn warn_unmaskable(names: &[String]) {
    for name in names {
        eprintln!(
            "pult: warning: secret `{name}` is too short to mask in output \
             (under {} characters)",
            mask::MIN_LEN
        );
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
                // Validate against static options — values only, never
                // labels/descriptions; dynamic sources accept any value so
                // direct invocation stays fast and scriptable.
                if let Some(opts) = &pick.options
                    && !opts.iter().any(|o| o.value() == v)
                {
                    let values: Vec<&str> =
                        opts.iter().map(crate::manifest::OptionDef::value).collect();
                    bail!(
                        "invalid value `{v}` for `{name}` (expected one of: {})",
                        values.join(", ")
                    );
                }
                v.clone()
            }
            // A provided secret is used for execution only — preview text
            // (`--print`, the ephemeral trust prompt) shows the slot instead,
            // so a secret passed on the command line is never printed back.
            (Some(_), ParamKind::Input(input)) if input.secret && run_dir.is_none() => {
                format!("<{name}>")
            }
            (Some(v), ParamKind::Input(_)) => v.clone(),
            // Preview: show the slot instead of prompting or shelling out.
            (None, _) if run_dir.is_none() => format!("<{name}>"),
            (None, ParamKind::Pick(pick)) => {
                // A throwaway `Redaction`, rebuilt from just the values
                // answered so far (never the run's own masker — there isn't
                // one yet: `build_masker` runs post-`fill`, in `execute`
                // above, but `resolve_pick` runs *inside* `fill`, before that
                // point exists). By the ordering rule a `from:` can only
                // reference params declared earlier, so "answered so far" is
                // exactly the complete needle set for this one `from:` —
                // nothing it could reference is missing. Both `redact` and
                // `build_masker` already tolerate a partial map (they look
                // values up by name and skip absent ones), so this is safe
                // per-call, not just at the end of `fill`.
                let display_values = redact(cmd, &values);
                let (masker, _unmaskable) = build_masker(cmd, &values);
                // Discarding `unmaskable` here (rather than warning) is
                // deliberate: warning on every pick resolution would be
                // noise, and the real run-level warning already fires later,
                // from the run's own masker built in `execute` above — gated
                // on `Journaling::Full`, which is where masking actually
                // matters.
                let redaction = options::Redaction {
                    display_values: &display_values,
                    masker: &masker,
                };
                let opts = options::resolve_pick(pick, &values, &redaction, run_dir.unwrap())?;
                let w = label::width();
                let labels: Vec<String> = opts.iter().map(|o| label::option_label(o, w)).collect();
                let i = prompt::select_index(&format!("{name}?"), labels)?;
                pick_selection(&opts, i)
            }
            (None, ParamKind::Input(input)) if input.secret => {
                prompt::password(&format!("{name}?"))?
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

/// Map a picker's chosen index back to the value that reaches the command —
/// never its display label (which may include a ` — description` suffix the
/// command must never see). `labels` and `opts` are built from the same
/// slice in the same order (see the `fill` call site), so `i` is always in
/// range for both.
fn pick_selection(opts: &[options::PickOption], i: usize) -> String {
    opts[i].value.clone()
}

/// Render the final command text for `values`: a plain `run:` interpolates to a
/// single line; a step list compiles to its bash script.
fn compose(cmd: &ResolvedCommand, values: &IndexMap<String, String>) -> Result<String> {
    match &cmd.run {
        ResolvedRun::Script(template) => interp::interpolate(template, values),
        ResolvedRun::Steps(_) => compile::compile(cmd, values, false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{manifest, resolver};

    /// §7.1 — the value handed to the command equals the selected option's
    /// `value`, never its label. Pins the new `pick_selection` call site
    /// (the mechanism itself — index into the option slice — is already
    /// proven live by flow.rs:56's `members[ci]`). Redden by having
    /// `pick_selection` return the label instead of `opts[i].value`.
    #[test]
    fn pick_selection_returns_the_value_not_the_label() {
        let opts = vec![
            options::PickOption {
                value: "dev".to_string(),
                description: None,
            },
            options::PickOption {
                value: "uat".to_string(),
                description: Some("User acceptance".to_string()),
            },
        ];
        let w = label::width();
        let labels: Vec<String> = opts.iter().map(|o| label::option_label(o, w)).collect();

        let picked = pick_selection(&opts, 1);
        assert_eq!(picked, "uat");
        assert_ne!(
            picked, labels[1],
            "the value must differ from its own label when a description is set"
        );
    }

    /// §7.7 — provided values validate against values, not labels. A
    /// description-bearing static option accepts its bare value; the label
    /// text (with or without the description) is rejected with a
    /// values-only message.
    #[test]
    fn provided_pick_value_validates_against_values_not_labels() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("pult.yaml"),
            "version: 1\ncommands:\n  - id: c\n    title: C\n    params:\n      \
             env: { pick: { options: [{ value: dev, description: Sandbox }] } }\n    \
             run: \"echo {env}\"\n",
        )
        .unwrap();
        let resolved =
            resolver::resolve(manifest::load(&dir.path().join("pult.yaml")).unwrap()).unwrap();
        let cmd = &resolved.commands[0];

        let ok: HashMap<_, _> = [("env".to_string(), "dev".to_string())].into();
        let values = fill(cmd, &ok, None).unwrap();
        assert_eq!(values["env"], "dev");

        for bad in ["Sandbox", "dev — Sandbox"] {
            let provided: HashMap<_, _> = [("env".to_string(), bad.to_string())].into();
            let err = fill(cmd, &provided, None).unwrap_err().to_string();
            assert!(err.contains("expected one of: dev"), "got: {err}");
        }
    }

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
            None,
        )
        .unwrap();
        assert_eq!(code, 0);
        assert!(
            !marker.exists(),
            "option source ran during a --print dry run"
        );
    }

    fn secret_cmd() -> (tempfile::TempDir, crate::resolver::Resolved) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("pult.yaml"),
            "version: 1\ncommands:\n  - id: login\n    title: Login\n    params:\n      \
             token: { input: { secret: true } }\n      region: { input: { default: eu } }\n    \
             run: \"aws login --token {token} --region {region}\"\n",
        )
        .unwrap();
        let resolved =
            resolver::resolve(manifest::load(&dir.path().join("pult.yaml")).unwrap()).unwrap();
        (dir, resolved)
    }

    #[test]
    fn provided_secret_never_appears_in_preview() {
        let (_d, resolved) = secret_cmd();
        let cmd = &resolved.commands[0];
        let provided: HashMap<_, _> = [
            ("token".to_string(), "hunter2".to_string()),
            ("region".to_string(), "us".to_string()),
        ]
        .into();
        // Preview fill (`--print`, ephemeral trust prompt): the secret becomes
        // its slot even though a value was provided; plain inputs stay concrete.
        let values = fill(cmd, &provided, None).unwrap();
        let preview = compose(cmd, &values).unwrap();
        assert!(!preview.contains("hunter2"), "leaked: {preview}");
        assert!(preview.contains("<token>"), "got: {preview}");
        assert!(preview.contains("--region us"), "got: {preview}");
    }

    #[test]
    fn redact_masks_secret_values_only() {
        let (_d, resolved) = secret_cmd();
        let cmd = &resolved.commands[0];
        let values: IndexMap<_, _> = [
            ("token".to_string(), "hunter2".to_string()),
            ("region".to_string(), "us".to_string()),
        ]
        .into_iter()
        .collect();
        let display = crate::interp::interpolate(
            match &cmd.run {
                ResolvedRun::Script(t) => t,
                _ => unreachable!(),
            },
            &redact(cmd, &values),
        )
        .unwrap();
        assert!(!display.contains("hunter2"), "leaked: {display}");
        assert!(display.contains("••••••"), "got: {display}");
        assert!(display.contains("--region us"), "got: {display}");
    }

    /// Issue #12's own reproduction, run through `fill` (not `resolve_pick`
    /// directly) — a unit test that hand-builds a `Redaction` and calls
    /// `resolve_pick` only proves `resolve_pick` honors its argument; it
    /// stays green if `fill` passed the real map for *both* fields, which is
    /// exactly the one-line call-site bug that would fully restore the leak.
    /// This goes through the real call site (`fill`'s `ParamKind::Pick` arm)
    /// so a regression there reddens it.
    ///
    /// `format!("{err:#}")` matters: `:#` is anyhow's "full chain" rendering,
    /// which is what `main.rs`'s `eprintln!("pult: error: {e:#}")` actually
    /// prints, and it's the only rendering that includes the `with_context`
    /// wrapper at `options.rs`'s spawn-failure site (a plain `{err}` shows
    /// only the outermost frame and would miss a regression there).
    #[test]
    fn fill_never_leaks_a_secret_through_a_failing_pick_from() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("pult.yaml"),
            "version: 1\ncommands:\n  - id: deploy\n    title: Deploy\n    params:\n      \
             token: { input: { secret: true } }\n      \
             target: { pick: { from: \"echo tok={token} >&2; exit 1\" } }\n    \
             run: \"echo {target}\"\n",
        )
        .unwrap();
        let resolved =
            resolver::resolve(manifest::load(&dir.path().join("pult.yaml")).unwrap()).unwrap();
        let cmd = &resolved.commands[0];

        let secret = "hunter2-super-secret";
        let provided: HashMap<_, _> = [("token".to_string(), secret.to_string())].into();
        let err = fill(cmd, &provided, Some(dir.path())).unwrap_err();
        let rendered = format!("{err:#}");

        assert!(!rendered.contains(secret), "leaked: {rendered}");
        assert!(rendered.contains("••••••"), "got: {rendered}");
        assert!(rendered.contains("<redacted>"), "got: {rendered}");
    }

    /// #7 review finding 1, pinned at the site it was filed against
    /// (`build_masker`, formerly `exec.rs:187`): a multi-line secret whose
    /// every individual line is too short to mask must be reported
    /// unmaskable, even though the *whole* value (joined with `\n`) is long
    /// enough that a naive whole-value length check — the rule
    /// `build_masker` used to apply, independently of what `Masker`
    /// actually does with the value — would miss it entirely. Reddens if
    /// `build_masker` goes back to deciding "too short" itself instead of
    /// reading it off `Masker::from_named`'s own result.
    #[test]
    fn build_masker_reports_a_multiline_secret_whose_every_line_is_too_short() {
        let (_d, resolved) = secret_cmd();
        let cmd = &resolved.commands[0];
        let values: IndexMap<_, _> = [
            ("token".to_string(), "ab\ncd\nef".to_string()),
            ("region".to_string(), "us".to_string()),
        ]
        .into_iter()
        .collect();
        let (_masker, unmaskable) = build_masker(cmd, &values);
        assert_eq!(unmaskable, vec!["token".to_string()]);
    }
}
