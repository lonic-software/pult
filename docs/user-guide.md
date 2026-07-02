# pult — user guide

`pult` is a launcher for your repo's operational commands. Someone in your team
declares the commands once in a `pult.yaml`; you run them — either through a
guided menu or directly from the command line. No runtime, no per-person setup:
one binary plus whatever the repo committed.

## Install

macOS / Linux / Git Bash:

```sh
curl -fsSL https://raw.githubusercontent.com/lonic-software/pult/main/install.sh | sh
```

Windows PowerShell:

```powershell
irm https://raw.githubusercontent.com/lonic-software/pult/main/install.ps1 | iex
```

Pin a version with `PULT_VERSION=v0.1.0`, change the destination with
`PULT_INSTALL_DIR`. From source: `cargo install --path .`.

Once installed, updating is built in:

```sh
pult update            # self-update to the latest release
pult update v0.2.0     # or to a specific version (up or down)
```

It downloads the right binary for your platform, verifies its checksum, and
swaps it in place atomically (rolling back if anything fails). Works from
anywhere — no manifest needed.

> **Windows note:** the binary is native, but commands execute via `sh`/`bash`
> and git modules are fetched via `git`. Install Git for Windows and run `pult`
> from Git Bash.

## Everyday use

`pult` finds the nearest `pult.yaml` walking **up** from wherever you are (like
eslint or vite find their config), so it works from any subdirectory. Commands
always run with the manifest's directory as the working directory.

```sh
pult              # guided flow: pick a command, answer the prompts
pult --list       # what does this repo offer? (with argument names and origins)
pult --list --json             # the same, machine-readable (tooling, agents)
pult shell demo-leeds dev      # direct: values in declared order
pult shell demo-leeds          # partial: missing values are prompted
pult shell --help              # per-command help, generated from the manifest
pult deploy --print            # show the exact script it would run, don't run it
```

- Pickers with a fixed option list reject invalid values on the command line;
  pickers fed by a live source (a shell command) accept any value directly so
  scripting stays fast.
- Esc or Ctrl-C during a prompt exits quietly (code 130). A command's exit
  code passes through unchanged, so `pult` works fine inside scripts and CI.
- Interactive things (shells into containers, tunnels) just work — `pult` runs
  commands with your terminal attached.

## Your personal launcher

When you run `pult` somewhere with no repo manifest anywhere up the tree, it
falls back to your **user manifest** at `~/.config/pult/pult.yaml` (override
with `PULT_USER_MANIFEST`). Same format, same trust prompt, same everything —
it's your personal toolbox for commands that don't belong to any repo: VPN
up/down, docker cleanup, cloud session helpers, the things that otherwise live
as shell aliases. Two differences worth knowing:

- Personal commands run in your **current directory** (repo commands run at
  the manifest's directory).
- A repo manifest **always wins** — inside a repo you see the repo's commands
  only, so nothing personal ever shadows or leaks into a repo's namespace.

Your user manifest can `include:` pinned git modules like any other, so a
shared personal-tooling module travels the same way repo modules do.

## The trust prompt

A `pult.yaml` is a list of things to *execute*, so the first time you run
anything from a given manifest — and any time it or a module it includes
changes (the yaml, or any file a local module directory ships, executables
included) — `pult` shows you what resolved and asks once:

```
Trust the manifest /repo/pult.yaml?
  It includes:
    · github.com/opskit/aws-common@v1.4.2 (commit 8a6e6fd4a4)
 Commands in these files will be executed. Trust? [y/N]
```

Answers are remembered per file per user (nothing is stored in the repo). In
non-interactive contexts (CI), an untrusted manifest is refused; pass
`--trust` to accept explicitly — it records trust even on a non-executing
invocation like `pult --trust --list`.

Treat this prompt like `curl | sh`: read it. If a manifest changed and you
didn't expect it to, that's the moment to look. And know what the prompt is
not: it's change visibility, not a sandbox — a trusted command runs as
ordinary shell with your credentials.

## Git modules, pins, and the cache

Repos can include command sets from git repos, pinned to a tag or commit:

```yaml
includes:
  - source: github.com/opskit/aws-common@v1.4.2
```

What that means for you as a user:

- **First run fetches, everything after is offline.** Modules are cached
  immutably (macOS `~/Library/Caches/pult/modules`, Linux `~/.cache/pult/modules`;
  override with `PULT_CACHE_DIR`). A dead network or deleted upstream doesn't
  break your commands.
- **What you trusted is what runs.** Even if someone force-moves the tag
  upstream, your cached pin is untouched.
- **`pult includes verify`** checks that every include still resolves and that
  no pinned tag has moved on its remote. Exit 1 on drift — put it in CI:

  ```
  ok     github.com/opskit/aws-common@v1.4.2 — tag v1.4.2 still at 8a6e6fd4a4
  drift  git::…/gitmod@v1.0.0 — tag v1.0.0 MOVED on the remote: cached 8a6e6fd4a4 but remote is e3cc39b41c
  ```

Fetching uses your system `git`, so private repos work with whatever auth you
already have (ssh keys, credential helpers) — there is nothing to log into.

## Environment variables

| Variable | Effect |
|---|---|
| `PULT_TRUST_STORE` | alternate trust-store file (default: `<config>/pult/trust.json`) |
| `PULT_CACHE_DIR` | alternate module cache (default: `<cache>/pult/modules`) |
| `PULT_USER_MANIFEST` | alternate user manifest (default: `~/.config/pult/pult.yaml`) |

## Troubleshooting

- **"manifest … is not trusted"** in a script/CI → run interactively once, or
  pass `--trust`.
- **"no pult.yaml found"** → you're not inside a repo that has one (it
  searches upward from the current directory), and you have no user manifest
  at `~/.config/pult/pult.yaml` to fall back to.
- **A picker shows no options / errors** → the option source is a shell
  command defined in the manifest; the error includes its stderr. Often an
  expired cloud session — refresh it and retry.
- **What is it actually going to run?** → `pult <command> --print`.
- **"failed to fetch git module"** → the error includes git's own message
  (auth, typo'd tag, unreachable host). Warm caches keep working offline.
