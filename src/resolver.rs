use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use indexmap::IndexMap;
use sha2::{Digest, Sha256};

use crate::fetch::{self, Source};
use crate::interp;
use crate::manifest::{
    self, IncludeDef, Loaded, Manifest, ParamDef, ParamKind, PipeEntry, RunEntry, RunSpec, StepDef,
};

/// Command ids the engine claims for its own subcommands — current and
/// future. Seeded generously while pult has no users: adding a word later
/// breaks any manifest already using it, so anything the engine might
/// plausibly want is parked now (`self` is the umbrella for everything
/// unforeseen). Everything NOT on this list is promised to manifests forever.
const RESERVED_IDS: [&str; 8] = [
    "includes", "registry", "module", "update", "self", "init", "trust", "cache",
];

/// A root manifest with all includes resolved, vars substituted, `use:`
/// references inlined, and cross-file contracts validated. Everything
/// downstream (prompting, CLI generation, compilation) reads this.
#[derive(Debug)]
pub struct Resolved {
    pub name: String,
    /// Root manifest path (trust identity).
    pub path: PathBuf,
    /// Root manifest directory — the base local includes resolve against.
    pub dir: PathBuf,
    /// Where commands and option sources run. Defaults to `dir`; user-scoped
    /// manifests override it to the invocation directory (personal commands
    /// act on wherever you are, not on ~/.config/pult).
    pub run_dir: PathBuf,
    /// Hash over the root + every resolved include — the trust unit.
    pub trust_hash: String,
    /// One line per include, for the trust prompt.
    pub include_summary: Vec<String>,
    /// What each include is pinned to — the input to `pult includes verify`.
    pub pins: Vec<PinInfo>,
    pub commands: Vec<ResolvedCommand>,
}

#[derive(Debug)]
pub enum PinInfo {
    Local {
        source: String,
    },
    Git {
        source: String,
        url: String,
        rev: String,
        /// "tag" or "sha".
        rev_kind: String,
        resolved_sha: String,
    },
}

#[derive(Debug)]
pub struct ResolvedCommand {
    pub id: String,
    pub title: String,
    /// Concrete params only — every `use:` has been inlined.
    pub params: IndexMap<String, ParamDef>,
    pub run: ResolvedRun,
    /// Include source this command came from; None = declared locally.
    pub origin: Option<String>,
}

#[derive(Debug)]
pub enum ResolvedRun {
    /// Plain command line — executed via `sh -c`, original semantics.
    Script(String),
    /// Step list — compiled to one bash script (compile.rs).
    Steps(Vec<ResolvedEntry>),
}

#[derive(Debug)]
pub enum ResolvedEntry {
    Inline(String),
    Call(ResolvedCall),
    Pipe(Vec<ResolvedSeg>),
}

#[derive(Debug)]
pub struct ResolvedCall {
    /// Step name as the user knows it (post-prefix), for messages.
    pub name: String,
    /// Script after `with:` rebinding; still contains `{param}` placeholders.
    pub script: String,
    pub outputs: Vec<String>,
    pub exports: IndexMap<String, String>,
}

#[derive(Debug)]
pub enum ResolvedSeg {
    Inline(String),
    Call { name: String, script: String },
}

struct Namespaces {
    params: IndexMap<String, (ParamDef, Option<String>)>,
    steps: IndexMap<String, (StepDef, Option<String>)>,
}

pub fn resolve(loaded: Loaded) -> Result<Resolved> {
    resolve_with(loaded, None)
}

/// `cache_root`: where git modules are cached; None = the default
/// (~/.cache/pult/modules, or PULT_CACHE_DIR). Tests pass an explicit root.
pub fn resolve_with(loaded: Loaded, cache_root: Option<&std::path::Path>) -> Result<Resolved> {
    let Loaded {
        manifest: root,
        path,
        dir,
        raw,
    } = loaded;
    let default_cache;
    let cache_root = match cache_root {
        Some(p) => p,
        None => {
            default_cache = fetch::default_cache_root()?;
            &default_cache
        }
    };

    if !root.vars.is_empty() {
        bail!(
            "{}: `vars:` belongs in included modules — the root manifest has no include site to bind them",
            path.display()
        );
    }

    let mut trust_input = raw;
    let mut include_summary = Vec::new();
    let mut pins = Vec::new();
    let mut ns = Namespaces {
        params: IndexMap::new(),
        steps: IndexMap::new(),
    };
    let mut merged: Vec<(manifest::CommandDef, Option<String>)> = Vec::new();

    for inc in &root.includes {
        let loaded_module = load_module(inc, &dir, cache_root, &mut trust_input)
            .with_context(|| format!("include `{}`", inc.source))?;
        include_summary.push(loaded_module.summary);
        pins.push(loaded_module.pin);
        merge_module(loaded_module.manifest, &inc.source, &mut ns, &mut merged)
            .with_context(|| format!("include `{}`", inc.source))?;
    }

    // Local blocks and commands merge last, unprefixed — same collision rules.
    for (name, def) in root.params {
        insert_unique(&mut ns.params, name, (def, None), "param")?;
    }
    for (name, step) in root.steps {
        insert_unique(&mut ns.steps, name, (step, None), "step")?;
    }
    for cmd in root.commands {
        merged.push((cmd, None));
    }

    let mut seen_ids = HashSet::new();
    let mut commands = Vec::new();
    for (cmd, origin) in merged {
        if RESERVED_IDS.contains(&cmd.id.as_str()) {
            bail!(
                "command id `{}` is reserved for pult's own subcommands — pick another id",
                cmd.id
            );
        }
        if !seen_ids.insert(cmd.id.clone()) {
            bail!(
                "duplicate command id `{}` after merging includes — add a `prefix:` to one of them",
                cmd.id
            );
        }
        commands.push(resolve_command(cmd, origin, &ns)?);
    }
    if commands.is_empty() {
        bail!("{}: no commands after resolving includes", path.display());
    }

    let name = root.name.unwrap_or_else(|| {
        dir.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "pult".to_string())
    });
    let digest = Sha256::digest(trust_input.as_bytes());
    let trust_hash = digest.iter().map(|b| format!("{b:02x}")).collect();

    Ok(Resolved {
        name,
        path,
        run_dir: dir.clone(),
        dir,
        trust_hash,
        include_summary,
        pins,
        commands,
    })
}

struct LoadedModule {
    manifest: Manifest,
    /// Line for the trust prompt (git includes show the resolved commit).
    summary: String,
    pin: PinInfo,
}

/// Load one include: a local path, or a pinned git module (fetched into the
/// cache on first use, then served offline forever — pins are immutable).
fn load_module(
    inc: &IncludeDef,
    root_dir: &std::path::Path,
    cache_root: &std::path::Path,
    trust_input: &mut String,
) -> Result<LoadedModule> {
    let (file, pin, summary, local_tree) = match fetch::parse_source(&inc.source)? {
        Source::Local => {
            let target = root_dir.join(&inc.source);
            let (file, tree) = if target.is_dir() {
                (target.join("module.yaml"), Some(target))
            } else {
                (target, None)
            };
            (
                file,
                PinInfo::Local {
                    source: inc.source.clone(),
                },
                inc.source.clone(),
                tree,
            )
        }
        Source::Git(git_src) => {
            let (checkout, meta) = fetch::ensure_fetched(&git_src, cache_root)?;
            let file = match &git_src.subpath {
                Some(p) if p.ends_with(".yaml") || p.ends_with(".yml") => checkout.join(p),
                Some(p) => checkout.join(p).join("module.yaml"),
                None => checkout.join("module.yaml"),
            };
            let short = &meta.resolved_sha[..10.min(meta.resolved_sha.len())];
            (
                file,
                PinInfo::Git {
                    source: inc.source.clone(),
                    url: meta.url.clone(),
                    rev: meta.rev.clone(),
                    rev_kind: meta.rev_kind.clone(),
                    resolved_sha: meta.resolved_sha.clone(),
                },
                format!("{} (commit {short})", inc.source),
                None,
            )
        }
    };

    let raw = std::fs::read_to_string(&file)
        .with_context(|| format!("failed to read {}", file.display()))?;

    if let Some(expected) = &inc.sha256 {
        let digest = Sha256::digest(raw.as_bytes());
        let actual: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        if &actual != expected {
            bail!(
                "sha256 mismatch for {} — expected {expected}, got {actual}. \
                 The module changed underneath its pin; refusing to continue.",
                file.display()
            );
        }
    }
    trust_input.push_str(&inc.source);
    if let PinInfo::Git { resolved_sha, .. } = &pin {
        trust_input.push_str(resolved_sha);
    }
    trust_input.push_str(&raw);
    // A directory module ships more than its yaml — executables under
    // ${module.dir} run too, so the whole tree is part of the trust unit.
    // (Git modules need no equivalent: the pinned sha identifies the tree.
    // Single-file includes cover only the file; their module.dir is ambient.)
    if let Some(tree) = &local_tree {
        trust_input.push_str(&hash_dir_tree(tree)?);
    }

    let mut module = manifest::parse(&raw, &file)?;
    if !module.includes.is_empty() {
        bail!("{}: transitive includes are not supported", file.display());
    }

    // Bind vars: unknown bindings are errors; required must be covered.
    for key in inc.vars.keys() {
        if !module.vars.contains_key(key) {
            let declared: Vec<_> = module.vars.keys().cloned().collect();
            bail!(
                "binds unknown var `{key}` — the module declares: {}",
                if declared.is_empty() {
                    "(none)".to_string()
                } else {
                    declared.join(", ")
                }
            );
        }
    }
    let mut bound: IndexMap<String, String> = IndexMap::new();
    for (vname, vdef) in &module.vars {
        match inc.vars.get(vname).or(vdef.default.as_ref()) {
            Some(v) => {
                bound.insert(vname.clone(), v.clone());
            }
            None => bail!("missing required var `{vname}`"),
        }
    }
    let module_dir = file
        .parent()
        .context("module has no parent directory")?
        .canonicalize()
        .with_context(|| format!("failed to resolve {}", file.display()))?;
    bound.insert("module.dir".to_string(), module_dir.display().to_string());

    visit_templates(&mut module, &|s| *s = interp::substitute_vars(s, &bound));

    if let Some(prefix) = &inc.prefix {
        if !manifest::is_valid_name(prefix) {
            bail!("invalid prefix `{prefix}`");
        }
        apply_prefix(&mut module, prefix);
    }
    Ok(LoadedModule {
        manifest: module,
        summary,
        pin,
    })
}

/// Deterministic digest of a local module directory: relative paths, contents,
/// and (on unix) the executable bit — so editing a shipped script, adding a
/// file, or flipping a mode re-triggers trust the same way editing the yaml
/// does. `.git` is skipped in case the include points at a checkout.
fn hash_dir_tree(root: &std::path::Path) -> Result<String> {
    let mut hasher = Sha256::new();
    hash_dir_into(root, root, &mut hasher)?;
    let digest = hasher.finalize();
    Ok(digest.iter().map(|b| format!("{b:02x}")).collect())
}

fn hash_dir_into(root: &std::path::Path, dir: &std::path::Path, hasher: &mut Sha256) -> Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .and_then(|it| it.collect::<std::io::Result<Vec<_>>>())
        .with_context(|| format!("failed to read {}", dir.display()))?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        if entry.file_name() == ".git" {
            continue;
        }
        let path = entry.path();
        let rel = path.strip_prefix(root).expect("entry is under root");
        let meta = std::fs::symlink_metadata(&path)
            .with_context(|| format!("failed to stat {}", path.display()))?;
        if meta.file_type().is_symlink() {
            let target = std::fs::read_link(&path)
                .with_context(|| format!("failed to read link {}", path.display()))?;
            hasher.update(b"l");
            hasher.update(rel.to_string_lossy().as_bytes());
            hasher.update([0]);
            hasher.update(target.to_string_lossy().as_bytes());
            hasher.update([0]);
        } else if meta.is_dir() {
            hash_dir_into(root, &path, hasher)?;
        } else {
            hasher.update(b"f");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if meta.permissions().mode() & 0o111 != 0 {
                    hasher.update(b"x");
                }
            }
            hasher.update(rel.to_string_lossy().as_bytes());
            hasher.update([0]);
            let contents = std::fs::read(&path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            hasher.update((contents.len() as u64).to_le_bytes());
            hasher.update(&contents);
        }
    }
    Ok(())
}

fn merge_module(
    module: Manifest,
    source: &str,
    ns: &mut Namespaces,
    merged: &mut Vec<(manifest::CommandDef, Option<String>)>,
) -> Result<()> {
    let origin = Some(source.to_string());
    for (name, def) in module.params {
        insert_unique(&mut ns.params, name, (def, origin.clone()), "param")?;
    }
    for (name, step) in module.steps {
        insert_unique(&mut ns.steps, name, (step, origin.clone()), "step")?;
    }
    for cmd in module.commands {
        merged.push((cmd, origin.clone()));
    }
    Ok(())
}

fn insert_unique<T>(
    map: &mut IndexMap<String, T>,
    name: String,
    value: T,
    kind: &str,
) -> Result<()> {
    if map.contains_key(&name) {
        bail!(
            "duplicate {kind} name `{name}` after merging includes — add a `prefix:` to disambiguate"
        );
    }
    map.insert(name, value);
    Ok(())
}

/// Apply `${var}`-style substitution to every template-bearing string field.
fn visit_templates(m: &mut Manifest, f: &dyn Fn(&mut String)) {
    for def in m.params.values_mut() {
        visit_param(def, f);
    }
    for step in m.steps.values_mut() {
        visit_step(step, f);
    }
    for cmd in &mut m.commands {
        f(&mut cmd.title);
        for def in cmd.params.values_mut() {
            visit_param(def, f);
        }
        match &mut cmd.run {
            RunSpec::Script(s) => f(s),
            RunSpec::List(entries) => {
                for entry in entries {
                    match entry {
                        RunEntry::Inline(s) => f(s),
                        RunEntry::Use(u) => {
                            for v in u.with.values_mut() {
                                f(v);
                            }
                        }
                        RunEntry::Pipe(pg) => {
                            for seg in &mut pg.pipe {
                                match seg {
                                    PipeEntry::Inline(s) => f(s),
                                    PipeEntry::Use(pu) => {
                                        for v in pu.with.values_mut() {
                                            f(v);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn visit_param(def: &mut ParamDef, f: &dyn Fn(&mut String)) {
    if let Some(pick) = &mut def.pick {
        if let Some(from) = &mut pick.from {
            f(from);
        }
        if let Some(options) = &mut pick.options {
            for o in options {
                f(o);
            }
        }
    }
    if let Some(input) = &mut def.input
        && let Some(d) = &mut input.default
    {
        f(d);
    }
}

fn visit_step(step: &mut StepDef, f: &dyn Fn(&mut String)) {
    match step {
        StepDef::Plain(s) => f(s),
        StepDef::Full(full) => f(&mut full.script),
    }
}

/// Prefix everything a module exports — and every internal reference, so the
/// module keeps referring to its own (now-renamed) blocks.
fn apply_prefix(m: &mut Manifest, prefix: &str) {
    let pre = |name: &str| format!("{prefix}:{name}");
    m.params = std::mem::take(&mut m.params)
        .into_iter()
        .map(|(k, v)| (pre(&k), v))
        .collect();
    m.steps = std::mem::take(&mut m.steps)
        .into_iter()
        .map(|(k, v)| (pre(&k), v))
        .collect();
    for cmd in &mut m.commands {
        cmd.id = pre(&cmd.id);
        for def in cmd.params.values_mut() {
            if let Some(u) = &mut def.use_ {
                *u = pre(u);
            }
        }
        if let RunSpec::List(entries) = &mut cmd.run {
            for entry in entries {
                match entry {
                    RunEntry::Use(u) => u.use_ = pre(&u.use_),
                    RunEntry::Pipe(pg) => {
                        for seg in &mut pg.pipe {
                            if let PipeEntry::Use(pu) = seg {
                                pu.use_ = pre(&pu.use_);
                            }
                        }
                    }
                    RunEntry::Inline(_) => {}
                }
            }
        }
    }
}

fn resolve_command(
    cmd: manifest::CommandDef,
    origin: Option<String>,
    ns: &Namespaces,
) -> Result<ResolvedCommand> {
    let ctx = || format!("command `{}`", cmd.id);

    // Inline `use:` params; everything downstream sees concrete definitions.
    let mut params: IndexMap<String, ParamDef> = IndexMap::new();
    for (name, def) in &cmd.params {
        let concrete = match def.kind() {
            ParamKind::Use(target) => {
                let (named, _) = ns.params.get(target).with_context(|| {
                    format!(
                        "{}: param `{name}` uses `{target}`, which no include exports — available: {}",
                        ctx(),
                        available(ns.params.keys())
                    )
                })?;
                if named.use_.is_some() {
                    bail!(
                        "{}: named param `{target}` is itself a `use:` — named params must be concrete",
                        ctx()
                    );
                }
                named.clone()
            }
            _ => def.clone(),
        };
        params.insert(name.clone(), concrete);
    }

    // Dependent-picker ordering: a `from:` may reference only earlier params.
    let mut seen: HashSet<&str> = HashSet::new();
    for (name, def) in &params {
        if let Some(pick) = &def.pick
            && let Some(from) = &pick.from
        {
            for ph in interp::placeholders(from)? {
                if !seen.contains(ph.as_str()) {
                    bail!(
                        "{}: param `{name}`: option source references `{{{ph}}}`, which is not declared before it",
                        ctx()
                    );
                }
            }
        }
        seen.insert(name.as_str());
    }

    let run = match &cmd.run {
        RunSpec::Script(s) => {
            // Strict single-line template: placeholders must be declared params.
            for ph in interp::placeholders(s)? {
                if !params.contains_key(&ph) {
                    bail!(
                        "{}: run references `{{{ph}}}`, which is not a declared param",
                        ctx()
                    );
                }
            }
            ResolvedRun::Script(s.clone())
        }
        RunSpec::List(entries) => {
            let mut resolved = Vec::new();
            let mut output_names: HashSet<String> = HashSet::new();
            for entry in entries {
                resolved.push(resolve_entry(entry, &cmd, &params, ns, &mut output_names)?);
            }
            ResolvedRun::Steps(resolved)
        }
    };

    Ok(ResolvedCommand {
        id: cmd.id,
        title: cmd.title,
        params,
        run,
        origin,
    })
}

fn resolve_entry(
    entry: &RunEntry,
    cmd: &manifest::CommandDef,
    params: &IndexMap<String, ParamDef>,
    ns: &Namespaces,
    output_names: &mut HashSet<String>,
) -> Result<ResolvedEntry> {
    match entry {
        RunEntry::Inline(s) => Ok(ResolvedEntry::Inline(s.clone())),
        RunEntry::Use(u) => {
            let (script, step_outputs) = resolve_use(&cmd.id, &u.use_, &u.with, params, ns)?;
            for (from, to) in &u.exports {
                if !step_outputs.iter().any(|o| o == from) {
                    bail!(
                        "command `{}`: exports renames `{from}`, but step `{}` declares outputs: {}",
                        cmd.id,
                        u.use_,
                        available(step_outputs.iter())
                    );
                }
                if !manifest::is_valid_name(to) {
                    bail!("command `{}`: invalid export name `{to}`", cmd.id);
                }
            }
            for out in &step_outputs {
                let final_name = u.exports.get(out).unwrap_or(out).clone();
                if !output_names.insert(final_name.clone()) {
                    bail!(
                        "command `{}`: two steps in this run produce output `{final_name}` — rename one with `exports:`",
                        cmd.id
                    );
                }
            }
            Ok(ResolvedEntry::Call(ResolvedCall {
                name: u.use_.clone(),
                script,
                outputs: step_outputs,
                exports: u.exports.clone(),
            }))
        }
        RunEntry::Pipe(pg) => {
            let mut segs = Vec::new();
            for seg in &pg.pipe {
                segs.push(match seg {
                    PipeEntry::Inline(s) => {
                        if s.contains('\n') {
                            bail!(
                                "command `{}`: a multi-line script inside `pipe:` — give it a name under `steps:` instead",
                                cmd.id
                            );
                        }
                        ResolvedSeg::Inline(s.clone())
                    }
                    PipeEntry::Use(pu) => {
                        let (script, _outputs) =
                            resolve_use(&cmd.id, &pu.use_, &pu.with, params, ns)?;
                        // Variable outputs can't escape a pipe (subshell) — stdout only.
                        ResolvedSeg::Call {
                            name: pu.use_.clone(),
                            script,
                        }
                    }
                });
            }
            Ok(ResolvedEntry::Pipe(segs))
        }
    }
}

/// Resolve one step reference: look it up, validate the `with:` binding, and
/// return the rebound script + declared outputs.
fn resolve_use(
    cmd_id: &str,
    step_name: &str,
    with: &IndexMap<String, String>,
    params: &IndexMap<String, ParamDef>,
    ns: &Namespaces,
) -> Result<(String, Vec<String>)> {
    let (step, _) = ns.steps.get(step_name).with_context(|| {
        format!(
            "command `{cmd_id}`: uses step `{step_name}`, which no include exports — available: {}",
            available(ns.steps.keys())
        )
    })?;
    let script = step.script();
    let step_placeholders = interp::scan_placeholders(script);
    for (key, value) in with {
        if !step_placeholders.iter().any(|p| p == key) {
            bail!(
                "command `{cmd_id}`: `with:` binds `{key}`, but step `{step_name}` has no `{{{key}}}` placeholder — it has: {}",
                available(step_placeholders.iter())
            );
        }
        // A `with:` value is a strict mini-template over this command's params.
        for ph in interp::placeholders(value)? {
            if !params.contains_key(&ph) {
                bail!(
                    "command `{cmd_id}`: `with: {key}: \"{value}\"` references `{{{ph}}}`, which is not a declared param",
                );
            }
        }
    }
    Ok((
        interp::rename_placeholders(script, with),
        step.outputs().to_vec(),
    ))
}

fn available<'a>(names: impl Iterator<Item = &'a String>) -> String {
    let list: Vec<_> = names.map(String::as_str).collect();
    if list.is_empty() {
        "(none)".to_string()
    } else {
        list.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write a root pult.yaml + optional module files into a tempdir, resolve.
    fn setup(root: &str, files: &[(&str, &str)]) -> (tempfile::TempDir, Result<Resolved>) {
        let dir = tempfile::tempdir().unwrap();
        for (rel, content) in files {
            let path = dir.path().join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, content).unwrap();
        }
        std::fs::write(dir.path().join("pult.yaml"), root).unwrap();
        let loaded = manifest::load(&dir.path().join("pult.yaml")).unwrap();
        let resolved = resolve(loaded);
        (dir, resolved)
    }

    const MODULE: &str = r#"
version: 1
name: awsmod
vars:
  cluster_prefix: { required: true }
  region: { default: eu-west-2 }
params:
  env: { pick: { options: [dev, uat] } }
  svc: { pick: { from: "${module.dir}/bin/list --cluster ${cluster_prefix}-{env}" } }
steps:
  session: |
    login ${cluster_prefix} in ${region}
  resolve:
    outputs: [TASK]
    script: |
      TASK=$(find {env})
commands:
  - id: shell
    title: Module shell
    params:
      env: { use: env }
    run:
      - use: session
      - use: resolve
      - "connect $TASK"
"#;

    const ROOT: &str = r#"
version: 1
name: rootproj
includes:
  - source: ./mods/aws
    vars: { cluster_prefix: dirconn }
    prefix: aws
commands:
  - id: deploy
    title: Deploy
    params:
      env: { use: aws:env }
      target: { input: { default: all } }
    run:
      - use: aws:session
      - use: aws:resolve
        exports: { TASK: DEPLOY_TASK }
      - "deploy {target} via $DEPLOY_TASK"
"#;

    #[test]
    fn resolves_includes_end_to_end() {
        let (_d, resolved) = setup(ROOT, &[("mods/aws/module.yaml", MODULE)]);
        let r = resolved.unwrap();
        assert_eq!(r.name, "rootproj");
        assert_eq!(r.include_summary, ["./mods/aws"]);

        let ids: Vec<_> = r.commands.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["aws:shell", "deploy"], "includes first, then local");
        assert_eq!(r.commands[0].origin.as_deref(), Some("./mods/aws"));
        assert_eq!(r.commands[1].origin, None);

        // use: param inlined to the module's concrete picker
        let deploy = &r.commands[1];
        let env = &deploy.params["env"];
        assert_eq!(
            env.pick.as_ref().unwrap().options.as_ref().unwrap(),
            &["dev".to_string(), "uat".to_string()]
        );

        // vars substituted into the step script; exports carried through
        let ResolvedRun::Steps(entries) = &deploy.run else {
            panic!()
        };
        let ResolvedEntry::Call(session) = &entries[0] else {
            panic!()
        };
        assert_eq!(session.name, "aws:session");
        assert!(
            session.script.contains("login dirconn in eu-west-2"),
            "got: {}",
            session.script
        );
        let ResolvedEntry::Call(resolve_call) = &entries[1] else {
            panic!()
        };
        assert_eq!(resolve_call.exports["TASK"], "DEPLOY_TASK");
    }

    #[test]
    fn module_dir_is_substituted_in_pick_from() {
        let (dir, resolved) = setup(
            r#"
version: 1
includes:
  - source: ./mods/aws
    vars: { cluster_prefix: x }
commands:
  - id: c
    title: C
    params:
      env: { use: env }
      svc: { use: svc }
    run: "echo {svc}"
"#,
            &[("mods/aws/module.yaml", MODULE)],
        );
        let r = resolved.unwrap();
        let cmd = r.commands.iter().find(|c| c.id == "c").unwrap();
        let from = cmd.params["svc"]
            .pick
            .as_ref()
            .unwrap()
            .from
            .clone()
            .unwrap();
        let expected_dir = dir.path().join("mods/aws").canonicalize().unwrap();
        assert!(
            from.starts_with(&format!("{}/bin/list", expected_dir.display())),
            "got: {from}"
        );
        assert!(from.contains("x-{env}"), "got: {from}");
    }

    #[test]
    fn missing_required_var_errors() {
        let (_d, resolved) = setup(
            "version: 1\nincludes:\n  - source: ./mods/aws\ncommands:\n  - { id: c, title: C, run: \"true\" }\n",
            &[("mods/aws/module.yaml", MODULE)],
        );
        let err = format!("{:#}", resolved.unwrap_err());
        assert!(
            err.contains("missing required var `cluster_prefix`"),
            "got: {err}"
        );
    }

    #[test]
    fn unknown_var_binding_errors() {
        let (_d, resolved) = setup(
            "version: 1\nincludes:\n  - source: ./mods/aws\n    vars: { cluster_prefix: x, nope: y }\ncommands:\n  - { id: c, title: C, run: \"true\" }\n",
            &[("mods/aws/module.yaml", MODULE)],
        );
        let err = format!("{:#}", resolved.unwrap_err());
        assert!(err.contains("unknown var `nope`"), "got: {err}");
        assert!(
            err.contains("cluster_prefix"),
            "should list declared vars, got: {err}"
        );
    }

    #[test]
    fn duplicate_command_id_suggests_prefix() {
        let (_d, resolved) = setup(
            r#"
version: 1
includes:
  - source: ./mods/aws
    vars: { cluster_prefix: x }
commands:
  - id: shell
    title: Local shell
    run: "true"
"#,
            &[("mods/aws/module.yaml", MODULE)],
        );
        // module exports `shell` (no prefix) and root declares `shell`
        let err = format!("{:#}", resolved.unwrap_err());
        assert!(err.contains("duplicate command id `shell`"), "got: {err}");
        assert!(err.contains("prefix"), "got: {err}");
    }

    #[test]
    fn transitive_includes_are_rejected() {
        let (_d, resolved) = setup(
            "version: 1\nincludes:\n  - source: ./m.yaml\ncommands: []\n",
            &[(
                "m.yaml",
                "version: 1\nincludes:\n  - source: ./deeper.yaml\ncommands: []\n",
            )],
        );
        let err = format!("{:#}", resolved.unwrap_err());
        assert!(err.contains("transitive includes"), "got: {err}");
    }

    // ── git modules (local repos as remotes; no network) ──

    fn git_cmd(args: &[&str], cwd: &std::path::Path) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A local git repo standing in for a remote, committed and tagged v1.
    fn make_remote(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (rel, content) in files {
            let path = dir.path().join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, content).unwrap();
        }
        git_cmd(&["init", "-q"], dir.path());
        git_cmd(&["add", "-A"], dir.path());
        git_cmd(
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-qm",
                "init",
            ],
            dir.path(),
        );
        git_cmd(&["tag", "v1"], dir.path());
        dir
    }

    const GIT_MODULE: &str = r#"
version: 1
name: gitmod
steps:
  hi:
    outputs: [WHO]
    script: |
      WHO=$(cat ${module.dir}/who.txt)
commands:
  - id: greet
    title: Greet from git
    run:
      - use: hi
      - "echo hello $WHO"
"#;

    fn root_including(remote: &std::path::Path, suffix: &str) -> String {
        format!(
            "version: 1\nname: consumer\nincludes:\n  - source: git::{}{suffix}\n    prefix: g\ncommands:\n  - {{ id: local, title: L, run: \"true\" }}\n",
            remote.display()
        )
    }

    fn resolve_root(root_yaml: &str, cache: &std::path::Path) -> Result<Resolved> {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("pult.yaml"), root_yaml).unwrap();
        let loaded = manifest::load(&dir.path().join("pult.yaml")).unwrap();
        resolve_with(loaded, Some(cache))
    }

    #[test]
    fn git_include_end_to_end_and_warm_cache() {
        let remote = make_remote(&[
            ("module.yaml", GIT_MODULE),
            ("who.txt", "world"),
            ("bin/helper", "#!/bin/sh\necho helper\n"),
        ]);
        let cache = tempfile::tempdir().unwrap();
        let root = root_including(remote.path(), "@v1");

        let r = resolve_root(&root, cache.path()).unwrap();
        let ids: Vec<_> = r.commands.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["g:greet", "local"]);
        assert!(
            r.include_summary[0].contains("(commit "),
            "got: {:?}",
            r.include_summary
        );
        let PinInfo::Git {
            rev_kind,
            resolved_sha,
            ..
        } = &r.pins[0]
        else {
            panic!("expected git pin")
        };
        assert_eq!(rev_kind, "tag");
        assert_eq!(resolved_sha.len(), 40);

        // whole tree fetched: the module's data file + executable are cached,
        // ${module.dir} resolved into the cache
        let ResolvedRun::Steps(entries) = &r.commands[0].run else {
            panic!()
        };
        let ResolvedEntry::Call(call) = &entries[0] else {
            panic!()
        };
        assert!(call.script.contains("/who.txt"), "got: {}", call.script);
        let dir_in_script = call
            .script
            .split("$(cat ")
            .nth(1)
            .unwrap()
            .trim_end()
            .trim_end_matches("/who.txt)")
            .to_string();
        assert!(
            std::path::Path::new(&dir_in_script)
                .join("bin/helper")
                .is_file()
        );
        assert!(
            !std::path::Path::new(&dir_in_script).join(".git").exists(),
            ".git stripped"
        );

        // warm cache: delete the "remote" entirely; resolution still works
        drop(remote);
        let r2 = resolve_root(&root, cache.path()).unwrap();
        assert_eq!(r2.commands.len(), 2);
        assert_eq!(r.trust_hash, r2.trust_hash, "same pin, same trust");
    }

    #[test]
    fn branch_pins_are_rejected() {
        let remote = make_remote(&[("module.yaml", GIT_MODULE), ("who.txt", "x")]);
        let out = std::process::Command::new("git")
            .args(["symbolic-ref", "--short", "HEAD"])
            .current_dir(remote.path())
            .output()
            .unwrap();
        let branch = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let cache = tempfile::tempdir().unwrap();
        let err = resolve_root(
            &root_including(remote.path(), &format!("@{branch}")),
            cache.path(),
        )
        .unwrap_err();
        let err = format!("{err:#}");
        assert!(err.contains("is a branch"), "got: {err}");
    }

    #[test]
    fn full_sha_pins_resolve() {
        let remote = make_remote(&[("module.yaml", GIT_MODULE), ("who.txt", "x")]);
        git_cmd(
            &["config", "uploadpack.allowAnySHA1InWant", "true"],
            remote.path(),
        );
        let out = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(remote.path())
            .output()
            .unwrap();
        let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let cache = tempfile::tempdir().unwrap();
        let r = resolve_root(
            &root_including(remote.path(), &format!("@{sha}")),
            cache.path(),
        )
        .unwrap();
        let PinInfo::Git {
            rev_kind,
            resolved_sha,
            ..
        } = &r.pins[0]
        else {
            panic!()
        };
        assert_eq!(rev_kind, "sha");
        assert_eq!(*resolved_sha, sha);
    }

    #[test]
    fn subpath_module_within_repo() {
        let remote = make_remote(&[
            ("mods/aws/module.yaml", GIT_MODULE),
            ("mods/aws/who.txt", "sub"),
        ]);
        let cache = tempfile::tempdir().unwrap();
        let root = format!(
            "version: 1\nincludes:\n  - source: git::{}//mods/aws@v1\ncommands:\n  - {{ id: local, title: L, run: \"true\" }}\n",
            remote.path().display()
        );
        let r = resolve_root(&root, cache.path()).unwrap();
        assert!(r.commands.iter().any(|c| c.id == "greet"));
    }

    #[test]
    fn missing_module_yaml_names_the_path() {
        let remote = make_remote(&[("README.md", "not a module")]);
        let cache = tempfile::tempdir().unwrap();
        let err = format!(
            "{:#}",
            resolve_root(&root_including(remote.path(), "@v1"), cache.path()).unwrap_err()
        );
        assert!(err.contains("module.yaml"), "got: {err}");
    }

    #[test]
    fn moved_tag_is_detected_as_drift() {
        let remote = make_remote(&[("module.yaml", GIT_MODULE), ("who.txt", "x")]);
        let cache = tempfile::tempdir().unwrap();
        let root = root_including(remote.path(), "@v1");
        let r = resolve_root(&root, cache.path()).unwrap();
        let PinInfo::Git {
            url,
            rev,
            resolved_sha,
            ..
        } = &r.pins[0]
        else {
            panic!()
        };

        // tag still where we fetched it
        let remote_now = fetch::remote_tag_sha(url, rev).unwrap().unwrap();
        assert_eq!(remote_now, *resolved_sha);

        // move the tag on the "remote"
        std::fs::write(remote.path().join("who.txt"), "moved").unwrap();
        git_cmd(&["add", "-A"], remote.path());
        git_cmd(
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-qm",
                "moved",
            ],
            remote.path(),
        );
        git_cmd(&["tag", "-f", "v1"], remote.path());

        let remote_after = fetch::remote_tag_sha(url, rev).unwrap().unwrap();
        assert_ne!(remote_after, *resolved_sha, "tag moved");
        // the CI guard exits non-zero
        assert_eq!(crate::verify::run(&r).unwrap(), 1);
    }

    #[test]
    fn reserved_command_ids_are_rejected() {
        let (_d, resolved) = setup(
            "version: 1\ncommands:\n  - { id: includes, title: X, run: \"true\" }\n",
            &[],
        );
        let err = format!("{:#}", resolved.unwrap_err());
        assert!(err.contains("reserved"), "got: {err}");
    }

    #[test]
    fn unknown_use_lists_available_steps() {
        let (_d, resolved) = setup(
            r#"
version: 1
steps:
  real: "echo hi"
commands:
  - id: c
    title: C
    run:
      - use: fake
"#,
            &[],
        );
        let err = format!("{:#}", resolved.unwrap_err());
        assert!(err.contains("uses step `fake`"), "got: {err}");
        assert!(err.contains("real"), "should list available, got: {err}");
    }

    #[test]
    fn with_binding_unknown_placeholder_errors() {
        let (_d, resolved) = setup(
            r#"
version: 1
steps:
  s: "echo {env}"
commands:
  - id: c
    title: C
    run:
      - use: s
        with: { nope: "x" }
"#,
            &[],
        );
        let err = format!("{:#}", resolved.unwrap_err());
        assert!(err.contains("no `{nope}` placeholder"), "got: {err}");
    }

    #[test]
    fn with_value_must_reference_declared_params() {
        let (_d, resolved) = setup(
            r#"
version: 1
steps:
  s: "echo {env}"
commands:
  - id: c
    title: C
    run:
      - use: s
        with: { env: "{missing}" }
"#,
            &[],
        );
        let err = format!("{:#}", resolved.unwrap_err());
        assert!(err.contains("{missing}"), "got: {err}");
    }

    #[test]
    fn exports_of_undeclared_output_errors() {
        let (_d, resolved) = setup(
            r#"
version: 1
steps:
  s:
    outputs: [A]
    script: "A=1"
commands:
  - id: c
    title: C
    run:
      - use: s
        exports: { B: C }
"#,
            &[],
        );
        let err = format!("{:#}", resolved.unwrap_err());
        assert!(err.contains("exports renames `B`"), "got: {err}");
    }

    #[test]
    fn colliding_outputs_error_without_rename() {
        let (_d, resolved) = setup(
            r#"
version: 1
steps:
  a:
    outputs: [OUT]
    script: "OUT=1"
  b:
    outputs: [OUT]
    script: "OUT=2"
commands:
  - id: c
    title: C
    run:
      - use: a
      - use: b
"#,
            &[],
        );
        let err = format!("{:#}", resolved.unwrap_err());
        assert!(err.contains("produce output `OUT`"), "got: {err}");
    }

    #[test]
    fn multiline_inline_in_pipe_errors() {
        let (_d, resolved) = setup(
            "version: 1\ncommands:\n  - id: c\n    title: C\n    run:\n      - pipe:\n          - \"line one\\nline two\"\n",
            &[],
        );
        let err = format!("{:#}", resolved.unwrap_err());
        assert!(err.contains("multi-line"), "got: {err}");
    }

    #[test]
    fn root_vars_are_rejected() {
        let (_d, resolved) = setup(
            "version: 1\nvars:\n  x: { default: y }\ncommands:\n  - { id: c, title: C, run: \"true\" }\n",
            &[],
        );
        let err = format!("{:#}", resolved.unwrap_err());
        assert!(err.contains("belongs in included modules"), "got: {err}");
    }

    #[test]
    fn trust_hash_covers_included_bytes() {
        let root = "version: 1\nincludes:\n  - source: ./m.yaml\ncommands:\n  - { id: c, title: C, run: \"true\" }\n";
        let module_v1 = "version: 1\nsteps:\n  s: \"echo one\"\ncommands: []\n";
        let module_v2 = "version: 1\nsteps:\n  s: \"echo two\"\ncommands: []\n";
        let (_d1, r1) = setup(root, &[("m.yaml", module_v1)]);
        let (_d2, r2) = setup(root, &[("m.yaml", module_v2)]);
        assert_ne!(
            r1.unwrap().trust_hash,
            r2.unwrap().trust_hash,
            "changing an included module must change the trust hash"
        );
    }

    #[test]
    fn trust_hash_covers_local_module_executables() {
        let root = "version: 1\nincludes:\n  - source: ./mod\ncommands:\n  - { id: c, title: C, run: \"true\" }\n";
        let module = "version: 1\nsteps:\n  s: \"${module.dir}/bin/tool\"\ncommands: []\n";
        let (_d1, r1) = setup(
            root,
            &[("mod/module.yaml", module), ("mod/bin/tool", "echo one")],
        );
        let (_d2, r2) = setup(
            root,
            &[("mod/module.yaml", module), ("mod/bin/tool", "echo two")],
        );
        assert_ne!(
            r1.unwrap().trust_hash,
            r2.unwrap().trust_hash,
            "changing a shipped executable must change the trust hash"
        );
    }

    #[cfg(unix)]
    #[test]
    fn trust_hash_covers_exec_bit() {
        use std::os::unix::fs::PermissionsExt;
        let root = "version: 1\nincludes:\n  - source: ./mod\ncommands:\n  - { id: c, title: C, run: \"true\" }\n";
        let files: &[(&str, &str)] = &[
            ("mod/module.yaml", "version: 1\ncommands: []\n"),
            ("mod/bin/tool", "echo hi"),
        ];
        let (dir, before) = setup(root, files);
        let before = before.unwrap().trust_hash;
        std::fs::set_permissions(
            dir.path().join("mod/bin/tool"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let loaded = manifest::load(&dir.path().join("pult.yaml")).unwrap();
        let after = resolve(loaded).unwrap().trust_hash;
        assert_ne!(
            before, after,
            "flipping the exec bit must change the trust hash"
        );
    }

    #[test]
    fn sha256_pin_mismatch_is_fatal() {
        let (_d, resolved) = setup(
            "version: 1\nincludes:\n  - source: ./m.yaml\n    sha256: deadbeef\ncommands: []\n",
            &[("m.yaml", "version: 1\ncommands: []\n")],
        );
        let err = format!("{:#}", resolved.unwrap_err());
        assert!(err.contains("sha256 mismatch"), "got: {err}");
    }

    #[test]
    fn run_placeholder_validation_still_strict_for_plain_runs() {
        let (_d, resolved) = setup(
            "version: 1\ncommands:\n  - id: c\n    title: C\n    run: \"echo {nope}\"\n",
            &[],
        );
        let err = format!("{:#}", resolved.unwrap_err());
        assert!(err.contains("nope"), "got: {err}");
    }

    #[test]
    fn dependent_picker_must_reference_earlier_param() {
        let (_d, resolved) = setup(
            "version: 1\ncommands:\n  - id: c\n    title: C\n    params:\n      a: { pick: { from: \"ls {b}\" } }\n      b: { pick: { options: [x] } }\n    run: \"echo {a}\"\n",
            &[],
        );
        let err = format!("{:#}", resolved.unwrap_err());
        assert!(err.contains("not declared before"), "got: {err}");
    }
}
