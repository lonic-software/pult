# pult — authoring guide

How to write commands, compose them from building blocks, and publish modules
other repos can include. The complete field-by-field schema is in
[reference.md](reference.md); this guide is the narrative version.

The one design rule to keep in mind throughout: **YAML composes; executables
compute.** The manifest declares *what* runs and wires pieces together. The
moment something needs a conditional, a loop, or data juggling, write it as a
script or program and have the yaml call it — never grow logic into the yaml.

## 1 · Your first command

`pult init` scaffolds a starter manifest — and writes a schema modeline into
it, so an editor with the YAML language server gives you completion, typo
flagging, and hover docs as you type (see
[reference.md](reference.md#editor-support-json-schema)). This is what a
filled-in `pult.yaml` at the repo root looks like:

```yaml
version: 1            # required — lets future engines fail with "upgrade pult"
name: my-project

commands:
  - id: shell
    title: Open a shell
    params:
      env: { pick: { options: [dev, uat, pre] } }
      customer: { pick: { from: "./bin/list-customers --env {env}" } }
      note: { input: { default: "" } }
    run: "./bin/open-shell {customer} {env}"
```

Commit it. Everyone who pulls now has `pult shell`, a guided flow, `--help`,
and `--list` — all generated from this declaration.

**Params are prompted in declared order.** That ordering is load-bearing: a
`from:` option source may reference `{param}` only for params declared
*before* it (the answer already exists when the source runs — that's how
dependent pickers work, and it's validated at load).

**Interpolation in `run:` strings and `from:` sources is strict**: `{env}`
must be a declared param, unknown placeholders are load errors, `{{`/`}}` are
literal braces. Interpolated values are shell-quoted — a user typing
`; rm -rf /` into an input becomes a harmless quoted string.

## 2 · Composing commands from steps

For anything beyond a one-liner, declare named building blocks and compose:

```yaml
params:                       # named params — reusable pickers/inputs
  env: { pick: { options: [dev, uat, pre] } }

steps:                        # named script fragments
  ensure-session: |
    aws sts get-caller-identity >/dev/null 2>&1 || aws sso login
  resolve-task:
    outputs: [TASK]           # a declared contract: this step sets $TASK
    script: |
      TASK=$(./bin/find-task --env {env})

commands:
  - id: attach
    title: Attach to the running task
    params:
      env: { use: env }       # reference the named param
    run:                      # a step list — compiled into ONE bash script
      - use: ensure-session
      - use: resolve-task
      - "aws ecs execute-command --task $TASK --interactive --command /bin/sh"
```

How step lists execute: each named step becomes a **shell function** in a
single generated bash script (`set -euo pipefail`), and the run list becomes
the calls. Consequences:

- **Fail-fast**: any failing step aborts the command with its exit code.
- **Shell variables are the wiring**: `resolve-task` sets `TASK`, the next
  line uses `$TASK`. There is no other data-passing mechanism, on purpose.
- **Declared outputs are enforced**: if `resolve-task` exits successfully but
  never set `TASK`, the command fails right there with
  `pult: step resolve-task did not set declared output TASK` — not three steps
  later with an empty string.
- **`--print` shows the whole generated script.** Use it constantly while
  authoring; it's exactly what runs.

### Renaming and rebinding

When two steps produce the same output name, or a step's placeholder doesn't
match your param name:

```yaml
run:
  - use: resolve-task
    with: { env: "{target_env}" }        # step's {env} ← your {target_env}
    exports: { TASK: BACKEND_TASK }      # step's $TASK → your $BACKEND_TASK
```

Both are validated at load: `with:` keys must be placeholders the step
actually has, `with:` values may only reference your declared params, and
`exports:` may only rename outputs the step declares. Colliding outputs
without a rename are a load error.

### Pipes

For stream-shaped composition, `pipe:` compiles to a real shell pipe:

```yaml
run:
  - pipe:
      - use: list-tasks          # emits one per line
      - "grep -v draining"
      - "head -1"
```

Shell semantics apply: pipe segments run in subshells, so a step inside a
pipe contributes **stdout only** — its variable outputs don't escape. Pipe
segments must be single-line (give a multi-line script a name under `steps:`).

### Interpolation inside scripts is lenient

Step scripts and inline fragments are real shell, so `{name}` substitutes
**only** when `name` is a declared param and isn't preceded by `$`. Everything
else passes through untouched: `${TASK+x}`, `"${TASK}"`, `awk '{print $1}'`,
brace groups — no escaping needed. The flip side: a typo'd `{evn}` stays
literal and surfaces at run time (or in `--print`), not at load.

## 3 · Writing a module

A module is a yaml file — or a directory with `module.yaml` plus anything
else (scripts, binaries) — that exports params, steps, and commands for other
repos:

```yaml
# module.yaml
version: 1
name: aws-common
description: Shared AWS session & ECS blocks

vars:                          # configuration the consumer binds
  cluster_prefix:
    required: true
    description: ECS cluster name prefix, e.g. `dirconn`
  region:
    default: eu-west-2

params:
  env: { pick: { options: [dev, uat, pre] } }

steps:
  resolve-task:
    outputs: [TASK]
    script: |
      TASK=$(${module.dir}/bin/find-task --cluster ${cluster_prefix}-{env} --region ${region})

commands:
  - id: shell                  # consumers see this as <prefix>:shell
    title: Shell into a task
    params:
      env: { use: env }        # a module references its own blocks unprefixed
    run:
      - use: resolve-task
      - "aws ecs execute-command --task $TASK --interactive --command /bin/sh"
```

The pieces:

- **`vars`** are load-time configuration, bound by the consumer's include
  site. Each var must be `required: true` or have a `default` (not both).
  `${var}` is substituted when the module loads; any `${…}` that isn't a
  declared var passes through as shell (so `${TASK+x}` is safe).
- **`${module.dir}`** is the absolute path of the module's own directory —
  how you address shipped executables. Ship real logic as executables next to
  the yaml, in any language; the step is the interface, the executable is the
  implementation. You can rewrite `bin/find-task` from bash to Rust and no
  consumer changes anything.
- **Shipped files are part of the trust unit.** For a local directory module,
  the whole tree is hashed — editing `bin/find-task` re-triggers the trust
  prompt on every consuming manifest, same as editing the yaml. (Git modules
  get this from the pinned commit.) A single-file include covers only that
  file, so ship executables in directory modules, not next to a bare yaml.
- Modules cannot have `includes:` of their own (no transitivity).

## 4 · Consuming a module

The quick path is `pult includes add <source> [--prefix aws]` — it pins the
latest version tag, prompts for required vars, and writes the include for
you. What it writes is the following, which you can also author by hand:

```yaml
version: 1
name: my-service

includes:
  - source: github.com/opskit/aws-common@v1.4.2   # or @<full 40-char sha>
    vars: { cluster_prefix: mysvc }
    prefix: aws                                    # exports become aws:<name>
    # sha256: 9f2c…                                # optional integrity pin

commands:
  - id: deploy
    title: Deploy
    params:
      env: { use: aws:env }          # reuse the module's picker
    run:
      - use: aws:resolve-task        # reuse the module's step
      - "./scripts/deploy.sh {env} $TASK"
```

Source forms:

| Form | Meaning |
|---|---|
| `./tools` | local dir (with `module.yaml`) or yaml file, relative to this manifest |
| `github.com/org/repo@v1.2.3` | git over https |
| `github.com/org/repo//sub/dir@<sha>` | module in a subdirectory of a repo |
| `git::ssh://git@host/repo.git//sub@v2` | any git transport |

Rules that keep resolution deterministic and safe:

- **Remote sources must be pinned** to a tag or a **full 40-char commit sha**.
  Branches are rejected outright — a branch pin would silently freeze at
  first fetch. Prefer sha pins for anything you don't control; for tag pins,
  add `sha256:` (of the module yaml) or run `pult includes verify` in CI to
  catch moved tags.
- **Merging is explicit**: includes merge in order, local declarations last;
  any duplicate name (command, param, or step) in the merged whole is a load
  error — use `prefix:` to disambiguate. Nothing ever silently shadows.
- The command ids `includes`, `registry`, `module`, `update`, `self`, `init`,
  `trust`, `cache`, `ui`, and `events` are reserved for pult's own (current
  and future) subcommands; every other id is promised to manifests forever.

## 5 · Publishing a module

There is no registry to publish to — **publishing is pushing a git tag**:

```sh
cd aws-common && git tag v1.4.3 && git push --tags
```

Consumers bump their pin and get the change, with two safety nets built in:
their next run shows a re-trust prompt (the resolved content changed), and
your tag can't be re-pointed without `pult includes verify` flagging drift.
Access control is your git host's: a private repo is a private module, using
each consumer's existing git credentials.

### One repo, many modules

A module does **not** need its own repo. The `//subdir` source form means one
"toolbox" repo can host any number of modules, each in its own directory:

```
ops-modules/
  aws/module.yaml       # + aws/bin/…
  github/module.yaml
  oncall/module.yaml
```

```yaml
includes:
  - source: github.com/your-org/ops-modules//aws@v3.2.0
    prefix: aws
  - source: github.com/your-org/ops-modules//oncall@v3.2.0
    prefix: oc
```

One pushed tag versions the whole toolbox, and consumers of several modules
from the same repo@tag share a single cached checkout. Small modules — a few
lines of yaml and a script — belong together in a toolbox repo; give a module
its own repo only when it needs its own release cadence or access control.
(Pins are per-repo: `@v3.2.0` names one commit of the whole toolbox. Per-module
tags inside one repo aren't supported — a pin cannot contain `/`.)

And below git modules there's an even lighter tier: modules that only one
repo uses should just be local includes (`./tools`) in that repo — publishing
is only for sharing across repos.

Module design tips:

- Declare `outputs:` on any step a consumer might chain from — it's the
  difference between an interface and a script someone greps.
- Keep step scripts thin; push logic into `${module.dir}/bin/…` executables.
- Give vars `description:`s and prefer `default:` over `required:` where a
  sane default exists — every required var is friction at every include site.
- Treat renaming exported params/steps/outputs or changing their meaning as a
  breaking change: consumers reference them by name.
