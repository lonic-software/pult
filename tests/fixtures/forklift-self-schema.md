# forklift-self-schema.json

Forklift's real `forklift self schema --json` envelope, verbatim — the document
`pult tool forklift` reads. The unit tests in `src/tool.rs` parse it as the
contract between the two tools: every command loads, in Forklift's order, and
composes to the argv Forklift's own CLI accepts, and every param carries its
help as its description (one is pinned, so a change to that help shows up there).

**Provenance.** Captured from lonic-software/forklift at commit
`2cd0504469dc9ac039e347f82ffa3ec9693b0369` (`forklift version` 0.3.1, the
completed command-surface redesign), debug build:

```sh
cargo build -p forklift
target/debug/forklift self schema --json > forklift-self-schema.json
```

**Refreshing.** The env-gated test in `tests/tool.rs` compares a live binary's
`data` with this file's and runs `pult tool <forklift> version` end to end:

```sh
PULT_TEST_FORKLIFT=/path/to/forklift cargo test --test tool live
```

When it reports drift, recapture with the command above, update the commit here,
and rerun `cargo test` — a changed command shape shows up in the unit tests'
argv table.
