//! A git module's shipped scripts run even when the user's git converts line
//! endings on checkout — Git for Windows's default `core.autocrlf=true`,
//! reproduced here on any host through `GIT_CONFIG_GLOBAL`. A CRLF checkout
//! of `bin/say` would fail in sh with `$'\r': command not found`.

use std::path::Path;
use std::process::{Command, Output, Stdio};

fn git(args: &[&str], cwd: &Path, config: &Path) {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", config)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn an_autocrlf_git_config_does_not_reach_module_scripts() {
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join("gitconfig");
    std::fs::write(
        &config,
        "[core]\n\tautocrlf = true\n[user]\n\tname = t\n\temail = t@t\n[commit]\n\tgpgsign = false\n[tag]\n\tgpgsign = false\n",
    )
    .unwrap();

    let remote = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(remote.path().join("bin")).unwrap();
    std::fs::write(
        remote.path().join("pult.module.yaml"),
        "version: 1\ncommands:\n  - { id: hi, title: Hi, run: \"${module.dir}/bin/say\" }\n",
    )
    .unwrap();
    let say = remote.path().join("bin").join("say");
    std::fs::write(&say, "#!/bin/sh\nset -e\nWORD=hi\necho \"said $WORD\"\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&say, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    git(&["init", "-q"], remote.path(), &config);
    git(&["add", "-A"], remote.path(), &config);
    git(&["commit", "-q", "-m", "init"], remote.path(), &config);
    git(&["tag", "v1"], remote.path(), &config);

    let repo = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let source = remote.path().to_string_lossy().replace('\\', "/");
    std::fs::write(
        repo.path().join("pult.yaml"),
        format!("version: 1\nincludes:\n  - source: \"git::{source}@v1\"\ncommands: []\n"),
    )
    .unwrap();

    let out: Output = Command::new(env!("CARGO_BIN_EXE_pult"))
        .current_dir(repo.path())
        .env("GIT_CONFIG_GLOBAL", &config)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("PULT_CACHE_DIR", state.path().join("cache"))
        .env("PULT_STATE_DIR", state.path())
        .env("PULT_TRUST_STORE", state.path().join("trust.json"))
        .env("PULT_USER_MANIFEST", repo.path().join("none.yaml"))
        .args(["--trust", "hi"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && stdout.contains("said hi\n"),
        "stdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
