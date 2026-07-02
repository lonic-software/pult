# pult — reference

Complete schema, CLI, and behavior reference. Narrative introductions:
[user-guide.md](user-guide.md) (operators), [authoring.md](authoring.md)
(command & module authors).

## Manifest discovery

`pult` searches for `pult.yaml`, then `pult.yml`, in the current directory and
each ancestor, nearest wins (**repo scope**). If nothing is found all the way
up, it falls back to the **user manifest**: `$PULT_USER_MANIFEST`, or
`~/.config/pult/pult.{yaml,yml}` (**user scope**). A repo manifest always
wins; user commands are never merged into a repo's namespace.

The scopes differ in one deliberate way — the working directory of commands
and option sources. Repo scope runs them at the manifest's directory; user
scope runs them at the *invocation* directory (personal commands act on
wherever you are). In both scopes, local `includes:` resolve relative to the
manifest, and trust is per manifest path as usual.

## Manifest schema

```yaml
version: 1                # required; must be ≤ the engine's supported version
name: my-project          # optional; defaults to the directory name
description: …            # optional

includes: [ <include> ]   # root manifests only
vars: { <name>: <var> }   # modules only
params: { <name>: <param> }   # named params, referenced via `use:`
steps: { <name>: <step> }     # named steps, referenced via `use:`
commands: [ <command> ]
```

The same schema serves root manifests and modules; the differences are:
a root manifest may have `includes` but not `vars`; a module may have `vars`
but not `includes` (no transitive includes). `registries:` is reserved for a
future phase and rejected with an explanatory error.

### `<command>`

```yaml
- id: shell               # unique after merging; ids `includes`, `registry`, `update`,
  title: Open a shell     #   and `module` are reserved
  params:                 # ordered map — prompted in this order
    <name>: <param>
  run: <run>
```

### `<param>` — exactly one of `pick`, `input`, `use`

```yaml
env:      { pick: { options: [dev, uat, pre] } }   # static list
customer: { pick: { from: "cmd --env {env}" } }    # dynamic source
note:     { input: { default: "hi" } }             # free text
region:   { use: aws:region }                      # copy of a named param
```

- `pick.options` — static; values provided on the CLI are validated against it.
- `pick.from` — a shell command (strict template); its stdout lines (trimmed,
  non-empty) become the options. May reference only params declared earlier in
  the same command. An empty result or non-zero exit is an error (stderr shown).
  CLI-provided values are **not** validated against dynamic sources.
- `use` — must reference an existing named param, which must itself be
  concrete (a named param cannot be another `use:`).

### `<step>`

```yaml
plain: "echo hi"          # string form: script, no contract
full:                     # structured form
  outputs: [TASK]         # shell variables this step promises to set
  script: |
    TASK=$(…)
```

Output names must be valid shell identifiers. At run time, each declared
output is asserted after the step runs; a step that exits 0 without setting
one fails the command with `pult: step <name> did not set declared output <o>`.

### `<run>` — a string or a list

**String form** — executed via `sh -c`. Strict interpolation: `{param}` must
be declared, `{{`/`}}` escape literal braces, values are shell-quoted.

**List form** — compiled to one bash script (`set -euo pipefail`); named steps
become shell functions, entries become statements, in order. Entries:

```yaml
run:
  - "echo inline fragment"          # inline script (lenient interpolation)
  - use: <step-name>                # call a named step
    with: { <placeholder>: <tpl> }  # rebind the step's {placeholder}s
    exports: { <OUT>: <NEW> }       # rename declared outputs
  - pipe:                           # a shell pipeline
      - use: <step-name>            #   (with: allowed; exports: not — stdout only)
      - "single-line filter"
```

Load-time validation: `use:` must resolve (error lists what exists); `with:`
keys must be placeholders the step has; `with:` values are strict templates
over the command's params; `exports:` keys must be declared outputs; two steps
producing the same (post-`exports`) output name is an error; pipe segments
must be single-line.

### Interpolation summary

| Syntax | When | Where | Unknown name |
|---|---|---|---|
| `{param}` strict | run time | string `run:`, `pick.from`, `with:` values | load error; `{{ }}` escapes |
| `{param}` lenient | run time | step scripts, inline fragments | passes through untouched; `${…}` never matches (`$`-guard) |
| `${var}`, `${module.dir}` | load time | all strings in a module | passes through as shell |

All substituted param values are shell-quoted.

### `<var>` (modules only)

```yaml
cluster_prefix:
  required: true          # or:
  default: eu-west-2      # exactly one of the two
  description: …          # optional
```

The include site binds vars; binding an undeclared var, or omitting a
required one, is a load error. `module.dir` is implicitly defined as the
module's absolute directory; the `module.*` namespace is reserved.

### `<include>` (root manifests only)

```yaml
- source: <source>
  vars: { <name>: <value> }   # binds the module's vars
  prefix: aws                 # namespaces ALL exports: aws:<id>, aws:<step>…
  sha256: <hex>               # optional pin on the module yaml's bytes
```

Sources:

| Form | Kind |
|---|---|
| `./path`, `../path` | local file, or directory containing `module.yaml` |
| `host.tld/org/repo[//sub]@<tag\|sha>` | git over https |
| `git::<url>[//sub]@<tag\|sha>` | git, any transport (ssh, file, …) |

`//sub` may be a directory (containing `module.yaml`) or a direct path to a
yaml file. Pins are mandatory for git sources: a tag, or a full 40-char commit
sha. Branch names are rejected. `sha256:` mismatches are hard errors.

Merge order: includes in declared order, then local. Duplicate command/param/
step names across the merged whole are errors (disambiguate with `prefix:`).

## Trust model

Trust-on-first-use over the **resolved whole**: the stored hash covers the
root manifest bytes, every include's source string, resolved commit (for git),
module bytes, and — for local **directory** modules — a digest of the whole
module tree (relative paths, file contents, symlink targets, and the unix
executable bit; `.git` skipped). Editing a shipped executable re-prompts the
same way editing the yaml does. Git modules need no separate tree digest: the
resolved commit sha identifies the tree, and the cache is immutable.
Single-file local includes (`./steps.yaml`) cover only that file — a module
that ships executables should be a directory module. Any change → re-prompt. Stored per user in
`<config>/pult/trust.json` (macOS: `~/Library/Application Support/pult/`,
Linux: `~/.config/pult/`, Windows: `%APPDATA%\pult\`); override with
`PULT_TRUST_STORE`. Non-TTY + untrusted = refusal; `--trust` accepts
explicitly and records immediately, even for non-executing invocations.

## Module cache

Git modules are fetched shallowly via system `git` (auth inherited;
`GIT_TERMINAL_PROMPT=0`) into `<cache>/pult/modules/<name>-<hash>/` (macOS:
`~/Library/Caches/pult/modules`, Linux: `~/.cache/pult/modules`; override:
`PULT_CACHE_DIR`). The checkout is stored without `.git`, alongside a
`meta.json` recording the pin and the commit it resolved to. Cache entries are
immutable and never revalidated — warm caches work fully offline. Deleting a
cache directory is always safe (next run re-fetches).

## CLI

```
pult                          guided flow
pult <command> [values…]      direct invocation (missing values are prompted)
pult <command> --help         generated per-command help
pult --list                   commands, params, and origins
pult --list --json            the same, machine-readable (schema below)
pult <command> --print        print the composed script instead of running
pult --trust …                trust this manifest without prompting (records immediately)
pult includes verify          CI guard: pins still resolve, no tag moved (exit 1 on drift)
pult update [VERSION]         self-update to the latest (or given) release; needs no manifest
pult --version / -V           engine version
```

## Machine-readable listing — `pult --list --json`

The stable surface for tooling and agents. `schema` is `1`; changes within a
schema version are **additive only** (new fields may appear; existing fields
keep their meaning), breaking changes bump `schema`.

```json
{
  "schema": 1,
  "pult_version": "0.1.0",
  "name": "demo",
  "manifest": "/repo/pult.yaml",
  "dir": "/repo",
  "run_dir": "/repo",
  "scope": "repo",
  "trusted": false,
  "includes": [
    { "source": "./tools", "kind": "local" },
    { "source": "github.com/opskit/aws-common@v1.4.2", "kind": "git",
      "url": "https://github.com/opskit/aws-common",
      "rev": "v1.4.2", "rev_kind": "tag", "resolved_sha": "8a6e6fd4…" }
  ],
  "commands": [
    {
      "id": "shell",
      "title": "Open a shell",
      "origin": null,
      "params": [
        { "name": "env", "kind": "pick", "options": ["dev", "uat", "pre"] },
        { "name": "customer", "kind": "pick",
          "source": "./bin/impl list --env {env}", "depends_on": ["env"] },
        { "name": "note", "kind": "input", "default": "" }
      ]
    }
  ]
}
```

Field notes:

- `scope` — `"repo"` or `"user"` (see Manifest discovery); `dir` is the
  manifest's directory (the include base), `run_dir` is where commands and
  option sources execute — they differ only in user scope.
- `trusted` — whether this manifest is trusted **at its current resolved
  hash**. `false` means invoking a command will prompt interactively (or be
  refused non-interactively without `--trust`) — tooling should surface that
  to a human rather than pass `--trust` itself.
- `origin` — the include source a command came from; `null` = declared in the
  root manifest.
- Params appear in **declared order**, which is also positional-argument
  order: `pult <id> <first> <second> …`.
- Param kinds: `pick` with `options` (static; CLI values are validated
  against it), `pick` with `source` (a shell-out; its stdout lines become
  options; `depends_on` lists params the source interpolates — supply those
  first), and `input` (free text, `default` may be `null`).
- `pult <id> <values…> --print` prints the exact composed script without
  running it — the natural dry-run step before an agent executes anything.

## Exit codes

| Code | Meaning |
|---|---|
| (passthrough) | the executed command's own exit code |
| 128+n | command killed by signal n |
| 130 | prompt cancelled (Esc / Ctrl-C) |
| 1 | pult error (untrusted manifest, invalid manifest, fetch failure, …) |
| 2 | usage error for engine subcommands |

## Environment variables

| Variable | Effect |
|---|---|
| `PULT_TRUST_STORE` | alternate trust-store path |
| `PULT_CACHE_DIR` | alternate module cache root |
| `PULT_USER_MANIFEST` | alternate user-manifest path (default: `~/.config/pult/pult.yaml`) |
| `PULT_REPO` | GitHub repo slug `pult update` fetches from |
| `PULT_BASE_URL` | asset base URL for `pult update` (mirrors / air-gapped; bypasses GitHub) |

## Requirements

- `sh` and `bash` on PATH (command execution; compiled step scripts use bash).
- `git` on PATH for git module sources.
- Windows: both are provided by Git for Windows; run `pult` from Git Bash.
