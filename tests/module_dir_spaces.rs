//! `${module.dir}` under a path with a space (a Windows home like
//! `C:\Users\Máté Kolonics`, where the module cache lives) still reaches the
//! module's shipped executables, wherever the author put the reference: bare,
//! inside double quotes, in a `check:` or a compiled step.

use std::process::{Command, Output, Stdio};

const MODULE: &str = r#"version: 1
params:
  word:
    pick: { from: "${module.dir}/bin/say picked" }
steps:
  quoted: |
    OUT="$("${module.dir}/bin/say" quoted)"
    echo "$OUT"
commands:
  - id: bare
    title: Bare
    check: "${module.dir}/bin/say check"
    run: "${module.dir}/bin/say bare"
  - id: steps
    title: Steps
    params:
      word: { use: word }
    run:
      - use: quoted
      - "${module.dir}/bin/say {word}"
"#;

const ROOT: &str = r#"version: 1
includes:
  - source: "./Máté Kolonics/mod"
commands: []
"#;

struct Env {
    repo: tempfile::TempDir,
    state: tempfile::TempDir,
    trust: tempfile::TempDir,
}

impl Env {
    fn new() -> Env {
        let env = Env {
            repo: tempfile::tempdir().unwrap(),
            state: tempfile::tempdir().unwrap(),
            trust: tempfile::tempdir().unwrap(),
        };
        let module = env.repo.path().join("Máté Kolonics").join("mod");
        std::fs::create_dir_all(module.join("bin")).unwrap();
        std::fs::write(module.join("pult.module.yaml"), MODULE).unwrap();
        let say = module.join("bin").join("say");
        std::fs::write(&say, "#!/bin/sh\necho \"said $1\"\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&say, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::write(env.repo.path().join("pult.yaml"), ROOT).unwrap();
        env
    }

    fn pult(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_pult"))
            .current_dir(self.repo.path())
            .env("PULT_STATE_DIR", self.state.path())
            .env("PULT_TRUST_STORE", self.trust.path().join("trust.json"))
            .env(
                "PULT_USER_MANIFEST",
                self.repo.path().join("no-such-user-manifest.yaml"),
            )
            .args(args)
            .stdin(Stdio::null())
            .output()
            .expect("failed to run the pult binary")
    }
}

fn ok_stdout(out: &Output) -> String {
    assert!(
        out.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn a_bare_reference_runs_the_shipped_script() {
    let env = Env::new();
    let out = ok_stdout(&env.pult(&["--trust", "bare"]));
    assert!(out.contains("said bare"), "got: {out}");
}

#[test]
fn quoted_and_compiled_references_run_the_shipped_script() {
    let env = Env::new();
    let out = ok_stdout(&env.pult(&["--trust", "steps", "hello"]));
    assert!(out.contains("said quoted"), "got: {out}");
    assert!(out.contains("said hello"), "got: {out}");
}

#[test]
fn a_check_reaches_the_shipped_script() {
    let env = Env::new();
    let out = ok_stdout(&env.pult(&["--trust", "doctor"]));
    assert!(out.contains('✓'), "check did not pass: {out}");
}
