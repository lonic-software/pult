use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use anyhow::{Result, bail};
use indexmap::IndexMap;

use crate::compile::Guards;
use crate::events::{self, InheritedChannel};
use crate::journal;
use crate::label;
use crate::manifest::{ParamKind, RepeatDef};
use crate::mask::{self, Masker};
use crate::resolver::{ArgvWord, Origin, Resolved, ResolvedCommand, ResolvedRun};
use crate::runner::Journaling;
use crate::value::{self, Value, Values};
use crate::{compile, interp, machine, options, prompt, runner, shell, trust};

/// How `fill` asks for a param nobody supplied, keyed to the call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prompting {
    /// The guided flow (bare `pult`, `pult x <source>` with no command):
    /// every param is asked in order, a flag as yes/no and a repeat one
    /// item per line.
    Guided,
    /// A direct invocation (`pult <cmd>`, `pult x <source> <cmd>`): an
    /// absent flag is false and an absent optional repeat is empty, neither
    /// asked; a repeat with `min` ≥ 1 is asked on a terminal and an error
    /// otherwise. Pick and input are asked as in the guided flow.
    Direct,
}

/// Execute one command: fill its params (from provided values or interactive
/// prompts, in declared order — which is what makes dependent `from:` sources
/// work), build the final script, and hand off to the runner (or print it).
///
/// This is the single choke point through which anything from a manifest gets
/// executed, so the trust check lives here, keyed to the `Resolved`'s
/// [`Origin`]: a tool document (`pult tool`) has no trust gate at all. For an
/// ephemeral `pult x` source the trust prompt also shows the **composed
/// command about to run** — honest there because the trust unit is that one
/// invocation. A real manifest's trust covers
/// every command it declares, so showing one would misrepresent the scope; its
/// prompt stays the file/include summary. Either way `--print` prints that same
/// composed command with no trust gate — a side-effect-free dry run, since the
/// preview is built without running any module code (dynamic option sources stay
/// unresolved, shown as `<name>` metavars).
pub fn execute(
    resolved: &Resolved,
    cmd: &ResolvedCommand,
    provided: &HashMap<String, Value>,
    prompting: Prompting,
    grant: &crate::trust::Grant,
    print: bool,
    run_id: Option<&str>,
) -> Result<i32> {
    if print {
        // Dry run: compose without executing anything (provided values
        // concrete, the rest as `<name>`), print, and stop — no trust needed.
        let preview = compose(resolved, cmd, &fill(cmd, provided, None, prompting)?)?;
        println!("{preview}");
        return Ok(0);
    }

    match resolved.origin {
        Origin::Tool => {}
        Origin::Manifest | Origin::Module => {
            // Only an ephemeral source shows its command in the trust prompt
            // (see above).
            let about_to_run = if resolved.origin == Origin::Module {
                Some(compose(
                    resolved,
                    cmd,
                    &fill(cmd, provided, None, prompting)?,
                )?)
            } else {
                None
            };
            trust::ensure_trusted(
                &resolved.path,
                &resolved.trust_hash,
                &resolved.include_summary,
                grant,
                about_to_run.as_deref(),
            )?;
        }
    }

    // Trusted now — fill for real (prompting for and resolving any values not
    // given on the command line) and run.
    let values = fill(cmd, provided, Some(&resolved.run_dir), prompting)?;

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
        // Spawned directly; no shell to resolve.
        ResolvedRun::Argv(_) => {}
    }

    // Build the run's masker once, post-`fill` (so an interactively prompted
    // secret, not just one supplied on the command line, is covered).
    // Unconditional — not just on the journaled path — because `masker` now
    // has two more jobs beyond the journal: masking the `running:` banner
    // below, and (already run, inside `fill` above) masking option-source
    // error text. An ephemeral `pult x` source can carry secret params and
    // render a banner too, so it needs the same masker a journaled run gets.
    let (masker, unmaskable) = build_masker(cmd, &values);

    // Open the run journal (see journal.rs). Ephemeral `pult x` sources and
    // tool documents are skipped — no durable repo identity to journal under. The journal's
    // param audit record gets `<redacted>` for secrets (the spec's literal,
    // machine-recognizable marker — distinct from the `••••••` display mask
    // below, which is a human-facing rendering).
    let journaling = if resolved.origin != Origin::Manifest {
        Journaling::Off
    } else {
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
            Some(j) => Journaling::Full(j, masker.clone()),
            None => Journaling::Off,
        }
    };

    // Warn for every secret this run's masker can't mask, but only when
    // something at the run level actually masks with it: `Journaling::Full`
    // (the journal does), or `ResolvedRun::Script` (the `running:` banner
    // below does, regardless of journaling state, since `masker` is applied
    // there unconditionally). A `ResolvedRun::Steps` run that isn't
    // `Journaling::Full` has nothing at this level that masks anything — its
    // banner is just `cmd.id` (see `run_bash`'s call below), no masker ever
    // touches it — so warning there would be exactly the misleading noise
    // #11's narrower gate existed to prevent, reintroduced by gating on "the
    // banner is masked" without checking which arm that's actually true for.
    //
    // This does *not* cover a run that never reaches this point: a failing
    // `pick.from` aborts inside `fill`, above, before `execute` resumes —
    // that's exactly the #12 scenario (a too-short secret, a failing option
    // source, the secret in stderr) — so `fill`'s own `Pick` arm calls
    // `warn_unmaskable` too, unconditionally (option-source masking runs
    // regardless of `cmd.run`'s shape or journaling state), at the one point
    // that precedes that abort. `warn_unmaskable` dedupes by name (a
    // process-global set) so a secret already warned about there isn't
    // warned about again here.
    // An argv run's banner is masked the same way a script's is.
    if matches!(journaling, Journaling::Full(..))
        || matches!(cmd.run, ResolvedRun::Script(_) | ResolvedRun::Argv(_))
    {
        warn_unmaskable(&unmaskable);
    }

    let channel = events::inherited_channel();
    match &cmd.run {
        ResolvedRun::Script(template) => {
            // The `running:` banner (and any error message) shows a line
            // with secrets masked twice over: by name (`redact` — no length
            // floor, but only ever touches a secret in its own declared
            // slot) and then by value (`masker` — `MIN_LEN` floor, but
            // catches that same secret's value resurfacing through a
            // *different*, non-secret param, e.g. a pick source that echoed
            // it back, or a URL built from it). Neither pass alone covers
            // both leak shapes; see `options::resolve_pick`'s `display` for
            // the same two-pass reasoning applied to option-source errors,
            // and for two residual gaps in the value-based pass (the
            // `MIN_LEN` floor, and `shell_quote`'s quote-mangling) that
            // apply here identically, since this goes through the same
            // `value::words` → `shell_quote` path.
            let display = masker
                .apply(&interp::interpolate(
                    template,
                    &value::words(&cmd.params, &redact(cmd, &values)),
                )?)
                .into_owned();
            runner::run_sh(
                &interp::interpolate(template, &value::words(&cmd.params, &values))?,
                &display,
                &resolved.run_dir,
                journaling,
                channel,
            )
        }
        // The same two-pass display as a script's: by name, then by value.
        ResolvedRun::Argv(words) => runner::run_argv(
            &resolved.path,
            &argv(cmd, words, &values, &values),
            &masker.apply(&display_argv(
                &resolved.path,
                &argv(cmd, words, &values, &redact(cmd, &values)),
            )),
            &resolved.run_dir,
            journaling,
            channel,
        ),
        // A channel inherited from upstream belongs to whoever set it, and
        // only a channel's owner emits `step`: on someone else's channel
        // each step is named by a `status` line instead.
        ResolvedRun::Steps(_) => runner::run_bash(
            &compile::compile(
                cmd,
                &values,
                match channel {
                    InheritedChannel::Owned => Guards::Status,
                    InheritedChannel::Absent | InheritedChannel::Invalid(_) => Guards::Step,
                },
            )?,
            &cmd.id,
            &resolved.run_dir,
            journaling,
            channel,
        ),
    }
}

/// `values` for the journal's audit record, as the strings the journal
/// keeps (see `Value::journal_text`): secrets as the spec's literal
/// `"<redacted>"` — the value itself never reaches the journal module.
fn journal_params(cmd: &ResolvedCommand, values: &Values) -> IndexMap<String, String> {
    values
        .iter()
        .map(|(name, v)| {
            let secret = cmd.params.get(name).is_some_and(|def| def.is_secret());
            let text = if secret {
                "<redacted>".to_string()
            } else {
                v.journal_text()
            };
            (name.clone(), text)
        })
        .collect()
}

/// `values` with every secret replaced by a fixed mask (fixed so the mask
/// doesn't leak the value's length): a secret input's text, and each item of
/// a secret repeat. Stays typed, because what it returns is rendered again
/// for display.
fn redact(cmd: &ResolvedCommand, values: &Values) -> Values {
    let mut out = values.clone();
    for (name, def) in &cmd.params {
        if !def.is_secret() {
            continue;
        }
        match out.get_mut(name) {
            Some(Value::Text(v)) => *v = "••••••".to_string(),
            Some(Value::List(items)) => {
                for item in items {
                    *item = "••••••".to_string();
                }
            }
            Some(Value::Flag(_)) | None => {}
        }
    }
    out
}

/// Build the run's `Masker` from post-`fill` secret values — a secret
/// repeat contributes each of its items — returning alongside it the name of
/// every secret `Masker::from_named` reports as producing no maskable
/// segment at all. Deliberately does *not* decide for itself which values
/// are "too short" — that would be exactly the drift this fixes
/// (`build_masker` used to run its own whole-value length check, a different
/// rule from what `Masker::from_named` actually splits and filters per line;
/// a multi-line value whose every individual line was too short passed the
/// whole-value check while masking nothing). One rule, derived from what
/// construction actually did; the caller (`execute`, above) decides
/// *whether* to warn from it.
fn build_masker(cmd: &ResolvedCommand, values: &Values) -> (Masker, Vec<String>) {
    let secrets: Vec<(String, String)> = cmd
        .params
        .iter()
        .filter(|(_, def)| def.is_secret())
        .filter_map(|(name, _)| values.get(name).map(|v| (name, v)))
        .flat_map(|(name, v)| {
            v.strings()
                .into_iter()
                .map(|s| (name.clone(), s.to_string()))
                .collect::<Vec<_>>()
        })
        .collect();
    Masker::from_named(secrets)
}

/// Cheap, honest, no knob (see `mask::MIN_LEN`'s doc comment for why there's
/// a floor at all): one line per secret `build_masker` reports as unmaskable.
///
/// Idempotent per secret **name**, via a process-global set — not because a
/// name recurs across runs (one process is one run), but because within a
/// single run this can now legitimately be called more than once for the
/// same name: `fill`'s `Pick` arm calls it once per dynamic picker (see that
/// call site for why it has to), and `execute` calls it again afterward with
/// the full post-`fill` values. Both need to fire to close the coverage gap
/// each documents at its own call site; neither should make the user read
/// the same warning twice.
fn warn_unmaskable(names: &[String]) {
    static WARNED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let mut warned = WARNED
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for name in names {
        if warned.insert(name.clone()) {
            eprintln!(
                "pult: warning: secret `{name}` is too short to mask in output \
                 (under {} characters)",
                mask::MIN_LEN
            );
        }
    }
}

/// Fill a command's params in declared order. `run_dir: None` = **preview**:
/// never prompt or run a dynamic option source (both would execute code from a
/// possibly-untrusted module); unprovided params become `<name>` metavars,
/// except that an absent flag is unset and an absent optional repeat empty.
/// `Some(dir)` = **live**: fill anything not provided as `prompting` says,
/// running dynamic pickers against `dir` — reached only after the trust gate.
///
/// A provided value's type matches its param's kind: the argv binding and
/// `--params-json` (main.rs) both build it from the kind.
pub(crate) fn fill(
    cmd: &ResolvedCommand,
    provided: &HashMap<String, Value>,
    run_dir: Option<&Path>,
    prompting: Prompting,
) -> Result<Values> {
    let preview = run_dir.is_none();
    let mut values = Values::new();
    for (name, def) in &cmd.params {
        let value = match (provided.get(name), def.kind()) {
            (Some(Value::Text(v)), ParamKind::Pick(pick)) => {
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
                Value::Text(v.clone())
            }
            // A provided secret is used for execution only — preview text
            // (`--print`, the ephemeral trust prompt) shows the slot instead,
            // so a secret passed on the command line is never printed back.
            (Some(Value::Text(_)), ParamKind::Input(input)) if input.secret && preview => {
                Value::Text(format!("<{name}>"))
            }
            // Deliberate exclusion, not an oversight: if this non-secret
            // value happens to embed a *different* param's secret (e.g. a
            // URL the user assembled themselves before invoking pult),
            // `compose` below applies no masking pass at all, so `--print`
            // and the ephemeral trust prompt show it exactly as given. That's
            // fine — the value is already sitting in the user's own argv and
            // shell history regardless of what pult does with it — but it's
            // an exclusion from the live-run masking this PR adds (the
            // `running:` banner, option-source errors, picker options),
            // which only ever applies once execution begins, not to these
            // pre-trust previews.
            (Some(v @ Value::Text(_)), ParamKind::Input(_)) => v.clone(),
            (Some(Value::Flag(b)), ParamKind::Flag(_)) => Value::Flag(*b),
            (Some(Value::List(items)), ParamKind::Repeat(repeat)) => {
                check_count(name, repeat, items.len())?;
                // Like a secret input: the preview shows the slot.
                if repeat.secret && preview {
                    Value::List(vec![format!("<{name}>")])
                } else {
                    Value::List(items.clone())
                }
            }
            (Some(_), _) => bail!("param `{name}` was given a value of the wrong type"),
            // An absent flag is unset unless the guided flow asks for it.
            (None, ParamKind::Flag(_)) if preview || prompting == Prompting::Direct => {
                Value::Flag(false)
            }
            (None, ParamKind::Repeat(repeat))
                if repeat.min == 0 && (preview || prompting == Prompting::Direct) =>
            {
                Value::List(Vec::new())
            }
            (None, ParamKind::Repeat(_)) if preview => Value::List(vec![format!("<{name}>")]),
            // Preview: show the slot instead of prompting or shelling out.
            (None, _) if preview => Value::Text(format!("<{name}>")),
            (None, ParamKind::Flag(_)) => Value::Flag(prompt::yes_no(&format!("{name}?"))?),
            (None, ParamKind::Repeat(repeat)) => Value::List(prompt_items(name, repeat)?),
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
                let words = value::words(&cmd.params, &values);
                let display_words = value::words(&cmd.params, &redact(cmd, &values));
                let (masker, unmaskable) = build_masker(cmd, &values);
                // Warn *here*, not just at the run level in `execute` — this
                // is the one point that precedes a failing `pick.from`
                // aborting `fill` entirely (the #12 scenario: a too-short
                // secret, a failing option source, the secret in stderr).
                // `execute`'s own call happens after `fill` returns, so it
                // never runs if this one doesn't. `warn_unmaskable` dedupes
                // by name, so a secret already warned about here isn't
                // warned about again at the run level (or at an earlier
                // pick, for a command with more than one).
                warn_unmaskable(&unmaskable);
                let redaction = options::Redaction {
                    values: &words,
                    display_values: &display_words,
                    masker: &masker,
                };
                let opts = options::resolve_pick(pick, &redaction, run_dir.unwrap())?;
                // A `from:` source can echo a secret back on *stdout* too —
                // an argv-echoing CLI, or a source that just reflects one of
                // its inputs — and that line becomes a `PickOption`, not an
                // error string, so none of `resolve_pick`'s own redaction
                // touches it. `opts` stays real (`pick_selection` below needs
                // the raw value for execution); only the rendered label copy
                // is masked, via the same throwaway masker used above.
                let w = label::width();
                let labels = masked_option_labels(&opts, &masker, w);
                let i = prompt::select_index(&format!("{name}?"), labels)?;
                Value::Text(pick_selection(&opts, i))
            }
            (None, ParamKind::Input(input)) if input.secret => {
                Value::Text(prompt::password(&format!("{name}?"))?)
            }
            (None, ParamKind::Input(input)) => {
                Value::Text(prompt::text(&format!("{name}?"), input.default.as_deref())?)
            }
            (_, ParamKind::Use(_)) => unreachable!("resolver inlines every use: param"),
        };
        values.insert(name.clone(), value);
    }
    Ok(values)
}

/// A repeat's item count must lie within its `min`/`max`.
fn check_count(name: &str, repeat: &RepeatDef, count: usize) -> Result<()> {
    let min = repeat.min as usize;
    if count < min {
        return Err(machine::usage(format!(
            "`{name}` needs at least {min} value(s), but {count} were given"
        )));
    }
    if let Some(max) = repeat.max
        && count > max as usize
    {
        return Err(machine::usage(format!(
            "`{name}` takes at most {max} value(s), but {count} were given"
        )));
    }
    Ok(())
}

/// Ask for a repeat's items one per line. An empty line ends the list once
/// `min` items are in, and the list ends by itself at `max`, so an empty
/// item can only be given on the command line or in `--params-json`. A
/// secret repeat's items are asked without echo.
fn prompt_items(name: &str, repeat: &RepeatDef) -> Result<Vec<String>> {
    // Checked here rather than left to the first prompt, whose label would
    // name an item instead of the param.
    prompt::require_tty(&if repeat.min > 0 {
        format!("`{name}` needs at least {} value(s)", repeat.min)
    } else {
        format!("`{name}` needs interactive input")
    })?;
    let mut items = Vec::new();
    loop {
        if repeat.max.is_some_and(|max| items.len() >= max as usize) {
            return Ok(items);
        }
        let done = items.len() >= repeat.min as usize;
        let message = if done {
            format!("{name}? (item {}, empty line to finish)", items.len() + 1)
        } else {
            format!("{name}? (item {})", items.len() + 1)
        };
        let item = if repeat.secret {
            prompt::password(&message)?
        } else {
            prompt::text(&message, None)?
        };
        if item.is_empty() {
            if done {
                return Ok(items);
            }
            continue;
        }
        items.push(item);
    }
}

/// Render a picker's options into on-screen labels with `masker` applied — a
/// separate copy from `opts`, which stays real and untouched: `pick_selection`
/// below hands `opts[i].value` straight to the command, so masking `opts`
/// itself would make pult *execute* the mask instead of the secret. This is
/// the seam a `from:` source's stdout leak (found in review of #12's initial
/// fix — a dynamic source can echo a secret in a value it emits, not just in
/// its own failure text) needs and `resolve_pick`'s return type doesn't: the
/// value stays exactly what it was, only its rendered text changes.
///
/// Masks `value` and `description` **before** calling `label::option_label`,
/// not the composed label string afterward — order matters. `option_label`
/// (via `label::compose`) truncates a too-long description to fit `width`
/// and appends `…`, keeping only the *head* (the value) whole; masking the
/// already-truncated composed string would only ever see a byte-prefix of a
/// description-borne secret, which no longer matches the full needle, so
/// nothing gets replaced and the prefix renders in cleartext (review found
/// this live, in an earlier revision of this function — a fixed-width test
/// with no description couldn't catch it because truncation never
/// triggered). Masking first means `compose` truncates an already-safe
/// `<redacted>` marker at worst, never a raw secret fragment.
fn masked_option_labels(
    opts: &[options::PickOption],
    masker: &Masker,
    width: usize,
) -> Vec<String> {
    opts.iter()
        .map(|o| {
            let masked = options::PickOption {
                value: masker.apply(&o.value).into_owned(),
                description: o
                    .description
                    .as_deref()
                    .map(|d| masker.apply(d).into_owned()),
            };
            label::option_label(&masked, width)
        })
        .collect()
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
/// single line; a step list compiles to its bash script; an argv shows as the
/// program and its arguments, shell-quoted.
fn compose(resolved: &Resolved, cmd: &ResolvedCommand, values: &Values) -> Result<String> {
    match &cmd.run {
        ResolvedRun::Script(template) => {
            interp::interpolate(template, &value::words(&cmd.params, values))
        }
        ResolvedRun::Steps(_) => compile::compile(cmd, values, Guards::None),
        ResolvedRun::Argv(words) => Ok(display_argv(
            &resolved.path,
            &argv(cmd, words, values, values),
        )),
    }
}

/// The arguments an argv run spawns its program with. A literal is itself;
/// a param is its value as zero or more arguments: text one, a set flag its
/// `arg`, an unset flag or empty list none, a list each item after its `arg`
/// (or alone, without one).
///
/// One rule is the tool path's own, not the shared shell renderer's: under
/// an `arg` starting with `--`, an item that itself starts with `-` becomes
/// the single argument `--arg=item`, so the tool binds it as the value
/// rather than refusing it as an unknown flag. `value::render` keeps the
/// two-word form, since a hand-written manifest calls tools whose option
/// syntax pult cannot know.
///
/// `shown` supplies the words actually emitted: `values` itself for the run,
/// or its redaction for the banner. The shape, including that join, always
/// follows `values`, so the banner shows the argv that ran even when a
/// secret is masked.
pub(crate) fn argv(
    cmd: &ResolvedCommand,
    words: &[ArgvWord],
    values: &Values,
    shown: &Values,
) -> Vec<String> {
    let mut out = Vec::new();
    for word in words {
        let name = match word {
            ArgvWord::Literal(s) => {
                out.push(s.clone());
                continue;
            }
            ArgvWord::Param(name) => name,
        };
        let def = &cmd.params[name.as_str()];
        match (&values[name.as_str()], &shown[name.as_str()]) {
            (Value::Text(_), Value::Text(s)) => out.push(s.clone()),
            (Value::Flag(false), _) => {}
            (Value::Flag(true), _) => {
                let ParamKind::Flag(flag) = def.kind() else {
                    unreachable!("a flag value belongs to a flag param");
                };
                out.push(
                    flag.arg
                        .clone()
                        .expect("a resolved flag always has an `arg`"),
                );
            }
            (Value::List(items), Value::List(shown)) => {
                let arg = match def.kind() {
                    ParamKind::Repeat(repeat) => repeat.arg.as_deref(),
                    _ => unreachable!("a list value belongs to a repeat param"),
                };
                for (item, word) in items.iter().zip(shown) {
                    match arg {
                        Some(arg) if arg.starts_with("--") && item.starts_with('-') => {
                            out.push(format!("{arg}={word}"))
                        }
                        Some(arg) => out.extend([arg.to_string(), word.clone()]),
                        None => out.push(word.clone()),
                    }
                }
            }
            _ => unreachable!("a redaction keeps each value's kind"),
        }
    }
    out
}

/// An argv for display only: the program and each argument shell-quoted.
fn display_argv(program: &Path, args: &[String]) -> String {
    std::iter::once(program.display().to_string())
        .chain(args.iter().cloned())
        .map(|a| interp::shell_quote(&a))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{manifest, resolver};

    /// Text values, as the argv binding builds them for pick and input params.
    fn text<C: FromIterator<(String, Value)>>(pairs: &[(&str, &str)]) -> C {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), Value::Text(v.to_string())))
            .collect()
    }

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

        let ok: HashMap<_, _> = text(&[("env", "dev")]);
        let values = fill(cmd, &ok, None, Prompting::Direct).unwrap();
        assert_eq!(values["env"], Value::Text("dev".to_string()));

        for bad in ["Sandbox", "dev — Sandbox"] {
            let provided: HashMap<_, _> = text(&[("env", bad)]);
            let err = fill(cmd, &provided, None, Prompting::Direct)
                .unwrap_err()
                .to_string();
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
            Prompting::Direct,
            &crate::trust::Grant::Ask,
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
        let provided: HashMap<_, _> = text(&[("token", "hunter2"), ("region", "us")]);
        // Preview fill (`--print`, ephemeral trust prompt): the secret becomes
        // its slot even though a value was provided; plain inputs stay concrete.
        let values = fill(cmd, &provided, None, Prompting::Direct).unwrap();
        let preview = compose(&resolved, cmd, &values).unwrap();
        assert!(!preview.contains("hunter2"), "leaked: {preview}");
        assert!(preview.contains("<token>"), "got: {preview}");
        assert!(preview.contains("--region us"), "got: {preview}");
    }

    #[test]
    fn redact_masks_secret_values_only() {
        let (_d, resolved) = secret_cmd();
        let cmd = &resolved.commands[0];
        let values: Values = text(&[("token", "hunter2"), ("region", "us")]);
        let display = crate::interp::interpolate(
            match &cmd.run {
                ResolvedRun::Script(t) => t,
                _ => unreachable!(),
            },
            &value::words(&cmd.params, &redact(cmd, &values)),
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
        let provided: HashMap<_, _> = text(&[("token", secret)]);
        let err = fill(cmd, &provided, Some(dir.path()), Prompting::Direct).unwrap_err();
        let rendered = format!("{err:#}");

        assert!(!rendered.contains(secret), "leaked: {rendered}");
        assert!(rendered.contains("••••••"), "got: {rendered}");
        assert!(rendered.contains("<redacted>"), "got: {rendered}");
    }

    /// Follow-up review finding on #12's fix: `redact` masks by **name**, so
    /// a secret's *value* surfacing inside a **different**, non-secret param
    /// sails through untouched — `redact` never even looks at `url` here,
    /// only `token`. Only the run's `masker` (value-based) can catch that.
    ///
    /// The `from:` below references `{url}` only, never `{token}`, and the
    /// source writes nothing to stdout or stderr (a shell comment, not an
    /// echo) — so any leak here can only come from `display` itself, the
    /// interpolated command text embedded in the "option source `{display}`"
    /// part of the error. That isolates this from the stderr route (already
    /// covered by `fill_never_leaks_a_secret_through_a_failing_pick_from`
    /// above) and pins specifically that `resolve_pick` now runs `display`
    /// through `masker.apply` too, not just `redact`. Redden by dropping
    /// that `masker.apply` pass in `options.rs`'s `resolve_pick`.
    #[test]
    fn fill_masks_a_secret_that_surfaces_through_a_different_non_secret_param() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("pult.yaml"),
            "version: 1\ncommands:\n  - id: deploy\n    title: Deploy\n    params:\n      \
             token: { input: { secret: true } }\n      \
             url: { input: {} }\n      \
             target: { pick: { from: \"exit 1 # ref {url}\" } }\n    \
             run: \"echo {target}\"\n",
        )
        .unwrap();
        let resolved =
            resolver::resolve(manifest::load(&dir.path().join("pult.yaml")).unwrap()).unwrap();
        let cmd = &resolved.commands[0];

        let secret = "hunter2-super-secret";
        let url = format!("https://api.example/?token={secret}");
        let provided: HashMap<_, _> = text(&[("token", secret), ("url", &url)]);
        let err = fill(cmd, &provided, Some(dir.path()), Prompting::Direct).unwrap_err();
        let rendered = format!("{err:#}");

        assert!(!rendered.contains(secret), "leaked: {rendered}");
        assert!(rendered.contains("<redacted>"), "got: {rendered}");
    }

    /// Follow-up review finding on #12's fix: a `from:` source can echo a
    /// secret back on *stdout*, not just in its failure text — a CLI that
    /// echoes its own argv is a common idiom, and that line becomes a
    /// `PickOption`, rendered verbatim in the picker. `resolve_pick`'s own
    /// redaction (which only covers error strings) never touches it.
    ///
    /// This exercises `masked_option_labels` directly rather than through
    /// `fill`: `fill`'s `Pick` arm always reaches `prompt::select_index`
    /// after a successful `resolve_pick`, and that call hard-errors without
    /// a TTY (see `prompt::require_tty`) — there is no way to drive the
    /// success path end-to-end in a headless test. `masked_option_labels` is
    /// the seam that makes the masking step itself testable without one.
    ///
    /// Pins two things at once: the label shown on screen is masked, and
    /// `opts` itself is untouched — `pick_selection` needs the raw value to
    /// hand to the command, so masking `opts` in place would make pult
    /// execute the mask instead of the secret.
    #[test]
    fn masked_option_labels_hides_a_secret_a_pick_source_echoed_back() {
        let secret = "hunter2-super-secret";
        let opts = vec![options::PickOption {
            value: secret.to_string(),
            description: None,
        }];
        let masker = Masker::new([secret.to_string()]);

        let labels = masked_option_labels(&opts, &masker, 80);

        assert_eq!(labels.len(), 1);
        assert!(!labels[0].contains(secret), "leaked: {:?}", labels[0]);
        assert!(labels[0].contains("<redacted>"), "got: {:?}", labels[0]);
        // The option itself must stay real for execution.
        assert_eq!(opts[0].value, secret);
    }

    /// Adversarial-review finding on `masked_option_labels`: `option_label`
    /// (via `label::compose`) truncates a too-long *description* to fit
    /// `width`, keeping the value whole. Masking the label string *after*
    /// composing (the bug this test reddens against) only ever sees
    /// whatever byte-prefix of a description-borne secret survived
    /// truncation — never the full needle — so nothing matches and that
    /// prefix renders in cleartext. The sibling test above uses width 80
    /// and no description, so truncation never triggers and can't catch
    /// this; this one picks a `width` deliberately too narrow to fit the
    /// whole secret in the description, and checks every prefix of the
    /// secret from `mask::MIN_LEN` characters up to the whole thing, not
    /// just the full string — a partial leak is still a leak.
    #[test]
    fn masked_option_labels_hides_every_prefix_of_a_secret_truncated_out_of_a_description() {
        let secret = "hunter2-super-secret"; // 20 chars
        let opts = vec![options::PickOption {
            value: "v".to_string(),
            description: Some(secret.to_string()),
        }];
        let masker = Masker::new([secret.to_string()]);

        // Narrow enough that `label::compose` must truncate the description
        // if it were still the raw secret at that point.
        let labels = masked_option_labels(&opts, &masker, 15);

        assert_eq!(labels.len(), 1);
        for len in mask::MIN_LEN..=secret.len() {
            let prefix = &secret[..len];
            assert!(
                !labels[0].contains(prefix),
                "leaked prefix `{prefix}` in label {:?}",
                labels[0]
            );
        }
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
        let values: Values = text(&[("token", "ab\ncd\nef"), ("region", "us")]);
        let (_masker, unmaskable) = build_masker(cmd, &values);
        assert_eq!(unmaskable, vec!["token".to_string()]);
    }
}
