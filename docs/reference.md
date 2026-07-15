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
- id: shell               # unique after merging; reserved ids: `includes`, `registry`,
  title: Open a shell     #   `module`, `update`, `self`, `init`, `trust`, `cache`, `ui`,
                          #   `events`, `x`, `tap`, `registries`, `serve`, `doctor`
  params:                 # ordered map — prompted in this order
    <name>: <param>
  run: <run>
  check: "command -v aws" # optional readiness probe — see below
  interactive: true       # optional: `run:` needs a controlling terminal
  category: Deploy        # optional: display grouping — see below
  description: "Deploys the app to the given environment."  # optional — see below
```

- `category:` — an author-assigned display group ("Deploy", "Tests") for the
  guided flow, `--list --json`, and future surfaces (palette, desktop app);
  the flat `--list` text stays ungrouped (see CLI section below) so the
  scripts that parse it keep working. Grouping rule, implemented once and
  shared by every grouped surface: a command's group is its
  `category` if set, else the module's declared `name:` (see the manifest
  `name:` field in [authoring.md](authoring.md)) for commands that came from an
  include, else the include it came from (`origin`, the raw source string),
  else the implicit `local` group; groups containing at least one
  locally-declared command come first (in order of first appearance), then
  the remaining groups in include order. Categories merge across sources — a
  module tagging its exports `Deploy` joins the local `Deploy` group, not a
  separate one.
- `check:` — a shell command whose exit 0 means "ready to run". It may not
  reference `{param}`s (it runs before any param exists); `${var}`s are
  substituted as usual. Run via `pult doctor` (all checks, trust-gated, exit 1
  if any fail) and surfaced to UIs via `--list --json`; never run implicitly
  before `run:` — put real preflight/setup in the playbook itself.
- `interactive:` — declares that `run:` requires a controlling terminal at
  runtime (a REPL, a TUI, a shell into a container). The contract: a command
  *without* this flag must be fully non-interactive once its params are filled
  — declare a param instead of `read`-ing — which is what makes non-terminal
  surfaces (the future pane runner and desktop app) safe. The plain CLI
  ignores the flag; stdio is inherited either way.
- `description:` — one or two sentences explaining what the command does.
  The title names the control; the description explains it. Shown by
  `pult <cmd> --help`, `--list --json`, and UIs (the command card); the flat
  `--list` and guided-flow text stay unchanged.

### `<param>` — exactly one of `pick`, `input`, `use`

```yaml
env:      { pick: { options: [dev, uat, pre] } }   # static list
customer: { pick: { from: "cmd --env {env}" } }    # dynamic source
note:     { input: { default: "hi" } }             # free text
token:    { input: { secret: true } }              # prompted without echo
region:   { use: aws:region }                      # copy of a named param
```

- `pick.options` — static; values provided on the CLI are validated against it.
- `pick.from` — a shell command (strict template); its stdout lines (trimmed,
  non-empty) become the options. May reference only params declared earlier in
  the same command. An empty result or non-zero exit is an error (stderr shown).
  CLI-provided values are **not** validated against dynamic sources.
- `use` — must reference an existing named param, which must itself be
  concrete (a named param cannot be another `use:`).
- `input.secret` — prompted masked (no echo into scrollback) and redacted
  wherever the composed command line is *displayed*: the `running:` banner,
  `--print`, and the ephemeral trust prompt show `••••••` / `<name>` instead
  of the value. A `default:` is rejected for secrets — that would commit a
  credential to the manifest. The value still reaches the child process argv
  as usual; a value passed as a CLI argument also lands in your shell history
  — prefer the prompt for anything truly sensitive, or `--params-json` (below)
  when tooling needs to pass it non-interactively without putting it in argv.

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
| `./path`, `../path` | local file, or directory containing `pult.module.yaml` |
| `host.tld/org/repo[//sub]@<tag\|sha>` | git over https |
| `git::<url>[//sub]@<tag\|sha>` | git, any transport (ssh, file, …) |

`//sub` may be a directory (containing `pult.module.yaml`) or a direct path to a
yaml file. Pins are mandatory for git sources: a tag, or a full 40-char commit
sha. Branch names are rejected. `sha256:` mismatches are hard errors.

A **module** file (`pult.module.yaml`) is what a repo *exposes* to consumers via
`pult x` and `includes`; it is distinct from the repo's own `pult.yaml`, which
local discovery resolves and which `pult x` / `includes` never touch — so
internal commands are unreachable remotely. The pre-0.3 name `module.yaml` is
still resolved as a fallback (`pult.module.yaml` wins when both exist).

Merge order: includes in declared order, then local. Duplicate command/param/
step names across the merged whole are errors (disambiguate with `prefix:`).

### `pult includes add`

`pult includes add <SOURCE> [--prefix P] [--user]` appends an include without
hand-editing yaml. A git source without a pin resolves to the remote's highest
version-shaped tag (`v1.2.3` / `1.2.3`; suffixed tags like `-rc1` are ignored)
— the written include is always pinned. It fetches the module, prints the
commands it brings, prompts for any **required vars** (interactively only),
asks for confirmation on a TTY, and edits the manifest **textually** so
comments and formatting survive. After writing it re-resolves the manifest and
**rolls back** on any error (collisions, invalid module), so a broken manifest
is never left behind. A source already included (same base, any pin) is
refused. Default target is the nearest manifest; `--user` targets the user
manifest and creates it if missing.

### `pult x` — ephemeral execution

`pult x <SOURCE> [COMMAND] [values…]` runs a command straight from a module
source without adding it to any manifest — the same source syntax, pinning, and
immutable cache as an include (a bare git source resolves to its latest version
tag; pin explicitly with `@<tag|sha>` to run offline from a warm cache). With no
`COMMAND` it opens the guided menu, or — non-interactively — lists what the
module offers and exits `2`. Commands run in the **invocation directory** (they
act on where you are, like user-scope commands); `${module.dir}` executables and
module `vars:` (bind them with `--var NAME=VALUE`, repeatable) work as they do in
an include. The same trust prompt gates the run: the trust identity is the
source itself — a pinned git source (globally unique) or a local module's
canonical path — so re-running a trusted source doesn't re-prompt, while a moved
tag or an edited local tree does. Because you're trusting one ad-hoc source to
run one command, the trust prompt shows the **composed command about to run**
(unsupplied params as `<name>`), so a single `y` approves the source and runs it
while you read the actual script; the preview never executes module code
(dynamic option sources stay unresolved), so it's safe before trust. `--print`
is the same preview with no prompt and nothing recorded — a trust-free dry run;
`--trust` records trust up front for CI. Use it to *try* a command set;
`pult includes add` to *keep* one. Intercepted before manifest discovery, so it
works where no `pult.yaml` exists.

## Trust model

Trust-on-first-use over the **resolved whole**: the stored hash covers the
root manifest bytes, every include's source string, resolved commit (for git),
module bytes, and — for local **directory** modules — a digest of the whole
module tree (relative paths, file contents, symlink targets, and the unix
executable bit; `.git` skipped). Editing a shipped executable re-prompts the
same way editing the yaml does. Git modules need no separate tree digest: the
resolved commit sha identifies the tree, and the cache is immutable.
Single-file local includes (`./steps.yaml`) cover only that file — a module
that ships executables should be a directory module. Any change → re-prompt. The
prompt summarizes the manifest and its includes, because trust covers **every**
command the manifest declares, not the single one you invoked. (The exception is
`pult x`, which trusts one ad-hoc source to run one command; there the prompt
also shows the composed command about to run — see below.) Stored per user in
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
pult --list                   commands, params, and origins (flat, one line each — scripts parse this)
pult --list --json            the same, machine-readable, grouped by category (schema below)
pult <command> --print        print the composed script instead of running
pult <command> --params-json  read this command's param values from stdin as a JSON
                                object (positional args still work; keeps them out of
                                pult's argv and shell history — see caveat below)
pult --trust …                trust this manifest without prompting (records immediately)
pult x <SOURCE> [COMMAND]     run a command from a module source, no manifest (npx-style)
     [values…] [--var N=V]      --trust / --print as elsewhere; a bare source takes the latest tag
pult includes add <SOURCE>    pin a module and append it to a manifest's includes
     [--prefix P] [--user]      (--user targets ~/.config/pult/pult.yaml, creating it)
pult includes verify          CI guard: pins still resolve, no tag moved (exit 1 on drift)
pult doctor [--json]          run every command's check: and report readiness (exit 1 if
                                any fail; trust-gated — checks are manifest code; --json for
                                the machine-readable form)
pult init [--user]            scaffold a starter manifest here (or your user manifest)
pult self schema              print the manifest JSON Schema (draft-07) to stdout
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
    { "source": "./tools", "kind": "local", "name": null },
    { "source": "github.com/opskit/aws-common@v1.4.2", "kind": "git",
      "url": "https://github.com/opskit/aws-common",
      "rev": "v1.4.2", "rev_kind": "tag", "resolved_sha": "8a6e6fd4…",
      "name": "AWS Tooling" }
  ],
  "commands": [
    {
      "id": "shell",
      "title": "Open a shell",
      "origin": null,
      "category": null,
      "description": "Opens an interactive shell for the given customer and environment.",
      "params": [
        { "name": "env", "kind": "pick", "options": ["dev", "uat", "pre"] },
        { "name": "customer", "kind": "pick",
          "source": "./bin/impl list --env {env}", "depends_on": ["env"] },
        { "name": "note", "kind": "input", "default": "", "secret": false }
      ],
      "check": "command -v aws",
      "interactive": true,
      "steps": null
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
- `includes[].name` — that include's declared `name:` (var-substituted), or
  `null` if the module declared none.
- `category` — the raw `category:` value the author declared; `null` = none.
  This is *not* the computed display group — it's the grouping rule's input,
  not its output (see `<command>` above); a surface applying the grouping rule
  combines this with the matching include's `name` and `origin` itself.
- `description` — the raw `description:` value the author declared; `null` =
  none. One or two sentences explaining what the command does, for
  `--help`, `--list --json`, and UIs.
- Params appear in **declared order**, which is also positional-argument
  order: `pult <id> <first> <second> …`.
- Param kinds: `pick` with `options` (static; CLI values are validated
  against it), `pick` with `source` (a shell-out; its stdout lines become
  options; `depends_on` lists params the source interpolates — supply those
  first), and `input` (free text, `default` may be `null`). An input with
  `"secret": true` must be rendered as a password field and never echoed,
  logged, or persisted by tooling.
- `check` — the readiness probe (`null` = none declared); run it yourself or
  via `pult doctor` (`--json` for the machine-readable runner output — see
  below), don't assume pult ran it. `interactive` — the command needs a
  controlling terminal; non-terminal surfaces should treat it as
  terminal-only rather than capturing its output.
- `steps` — the labels a live run emits as `step k/n <name>` events (see
  [Events protocol](#events-protocol--pult_events) below); `null` for a
  string-form `run:`, which has no steps to name.
- `pult <id> <values…> --print` prints the composed script without running it —
  the natural dry-run step before an agent executes anything. It is fully
  side-effect-free: it does **not** prompt, run dynamic option sources, or
  require trust, so you can preview an untrusted command safely. Params you
  don't supply appear as `<name>` metavars rather than being prompted for.

## Machine-readable readiness — `pult doctor --json`

Same trust gate and exit-code semantics as text-mode `pult doctor` (exit 1 if
any declared check failed), but as a stable document for tooling instead of a
printed table:

```json
{
  "schema": 1,
  "name": "demo",
  "manifest": "/repo/pult.yaml",
  "commands": [
    { "id": "import", "title": "Import data", "check": "command -v aws", "ready": true, "exit_code": 0 },
    { "id": "shell", "title": "Open a shell", "check": null, "ready": null, "exit_code": null }
  ]
}
```

`ready` and `exit_code` are `null` when the command declares no `check:` —
there's nothing to run, not a failure.

## `--params-json` — param values without argv

`pult <command> [values…] --params-json` reads the rest of a command's param
values from **stdin**, as a flat JSON object of string values, instead of (or
alongside) positional arguments — the channel the desktop app and scripts use
to avoid putting values on the command line:

```sh
echo '{"token":"hunter2"}' | pult import --params-json
```

Rules: stdin must be a JSON object whose values are all strings (anything
else — invalid JSON, a non-object, a non-string value — is a load error);
every key must be a param the invoked command declares (an unknown key names
the valid ones, for typo safety); a param given both positionally and via
`--params-json` is a conflict, not a silent override. Params in neither
source are still prompted for as usual — and since stdin is now claimed by
`--params-json`, an interactive terminal on stdin is rejected up front rather
than hanging (pipe or redirect it: `echo '{...}' | pult …`, or a file).
Only meaningful for a direct command invocation — `--list`, `doctor`,
`includes`, and the bare guided flow reject it. Combine with `--print` to
preview the composed command with concrete values (secrets still masked).

**What this does and doesn't protect.** `--params-json` keeps values out of
pult's own argv (so they never show up in `ps` for the `pult` process itself)
and out of your shell history. It does **not** make the values disappear
downstream: pult composes the final command line and runs it as
`sh -c "<cmdline>"`, so while that `sh` (and whatever it execs) is running,
the values are visible in *its* argv to anything that can read `ps` on the
same machine — the same exposure positional values already have. Treat it as
"don't put secrets in argv/history you control", not as a way to hide a
secret from other processes on the host.

## Events protocol — `PULT_EVENTS`

A running command *may* write progress lines to the fd named by
`$PULT_EVENTS` — stdout/stderr stay untouched either way, and a script that
never emits anything loses nothing. Guard every emission so the same script
behaves identically whether or not anything is listening:

```sh
[ -n "${PULT_EVENTS:-}" ] && echo "progress 40 restoring" >&3
```

(`3` here because that's the fd pult wires up today; always read the number
from `$PULT_EVENTS` rather than hardcoding it.)

Three verbs, v1:

```
progress <0-100|?> [text]    # determinate percent, or ? = indeterminate
status <text>                # transient activity line
step <k>/<n> <name>          # entering step k of n
```

**Unknown verbs and malformed lines are silently ignored** — never an error,
in either direction. This is what makes the vocabulary additive-forever: a
script targeting a newer pult, or an older script run under a newer one,
never breaks.

The plain CLI translates events to [OSC
9;4](https://learn.microsoft.com/en-us/windows/terminal/tutorials/progress-bar-sequences),
the ConEmu/Windows Terminal/WezTerm/Ghostty progress-bar protocol, so
terminals that render progress natively show it with zero drawing on our
side:

| Event | OSC |
|---|---|
| `progress <n>` | state 1 (set), pct `n` |
| `progress ?` | state 3 (indeterminate) |
| `step <k>/<n>` (no `progress` seen yet) | state 1, pct derived as `(k-1)*100/n` |
| a run that rendered at least one event finishes | state 0 (clear) |
| a run that never emitted a single event finishes | nothing — no OSC at all |

There's no persistent "error" OSC state: whatever the command's exit code, a
run that emitted anything always finishes by clearing the badge. A progress
indicator stuck red forever (nothing later un-sets it) is worse than none, so
non-zero exits clear too. A command that never emits a single valid event —
most scripts, since none of this is required — produces zero bytes of OSC
for the whole run, including at the end: pult doesn't invent activity a
command never reported.

`status` carries no CLI rendering — it's consumed and dropped; it exists for
richer surfaces (a pane runner, the desktop app) that can show a live text
line. **Explicit beats derived**: once any `progress` event has arrived in a
run, `step` milestones stop driving the percentage — they're a coarse
fallback for scripts that never call `progress` at all.

**Compiled step lists emit `step` events automatically.** A list-form `run:`
(see [authoring.md](authoring.md#2--composing-commands-from-steps)) gets a
guarded `step k/n <name>` emission injected before each top-level entry at
compile time — structure a command as steps and you get milestones for
free, no manifest changes required. `--print` shows the script without these
guards (they'd clutter the dry-run output you're meant to read); a live run
compiles them in. The same labels are exposed as `"steps"` in `--list --json`
(below), so a surface can render milestones without parsing the script.

**`PULT_EVENTS` passthrough**: if `PULT_EVENTS` is already set in pult's own
environment when it runs a command, pult does nothing — no pipe, no
translation. The var and its fd inherit through to the child as-is. This is
for a parent process (the future desktop app) that owns the channel itself
and wants to render events its own way; pult only creates its own channel
(and does the OSC translation above) when nothing upstream already claimed
one **and** stderr is a terminal. Piped/non-interactive runs (CI) get neither
— `PULT_EVENTS` stays unset, zero behavior change.

An inherited `PULT_EVENTS` is only honored if it's a bare fd number (plain
decimal digits, no sign, no surrounding whitespace) — bash's `>&word`
redirects to a *file* named `word` when `word` isn't numeric, so a stray
`PULT_EVENTS=events.log` in the environment would otherwise make every
injected `step` guard silently truncate a file by that name. An invalid
value is never forwarded to the child (pult strips it before running the
command) and pult prints a one-line warning to stderr; the run proceeds as
if `PULT_EVENTS` had never been set.

**Fd 3 conflicts**: pult's own channel always uses fd 3. If pult itself was
started with fd 3 already open (for example `pult import 3<seed.txt`), that
means the invoker deliberately passed a descriptor — their use of fd 3 wins,
so pult runs that command with no events channel at all rather than
clobbering it.

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

## Editor support (JSON Schema)

pult ships a JSON Schema for the manifest (`pult.schema.json`, draft-07,
generated from the parser's own types so it can't drift). Any editor running
the YAML language server (VS Code's Red Hat YAML extension, `yaml-language-server`
in Neovim/others) uses it for completion, inline validation, and hover docs.

`pult init` writes the modeline for you; to add it to an existing manifest,
put this first line in the file (version-pinned to match your binary):

```yaml
# yaml-language-server: $schema=https://raw.githubusercontent.com/lonic-software/pult/vX.Y.Z/pult.schema.json
```

Offline or in CI, `pult self schema` prints the compiled-in schema (it always
matches the running binary) — pipe it to a file and point the modeline at a
local path, or validate manifests in CI with any JSON-Schema validator.

## Requirements

- `sh` and `bash` on PATH (command execution; compiled step scripts use bash).
- `git` on PATH for git module sources.
- Windows: both are provided by Git for Windows; run `pult` from Git Bash.
