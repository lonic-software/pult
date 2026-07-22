# Design: per-value descriptions for `pick:` params

Status: **signed off — cleared for implementation** (in the §9 order).

Author: advisor (Fable, high) authored the substance; orchestrator transcribed and grounded every
`file:line` against source this session. A second advisor pass (Fable, high, adversarial)
reviewed the draft and found one blocker (§2a, derived-untagged silently drops non-string scalar
options — *reproduced independently* against serde_yaml 0.9) plus three lesser fixes; all are
folded in. Both intentional behavior changes are user-confirmed: the tab-in-value split (§3) and
unquoted-float rejection (§2b, "quote it" over silent `1.10`→`1.1` corruption).

Load-bearing claims are tagged `VERIFIED` (opened at the cited line this session),
`ARGUED` (follows from verified facts by the reasoning given), or `ASSUMED` (neither —
none survive as load-bearing here). Tags are kept visible; strip on ship if they clutter.

---

## 1. Goal

A picker param (`pick:`) should let each *option value* carry an optional human-facing
**description**, rendered next to the value in the interactive picker using the existing
house convention `Value — description` — the same one command menu rows already use
(`menu_label`, flow.rs:98-139, `VERIFIED`). Both option sources need it:

- **Static** — `pick: { options: [dev, uat, prod] }` (PickDef.options, manifest.rs:157-165, `VERIFIED`).
- **Dynamic** — `pick: { from: "<shell command>" }`, whose stdout lines become options
  (`resolve_pick`, options.rs:12-48, `VERIFIED`). A script must be able to *optionally*
  attach a description per emitted value.

**The value passed to the command stays the value; the description is display-only.** This
is the single invariant the whole design rests on (§7.1).

Out of scope / unaffected (blast-radius exclusions, all `VERIFIED`):
- Preview / `--print` / ephemeral-trust go through `fill(cmd, provided, None)`, which never
  resolves option sources (exec.rs:172; guarded by the test at exec.rs:205-237). No change.
- Secrets are `input:`-only (manifest.rs:170-179); picks are never secret.
- The journal records post-fill values, which remain plain strings (exec.rs:107-121).

---

## 2. Static YAML schema

Each option becomes **a scalar or a mapping**. It is *shaped* like the `StepDef` untagged
idiom (manifest.rs:182-214, `VERIFIED`), but it **must not use `#[serde(untagged)]`** — that
derive is a correctness bug here, proven below (§2a). Instead, hand-write `Deserialize` with a
scalar-aware visitor.

```rust
/// A pick option: a plain value, or a value with a display description.
#[derive(Debug, Clone)]
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
// derived untagged form is wrong (§2a).
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
                // A float scalar reaches us as an f64 with the source text already
                // lost (`1.10` → 1.1); silently accepting would corrupt a value that
                // works verbatim today. Fail loud; the author quotes it. See §2b.
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
    pub fn value(&self) -> &str { /* Plain(s)=>s, Full(f)=>&f.value */ }
    /// None for Plain, and for a Full whose description is absent or blank.
    pub fn description(&self) -> Option<&str> { /* filter non-empty */ }
}
```

**Schema derive** (`ARGUED`, `VERIFIED` mechanism): with a hand-written `Deserialize`, the
`JsonSchema` derive can no longer read a `#[serde(untagged)]` attribute off `OptionDef`. Derive
it with schemars' own untagged marker so `committed_schema_is_current` (manifest.rs:462-471,
`VERIFIED`) still emits "string-or-mapping":
`#[cfg_attr(test, derive(schemars::JsonSchema))] #[cfg_attr(test, schemars(untagged))]` on the
enum. (The schema already says `string` while today's parser coerces `[1]` to `"1"` — that
string-vs-coerced-scalar gap is pre-existing, not introduced here.)

`PickDef.options` becomes `Option<Vec<OptionDef>>` (was `Option<Vec<String>>`,
manifest.rs:160, `VERIFIED`). YAML:

```yaml
env:
  pick:
    options:
      - dev
      - value: uat
        description: User acceptance — mirrors prod data
      - prod
```

### 2a. Why not `#[serde(untagged)]` — the load-bearing correctness reason (`VERIFIED` by experiment)

The obvious `#[serde(untagged)]` derive **silently drops every non-string scalar option that
loads today.** Cause: an untagged enum deserializes via `deserialize_any`, which makes serde_yaml
resolve `1`/`true`/`1.10` as a *typed* number/bool and buffer it; `Plain(String)` then fails
against the buffered non-string, and since no variant matches, the whole option array errors.
Today's `Vec<String>` instead drives `deserialize_string`, and serde_yaml hands the raw scalar
text over verbatim — so `[1, 2]` loads as `"1","2"`.

Proven empirically against the repo's exact dependency (serde_yaml 0.9), derived-untagged vs the
manual visitor above:

| YAML | today `Vec<String>` | derived `#[untagged]` | manual visitor (§2) |
|---|---|---|---|
| `[dev, uat]` | `dev,uat` | ok | ok |
| `[1, 2]` (counts) | `1,2` | **Err "no variant"** | `1,2` |
| `[true, false]` | verbatim | **Err** | `true,false` |
| `[8080]` (ports) | `8080` | **Err** | `8080` |
| `[{value: 8080, description: Port}]` | (n/a today) | **Err** | `Full{value:"8080",…}` |
| `[{value: uat, desc: x}]` | (n/a today) | Err (vague) | **Err "unknown field `desc`"** |

The repo's own test **proves the regression is real**: `pick_needs_exactly_one_source`
(manifest.rs:564-570, `VERIFIED`) feeds `options: [1]` and asserts a *validation* error ("not
both") — i.e. `options: [1]` **parses** into `Vec<String>` today. `options: [8080, 9090]`,
`[1, 2, 3]`, `[1.21, 1.22]` are all plausible real manifests that the naive derive would stop
loading. The `StepDef` untagged precedent doesn't cover this because a step script is never a
bare number.

The manual visitor fixes all rows, and as a bonus turns the vague untagged mismatch into a
precise `unknown field \`desc\`, expected \`value\` or \`description\`` — so the "vague untagged
error" trade the earlier draft accepted **disappears** (removed from §10).

### 2b. Floats: fail loud, don't corrupt (`VERIFIED` by experiment)

A float scalar reaches `visit_f64` as an `f64` with source text already gone: `1.10` arrives as
`1.1`, indistinguishable from an authored `1.1` — the probe shows `[1.10, 1.21]` → `"1.1","1.21"`.
Lossy acceptance would silently corrupt a version-like value (`1.10` ≠ `1.1` to a human reader)
that works verbatim today. §2's `visit_f64` therefore **hard-errors** with "quote it," which is
loud and trivially remedied.

> **This is a second intentional behavior change** (alongside the §3 tab split, §10): an
> *unquoted* float option that loads today (`options: [1.5]`) becomes a load-time error tomorrow,
> demanding `["1.5"]`. Integers and bools that fit `i64`/`u64` are unaffected (they round-trip
> losslessly). **One further narrowing** (flagged in the medium code review): an integer literal
> *outside* `i64`/`u64` range (e.g. a 21-digit `options: [123456789012345678901]`) loaded as a
> string before this change, but serde_yaml now resolves the overflowing literal as an `f64`, so it
> too hits `visit_f64` and is rejected with the same "quote it" message. Same remedy (quote it),
> same near-zero exposure — pick options are overwhelmingly identifiers, not 20-digit numbers — and
> consistent with the "quote ambiguous numerics" rule; recorded here so the narrowing is not
> silent. Near-zero real exposure overall, and the fix is one pair of quotes. **Signed off at
> review** (the alternative, lossy `n.to_string()`, is a one-line change if the corruption risk is
> ever preferred).

**`deny_unknown_fields`** (`ARGUED`): `PickDef` still has one `options` field, so its own
`deny_unknown_fields` (manifest.rs:159-165, `VERIFIED`) is unaffected. `FullOption` carries its
own, so `{value: dev, desc: x}` is rejected with the precise message shown above. The
scalar-vs-mapping branch cannot be ambiguous: a YAML node is exactly one of the two, and the
visitor dispatches on that.

**Validation** — extend `validate_param` (manifest.rs:395-421, `VERIFIED`): for each `Full`
option, (a) `value` non-empty after trim; (b) `description`, if `Some`, non-blank after trim
— mirroring the command-description rule (manifest.rs:386-390, `VERIFIED`). No duplicate-value
rejection: duplicates are legal today and index-based selection (§4) makes them harmless.

**Committed schema** (`VERIFIED`): the drift test `committed_schema_is_current`
(manifest.rs:462-471) reddens the moment `PickDef` changes and stays red until
`cargo test regenerate_schema -- --ignored` (manifest.rs:475-479) rewrites `pult.schema.json`.
That regenerated file ships in the same PR.

**Rejected:**
- Mapping form `options: {dev: Sandbox}` — collapses duplicate keys silently, reads unordered,
  diverges from the `StepDef` idiom and from the `(value, description)` pair shape of the
  dynamic format.
- Parallel `descriptions:` map beside `options:` — splits one fact across two fields,
  typo-prone keys, no dynamic analogue.

---

## 3. Dynamic wire format

**Tab-separated, split at the first tab.** This is the fzf / gum / `kubectl -o custom-columns`
lineage, produced trivially by `printf '%s\t%s\n' "$value" "$desc"`.

Parse rule per stdout line, replacing options.rs:38-43 (`VERIFIED` current behavior):

0. **Blank after trim** (`line.trim().is_empty()`) → **skip**, *before any tab handling*. This
   preserves today's skip-blank contract (options.rs:40-41, `VERIFIED`) for whitespace-only lines
   that happen to contain a tab (`" \t "`, a bare `"\t"`, tab-indented heredoc chaff). Without
   this rule such lines would wrongly hit the rule-2 hard error, contradicting the very rationale
   below that "a blank line can be incidental formatting." Order matters: rule 0 fires first.
1. **No `\t`** (and not blank) → `line.trim()` value-only. **Byte-identical to today**
   (options.rs:39-41, `VERIFIED`).
2. **Contains `\t`** (and not blank) → split the **original** line at the **first** tab.
   `value = left.trim()`, `desc = right.trim()` (later tabs remain inside `desc` verbatim).
   - `value` empty after trim → **hard error** naming the source: `option source \`{cmd}\`
     emitted a line with an empty value before the tab`. Rationale: a blank line *can* be
     incidental formatting (so it stays silently skipped, options.rs:41, `VERIFIED`), but a
     tab-then-description line cannot be intentional — under this format it is almost certainly
     `printf '%s\t%s\n' "$v" "$d"` with `$v` empty/unset, the exact bug to catch at its source.
     Skipping would instead drop an option the author intended and hand the user a silently
     incomplete picker — the worst failure mode this feature has. The severity matches
     `resolve_pick`'s existing posture toward author errors: nonzero exit bails (options.rs:31-36,
     `VERIFIED`), zero options bails (options.rs:44-46, `VERIFIED`) — loud, not lossy. A producer
     that legitimately emits tab-bearing chaff filters it in the script (`grep -v`).
   - `desc` empty after trim → `description: None`, so `printf 'a\t\n'` ≡ `printf 'a\n'`.
3. Zero surviving options → the existing bail (options.rs:44-46, `VERIFIED`), unchanged.

**Why tab** (`ARGUED`): every other candidate delimiter (`|`, `::`, `,`, ` — `) realistically
occurs inside real option values (pipes, prefixed ids — the include prefix already uses `:`,
resolver.rs:625 `VERIFIED`; the em-dash is itself the *display* separator and so appears in
descriptions). A literal tab in a branch name / env / ARN / customer id does not occur in
practice. Trimming each side preserves the codebase's existing line-trim contract
(options.rs:40, `VERIFIED`).

The one common tab *producer* is `aws … --output text` (tab-delimited columns). Used as a
`from:` source today it yields one option whose value *contains* a tab — almost certainly
unintended (the entire multi-column line becomes the command's value). Under this design it
splits into value + description, which is what such an author wants. So the tab producer that
exists in the wild is precisely the case where splitting is the *desired* behavior — it
strengthens the choice rather than threatening it. The genuinely-adverse case (a value that must
retain an embedded tab) is what the unbuilt `format: tsv` escape hatch (§10) reserves for.

**The one intentional behavior change** (§10): a `from:` script that today emits a literal tab
*inside* a value would be split tomorrow. Accepted as near-zero real exposure. An escape hatch
(opt-in `format: tsv` on the pick) is identified but **not** built — add it only on evidence of
a real tab-bearing value source.

**Forward-compat decision (user-confirmed):** tabs are the format for now; if option metadata
ever grows past `value`/`description` (an icon, a group, a `disabled` flag), the agreed path is
an **opt-in `format: json`** on the pick — explicit mode, so no `{`-prefixed legacy value ever
becomes ambiguous — with tab-separated remaining the default. This is deliberately *not* built
now; it is recorded so the wire format stays a two-field TSV rather than being pre-generalized.

**Rejected:** JSON-lines (forces `jq` on every author; a legacy value beginning with `{`
becomes ambiguous); ` — ` em-dash (conflates wire and display, appears in descriptions);
`|`/`::`/`,` (collide with real values); a second `from_desc:` command (line-number
coordination across two processes — fragile).

---

## 4. Internal representation, rendering, and return

**Resolution product** — a new type in `src/options.rs` (this is the resolved product, not
manifest schema):

```rust
#[derive(Debug, Clone, PartialEq)]
pub struct PickOption {
    pub value: String,
    pub description: Option<String>,
}
```

`resolve_pick` (options.rs:12-48, `VERIFIED`) becomes `-> Result<Vec<PickOption>>`. Static arm
maps `OptionDef` via its accessors; dynamic arm applies the §3 parse. **It has exactly one
caller** — exec.rs:174 (`VERIFIED` by grep: the only `resolve_pick` call outside its own
definition/tests). The framing note "two call sites in `fill`" is wrong: the *provided-value*
arm reads `pick.options` directly and never calls `resolve_pick` (exec.rs:151-162, `VERIFIED`;
comment at exec.rs:152-153 states dynamic sources deliberately accept any provided value).

**Label helper — extract, don't duplicate.** The truncation logic in `menu_label`
(flow.rs:98-139, `VERIFIED`) — description absorbs all truncation, char-boundary-safe ellipsis,
description-dropped/base-label fallbacks — is subtle and already tested (flow.rs:141-221,
`VERIFIED`). Move it to a leaf module `src/label.rs`:

- `pub fn width() -> usize` — the moved `label_width` + `FALLBACK_WIDTH` + `INQUIRE_CHROME_MARGIN`
  (flow.rs:9-22, `VERIFIED`).
- `pub fn compose(head: &str, desc: Option<&str>, tail: &str, width: usize) -> String` —
  `menu_label` generalized with a pre-formatted `tail` (`"  (id)  ← src"` or `""`); the existing
  algorithm runs unchanged with an empty tail.
- `flow::menu_label` becomes a thin wrapper that builds the tail and delegates. **Its existing
  tests stay put and must remain green — that green is the proof the extraction is faithful.**
- `pub fn option_label(o: &PickOption, width: usize) -> String` = `compose(&o.value,
  o.description.as_deref(), "", width)` → `value — description`, one line, description ellipsized.

Rationale for a *leaf* module (`ARGUED`): `flow` already depends on `exec` (flow.rs:5,
`VERIFIED`). Putting the shared helper in `flow` and calling it from `exec` would make `exec`
import `flow` — not a *compile* error (Rust allows circular module references within a crate),
but it inverts the natural layering (a display primitive would live in the guided-flow module and
be pulled *down* into the lower-level executor). A dependency-free `label` leaf module both `exec`
and `flow` import keeps the layering clean. The motivation is design hygiene, not a compiler
constraint.

**`fill`, prompted arm** (exec.rs:173-176, `VERIFIED`) becomes index-based:

```rust
(None, ParamKind::Pick(pick)) => {
    let opts = options::resolve_pick(pick, &values, run_dir.unwrap())?;
    let w = label::width();
    let labels = opts.iter().map(|o| label::option_label(o, w)).collect();
    let i = prompt::select_index(&format!("{name}?"), labels)?;
    opts[i].value.clone()
}
```

`select_index` already exists and returns the chosen index via inquire's `raw_prompt`
(prompt.rs:59-62, `VERIFIED`). `labels` and `opts` are the same length by construction, so
`opts[i]` is total. **This is not a novel mechanism** — it is the identical pattern the guided
flow already uses to map a selected command index back to the command (flow.rs:56
`break members[ci]`, and flow.rs:42-46, `VERIFIED`). Delete `prompt::select` (prompt.rs:53-56):
exec.rs:175 is its sole caller (`VERIFIED` by grep).

**`fill`, provided arm** (exec.rs:151-162, `VERIFIED`): the membership check becomes
`!opts.iter().any(|o| o.value() == v)`, and the error's option list joins
`opts.iter().map(OptionDef::value)`. Provided values validate against **values only**, never
labels/descriptions. Dynamic picks still accept any provided value, unchanged.

Side effect (desirable, note in changelog): inquire's `Select` filters on label text, so typing
description words now filters the list.

---

## 5. `--list --json` and `--help`

**`--list --json`** (`list_json`, main.rs:482-543, `VERIFIED`) is a stable, additive-only Schema
1 contract (doc comment main.rs:478-481, `VERIFIED`). The current code serializes `pick.options`
**directly** into the `"options"` key (main.rs:519, `VERIFIED`) — so with the type change, that
key would silently become an array of objects, a **breaking** change. The fix keeps it additive:

- `"options"` stays an **array of plain strings** (the values):
  `options.iter().map(OptionDef::value).collect()`.
- New sibling key `"option_details"`: `[{"value":"dev","description":"Sandbox"},
  {"value":"uat","description":null}, ...]`. **Pairing rule (the single invariant):
  `option_details` is present iff `options` is present, same order, same length.** Old consumers
  ignore it.
- Dynamic picks (main.rs:521-528, `VERIFIED`): unchanged — they emit `source`/`depends_on` and
  no `"options"` key today, so by the pairing rule they get no `"option_details"` key either.
  Consumers already discriminate static from dynamic by presence of `"options"` vs `"source"`;
  the new key follows that same discriminator. Emitting `option_details: []` for a dynamic pick
  would be a *false* statement — it reads as "the option set is known and has no details," when
  the truth is the set is unknowable at list time because the source is never run (the same
  no-side-effects guarantee as the preview path, exec.rs:26 doc comment, `VERIFIED`).
- Update the Schema-1 doc comment (main.rs:478-481) to record that `option_details` is an
  additive Schema-1 field.

**Compile-time safety net** (`ARGUED`): `OptionDef` deliberately does **not** derive `Serialize`
(§2 derives only `Deserialize` + test-only `JsonSchema`). So the current `json!({"options":
options})` at main.rs:519 — which would silently reshape `"options"` into an array of objects —
**fails to compile** the moment the type changes, forcing the values-only + `option_details`
rewrite. The Schema-1 break cannot slip through silently.

**`--help`** (main.rs:461-463, `VERIFIED`): keep `one of: {values}` (join values only) — the
rendered text is byte-identical; descriptions in a one-line clap help string would be
unreadable. A `long_help` surface is a possible follow-up, not part of this design.

---

## 6. `visit_param` — descriptions are visited

In `visit_param` (resolver.rs:597-613, `VERIFIED`), visit the option **value** (preserving
today's per-option string visit, resolver.rs:602-606) **and** the **description**.

Justification (`ARGUED`): the visitor is the module `${var}`-substitution / prefixing pass, and
command-level `description` is **already** visited (resolver.rs:561-563, `VERIFIED`). A module
writing `description: "${cluster_prefix} env"` on an *option* must not be the one string class
where substitution silently fails, when it works for command descriptions and option values
alike. `apply_prefix` rewrites names structurally, not through this visitor (resolver.rs:624+,
`VERIFIED` header), so there is no prefix concern for descriptions.

---

## 7. Invariants and their falsifying tests

Each names the mutation that reddens the test (`revert X → red`).

1. **The value handed to the command equals the selected option's `value`, never its label.**
   Test: `opts` with descriptions; assert the prompted arm maps index `i` → `opts[i].value` and
   that this differs from `option_label(&opts[i], w)`. Redden by returning the label instead of
   `opts[i].value`. *(Mechanism already proven by flow.rs:56 — this test pins the new call site.)*
2. **A scalar YAML option behaves identically to today.** Test: `resolve_pick` on
   `options: [dev, uat]` yields values `["dev","uat"]`, all descriptions `None` (adapts
   options.rs:54-62). Redden by any scalar-path change.
2a. **Non-string scalar options still load (the §2a regression guard).** Test: a manifest with
   `options: [1, true, 8080]` loads and yields values `["1","true","8080"]`. Redden by switching
   `OptionDef` to `#[serde(untagged)]` — the exact bug §2a documents; this test is what makes that
   mistake impossible to reintroduce silently.
2b. **Unquoted float options are rejected at load; quoted ones accepted (the §2b behavior
   change).** Test: `options: [1.5]` → load `Err` mentioning "quoted"; `options: ["1.5"]` → value
   `"1.5"`. Redden by making `visit_f64` return `Plain(n.to_string())` (the lossy alternative).
3. **A value-only `from:` line is unchanged, including trim-and-skip-blank — even when the blank
   line contains a tab.** Test: extend `from_reads_stdout_lines` input to
   `'alpha\n  beta  \n\n \t \n'` → alpha, beta only (the `" \t "` line is skipped by rule 0, *not*
   errored; options.rs:64-72, `VERIFIED` current). Redden by requiring tabs, changing trimming, or
   handling the tab before the blank check (which would turn `" \t "` into the §7.6 hard error).
4. **Tab lines split at the first tab only.** Test: `printf 'a\tb\tc\n'` → value `a`,
   description `b\tc`. Redden with `splitn(3)` / `rsplit`.
5. **Empty description ≡ no description.** Test: `printf 'a\t\n'` → `None`. Redden with `Some("")`.
6. **Empty value before a tab is a hard error naming the source.** Test: `printf '\tdesc\n'` →
   `Err` containing the command string. Redden by silent skip.
7. **Provided values validate against values, not labels.** Test: static
   `{value: dev, description: Sandbox}`; provided `dev` accepted; `Sandbox` and `dev — Sandbox`
   rejected with the values-only message (exec.rs:151-162 arm). Redden by matching labels.
8. **`--list --json` `"options"` stays an array of plain strings; `"option_details"` is
   parallel.** Test: extend `list_json_exposes_params_and_dependencies` (main.rs:636-, `VERIFIED`)
   with a descriptioned option; assert `options[i]` is a JSON string and
   `option_details[i].value == options[i]`. Redden by emitting objects into `"options"`.
9. **Descriptions are `${var}`-substituted in modules.** Test: mirror the substituted-picker
   test (resolver.rs:954-960 / 981-1008, `VERIFIED`) with `description: "${cluster_prefix} env"`.
   Redden by skipping descriptions in `visit_param`.
10. **Blank YAML descriptions and empty `Full` values are rejected at load.** Tests mirroring
    `blank_description_is_rejected` (manifest.rs:634-640, `VERIFIED`). Redden by dropping the
    validation.
11. **Committed schema matches the structs.** Already falsified by `committed_schema_is_current`
    (manifest.rs:462-471, `VERIFIED`) — red until regen.
12. **Label truncation invariants** (description absorbs truncation; char-boundary safe; ≤ width).
    Existing `flow` tests (flow.rs:180-220, `VERIFIED`) keep covering the shared `compose`; add
    one `option_label` test at a small width with a multibyte description. Redden by
    re-implementing instead of sharing, then diverging.
13. **Preview never resolves option sources.** Existing test (exec.rs:205-237, `VERIFIED`) stays
    green untouched; any regression making preview call `resolve_pick` reddens it.

**No spike needed.** The design-doc discipline asks to spike the single load-bearing claim
(§7.1) before finalizing. Here that claim's mechanism — `select_index` → index into the option
slice — is not unknowable at design time: it is already live and passing in the guided flow for
command selection (flow.rs:41-65, `VERIFIED`). §7.1's test pins the *new* call site; it cannot
"fail for a different reason" in an informative way, so there is nothing a spike would teach that
the existing flow tests do not already establish.

---

## 8. Class analysis / call-site sweep

Complete set (grepped `resolve_pick`, `pick.options`, `PickDef`, `.options`, `::select(` across
`src/`). Verdict per site:

| Site | Verdict |
|---|---|
| manifest.rs:157-165 `PickDef` | **change** — `Option<Vec<OptionDef>>`; add `OptionDef` (manual `Deserialize` + `schemars(untagged)`, §2/§2a) / `FullOption` |
| manifest.rs:403-411 `validate_param` (pick branch) | **change** — add `Full`-option rules (§2) |
| manifest.rs:462-479 schema tests + `pult.schema.json` | **regen** |
| options.rs:12-48 `resolve_pick` (+ tests 50-97) | **change** — `Vec<PickOption>`, TSV parse |
| exec.rs:151-162 provided-pick validation | **change** — membership over `o.value()` |
| exec.rs:173-176 prompted pick | **change** — `select_index` + `opts[i].value` |
| exec.rs:172 preview arm; exec.rs:205-237 test | **correct as written** |
| prompt.rs:53-56 `select` | **delete** — sole caller (exec.rs:175) removed |
| prompt.rs:59-62 `select_index` | **correct as written** — becomes the sole select primitive |
| flow.rs:9-22, 98-139 width + `menu_label` (+ tests 141-221) | **change** — extract to `src/label.rs`, wrap; add `option_label` |
| main.rs:461-463 `--help` | **change** — join `OptionDef::value` (rendered text identical) |
| main.rs:517-530 `list_json` (+ test 636-) | **change** — values in `"options"`, add `"option_details"` |
| resolver.rs:597-613 `visit_param` | **change** — match the enum: visit `Plain(s)`, `Full.value`, and `Full.description` |
| resolver.rs:692-708 dependent-picker ordering | **correct as written** — reads `pick.from` only (resolver.rs:695-700) |
| resolver.rs:954-960, 981-1008 tests | **adapt** to `OptionDef` accessors |
| x.rs, compile.rs, doctor.rs, journal.rs, trust.rs, verify.rs, init.rs | **correct as written** — no pick-value reads; `pult x` flows through the same `exec::execute` (x.rs:157, `VERIFIED`) |

Optional docs polish (not load-bearing): `pult init` template comment and a README/authoring
example showing the new syntax.

---

## 9. Implementation order

Each step lands with its §7 falsifying tests:

1. `manifest.rs` — `OptionDef`/`FullOption` types + validation; regen `pult.schema.json`.
2. `src/label.rs` — extract `width`/`compose` from `flow.rs`, wrap `menu_label`; keep flow tests
   green; add `option_label`.
3. `options.rs` — `PickOption`, `resolve_pick -> Vec<PickOption>`, TSV parse.
4. `exec.rs` — both `fill` arms; delete `prompt::select`.
5. `main.rs` — `list_json` (`options` values + `option_details`) and `--help`.
6. `resolver.rs` — `visit_param` visits descriptions.

Steps 1–2 are independent of each other; 3 depends on 1; 4 depends on 2+3; 5–6 depend on 1.

---

## 10. Risks and open notes

- **Tab-in-value break** (§3) — one intentional behavior change; documented, near-zero real
  exposure; opt-in `format: tsv` escape hatch identified but deliberately unbuilt.
- **Unquoted-float rejection** (§2b) — the *second* intentional behavior change: an unquoted
  float option (`options: [1.5]`) that loads today becomes a load-time "quote it" error, chosen
  over silent `1.10`→`1.1` corruption. Ints/bools unaffected. **Needs sign-off at review** — the
  lossy alternative is a one-line change.
- **Extending past two fields** — settled (user-confirmed): tabs now; a future opt-in
  `format: json` on the pick is the agreed path if per-option metadata grows (§3), keeping TSV
  the default and avoiding the JSON-lines legacy-`{` ambiguity. Not in scope for this design.
- **Untagged parse errors** — *dissolved by the manual `Deserialize`* (§2a): malformed mapping
  options now get a precise `unknown field` message instead of serde's vague untagged text. No
  longer a trade.
- **Multi-line / block-scalar descriptions** render poorly in a one-line label — pre-existing
  parity with command descriptions (only *blank* is rejected, manifest.rs:386-390, `VERIFIED`);
  no new validation; shared limitation.
- **Truncation is char-count, not display-width** (flow.rs:116,126, `VERIFIED`) — inherited
  CJK/emoji limitation, not introduced here; out of scope.
- **`options: []`** passes validation today and would hand inquire an empty list — pre-existing,
  unchanged, out of scope.
- **Duplicate values** with differing descriptions: legal; distinct rows; both map to the same
  value — harmless by index selection.
- **Trust re-prompt**: descriptions live in the manifest raw text, so adding them re-triggers the
  trust prompt like any manifest edit (exec.rs:48-54, `VERIFIED`) — correct, no action.

---

## When settled

- [x] Advisor authored the substance; orchestrator transcribed and grounded every `file:line`.
- [x] Every load-bearing claim is `VERIFIED` or `ARGUED` — no load-bearing `UNKNOWN`/`ASSUMED`.
- [x] Every cited `file:line` was opened this session at that line.
- [x] Every invariant (§7) names the mutation that reddens its test.
- [x] Class analysis (§8) enumerates every call site with a per-site verdict, including the ones
      correct as written.

Ready for review. Implementation waits on the review.
