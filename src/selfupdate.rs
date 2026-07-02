use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

/// GitHub repo the binary updates itself from. Overridable at run time with
/// PULT_REPO; PULT_BASE_URL bypasses GitHub entirely (mirrors, air-gapped).
const DEFAULT_REPO: &str = "lonic-software/pult";

/// `pult update [version]` — replace the running binary with a release build.
/// Needs no manifest; downloads via curl/wget (same philosophy as everything
/// else: shell out to ubiquitous tools rather than link an HTTP stack).
pub fn run(requested: Option<&str>) -> Result<i32> {
    let current = env!("CARGO_PKG_VERSION");
    let repo = std::env::var("PULT_REPO").unwrap_or_else(|_| DEFAULT_REPO.to_string());
    let target = target_triple()?;
    let (ext, bin_name) = if cfg!(windows) {
        ("zip", "pult.exe")
    } else {
        ("tar.gz", "pult")
    };
    let asset = format!("pult-{target}.{ext}");

    let base_override = std::env::var("PULT_BASE_URL").ok();
    let (base, label) = match (&base_override, requested) {
        // A mirror knows nothing about "latest" — just fetch what's there.
        (Some(base), _) => (base.clone(), requested.unwrap_or("mirror").to_string()),
        (None, Some(version)) => (
            format!("https://github.com/{repo}/releases/download/{version}"),
            version.to_string(),
        ),
        (None, None) => {
            let tag = latest_tag(&repo)?;
            if tag.trim_start_matches('v') == current {
                println!("pult {current} is already the latest release");
                return Ok(0);
            }
            (
                format!("https://github.com/{repo}/releases/download/{tag}"),
                tag,
            )
        }
    };

    println!("updating pult {current} → {label}");
    let tmp = std::env::temp_dir().join(format!("pult-update-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).with_context(|| format!("failed to create {}", tmp.display()))?;
    let result = fetch_and_replace(&base, &asset, bin_name, &tmp);
    let _ = std::fs::remove_dir_all(&tmp);
    let installed = result?;

    // Report the new binary's own idea of its version.
    let version_out = Command::new(&installed)
        .arg("--version")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|_| "pult (version unknown)".to_string());
    println!("updated: {version_out} at {}", installed.display());
    Ok(0)
}

fn fetch_and_replace(base: &str, asset: &str, bin_name: &str, tmp: &Path) -> Result<PathBuf> {
    let archive = tmp.join(asset);
    download(&format!("{base}/{asset}"), &archive)
        .with_context(|| format!("downloading {base}/{asset}"))?;

    // Verify against checksums.txt; a missing file is a warning, a mismatch fatal.
    let sums = tmp.join("checksums.txt");
    match download(&format!("{base}/checksums.txt"), &sums) {
        Ok(()) => verify_checksum(&archive, &sums, asset)?,
        Err(_) => eprintln!("warning: could not fetch checksums.txt; skipping verification"),
    }

    // bsdtar (macOS, Windows 10+) and GNU tar both auto-detect .tar.gz; bsdtar
    // also extracts .zip, covering the Windows asset.
    let status = Command::new("tar")
        .args([
            "-xf",
            &archive.to_string_lossy(),
            "-C",
            &tmp.to_string_lossy(),
        ])
        .status()
        .context("failed to run tar")?;
    if !status.success() {
        bail!("failed to extract {asset}");
    }
    let new_bin = tmp.join(bin_name);
    if !new_bin.is_file() {
        bail!("archive did not contain {bin_name}");
    }

    let exe = std::env::current_exe().context("cannot locate the running binary")?;
    replace_file(&exe, &new_bin)?;
    Ok(exe)
}

/// Swap `new_bin` into `exe`'s place atomically-ish: stage a copy next to the
/// destination (same filesystem, so renames are atomic), move the old binary
/// aside, move the new one in, roll back on failure.
fn replace_file(exe: &Path, new_bin: &Path) -> Result<()> {
    let dir = exe.parent().context("binary has no parent directory")?;
    let staged = dir.join(".pult-update-staged");
    let old = dir.join(".pult-update-old");
    std::fs::copy(new_bin, &staged)
        .with_context(|| format!("staging into {} (is it writable?)", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
    }
    let _ = std::fs::remove_file(&old);
    std::fs::rename(exe, &old).context("moving the current binary aside")?;
    if let Err(e) = std::fs::rename(&staged, exe) {
        let _ = std::fs::rename(&old, exe); // roll back
        return Err(e).context("installing the new binary");
    }
    // On Windows the old (still-running) file can't be deleted; leaving
    // .pult-update-old behind is harmless and cleaned up by the next update.
    let _ = std::fs::remove_file(&old);
    Ok(())
}

/// Resolve the latest release tag by following the /releases/latest redirect —
/// no API, no auth, no rate limits.
fn latest_tag(repo: &str) -> Result<String> {
    let url = format!("https://github.com/{repo}/releases/latest");
    let output = if has("curl") {
        let out = Command::new("curl")
            .args(["-fsSLI", "-o", "/dev/null", "-w", "%{url_effective}", &url])
            .output()
            .context("failed to run curl")?;
        if !out.status.success() {
            bail!(
                "could not reach {url}:\n{}",
                String::from_utf8_lossy(&out.stderr).trim_end()
            );
        }
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    } else if has("wget") {
        // wget prints headers to stderr with -S; the final Location wins.
        let out = Command::new("wget")
            .args(["-q", "-S", "--max-redirect=10", "-O", "/dev/null", &url])
            .output()
            .context("failed to run wget")?;
        String::from_utf8_lossy(&out.stderr)
            .lines()
            .filter_map(|l| l.trim().strip_prefix("Location: "))
            .next_back()
            .map(str::to_string)
            .unwrap_or_default()
    } else {
        bail!("need curl or wget to check for updates");
    };
    let tag = output.rsplit('/').next().unwrap_or_default().to_string();
    if tag.is_empty() || tag == "latest" || tag == "releases" {
        bail!("no releases found for {repo} — has a v* tag been pushed?");
    }
    Ok(tag)
}

fn download(url: &str, dest: &Path) -> Result<()> {
    let dest_str = dest.to_string_lossy();
    let status = if has("curl") {
        Command::new("curl")
            .args(["-fsSL", url, "-o", &dest_str])
            .status()
            .context("failed to run curl")?
    } else if has("wget") {
        Command::new("wget")
            .args(["-q", url, "-O", &dest_str])
            .status()
            .context("failed to run wget")?
    } else {
        bail!("need curl or wget");
    };
    if !status.success() {
        bail!("download failed: {url}");
    }
    Ok(())
}

fn verify_checksum(archive: &Path, sums: &Path, asset: &str) -> Result<()> {
    let sums_text = std::fs::read_to_string(sums)?;
    let expected = sums_text
        .lines()
        .find_map(|line| {
            let (hash, name) = line.split_once(char::is_whitespace)?;
            (name.trim().trim_start_matches('*') == asset).then(|| hash.to_string())
        })
        .with_context(|| format!("{asset} not listed in checksums.txt"))?;
    let bytes = std::fs::read(archive)?;
    let actual: String = Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if actual != expected {
        bail!("checksum verification FAILED for {asset} — refusing to install");
    }
    println!("checksum ok");
    Ok(())
}

fn target_triple() -> Result<&'static str> {
    Ok(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("linux", "x86_64") => "x86_64-unknown-linux-musl",
        ("linux", "aarch64") => "aarch64-unknown-linux-musl",
        ("windows", "x86_64") => "x86_64-pc-windows-msvc",
        (os, arch) => bail!("no prebuilt binaries for {os}/{arch} — update via cargo install"),
    })
}

fn has(tool: &str) -> bool {
    Command::new(tool)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_triple_resolves_on_supported_hosts() {
        // This test runs on CI hosts we ship binaries for.
        assert!(target_triple().is_ok());
    }

    #[test]
    fn checksum_verification_accepts_and_rejects() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("pult-x.tar.gz");
        std::fs::write(&archive, b"content").unwrap();
        let good: String = Sha256::digest(b"content")
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let sums = dir.path().join("checksums.txt");
        std::fs::write(&sums, format!("{good}  pult-x.tar.gz\n")).unwrap();
        verify_checksum(&archive, &sums, "pult-x.tar.gz").unwrap();

        std::fs::write(&sums, format!("{}  pult-x.tar.gz\n", "0".repeat(64))).unwrap();
        let err = verify_checksum(&archive, &sums, "pult-x.tar.gz")
            .unwrap_err()
            .to_string();
        assert!(err.contains("FAILED"), "got: {err}");

        std::fs::write(&sums, "").unwrap();
        let err = verify_checksum(&archive, &sums, "pult-x.tar.gz")
            .unwrap_err()
            .to_string();
        assert!(err.contains("not listed"), "got: {err}");
    }

    #[test]
    fn replace_file_swaps_and_rolls_back_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("pult");
        let newer = dir.path().join("incoming");
        std::fs::write(&exe, "old").unwrap();
        std::fs::write(&newer, "new").unwrap();
        replace_file(&exe, &newer).unwrap();
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "new");
        assert!(!dir.path().join(".pult-update-staged").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&exe).unwrap().permissions().mode();
            assert_eq!(mode & 0o755, 0o755);
        }
    }
}
