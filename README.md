# pult

A manifest-driven launcher for your repo's operational commands: one shared
binary, one small `pult.yaml` committed per repo, and every declared command
usable two ways from the same declaration —

- **Direct CLI** — `pult shell demo-leeds dev`, with generated `--help`
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
  - source: ./tools            # dir with module.yaml, or a yaml file
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

- **Registry sources** — https static hosts and S3 as module backends, with
  token-helper / cloud-credential auth (decentralized, like Homebrew taps).
- **Full-screen dashboard** (maybe) — a ratatui view over the same manifest
  with live status, if a launcher-with-prompts ever proves insufficient.
