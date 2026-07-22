# pult

A manifest-driven launcher for your repo's operational commands: one shared
binary, one small `pult.yaml` committed per repo, and every declared command
usable two ways from the same declaration —

- **Direct CLI** — `pult build server prod`, with generated `--help`
- **Guided flow** — bare `pult` opens a menu, then one prompt per parameter

The consumer footprint stays language-agnostic: a Java, Rust, or TypeScript
repo drops a `pult.yaml` and shells out to its own native tooling. No Node,
no foreign runtime — commands travel in git, current for everyone who pulls.

## What that unlocks

The mechanics below — pinned includes, an immutable module cache,
trust-on-change, guided prompts — add up to more than a task runner: a
**distribution and trust layer for executable command sets**. The same binary,
pointed at different kinds of command set:

- **An org-wide paved road.** A platform team publishes
  `github.com/your-org/paved-road@v2`; every service repo includes it. Now
  `deploy`, `rotate-creds`, and `tail-logs` are the same named commands in
  every repo, versioned centrally, rolled out by bumping one pin — with
  `pult includes verify` in CI to catch drift.
- **Incident runbooks that execute.** At 3am nobody wants to reconstruct the
  exact `aws ecs execute-command` incantation. Run `pult`, arrow through, and
  live pickers fill the blanks. Wikis can't execute; shell scripts can't
  prompt; `pult` reads back what it will run before anything runs.
- **Something you hand to someone else.** Ship a client a repo they can
  deploy and maintain without you, or a workshop repo where the guided flow
  replaces a wall of copy-paste — pinned includes make it reproducible, and
  the trust prompt means they read what they execute.
- **A tool surface for agents.** `pult --list --json` gives an LLM agent an
  enumerable set of named operations with declared parameters and origins —
  a bounded, human-vetted, versioned alternative to handing it raw shell.

None of these need anything beyond a `pult.yaml`.

## Install

macOS / Linux / Git Bash:

```sh
curl -fsSL https://raw.githubusercontent.com/lonic-software/pult/main/install.sh | sh
```

Windows PowerShell (needs Git for Windows — `pult` executes via bash):

```powershell
irm https://raw.githubusercontent.com/lonic-software/pult/main/install.ps1 | iex
```

Pin a version with `PULT_VERSION=v0.1.0`; choose the destination with
`PULT_INSTALL_DIR`. From source:

```sh
cargo install --path .
```

Once installed, `pult update` self-updates to the latest release
(verified against the release checksums, atomic swap with rollback).

Releases are cut automatically by pushing a `vX.Y.Z` tag
(`.github/workflows/release.yml` builds macOS arm64/x64, Linux x64/arm64
static-musl, and Windows x64, with sha256 checksums).

## Documentation

- **[User guide](docs/user-guide.md)** — install, everyday use, the trust
  prompt, git modules from the consumer side, troubleshooting.
- **[Authoring guide](docs/authoring.md)** — writing commands, composing with
  steps/outputs/pipes, writing and publishing modules.
- **[Reference](docs/reference.md)** — full manifest schema, CLI, exit codes,
  file locations.

## Try it

```sh
cd examples
pult                 # guided flow: menu → pickers → run
pult --list          # what does this repo declare?
pult --list --json   # the same, machine-readable (for tooling and agents)
pult greet hello     # direct; missing params are prompted for
```

## The manifest

Starting from zero? **`pult init`** drops a commented starter manifest in the
current directory (`--user` for your personal one) that runs immediately — and
includes a JSON Schema modeline, so an editor with the YAML language server
gives you completion and inline validation as you author. (`pult self schema`
prints the schema for CI or offline use.)

`pult` discovers the nearest `pult.yaml` walking up from the current directory
(the way eslint/vite find their config). Commands run with the manifest's
directory as cwd. When no repo manifest exists anywhere up the tree, `pult`
falls back to your **user manifest** (`~/.config/pult/pult.yaml`) — the same
format, your personal toolbox, available wherever a repo isn't. User-scoped
commands run in your invocation directory instead; a repo manifest always
wins, so nothing personal ever shadows a repo's commands.

```yaml
version: 1            # required; newer versions fail with "upgrade pult"
name: directory-connect
commands:
  - id: shell
    title: Open a shell
    params:           # prompted in declared order
      env: { pick: { options: [dev, uat, pre] } }
      # dynamic source: stdout lines of a shell-out; may reference
      # earlier params — prompts are sequential, so the value exists
      customer: { pick: { from: "./bin/ops-impl list-customers --env {env}" } }
      note: { input: { default: "" } }
    run: "./bin/ops-impl shell {customer} {env}"
```

- `params` are ordered; a `from:` source may interpolate `{param}` only for
  params declared **before** it (validated at load).
- Pick options may carry an optional, display-only description — a static
  `{ value: uat, description: "User acceptance" }` or a `from:` line of
  `value<TAB>description` — shown as `value — description` in the picker; the
  bare value is always what's passed and validated.
- Interpolated values are shell-quoted, so picker/input values can't inject
  into the command line.
- `run` executes via `sh -c` with **inherited stdio** — interactive sessions
  (`aws ecs execute-command`, SSM tunnels) get a working PTY. This is why
  `pult` is a prompt flow, not a full-screen TUI.

## Building blocks & local modules

Manifests can also declare **named params** and **named steps**, compose them
into commands, and include modules — local paths or **pinned git repos**
(https/s3 registry sources are planned):

```yaml
includes:
  - source: ./tools            # dir with pult.module.yaml, or a yaml file
    vars: { marker: "»" }      # binds the module's declared vars
    prefix: t                  # everything becomes t:<name>

  # git module — publish by pushing a tag; auth is your existing git setup
  - source: github.com/opskit/aws-common@v1.4.2      # or a full commit sha
    prefix: aws
  # any transport / subdir: git::ssh://git@corp/ops.git//modules/common@v2

commands:
  - id: announce
    title: Announce
    params:
      fruit: { use: t:fruit }  # reuse the module's picker
    run:                       # a step list — compiled to ONE bash script
      - use: t:stamp           # module step with declared outputs: [NOW]
        exports: { NOW: STAMP }
      - pipe:                  # stdout chaining
          - "echo {fruit}"
          - use: t:shout
      - "echo \"(at $STAMP)\""
```

Step lists compile to a single bash script (`set -euo pipefail`): named steps
become shell functions, declared outputs get runtime assertions ("step X did
not set output Y"), and shell variables are the wiring between steps.
`pult <cmd> --print` shows the composed script instead of running it. Modules
can ship executables next to their yaml, addressed as `${module.dir}/bin/…` —
that's where real logic belongs; the yaml only composes. Trust covers the
resolved whole, shipped executables included: editing anything in an included
module directory re-triggers the trust prompt on every consuming manifest
(git modules get this from the pinned commit; local directory modules are
tree-hashed).

Adding an include by hand is optional — **`pult includes add <source>`** pins
the source's latest version tag, shows what commands it brings, and appends
it to the nearest manifest (or your user manifest with `--user`, creating it
if needed):

```sh
pult includes add github.com/your-org/ops-modules//ecs --prefix ecs --user
pult ecs:shell    # available everywhere, pinned, trust-prompted on first run
```

Git modules must be pinned to a **tag or full commit sha** — branches are
rejected, so the same manifest always resolves to the same commands. Fetches
are shallow, via your system `git` (ssh keys and credential helpers just
work), cached immutably under `~/Library/Caches/pult/modules` (override:
`PULT_CACHE_DIR`) — after the first fetch everything works offline. A cached
checkout has its `.git` stripped; what you trusted is what runs, even if the
remote's tag later moves. **`pult includes verify`** is the CI guard: it checks
every include still resolves and that no pinned tag has moved on the remote
(exit 1 on drift). Commit-sha pins are immutable by construction.

Try it: `cd examples && pult --list`, then `pult announce`, and
`pult announce apple hi --print` to see the generated script.

## Ephemeral execution — `pult x`

Not every command set is worth committing an include for. **`pult x <source>
[command]`** runs a module's command straight from its source — a local path or
a pinned git repo — without touching any `pult.yaml`:

```sh
pult x github.com/lonic/forklift install      # run forklift's installer, here, now
pult x ./toolbox                              # no command → a menu of what it offers
pult x github.com/lonic/forklift@v2 doctor    # pin explicitly; a bare source takes the latest tag
```

This is the `npx` of pult: it turns a repo into something a stranger can *run* —
an installer, a one-shot bootstrapper, a workshop demo — in a single line, with
nothing to configure first. What it does **not** drop is anything that makes
that safe. The source is pinned (a bare git source resolves to its latest
version tag, exactly like `includes add`), fetched into the same immutable
cache, and gated by the same trust-on-first-use prompt. That prompt shows **the
exact command it's about to run**, so a single `y` both approves the source and
runs it — you read the actual script as you approve it, and a moved pin
re-prompts. Commands run in your current directory (they act on where you are),
modules that ship executables under `${module.dir}` work unchanged, and
`--var NAME=VALUE` binds a module's `vars:`. "No config" never means "unseen
code."

Prefer to look before you commit to running anything? **`pult x <source>
<command> --print`** prints that same composed script with no prompt and no
trust recorded — a side-effect-free dry run (it never runs a module's option
sources either), with unsupplied params shown as `<name>`.

The trust identity is the source itself — a pinned git source (globally unique)
or a local module's canonical path — so re-running one you've trusted doesn't
re-prompt, while a new version, or an edited local tree, does. Ephemeral is the
*try*; **`pult includes add <source>`** is the *keep* — once a command set earns
a place in the repo, pin it into the manifest so everyone who pulls gets exactly
the same thing.

## Trust model

A discovered manifest is a list of things to *execute*, so `pult`
does direnv-style trust-on-first-use: the first time it sees a manifest — and
whenever anything it resolves changes — it asks before running anything, and
remembers the answer (sha256 per path, stored in
`~/Library/Application Support/pult/trust.json` on macOS; override with
`PULT_TRUST_STORE`). The hash covers the resolved whole: the root file, every
include's pin, module yaml, and — for local directory modules — every file
the module ships. What trust does **not** do is sandbox: a trusted command is
ordinary shell with your credentials. The guarantees are change visibility
and pin immutability — you always get a prompt before anything different runs. Non-interactive contexts refuse
untrusted manifests; pass `--trust` to accept explicitly (e.g. CI).

## Layout

| Module | Responsibility |
|---|---|
| `manifest.rs` | schema, version gate, intra-file validation |
| `discovery.rs` | walk up from cwd to find `pult.yaml` |
| `resolver.rs` | includes, vars, prefixing, `use:` resolution, merged validation |
| `fetch.rs` | git source parsing, pinned fetch, immutable cache |
| `compile.rs` | step lists → one bash script (functions, assertions, pipes) |
| `trust.rs` | trust-on-first-use store over the resolved whole |
| `interp.rs` | strict/lenient `{param}` interpolation, `${var}` substitution, quoting |
| `options.rs` | static / shell-out option sources |
| `exec.rs` | fill params (provided or prompted), the single execution choke point |
| `flow.rs` | bare-invocation guided flow |
| `prompt.rs` | inquire wrappers, TTY checks, cancel handling |
| `runner.rs` | `sh`/`bash` with inherited stdio, exit-code passthrough |
| `verify.rs` | `pult includes verify` — pin drift detection |
| `selfupdate.rs` | `pult update` — checksum-verified atomic self-update |

## Roadmap

Ephemeral execution (`pult x`, above) is the first half of turning pult into a
*distribution* channel, not just a per-repo launcher. The rest stays
deliberately **decentralized** — a source is a git repo you already control, and
trust is something you grant, never something a central index grants for you:

- **Named sources (`pult tap`)** — `pult x` and `includes add` already take a
  full `host/org/repo` source; the missing nicety is a short name for one.
  `pult tap lonic/forklift` records an alias in your user config, and then
  `pult x forklift install` resolves through it — Homebrew-tap style, opt-in,
  no central pult.io registry to typosquat or trust by default. A name is only
  ever an alias *you* added; the pin and the trust prompt still gate every run.
- **Registry transports** — https static hosts and S3 as module backends (today
  a source is a git repo), with token-helper / cloud-credential auth, so an org
  can host its command sets wherever it already hosts artifacts.

Then a richer interactive surface, in four independently useful steps —
each **optional for command authors**, and none may weaken the properties
that make pult work: inherited stdio for interactive commands, plain
scriptable output, no runtime.

Two small schema affordances landed *before* any of it (**shipped**), because
every later surface depends on them and both improve today's CLI:

- **`secret:` inputs** — a param input declarable as secret: no prompt echo,
  masked wherever an interpolated command line is displayed (`running:`
  banner, `--print`, the trust prompt), redacted from future teed/audit
  output, a password field in any form UI (`"secret"` in `--list --json`).
  Credentials pasted as params (a DB password, cloud keys) are the use case;
  it went first because retrofitting secrecy after values have leaked into
  scrollback is far worse than adding the flag.
- **`check:` readiness** — an optional per-command probe (exit 0 = ready) so
  a surface can say "Docker isn't running" *before* the run button, not
  after. The plain CLI gets `pult doctor` (trust-gated, exit 1 if any check
  fails); UIs read `"check"` from `--list --json`. Preflight/setup steps
  inside playbooks remain the real mitigation — `check:` only moves the
  signal ahead of the click.

The `interactive:` marker is also parsed, documented, and surfaced
(`--list`, `--list --json`) today, so authors can start declaring the
contract now; its runtime behavior (handoff / terminal-only) lands with the
surfaces below.

1. **Events protocol (shipped)** — scripts *may* write `progress` / `status`
   / `step` lines to a `PULT_EVENTS` descriptor (stdout/stderr stay
   untouched; non-emitting scripts lose nothing). Three verbs, on purpose —
   events carry semantics, surfaces own presentation: pult decides *what*
   happened, never *how* it's drawn. Unknown verbs and malformed lines are
   silently ignored, in either direction, which is what makes the vocabulary
   additive-forever rather than a versioned break waiting to happen — it
   must never grow into a framework contract. Compiled step lists emit
   `step k/n <name>` milestones automatically, one per top-level entry, with
   zero manifest changes; the same names are exposed as `"steps"` in
   `--list --json`, so a non-CLI surface can render them without parsing the
   script. Plain-CLI pult translates events to OSC 9;4, so terminals that
   render progress natively (Windows Terminal, WezTerm, Ghostty) show it
   with zero drawing on our side. If `PULT_EVENTS` is already set when pult
   runs a command, pult does nothing — the var and its fd pass straight
   through to the child — so a parent process (the future desktop app) can
   own the channel and render events its own way instead.
2. **Launcher palette** — evolve the bare-`pult` flow into a scope-aware,
   searchable palette (repo + user side by side; menus don't have the CLI's
   namespace-collision problem), organized into tabs/sections by command
   group with arrow-key navigation rather than one long list. The grouping
   data model is **shipped** ahead of the palette itself: a `category:` field
   on commands, falling back to include origin when unset, feeds a single
   grouping rule shared by every surface — today that's the two-stage
   guided flow and grouped `--list` output; the palette just renders the
   same groups as tabs. The palette always *exits before the command runs* —
   commands keep the real terminal.
3. **Pane runner** (`pult ui`, id reserved) — long-running non-interactive
   commands run in a pane with live output and event-driven progress
   (portable-pty + a vt100 grid); commands marked `interactive:` get a
   full-terminal handoff, lazygit-style, preserving today's guarantee.
   `interactive:` means exactly *"requires a controlling terminal at
   runtime"* — an undeclared command must be fully non-interactive once its
   declared params are filled (declare a param, don't `read`), which is the
   contract that makes any non-terminal surface safe. For the stray prompt
   an author didn't write (sudo, a host-key confirmation), the pty makes the
   question visible and a one-line stdin box under the pane answers it — a
   silent hang becomes an answerable prompt. Output teed to a file becomes
   an audit artifact (secrets redacted). The big one — only after 1 and 2
   prove the demand.
4. **Desktop app** (Tauri) — the same core with a windowed shell, for
   handing a repo's playbooks to people who won't open a terminal at all:
   point it at a folder (or a `pult x` source), the trust prompt becomes a
   native dialog, commands are listed per scope, params render as forms
   (secrets masked), `check:` state shows up front, pty output streams into
   a pane with the same one-line stdin box for stray prompts.
   `interactive:` commands are shown but terminal-only. The app bundles the
   pult binary itself as a Tauri sidecar and drives it over its existing
   machine surfaces — `--list --json` for command, param, and readiness
   metadata, `pult doctor --json` for readiness, `pult <command>` spawned
   under a pty for execution and output — rather than linking a core crate:
   the crate's runner inherits terminal stdio a windowed app doesn't have,
   so the app needs its own pty spawning either way, and the reusable part
   is already the stable JSON surface. The process boundary also keeps the
   pult binary the single trust choke point (an app bug can't bypass trust
   it never enforces itself) and makes the app dogfood the same
   `--list --json` contract agents and tooling rely on. The workspace split
   into a core crate isn't dead, just deferred until the app needs
   something the process boundary can't give it. A `pult serve` local-web
   mode was considered and rejected: anyone already in a terminal will just
   use pult there, and anyone who won't open a terminal shouldn't need one
   to start a server (`serve` and `doctor` stay reserved anyway — reserving
   is free). The environment burden stays in playbooks: preflight/setup
   steps, running the work in a container or remotely are things *authors*
   write; pult never grows a `runs_on:` or executor abstraction. That gate
   has been overridden: development starts now, in a separate
   `pult-desktop` repository, kept out of this one so the core stays
   dependency-light and the app can only consume the documented machine
   surfaces above — the sidecar contract, pinned to released pult
   versions. Packaging, signing, and an update channel are a permanent
   tax, but it lands in that repo, not here; the demand call is that
   handing executable playbooks to non-technical colleagues is itself
   the signal, justifying starting ahead of the palette and pane runner.
