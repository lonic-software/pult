//! Resolve `sh` and `bash` to spawn — the one place in pult allowed to name
//! a shell.
//!
//! **The bug this exists to fix (issue #9):** on Windows, `Command::new`'s
//! executable search consults `%SystemRoot%\System32` *before* `PATH`. A
//! stock Windows install ships a `bash.exe` stub there (the WSL launcher),
//! so `Command::new("bash")` resolves to that stub instead of Git for
//! Windows's `bash.exe` — even when Git's `bash.exe` is first on `PATH`.
//! With no WSL distro installed the stub exits 1 after writing a UTF-16LE
//! error message to stdout (mojibake to anything expecting UTF-8); with a
//! distro installed, the compiled step script silently executes inside a
//! Linux VM with a different filesystem. Either way every compiled
//! step-list command (`run_bash`) is broken.
//!
//! `sh` has a different (and independent) reason to route through here.
//! There's believed to be no `sh.exe` in `System32` — that's never actually
//! measured, just the absence of a counter-example — so `sh` doesn't share
//! `bash`'s wrong-binary failure mode. But a **default** Git for Windows
//! install puts only `Git\cmd` on `PATH`, not `Git\bin` or `Git\usr\bin`;
//! GitHub-hosted runners add those extra entries, real user machines don't.
//! So on a default install launched from PowerShell, bare `sh` isn't the
//! *wrong* binary, it's *absent* — string-form `run:`, every `check:`, and
//! every dynamic pick fail with a plain "program not found" instead of
//! reaching Git's actual `sh.exe`. That failure mode doesn't depend on
//! `System32`'s contents at all, which is why `sh` is resolved by the same
//! PATH-scan/exec-path/fixed-path search as `bash`, not a narrower one.
//!
//! The fix: resolve an absolute path ourselves on Windows, filtering out
//! anything that resolves under `System32`/`SysWOW64`/`Sysnative`/
//! `WindowsApps`, and fail loudly (never fall back to the bare name) if
//! nothing legitimate is found — a fallback would just silently reproduce
//! the defect it exists to catch.

use std::process::Command;

use anyhow::Result;

#[cfg(windows)]
use std::env;
#[cfg(windows)]
use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::sync::OnceLock;

#[cfg(windows)]
use anyhow::{Context, anyhow, bail};

/// Return a `Command` for `sh`, program resolved but no args set — callers
/// add `.arg("-c")` etc. as they do today.
#[cfg(unix)]
pub fn sh() -> Result<Command> {
    Ok(Command::new("sh"))
}

/// Return a `Command` for `bash`, program resolved but no args set.
#[cfg(unix)]
pub fn bash() -> Result<Command> {
    Ok(Command::new("bash"))
}

#[cfg(windows)]
pub fn sh() -> Result<Command> {
    Ok(Command::new(resolved_path("sh")?))
}

#[cfg(windows)]
pub fn bash() -> Result<Command> {
    Ok(Command::new(resolved_path("bash")?))
}

// ---------------------------------------------------------------------
// Pure decision logic — kept unconditional (compiled on every platform,
// unix included) so it's testable from the ubuntu/macOS CI leg, not only
// the Windows one. Nothing in this section touches the filesystem or the
// environment.
// ---------------------------------------------------------------------

/// Windows path-component words that mark a candidate as coming from one of
/// Windows' own system directories rather than a real shell install.
/// `System32`/`SysWOW64`/`Sysnative` all host (or transparently redirect
/// to) the WSL launcher stub; `WindowsApps` hosts app-execution-alias stubs
/// with the same "exits without doing what you asked" failure mode.
const SYSTEM_SHADOW_DIRS: [&str; 4] = ["System32", "SysWOW64", "Sysnative", "WindowsApps"];

/// Whether `path` has a path *component* — split on both `\` and `/`,
/// deliberately not `Path::components()`: on unix, `Path` treats `\` as an
/// ordinary character, so a Windows-style path wouldn't split into
/// components at all there, and a test built on `Path::components()` would
/// pass on unix for the wrong reason. Matching is case-insensitive (what a
/// real WSL-stub path looks like varies in case) and component-wise, so a
/// directory that merely *contains* one of these words as a substring
/// (`NotSystem32Stuff`) is not rejected. Returns the matched name, for the
/// error message, rather than a bool.
#[cfg_attr(not(windows), allow(dead_code))]
fn system_shadow_component(path: &str) -> Option<&'static str> {
    path.split(['\\', '/']).find_map(|part| {
        SYSTEM_SHADOW_DIRS
            .iter()
            .find(|dir| part.eq_ignore_ascii_case(dir))
            .copied()
    })
}

/// Whether `dir` (one `env::split_paths` entry, rendered as a string) is
/// usable as a search directory at all: non-empty and rooted — a drive
/// letter plus separator (`C:\` / `C:/`) or a UNC prefix (`\\server\share`
/// / `//server/share`). `env::split_paths` yields an *empty* `PathBuf` for
/// a trailing (or doubled) `;` in `PATH` — extremely common on Windows —
/// rather than skipping it; joining a candidate filename onto that empty
/// path and calling `exists()` resolves it against pult's own current
/// directory instead of any real search path. That's the exact "wrong
/// binary silently wins" failure this module exists to close, made worse
/// by the fact that a stray `bash.exe` checked into a repo never passes
/// the trust gate the way manifest code does. A bare drive letter with no
/// trailing separator (`C:`) is drive-*relative* in Windows path semantics
/// (relative to that drive's current directory), not a directory pult can
/// search, so it's rejected too.
#[cfg_attr(not(windows), allow(dead_code))]
fn is_absolute_dir_entry(dir: &str) -> bool {
    if dir.starts_with("\\\\") || dir.starts_with("//") {
        return true;
    }
    let bytes = dir.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
}

/// Derive the Git for Windows install root from `git --exec-path`'s
/// (already-trimmed) output. The exec path is
/// `<root>/mingw64/libexec/git-core` (or `mingw32/...`); dropping the last
/// three path segments and rejoining yields `<root>`. Pure and string-based
/// — never `Path::parent()` — specifically so it can be exercised with
/// both slash conventions on any host OS: Git for Windows itself prints
/// forward slashes, but nothing here assumes that.
///
/// Two shapes need help beyond naive splitting, both because a bare
/// `.filter(!is_empty())` erases information a plain path-segment list
/// doesn't carry on its own:
///
/// - **UNC roots** (`\\server\share\Git\...`): the leading `\\`/`//` marker
///   splits into two *empty* segments, which the empty-segment filter would
///   silently drop — turning the root into the relative `server\share\Git`.
///   The marker is split off first and reattached untouched.
/// - **A drive-letter-only root** (`C:/mingw64/...`, i.e. Git installed
///   directly at a drive root): joining a single `"C:"` component with
///   nothing after it leaves no separator following the colon, and
///   `Path::new("C:").join("bin")` is the drive-*relative* `C:bin`, not
///   `C:\bin`. Rooted explicitly by appending the separator in that one
///   case.
///
/// Both failure shapes only cost a missed resolution (falling through to
/// step 3), never a wrong one — `root_comps` failing to reconstruct a
/// sensible path just means the two `exists()` checks after it miss.
#[cfg_attr(not(windows), allow(dead_code))]
fn root_from_exec_path(exec_path: &str) -> Option<String> {
    let trimmed = exec_path.trim();
    let sep = if trimmed.contains('\\') && !trimmed.contains('/') {
        "\\"
    } else {
        "/"
    };

    let (unc_prefix, rest) = if let Some(r) = trimmed.strip_prefix("\\\\") {
        ("\\\\", r)
    } else if let Some(r) = trimmed.strip_prefix("//") {
        ("//", r)
    } else {
        ("", trimmed)
    };

    let comps: Vec<&str> = rest.split(['\\', '/']).filter(|s| !s.is_empty()).collect();
    if comps.len() < 4 {
        return None;
    }
    let root_comps = &comps[..comps.len() - 3];
    let mut root = format!("{unc_prefix}{}", root_comps.join(sep));
    if unc_prefix.is_empty() && root_comps.len() == 1 && root.ends_with(':') {
        root.push_str(sep);
    }
    Some(root)
}

// ---------------------------------------------------------------------
// Windows-only: actually touches the OS (env lookup, spawning `git`,
// `exists()`).
// ---------------------------------------------------------------------

#[cfg(windows)]
static SH_CACHE: OnceLock<Result<PathBuf, String>> = OnceLock::new();
#[cfg(windows)]
static BASH_CACHE: OnceLock<Result<PathBuf, String>> = OnceLock::new();

/// Memoized resolution — one `OnceLock` per shell, caching the error too:
/// resolution is deterministic per process, and both `doctor` (one `sh` per
/// `check:`) and the guided flow (re-resolving picks) would otherwise
/// re-pay a failed resolution N times. `anyhow::Error` isn't `Clone`, so the
/// cache holds `String` and a fresh `anyhow!` is rebuilt from it each call.
#[cfg(windows)]
fn resolved_path(which: &str) -> Result<PathBuf> {
    let cache = match which {
        "sh" => &SH_CACHE,
        "bash" => &BASH_CACHE,
        _ => unreachable!("shell.rs only resolves \"sh\" and \"bash\""),
    };
    cache
        .get_or_init(|| resolve_windows(which).map_err(|e| e.to_string()))
        .clone()
        .map_err(|e| anyhow!(e))
}

/// The candidate order, in full:
///
/// 1. **PATH scan.** Honors a deliberate user setup (scoop, portable Git, a
///    second Git install) and keeps the shell coming from the same
///    installation as `git` itself (`fetch.rs`'s `git()` also resolves via
///    PATH) — filtered to drop anything under a [`SYSTEM_SHADOW_DIRS`]
///    component, and to skip any entry [`is_absolute_dir_entry`] rejects
///    (empty or drive-relative — see its doc for why that matters).
/// 2. **`git --exec-path`-derived.** The workhorse for the common case: a
///    default install launched from PowerShell, where PATH has only
///    `Git\cmd` (so step 1 finds nothing) but `git` itself still resolves.
///    `exists()`-checked so a weird `git` can't poison it.
/// 3. **Fixed install paths, last resort.**
///
/// Never falls back to the bare name — that fallback *is* the bug. On
/// failure, every candidate tried is named along with why it lost.
#[cfg(windows)]
fn resolve_windows(which: &str) -> Result<PathBuf> {
    let exe = format!("{which}.exe");
    let mut trail: Vec<String> = Vec::new();

    // 1. PATH scan.
    match env::var_os("PATH") {
        Some(path_var) => {
            for dir in env::split_paths(&path_var) {
                if !is_absolute_dir_entry(&dir.to_string_lossy()) {
                    // Empty (trailing/doubled `;`) or drive-relative entry —
                    // never join a filename onto it, see `is_absolute_dir_entry`.
                    continue;
                }
                let candidate = dir.join(&exe);
                if !candidate.exists() {
                    continue;
                }
                if let Some(shadow) = system_shadow_component(&candidate.to_string_lossy()) {
                    let what = if shadow == "WindowsApps" {
                        "an app-execution-alias stub"
                    } else {
                        "the WSL launcher stub"
                    };
                    trail.push(format!(
                        "rejected: {} is under Windows' own {shadow}, where {exe} is {what}, \
                         not a real shell",
                        candidate.display()
                    ));
                    continue;
                }
                return Ok(prefer_bin_over_usr_bin(&candidate, &exe));
            }
        }
        None => trail.push("PATH is not set".to_string()),
    }

    // 2. `git --exec-path`-derived.
    match git_exec_path() {
        Ok(exec_path) => match root_from_exec_path(&exec_path) {
            Some(root) => {
                let root = Path::new(&root);
                for sub in ["bin", "usr/bin"] {
                    let candidate = root.join(sub).join(&exe);
                    if candidate.exists() {
                        return Ok(candidate);
                    }
                    trail.push(format!("not found: {}", candidate.display()));
                }
            }
            None => trail.push(format!(
                "git --exec-path printed `{exec_path}`, which doesn't look like \
                 <root>/mingw64|mingw32/libexec/git-core"
            )),
        },
        Err(e) => trail.push(format!("git --exec-path: {e}")),
    }

    // 3. Fixed paths, last resort.
    for (var, suffix) in [
        ("ProgramFiles", "Git"),
        ("ProgramFiles(x86)", "Git"),
        ("LocalAppData", "Programs/Git"),
    ] {
        match env::var_os(var) {
            Some(base) => {
                let candidate = Path::new(&base).join(suffix).join("bin").join(&exe);
                if candidate.exists() {
                    return Ok(candidate);
                }
                trail.push(format!("not found: {}", candidate.display()));
            }
            None => trail.push(format!("{var} is not set")),
        }
    }

    Err(anyhow!(
        "could not find a real {exe} — every candidate was rejected or missing:\n{}\n\n\
         Install Git for Windows (https://git-scm.com/download/win) and run pult from \
         Git Bash — see docs/reference.md's Requirements section.",
        trail
            .iter()
            .map(|line| format!("  - {line}"))
            .collect::<Vec<_>>()
            .join("\n")
    ))
}

/// `bin\bash.exe` is the wrapper that sets up the MSYS environment
/// (`MSYSTEM`, `/mingw64/bin` and `/usr/bin` on the shell's own internal
/// PATH), so script steps find `grep`/`curl`/etc. the same way an
/// interactive Git Bash session would. `usr\bin\bash.exe` runs, but
/// inherits only the Windows PATH. If a PATH hit lands on
/// `<root>\usr\bin\<exe>`, prefer the sibling `<root>\bin\<exe>` when it
/// exists; otherwise use the PATH hit as found.
#[cfg(windows)]
fn prefer_bin_over_usr_bin(candidate: &Path, exe_name: &str) -> PathBuf {
    let parent_is_bin = candidate
        .parent()
        .and_then(|p| p.file_name())
        .is_some_and(|n| n.eq_ignore_ascii_case("bin"));
    let grandparent_is_usr = candidate
        .parent()
        .and_then(Path::parent)
        .and_then(|p| p.file_name())
        .is_some_and(|n| n.eq_ignore_ascii_case("usr"));
    if !(parent_is_bin && grandparent_is_usr) {
        return candidate.to_path_buf();
    }
    let Some(root) = candidate
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
    else {
        return candidate.to_path_buf();
    };
    let sibling = root.join("bin").join(exe_name);
    if sibling.exists() {
        sibling
    } else {
        candidate.to_path_buf()
    }
}

#[cfg(windows)]
fn git_exec_path() -> Result<String> {
    let out = Command::new("git")
        .arg("--exec-path")
        .output()
        .context("failed to run git --exec-path — is git installed?")?;
    if !out.status.success() {
        bail!("git --exec-path exited {}", out.status);
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

/// (a) Source-lint tripwire, cross-platform: a literal `Command::new("sh")`
/// or `Command::new("bash")` must not reappear outside this file — the
/// exact form this fix removed from every call site. This is a tripwire
/// against *that* literal regressing, not a proof that no file spawns a
/// shell some other way: `Command::new("bash.exe")`, a `const`/variable
/// holding `"bash"`, or a re-export would all slip past a plain substring
/// match and this test would stay green. Narrow on purpose rather than
/// naming — or claiming — a broader guarantee this check doesn't make.
/// Deliberately lives in this file — its own source necessarily contains
/// the needle strings (the unix impls above), and this file is the one
/// exclusion the walk makes.
#[cfg(test)]
mod source_lint {
    use std::fs;
    use std::path::{Path, PathBuf};

    #[test]
    fn no_literal_bare_shell_spawn_outside_shell_rs() {
        let src_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders = Vec::new();
        walk(&src_dir, &mut offenders);
        assert!(
            offenders.is_empty(),
            "literal `Command::new(\"sh\")` / `Command::new(\"bash\")` found outside shell.rs — \
             route through shell::sh()/shell::bash() instead:\n{}",
            offenders.join("\n")
        );
    }

    fn walk(dir: &Path, offenders: &mut Vec<String>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries {
            let Ok(entry) = entry else { continue };
            let path = entry.path();
            if path.is_dir() {
                walk(&path, offenders);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            if path.file_name().and_then(|n| n.to_str()) == Some("shell.rs") {
                continue;
            }
            let Ok(contents) = fs::read_to_string(&path) else {
                continue;
            };
            for (i, line) in contents.lines().enumerate() {
                if line.contains("Command::new(\"sh\")") || line.contains("Command::new(\"bash\")")
                {
                    offenders.push(format!(
                        "{}:{}: {}",
                        display_relative(&path),
                        i + 1,
                        line.trim()
                    ));
                }
            }
        }
    }

    fn display_relative(path: &Path) -> String {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.strip_prefix(&manifest_dir)
            .unwrap_or(path)
            .display()
            .to_string()
    }
}

/// (b) Pure-logic unit tests, cross-platform: the component filter and the
/// exec-path derivation, exercised directly with no OS interaction.
#[cfg(test)]
mod pure_logic_tests {
    use super::*;

    #[test]
    fn accepts_a_normal_git_path() {
        assert_eq!(
            system_shadow_component(r"C:\Program Files\Git\bin\bash.exe"),
            None
        );
    }

    #[test]
    fn rejects_system32_case_sensitively_insensitive() {
        // Case matters in what's actually observed in the wild — both must
        // still be rejected.
        assert_eq!(
            system_shadow_component(r"C:\Windows\System32\bash.exe"),
            Some("System32")
        );
        assert_eq!(
            system_shadow_component(r"C:\WINDOWS\system32\bash.exe"),
            Some("System32")
        );
    }

    #[test]
    fn rejects_syswow64_sysnative_and_windowsapps() {
        assert_eq!(
            system_shadow_component(r"C:\Windows\SysWOW64\bash.exe"),
            Some("SysWOW64")
        );
        assert_eq!(
            system_shadow_component(r"C:\Windows\Sysnative\bash.exe"),
            Some("Sysnative")
        );
        assert_eq!(
            system_shadow_component(r"C:\Users\me\AppData\Local\Microsoft\WindowsApps\bash.exe"),
            Some("WindowsApps")
        );
    }

    #[test]
    fn a_substring_match_does_not_over_reject() {
        // A directory that merely contains "System32" as a substring of its
        // own name, rather than being that component, must pass.
        assert_eq!(
            system_shadow_component(r"C:\Tools\NotSystem32Stuff\bash.exe"),
            None
        );
    }

    #[test]
    fn forward_slash_paths_are_checked_too() {
        assert_eq!(
            system_shadow_component("C:/Windows/System32/bash.exe"),
            Some("System32")
        );
    }

    #[test]
    fn accepts_absolute_drive_and_unc_dir_entries() {
        assert!(is_absolute_dir_entry(r"C:\Program Files\Git\bin"));
        assert!(is_absolute_dir_entry("C:/Program Files/Git/bin"));
        assert!(is_absolute_dir_entry(r"\\server\share\Git\bin"));
        assert!(is_absolute_dir_entry("//server/share/Git/bin"));
    }

    /// `PATH` ending (or doubling up) in `;` — extremely common on Windows —
    /// makes `env::split_paths` yield an empty entry rather than skipping
    /// it; joining a filename onto that resolves against pult's own current
    /// directory instead of any real search path. Empty, and a bare
    /// drive-relative `C:` with nothing after it, must both be rejected.
    #[test]
    fn rejects_empty_and_drive_relative_dir_entries() {
        assert!(!is_absolute_dir_entry(""));
        assert!(!is_absolute_dir_entry("C:"));
        assert!(!is_absolute_dir_entry("Git\\cmd"));
        assert!(!is_absolute_dir_entry("."));
    }

    #[test]
    fn exec_path_root_derivation_mingw64_forward_slash() {
        assert_eq!(
            root_from_exec_path("C:/Program Files/Git/mingw64/libexec/git-core"),
            Some("C:/Program Files/Git".to_string())
        );
    }

    #[test]
    fn exec_path_root_derivation_mingw32_forward_slash() {
        assert_eq!(
            root_from_exec_path("C:/Program Files (x86)/Git/mingw32/libexec/git-core"),
            Some("C:/Program Files (x86)/Git".to_string())
        );
    }

    #[test]
    fn exec_path_root_derivation_back_slash() {
        assert_eq!(
            root_from_exec_path(r"C:\Program Files\Git\mingw64\libexec\git-core"),
            Some(r"C:\Program Files\Git".to_string())
        );
        assert_eq!(
            root_from_exec_path(r"C:\Program Files\Git\mingw32\libexec\git-core"),
            Some(r"C:\Program Files\Git".to_string())
        );
    }

    #[test]
    fn exec_path_root_derivation_trims_trailing_newline() {
        assert_eq!(
            root_from_exec_path("C:/Program Files/Git/mingw64/libexec/git-core\n"),
            Some("C:/Program Files/Git".to_string())
        );
    }

    #[test]
    fn exec_path_too_shallow_to_derive_a_root_is_none() {
        assert_eq!(root_from_exec_path("mingw64/libexec/git-core"), None);
    }

    /// A naive `.filter(!is_empty())` would erase a UNC root's leading
    /// `\\`/`//` marker (it splits into two empty segments), turning
    /// `\\server\share\Git` into the relative `server\share\Git`.
    #[test]
    fn exec_path_root_derivation_unc_backslash() {
        assert_eq!(
            root_from_exec_path(r"\\server\share\Git\mingw64\libexec\git-core"),
            Some(r"\\server\share\Git".to_string())
        );
    }

    #[test]
    fn exec_path_root_derivation_unc_forward_slash() {
        assert_eq!(
            root_from_exec_path("//server/share/Git/mingw64/libexec/git-core"),
            Some("//server/share/Git".to_string())
        );
    }

    /// Git installed directly at a drive root: the derived root is a bare
    /// `"C:"` unless a separator is appended, and `Path::new("C:")` is
    /// drive-relative rather than drive-rooted.
    #[test]
    fn exec_path_root_derivation_drive_root_back_slash() {
        assert_eq!(
            root_from_exec_path(r"C:\mingw64\libexec\git-core"),
            Some(r"C:\".to_string())
        );
    }

    #[test]
    fn exec_path_root_derivation_drive_root_forward_slash() {
        assert_eq!(
            root_from_exec_path("C:/mingw64/libexec/git-core"),
            Some("C:/".to_string())
        );
    }
}

/// (c) Windows behavioral tests: resolve for real and spawn for real. Not
/// runnable here (compiled and executed only on the Windows CI leg — see
/// `.github/workflows/ci.yml`'s `windows-shell-smoke` job), but type-checked
/// on every `cargo clippy --target x86_64-pc-windows-msvc` run so this
/// can't silently rot between Windows-leg runs.
#[cfg(all(test, windows))]
mod windows_behavioral_tests {
    use super::*;

    fn run(mut cmd: Command, script: &str) -> (bool, String) {
        let out = cmd
            .arg("-c")
            .arg(script)
            .output()
            .expect("failed to spawn resolved shell");
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
        )
    }

    /// Primary assertion, deliberately first: the resolved bash has no
    /// `System32` component. This is what stays red if the resolution logic
    /// regresses even in a future where GitHub Actions preinstalls a WSL
    /// distro (which would make a reverted bare `bash -c` exit 0 again,
    /// same as real Git Bash — see the next test's doc comment).
    #[test]
    fn resolved_bash_exists_and_has_no_system32_component() {
        let path = resolved_path("bash").expect("bash should resolve on the Windows CI runner");
        assert!(path.exists(), "resolved path does not exist: {path:?}");
        assert_eq!(
            system_shadow_component(&path.to_string_lossy()),
            None,
            "resolved bash path has a System32-family component: {path:?}"
        );
    }

    /// Spawning the resolved bash actually runs a command and exits 0.
    /// Redness here under a revert to the bare name depends on the CI
    /// runner having no WSL distro installed (true today, per the issue's
    /// own evidence) — the path assertion above is the assertion that
    /// stays meaningful regardless.
    #[test]
    fn resolved_bash_runs_and_prints_probe() {
        let cmd = Command::new(resolved_path("bash").unwrap());
        let (success, stdout) = run(cmd, "echo probe");
        assert!(success);
        assert_eq!(stdout, "probe");
    }

    /// The actual documented `run_bash` contract (`runner.rs`: compiled
    /// step scripts rely on `set -o pipefail`).
    #[test]
    fn resolved_bash_supports_pipefail() {
        let cmd = Command::new(resolved_path("bash").unwrap());
        let (success, stdout) = run(cmd, "set -o pipefail; false | true; echo $?");
        assert!(success);
        assert_eq!(stdout, "1", "pipefail should surface false's exit code");
    }

    /// Confirms `sh` resolves through this module to a real, working shell
    /// too, mirroring the two `bash` assertions above. This does *not*
    /// measure whether `System32` truly lacks an `sh.exe` — that stays a
    /// belief, not something checked here or anywhere else — it only
    /// measures that whatever `sh` resolves to is clean and runs, which is
    /// what `sh`'s independent reason for routing through this module (see
    /// the module doc) actually depends on.
    #[test]
    fn resolved_sh_exists_has_no_system32_component_and_runs() {
        let path = resolved_path("sh").expect("sh should resolve on the Windows CI runner");
        assert!(path.exists(), "resolved path does not exist: {path:?}");
        assert_eq!(system_shadow_component(&path.to_string_lossy()), None);

        let cmd = Command::new(&path);
        let (success, stdout) = run(cmd, "echo probe");
        assert!(success);
        assert_eq!(stdout, "probe");
    }
}
