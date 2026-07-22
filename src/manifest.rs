use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use indexmap::IndexMap;
use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};

/// The manifest schema version this binary understands.
pub const SUPPORTED_VERSION: u32 = 1;

/// A manifest together with where it came from.
#[derive(Debug)]
pub struct Loaded {
    pub manifest: Manifest,
    /// Absolute path to the pult.yaml file.
    pub path: PathBuf,
    /// Directory containing the manifest; all commands run with this as cwd.
    pub dir: PathBuf,
    /// Raw file contents, used for trust hashing.
    pub raw: String,
}

/// One file's worth of schema — serves both root manifests and included
/// modules; the resolver enforces which fields are legal in which role.
#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// Checked via `VersionProbe` before full parsing; kept so the schema is
    /// complete and future versions can branch on it.
    #[allow(dead_code)]
    pub version: u32,
    /// Display name shown in the guided flow header; defaults to the directory name.
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    #[allow(dead_code)] // surfaced by future `pult module info`
    pub description: Option<String>,
    /// Root manifests only. Local `./path` sources in Phase A.
    #[serde(default)]
    pub includes: Vec<IncludeDef>,
    /// Reserved for Phase C — rejected with a clear message if present.
    #[serde(default)]
    #[cfg_attr(test, schemars(with = "Option<serde_json::Value>"))]
    pub registries: Option<serde_yaml::Value>,
    /// Modules only: variables the include site binds.
    #[serde(default)]
    pub vars: IndexMap<String, VarDef>,
    /// Named, reusable param definitions (referenced via `use:`).
    #[serde(default)]
    pub params: IndexMap<String, ParamDef>,
    /// Named, reusable script fragments (referenced via `use:` in run lists).
    #[serde(default)]
    pub steps: IndexMap<String, StepDef>,
    #[serde(default)]
    pub commands: Vec<CommandDef>,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct IncludeDef {
    pub source: String,
    #[serde(default)]
    pub vars: IndexMap<String, String>,
    #[serde(default)]
    pub prefix: Option<String>,
    #[serde(default)]
    pub sha256: Option<String>,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct VarDef {
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default)]
    #[allow(dead_code)] // surfaced by future `pult module info`
    pub description: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct CommandDef {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub params: IndexMap<String, ParamDef>,
    pub run: RunSpec,
    /// Optional readiness probe: a shell command (no `{param}` placeholders —
    /// it runs before params exist) whose exit 0 means "ready to run". Surfaced
    /// by `pult doctor` and by UIs before the run button; never run implicitly
    /// before `run:` — preflight steps in the playbook remain the mitigation.
    #[serde(default)]
    pub check: Option<String>,
    /// Declares that `run:` requires a controlling terminal at runtime (REPLs,
    /// TUIs, shells into containers). The contract: an *undeclared* command
    /// must be fully non-interactive once its params are filled — declare a
    /// param, don't `read` — which is what makes non-terminal surfaces safe.
    /// The plain CLI ignores it (stdio is inherited either way).
    #[serde(default)]
    pub interactive: bool,
    /// Display grouping for the guided flow, the palette, and `--list`: an
    /// author-assigned label ("Deploy", "Tests"). Commands sharing a category
    /// are grouped together regardless of which file declared them — a module
    /// tagging its exports "Deploy" joins the local "Deploy" group. When
    /// unset, grouping falls back to the include origin (see
    /// `ResolvedCommand::group_label`), so a manifest with no categories at
    /// all degrades to today's flat list/menu.
    #[serde(default)]
    pub category: Option<String>,
    /// One or two sentences explaining what the command does, shown by
    /// `--help`, `--list --json`, and UIs — the title names the control, the
    /// description explains it.
    #[serde(default)]
    pub description: Option<String>,
}

/// A param is a picker, free input, or a reference to a named param; exactly
/// one of the three must be set (validated at load, so `kind()` is infallible).
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ParamDef {
    #[serde(default)]
    pub pick: Option<PickDef>,
    #[serde(default)]
    pub input: Option<InputDef>,
    #[serde(default, rename = "use")]
    pub use_: Option<String>,
}

pub enum ParamKind<'a> {
    Pick(&'a PickDef),
    Input(&'a InputDef),
    Use(&'a str),
}

impl ParamDef {
    pub fn kind(&self) -> ParamKind<'_> {
        match (&self.pick, &self.input, &self.use_) {
            (Some(pick), None, None) => ParamKind::Pick(pick),
            (None, Some(input), None) => ParamKind::Input(input),
            (None, None, Some(name)) => ParamKind::Use(name),
            _ => unreachable!("validated at load: exactly one of pick/input/use"),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct PickDef {
    /// Static option list.
    #[serde(default)]
    pub options: Option<Vec<OptionDef>>,
    /// Shell command whose stdout lines become the options. May interpolate
    /// `{param}` for params declared *earlier* in the same command.
    #[serde(default)]
    pub from: Option<String>,
}

/// A pick option: a plain value, or a value with a display description.
///
/// Deliberately *not* `#[serde(untagged)]` — see the manual `Deserialize`
/// below for why. The `JsonSchema` derive can't read a `#[serde(untagged)]`
/// attribute off a manually-`Deserialize`d type, so it gets schemars' own
/// untagged marker (test-only) to keep the emitted schema
/// "string-or-mapping", matching the `StepDef` idiom.
#[derive(Debug, Clone)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[cfg_attr(test, schemars(untagged))]
pub enum OptionDef {
    Plain(String),
    Full(FullOption),
}

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct FullOption {
    pub value: String,
    /// Shown as `value — description` in the picker; display-only.
    #[serde(default)]
    pub description: Option<String>,
}

// Manual impl: a scalar (string/int/bool) → Plain(text); a mapping → Full.
// deserialize_any is required to branch on the node kind, which is *why* the
// derived untagged form is wrong (§2a of the design doc: it silently drops
// non-string scalar options that load today).
impl<'de> Deserialize<'de> for OptionDef {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = OptionDef;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a value string or a {value, description} mapping")
            }
            fn visit_str<E: de::Error>(self, s: &str) -> Result<OptionDef, E> {
                Ok(OptionDef::Plain(s.to_owned()))
            }
            fn visit_bool<E: de::Error>(self, b: bool) -> Result<OptionDef, E> {
                Ok(OptionDef::Plain(b.to_string()))
            }
            fn visit_i64<E: de::Error>(self, n: i64) -> Result<OptionDef, E> {
                Ok(OptionDef::Plain(n.to_string()))
            }
            fn visit_u64<E: de::Error>(self, n: u64) -> Result<OptionDef, E> {
                Ok(OptionDef::Plain(n.to_string()))
            }
            fn visit_f64<E: de::Error>(self, _n: f64) -> Result<OptionDef, E> {
                // A float scalar reaches us as an f64 with the source text
                // already lost (`1.10` → 1.1); silently accepting would
                // corrupt a value that works verbatim today. Fail loud; the
                // author quotes it.
                Err(E::custom(
                    "option values that look like floats must be quoted, e.g. \"1.10\"",
                ))
            }
            fn visit_map<A: MapAccess<'de>>(self, m: A) -> Result<OptionDef, A::Error> {
                FullOption::deserialize(de::value::MapAccessDeserializer::new(m))
                    .map(OptionDef::Full)
            }
        }
        d.deserialize_any(V)
    }
}

impl OptionDef {
    pub fn value(&self) -> &str {
        match self {
            OptionDef::Plain(s) => s,
            OptionDef::Full(f) => &f.value,
        }
    }

    /// `None` for `Plain`, and for a `Full` whose description is absent or
    /// blank.
    pub fn description(&self) -> Option<&str> {
        match self {
            OptionDef::Plain(_) => None,
            // trim().is_empty(), matching validate_param's rule — a
            // whitespace-only description is load-rejected, but this keeps
            // the accessor itself defense-in-depth-consistent for any
            // caller that runs before validation.
            OptionDef::Full(f) => f.description.as_deref().filter(|d| !d.trim().is_empty()),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct InputDef {
    #[serde(default)]
    pub default: Option<String>,
    /// Secret values are prompted without echo and masked wherever an
    /// interpolated command line is displayed (`running:` banner, `--print`,
    /// the ephemeral trust prompt). A `default:` is rejected for secrets — a
    /// default would be a credential committed to the manifest.
    #[serde(default)]
    pub secret: bool,
}

/// A step: a plain script string, or a script with a declared contract.
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(untagged)]
pub enum StepDef {
    Plain(String),
    Full(FullStep),
}

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct FullStep {
    pub script: String,
    /// Shell variable names this step promises to set.
    #[serde(default)]
    pub outputs: Vec<String>,
}

impl StepDef {
    pub fn script(&self) -> &str {
        match self {
            StepDef::Plain(s) => s,
            StepDef::Full(f) => &f.script,
        }
    }

    pub fn outputs(&self) -> &[String] {
        match self {
            StepDef::Plain(_) => &[],
            StepDef::Full(f) => &f.outputs,
        }
    }
}

/// `run:` is a single command line (executed via `sh -c`, exactly the original
/// behavior) or a step list compiled into one bash script (see compile.rs).
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(untagged)]
pub enum RunSpec {
    Script(String),
    List(Vec<RunEntry>),
}

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(untagged)]
pub enum RunEntry {
    Inline(String),
    Use(UseRef),
    Pipe(PipeGroup),
}

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct UseRef {
    #[serde(rename = "use")]
    pub use_: String,
    /// Rebind the step's `{placeholder}` names (values may reference this
    /// command's params).
    #[serde(default)]
    pub with: IndexMap<String, String>,
    /// Rename the step's declared outputs: `{ TASK: BACKEND_TASK }`.
    #[serde(default)]
    pub exports: IndexMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct PipeGroup {
    pub pipe: Vec<PipeEntry>,
}

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(untagged)]
pub enum PipeEntry {
    Inline(String),
    Use(PipeUseRef),
}

/// A step in a pipe contributes stdout only, so no `exports:` here.
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct PipeUseRef {
    #[serde(rename = "use")]
    pub use_: String,
    #[serde(default)]
    pub with: IndexMap<String, String>,
}

/// Probe just the version field so a future-versioned manifest fails with an
/// "upgrade pult" message instead of a parse error about unknown fields.
#[derive(Deserialize)]
struct VersionProbe {
    version: Option<u32>,
}

pub fn load(path: &Path) -> Result<Loaded> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let manifest = parse(&raw, path)?;
    let path = path
        .canonicalize()
        .with_context(|| format!("failed to resolve {}", path.display()))?;
    let dir = path
        .parent()
        .context("manifest has no parent directory")?
        .to_path_buf();
    Ok(Loaded {
        manifest,
        path,
        dir,
        raw,
    })
}

/// Parse + intra-file validation. Used for both root manifests and modules;
/// role-specific rules (includes only at root, vars only in modules, ordering
/// of dependent pickers, run/step reference checks) live in the resolver.
pub fn parse(raw: &str, origin: &Path) -> Result<Manifest> {
    let probe: VersionProbe = serde_yaml::from_str(raw)
        .with_context(|| format!("{} is not valid YAML", origin.display()))?;
    match probe.version {
        None => bail!(
            "{} has no `version:` field — add `version: {SUPPORTED_VERSION}`",
            origin.display()
        ),
        Some(v) if v > SUPPORTED_VERSION => bail!(
            "{} uses manifest version {v}, but this pult binary supports up to {SUPPORTED_VERSION} — upgrade pult",
            origin.display()
        ),
        Some(v) if v < 1 => bail!("{} has invalid manifest version {v}", origin.display()),
        Some(_) => {}
    }
    let manifest: Manifest = serde_yaml::from_str(raw)
        .with_context(|| format!("failed to parse {}", origin.display()))?;
    validate_file(&manifest).with_context(|| format!("invalid manifest {}", origin.display()))?;
    Ok(manifest)
}

/// A valid name for params, steps, vars, outputs, prefixes.
pub fn is_valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
}

fn validate_file(manifest: &Manifest) -> Result<()> {
    if manifest.registries.is_some() {
        bail!(
            "`registries:` (remote sources) is not built yet — only local `./path` includes are supported"
        );
    }
    for (name, def) in &manifest.params {
        validate_param(name, def).context("in top-level `params:`")?;
    }
    for (name, step) in &manifest.steps {
        validate_step(name, step)?;
    }
    for (vname, vdef) in &manifest.vars {
        if !is_valid_name(vname) {
            bail!("invalid var name `{vname}`");
        }
        if vdef.required && vdef.default.is_some() {
            bail!("var `{vname}`: `required: true` and a `default` are contradictory");
        }
        if !vdef.required && vdef.default.is_none() {
            bail!("var `{vname}` must be `required: true` or have a `default`");
        }
    }
    let mut ids = HashSet::new();
    for cmd in &manifest.commands {
        if cmd.id.is_empty() {
            bail!("a command has an empty id");
        }
        if !ids.insert(cmd.id.as_str()) {
            bail!("duplicate command id `{}`", cmd.id);
        }
        for (name, def) in &cmd.params {
            validate_param(name, def).with_context(|| format!("in command `{}`", cmd.id))?;
        }
        match &cmd.run {
            RunSpec::Script(s) if s.trim().is_empty() => {
                bail!("command `{}` has an empty run", cmd.id)
            }
            RunSpec::List(entries) if entries.is_empty() => {
                bail!("command `{}` has an empty run list", cmd.id)
            }
            _ => {}
        }
        if let Some(check) = &cmd.check
            && check.trim().is_empty()
        {
            bail!("command `{}` has an empty check", cmd.id);
        }
        if let Some(category) = &cmd.category
            && category.trim().is_empty()
        {
            bail!("command `{}` has an empty category", cmd.id);
        }
        if let Some(description) = &cmd.description
            && description.trim().is_empty()
        {
            bail!("command `{}` has an empty description", cmd.id);
        }
    }
    Ok(())
}

fn validate_param(name: &str, def: &ParamDef) -> Result<()> {
    if !is_valid_name(name) {
        bail!("invalid param name `{name}`");
    }
    let set = [def.pick.is_some(), def.input.is_some(), def.use_.is_some()];
    if set.iter().filter(|b| **b).count() != 1 {
        bail!("param `{name}`: needs exactly one of `pick`, `input`, or `use`");
    }
    if let Some(pick) = &def.pick {
        match (&pick.options, &pick.from) {
            (Some(_), Some(_)) => bail!(
                "param `{name}`: `pick` must have exactly one of `options` or `from`, not both"
            ),
            (None, None) => bail!("param `{name}`: `pick` needs either `options` or `from`"),
            _ => {}
        }
        if let Some(options) = &pick.options {
            for opt in options {
                // Uniform across Plain and Full — `value()` abstracts the
                // variant, so a bare `Plain("")`/`Plain("  ")` (e.g.
                // `options: ["", dev]`) is rejected exactly like an empty
                // `Full.value`, instead of only the latter slipping through.
                if opt.value().trim().is_empty() {
                    bail!("param `{name}`: an option has an empty `value`");
                }
                if let OptionDef::Full(full) = opt
                    && let Some(description) = &full.description
                    && description.trim().is_empty()
                {
                    bail!("param `{name}`: an option has an empty `description`");
                }
            }
        }
    }
    if let Some(input) = &def.input
        && input.secret
        && input.default.is_some()
    {
        bail!(
            "param `{name}`: a secret input can't have a `default` — that would commit a credential to the manifest"
        );
    }
    Ok(())
}

fn validate_step(name: &str, step: &StepDef) -> Result<()> {
    if !is_valid_name(name) {
        bail!("invalid step name `{name}`");
    }
    if step.script().trim().is_empty() {
        bail!("step `{name}` has an empty script");
    }
    for out in step.outputs() {
        let shell_ok = !out.is_empty()
            && !out.starts_with(|c: char| c.is_ascii_digit())
            && out.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !shell_ok {
            bail!("step `{name}`: output `{out}` is not a valid shell variable name");
        }
    }
    Ok(())
}

/// The manifest JSON Schema, pretty-printed with a trailing newline. Derived
/// from the structs above (test-only), so it can't drift from what pult
/// actually parses. The committed `pult.schema.json` — served by
/// `pult self schema` and referenced by `pult init`'s modeline — must equal
/// this; the drift test enforces it, the (ignored) regen test rewrites it.
#[cfg(test)]
fn generated_schema() -> String {
    let schema = schemars::schema_for!(Manifest);
    let mut s = serde_json::to_string_pretty(&schema).expect("schema serializes");
    s.push('\n');
    s
}

#[cfg(test)]
const SCHEMA_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/pult.schema.json");

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn committed_schema_is_current() {
        let committed = std::fs::read_to_string(SCHEMA_PATH).unwrap_or_default();
        assert_eq!(
            committed,
            generated_schema(),
            "pult.schema.json is stale — regenerate: \
             `cargo test regenerate_schema -- --ignored`"
        );
    }

    /// Not a test — the writer for the committed schema. Run explicitly after
    /// changing the manifest structs: `cargo test regenerate_schema -- --ignored`.
    #[test]
    #[ignore = "writer, not a check"]
    fn regenerate_schema() {
        std::fs::write(SCHEMA_PATH, generated_schema()).unwrap();
    }

    fn write_manifest(content: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pult.yaml");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        (dir, path)
    }

    const GOOD: &str = r#"
version: 1
name: demo
commands:
  - id: shell
    title: Open a shell
    params:
      env: { pick: { options: [dev, uat, pre] } }
      customer: { pick: { from: "./bin/impl list --env {env}" } }
      note: { input: { default: hi } }
    run: "./bin/impl shell {customer} {env}"
"#;

    #[test]
    fn parses_and_preserves_param_order() {
        let (_d, path) = write_manifest(GOOD);
        let loaded = load(&path).unwrap();
        let cmd = &loaded.manifest.commands[0];
        let names: Vec<_> = cmd.params.keys().cloned().collect();
        assert_eq!(names, ["env", "customer", "note"]);
        assert_eq!(loaded.manifest.name.as_deref(), Some("demo"));
    }

    #[test]
    fn parses_blocks_and_run_lists() {
        let (_d, path) = write_manifest(
            r#"
version: 1
params:
  env: { pick: { options: [dev, uat] } }
steps:
  plain: "echo hi"
  contract:
    outputs: [OUT]
    script: "OUT=42"
commands:
  - id: x
    title: X
    params:
      env: { use: env }
    run:
      - use: contract
        exports: { OUT: RESULT }
      - pipe:
          - use: plain
          - "tr a-z A-Z"
      - "echo $RESULT"
"#,
        );
        let loaded = load(&path).unwrap();
        let m = &loaded.manifest;
        assert_eq!(m.steps["contract"].outputs(), ["OUT"]);
        let RunSpec::List(entries) = &m.commands[0].run else {
            panic!("expected run list");
        };
        assert_eq!(entries.len(), 3);
        assert!(matches!(&entries[0], RunEntry::Use(u) if u.exports["OUT"] == "RESULT"));
        assert!(matches!(&entries[1], RunEntry::Pipe(p) if p.pipe.len() == 2));
    }

    #[test]
    fn newer_version_asks_for_upgrade() {
        let (_d, path) = write_manifest("version: 99\nfuture_field: x\ncommands: []\n");
        let err = load(&path).unwrap_err().to_string();
        assert!(err.contains("upgrade pult"), "got: {err}");
    }

    #[test]
    fn missing_version_is_rejected() {
        let (_d, path) = write_manifest("commands: []\n");
        let err = load(&path).unwrap_err().to_string();
        assert!(err.contains("version"), "got: {err}");
    }

    #[test]
    fn pick_needs_exactly_one_source() {
        let (_d, path) = write_manifest(
            "version: 1\ncommands:\n  - id: x\n    title: X\n    params:\n      a: { pick: { options: [1], from: \"ls\" } }\n    run: \"echo {a}\"\n",
        );
        let err = format!("{:#}", load(&path).unwrap_err());
        assert!(err.contains("not both"), "got: {err}");
    }

    #[test]
    fn var_must_be_required_or_defaulted() {
        let (_d, path) = write_manifest(
            "version: 1\nvars:\n  x: { description: hm }\ncommands:\n  - { id: a, title: A, run: \"true\" }\n",
        );
        let err = format!("{:#}", load(&path).unwrap_err());
        assert!(err.contains("required: true"), "got: {err}");
    }

    #[test]
    fn secret_input_rejects_a_default() {
        let (_d, path) = write_manifest(
            "version: 1\ncommands:\n  - id: x\n    title: X\n    params:\n      \
             token: { input: { secret: true, default: oops } }\n    run: \"echo {token}\"\n",
        );
        let err = format!("{:#}", load(&path).unwrap_err());
        assert!(err.contains("credential"), "got: {err}");
    }

    #[test]
    fn empty_check_is_rejected() {
        let (_d, path) = write_manifest(
            "version: 1\ncommands:\n  - { id: x, title: X, run: \"true\", check: \"  \" }\n",
        );
        let err = format!("{:#}", load(&path).unwrap_err());
        assert!(err.contains("empty check"), "got: {err}");
    }

    #[test]
    fn category_parses() {
        let (_d, path) = write_manifest(
            "version: 1\ncommands:\n  - { id: a, title: A, run: \"true\", category: Deploy }\n",
        );
        let loaded = load(&path).unwrap();
        assert_eq!(
            loaded.manifest.commands[0].category.as_deref(),
            Some("Deploy")
        );
    }

    #[test]
    fn blank_category_is_rejected() {
        let (_d, path) = write_manifest(
            "version: 1\ncommands:\n  - { id: a, title: A, run: \"true\", category: \"  \" }\n",
        );
        let err = format!("{:#}", load(&path).unwrap_err());
        assert!(err.contains("empty category"), "got: {err}");
    }

    #[test]
    fn description_parses() {
        let (_d, path) = write_manifest(
            "version: 1\ncommands:\n  - { id: a, title: A, run: \"true\", description: Deploys the app }\n",
        );
        let loaded = load(&path).unwrap();
        assert_eq!(
            loaded.manifest.commands[0].description.as_deref(),
            Some("Deploys the app")
        );
    }

    #[test]
    fn blank_description_is_rejected() {
        let (_d, path) = write_manifest(
            "version: 1\ncommands:\n  - { id: a, title: A, run: \"true\", description: \"  \" }\n",
        );
        let err = format!("{:#}", load(&path).unwrap_err());
        assert!(err.contains("empty description"), "got: {err}");
    }

    #[test]
    fn registries_are_rejected_clearly() {
        let (_d, path) = write_manifest(
            "version: 1\nregistries: { acme: s3://x }\ncommands:\n  - { id: a, title: A, run: \"true\" }\n",
        );
        let err = format!("{:#}", load(&path).unwrap_err());
        assert!(err.contains("not built yet"), "got: {err}");
    }

    // ── pick option descriptions (§7.2, §7.2a, §7.2b, §7.10) ──

    #[test]
    fn full_option_parses_with_description() {
        let (_d, path) = write_manifest(
            "version: 1\ncommands:\n  - id: a\n    title: A\n    params:\n      \
             env: { pick: { options: [dev, { value: uat, description: \"User acceptance\" }, prod] } }\n    \
             run: \"echo {env}\"\n",
        );
        let loaded = load(&path).unwrap();
        let opts = loaded.manifest.commands[0].params["env"]
            .pick
            .as_ref()
            .unwrap()
            .options
            .as_ref()
            .unwrap();
        assert_eq!(opts[0].value(), "dev");
        assert_eq!(opts[0].description(), None);
        assert_eq!(opts[1].value(), "uat");
        assert_eq!(opts[1].description(), Some("User acceptance"));
        assert_eq!(opts[2].value(), "prod");
    }

    #[test]
    fn full_option_rejects_unknown_field() {
        let (_d, path) = write_manifest(
            "version: 1\ncommands:\n  - id: a\n    title: A\n    params:\n      \
             env: { pick: { options: [{ value: uat, desc: x }] } }\n    run: \"echo {env}\"\n",
        );
        let err = format!("{:#}", load(&path).unwrap_err());
        assert!(err.contains("unknown field"), "got: {err}");
        assert!(err.contains("desc"), "got: {err}");
    }

    /// §7.2a — the §2a regression guard: non-string scalar options still
    /// load. Redden by switching `OptionDef` to `#[serde(untagged)]`.
    #[test]
    fn non_string_scalar_options_still_load() {
        let (_d, path) = write_manifest(
            "version: 1\ncommands:\n  - id: a\n    title: A\n    params:\n      \
             env: { pick: { options: [1, true, 8080] } }\n    run: \"echo {env}\"\n",
        );
        let loaded = load(&path).unwrap();
        let opts = loaded.manifest.commands[0].params["env"]
            .pick
            .as_ref()
            .unwrap()
            .options
            .as_ref()
            .unwrap();
        let values: Vec<&str> = opts.iter().map(OptionDef::value).collect();
        assert_eq!(values, ["1", "true", "8080"]);
    }

    /// §7.2b — unquoted float options are rejected at load; quoted ones
    /// accepted. Redden by making `visit_f64` return `Plain(n.to_string())`.
    #[test]
    fn unquoted_float_option_is_rejected() {
        let (_d, path) = write_manifest(
            "version: 1\ncommands:\n  - id: a\n    title: A\n    params:\n      \
             env: { pick: { options: [1.5] } }\n    run: \"echo {env}\"\n",
        );
        let err = format!("{:#}", load(&path).unwrap_err());
        assert!(err.contains("quoted"), "got: {err}");
    }

    #[test]
    fn quoted_float_option_is_accepted() {
        let (_d, path) = write_manifest(
            "version: 1\ncommands:\n  - id: a\n    title: A\n    params:\n      \
             env: { pick: { options: [\"1.5\"] } }\n    run: \"echo {env}\"\n",
        );
        let loaded = load(&path).unwrap();
        let opts = loaded.manifest.commands[0].params["env"]
            .pick
            .as_ref()
            .unwrap()
            .options
            .as_ref()
            .unwrap();
        assert_eq!(opts[0].value(), "1.5");
    }

    /// §7.10 — blank descriptions and empty `Full` values are rejected.
    #[test]
    fn blank_option_description_is_rejected() {
        let (_d, path) = write_manifest(
            "version: 1\ncommands:\n  - id: a\n    title: A\n    params:\n      \
             env: { pick: { options: [{ value: uat, description: \"  \" }] } }\n    run: \"echo {env}\"\n",
        );
        let err = format!("{:#}", load(&path).unwrap_err());
        assert!(err.contains("empty `description`"), "got: {err}");
    }

    #[test]
    fn empty_option_value_is_rejected() {
        let (_d, path) = write_manifest(
            "version: 1\ncommands:\n  - id: a\n    title: A\n    params:\n      \
             env: { pick: { options: [{ value: \"  \", description: x }] } }\n    run: \"echo {env}\"\n",
        );
        let err = format!("{:#}", load(&path).unwrap_err());
        assert!(err.contains("empty `value`"), "got: {err}");
    }

    /// Code-review finding #1: a bare blank `Plain` scalar option (not just
    /// an empty `Full.value`) is rejected uniformly, via `opt.value()`.
    /// Redden by reverting to the variant-only (`OptionDef::Full` match arm)
    /// check.
    #[test]
    fn blank_plain_option_is_rejected() {
        let (_d, path) = write_manifest(
            "version: 1\ncommands:\n  - id: a\n    title: A\n    params:\n      \
             env: { pick: { options: [\"\", dev] } }\n    run: \"echo {env}\"\n",
        );
        let err = format!("{:#}", load(&path).unwrap_err());
        assert!(err.contains("empty `value`"), "got: {err}");
    }

    #[test]
    fn whitespace_only_plain_option_is_rejected() {
        let (_d, path) = write_manifest(
            "version: 1\ncommands:\n  - id: a\n    title: A\n    params:\n      \
             env: { pick: { options: [\"  \"] } }\n    run: \"echo {env}\"\n",
        );
        let err = format!("{:#}", load(&path).unwrap_err());
        assert!(err.contains("empty `value`"), "got: {err}");
    }

    /// Code-review finding #3: `OptionDef::description()` filters with
    /// `trim().is_empty()`, matching `validate_param`'s predicate — a
    /// whitespace-only description never leaks `Some("  ")` to a caller
    /// that runs before load validation. Redden by reverting the accessor
    /// to a bare `is_empty()` filter.
    #[test]
    fn description_accessor_treats_whitespace_only_as_absent() {
        let full = FullOption {
            value: "uat".to_string(),
            description: Some("   ".to_string()),
        };
        let opt = OptionDef::Full(full);
        assert_eq!(opt.description(), None);
    }
}
