use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Where a module comes from. (Local sources stay as the include's own
/// `source` string, so the variant carries nothing.)
#[derive(Debug)]
pub enum Source {
    Local,
    Git(GitSource),
}

#[derive(Debug, Clone)]
pub struct GitSource {
    /// Original source string, for messages.
    pub display: String,
    /// Clone URL.
    pub url: String,
    /// Path within the repo: a directory (containing module.yaml) or a yaml file.
    pub subpath: Option<String>,
    /// The pin: a tag or a full 40-char commit sha. Mandatory.
    pub rev: String,
}

/// Written next to each cached checkout; what makes tag-drift detectable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheMeta {
    pub source: String,
    pub url: String,
    pub rev: String,
    /// "tag" or "sha".
    pub rev_kind: String,
    /// The commit the pin resolved to at fetch time.
    pub resolved_sha: String,
}

/// Recognize a source string:
/// - `./path`, `../path`               → local
/// - `host.tld/org/repo[//sub]@rev`    → git over https (GitHub-style shorthand)
/// - `git::<url>[//sub]@rev`           → git, any transport (ssh, file, https)
pub fn parse_source(source: &str) -> Result<Source> {
    if source.starts_with("./") || source.starts_with("../") {
        return Ok(Source::Local);
    }
    if let Some(rest) = source.strip_prefix("git::") {
        return Ok(Source::Git(parse_git(source, rest, true)?));
    }
    let first_seg = source.split('/').next().unwrap_or("");
    if first_seg.contains('.') && !first_seg.contains('@') {
        return Ok(Source::Git(parse_git(source, source, false)?));
    }
    bail!(
        "source `{source}` is not recognized — use `./path` for local modules, \
         `host.tld/org/repo[//subdir]@<tag|sha>` or `git::<url>[//subdir]@<rev>` for git modules \
         (https/s3 registries arrive in Phase C)"
    );
}

fn parse_git(display: &str, rest: &str, explicit_url: bool) -> Result<GitSource> {
    // The pin is after the last `@` and cannot contain `/` (ssh URLs contain `@`).
    let (body, rev) = match rest.rfind('@') {
        Some(i) if i + 1 < rest.len() && !rest[i + 1..].contains('/') => {
            (&rest[..i], rest[i + 1..].to_string())
        }
        _ => bail!(
            "git module `{display}` must be pinned — append `@<tag>` or `@<full-commit-sha>` \
             (branches are not accepted)"
        ),
    };
    // Subpath split on `//`, skipping a scheme's `://`.
    let scheme_end = body.find("://").map(|i| i + 3).unwrap_or(0);
    let (base, subpath) = match body[scheme_end..].find("//") {
        Some(i) => {
            let split = scheme_end + i;
            (&body[..split], Some(body[split + 2..].to_string()))
        }
        None => (body, None),
    };
    if let Some(sub) = &subpath
        && (sub.split('/').any(|seg| seg == "..") || sub.starts_with('/'))
    {
        bail!("git module `{display}`: subpath must be repo-relative without `..`");
    }
    if base.is_empty() {
        bail!("git module `{display}` has an empty URL");
    }
    let url = if explicit_url {
        base.to_string()
    } else {
        format!("https://{base}")
    };
    Ok(GitSource {
        display: display.to_string(),
        url,
        subpath,
        rev,
    })
}

/// `~/.cache/pult/modules`, overridable via PULT_CACHE_DIR (tests pass an
/// explicit root instead).
pub fn default_cache_root() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("PULT_CACHE_DIR") {
        return Ok(PathBuf::from(p));
    }
    let base = dirs::cache_dir().context("could not determine the user cache directory")?;
    Ok(base.join("pult").join("modules"))
}

/// Return the checkout directory for a pinned git module, fetching on a cold
/// cache. Pins are immutable, so a cache hit never touches the network.
pub fn ensure_fetched(git_src: &GitSource, cache_root: &Path) -> Result<(PathBuf, CacheMeta)> {
    let entry = cache_root.join(cache_key(git_src));
    let repo = entry.join("repo");
    let meta_path = entry.join("meta.json");
    if repo.is_dir()
        && let Ok(raw) = std::fs::read_to_string(&meta_path)
    {
        let meta: CacheMeta = serde_json::from_str(&raw)
            .with_context(|| format!("corrupt cache metadata {}", meta_path.display()))?;
        return Ok((repo, meta));
    }
    let meta = fetch(git_src, cache_root, &entry)
        .with_context(|| format!("failed to fetch git module `{}`", git_src.display))?;
    Ok((entry.join("repo"), meta))
}

fn cache_key(git_src: &GitSource) -> String {
    let digest = Sha256::digest(format!("{}@{}", git_src.url, git_src.rev).as_bytes());
    let hex: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    let name = git_src
        .url
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("module")
        .trim_end_matches(".git");
    let name: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    format!("{name}-{hex}")
}

/// Fetch into a temp dir, then atomically rename into the cache slot — a
/// half-finished clone must never look like a valid cache entry.
fn fetch(git_src: &GitSource, cache_root: &Path, entry: &Path) -> Result<CacheMeta> {
    std::fs::create_dir_all(cache_root)
        .with_context(|| format!("failed to create {}", cache_root.display()))?;
    let tmp = cache_root.join(format!(
        ".tmp-{}-{}",
        entry.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&tmp);

    let result = fetch_into(git_src, &tmp);
    match result {
        Ok(meta) => {
            match std::fs::rename(&tmp, entry) {
                Ok(()) => {}
                // Lost a race with a concurrent pult — the winner's entry is
                // the same immutable content.
                Err(_) if entry.join("meta.json").is_file() => {
                    let _ = std::fs::remove_dir_all(&tmp);
                }
                Err(e) => {
                    let _ = std::fs::remove_dir_all(&tmp);
                    return Err(e)
                        .with_context(|| format!("failed to move fetch into {}", entry.display()));
                }
            }
            Ok(meta)
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(&tmp);
            Err(e)
        }
    }
}

fn fetch_into(git_src: &GitSource, tmp: &Path) -> Result<CacheMeta> {
    let repo = tmp.join("repo");
    std::fs::create_dir_all(&repo)
        .with_context(|| format!("failed to create {}", repo.display()))?;

    let (rev_kind, resolved_sha) = if is_full_sha(&git_src.rev) {
        git(&["init", "-q"], Some(&repo))?;
        git(&["remote", "add", "origin", &git_src.url], Some(&repo))?;
        git(
            &["fetch", "--depth", "1", "-q", "origin", &git_src.rev],
            Some(&repo),
        )
        .with_context(|| {
            format!(
                "fetching commit {} — some servers only serve tags/branches; consider pinning a tag",
                git_src.rev
            )
        })?;
        git(&["checkout", "-q", "FETCH_HEAD"], Some(&repo))?;
        ("sha", git_src.rev.clone())
    } else {
        classify_ref(&git_src.url, &git_src.rev)?;
        // `git clone` wants to create the target itself.
        std::fs::remove_dir_all(&repo).ok();
        git(
            &[
                "clone",
                "-q",
                "--depth",
                "1",
                "--branch",
                &git_src.rev,
                &git_src.url,
                repo.to_str().context("non-UTF8 cache path")?,
            ],
            None,
        )?;
        let head = git(&["rev-parse", "HEAD"], Some(&repo))?;
        ("tag", head.trim().to_string())
    };

    // The checkout is an immutable snapshot; drop the .git machinery.
    let _ = std::fs::remove_dir_all(repo.join(".git"));

    let meta = CacheMeta {
        source: git_src.display.clone(),
        url: git_src.url.clone(),
        rev: git_src.rev.clone(),
        rev_kind: rev_kind.to_string(),
        resolved_sha,
    };
    let json = serde_json::to_string_pretty(&meta)?;
    std::fs::write(tmp.join("meta.json"), json)?;
    Ok(meta)
}

/// Reject branch pins with a specific message; anything that is neither a tag
/// nor a full sha is unresolvable.
fn classify_ref(url: &str, rev: &str) -> Result<()> {
    let ls = git(&["ls-remote", "--tags", "--heads", url], None)
        .with_context(|| format!("listing refs of {url}"))?;
    let mut is_tag = false;
    let mut is_branch = false;
    for line in ls.lines() {
        let Some((_sha, r)) = line.split_once('\t') else {
            continue;
        };
        if r == format!("refs/tags/{rev}") || r == format!("refs/tags/{rev}^{{}}") {
            is_tag = true;
        }
        if r == format!("refs/heads/{rev}") {
            is_branch = true;
        }
    }
    if is_tag {
        return Ok(());
    }
    if is_branch {
        bail!(
            "`{rev}` is a branch — remote modules must be pinned to a tag or a full commit sha, \
             so the same manifest always resolves to the same commands"
        );
    }
    bail!("`{rev}` is not a tag on {url} — pin an existing tag or a full 40-char commit sha");
}

/// What a tag points at on the remote, right now (peeled for annotated tags).
/// `Ok(None)` = the tag no longer exists.
pub fn remote_tag_sha(url: &str, tag: &str) -> Result<Option<String>> {
    let ls = git(
        &[
            "ls-remote",
            url,
            &format!("refs/tags/{tag}"),
            &format!("refs/tags/{tag}^{{}}"),
        ],
        None,
    )?;
    let mut plain = None;
    let mut peeled = None;
    for line in ls.lines() {
        let Some((sha, r)) = line.split_once('\t') else {
            continue;
        };
        if r.ends_with("^{}") {
            peeled = Some(sha.to_string());
        } else {
            plain = Some(sha.to_string());
        }
    }
    Ok(peeled.or(plain))
}

fn is_full_sha(rev: &str) -> bool {
    rev.len() == 40 && rev.chars().all(|c| c.is_ascii_hexdigit())
}

fn git(args: &[&str], cwd: Option<&Path>) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.args(args).env("GIT_TERMINAL_PROMPT", "0");
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let out = cmd
        .output()
        .context("failed to run git — is git installed?")?;
    if !out.status.success() {
        bail!(
            "git {} failed:\n{}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim_end()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_github_shorthand() {
        let Source::Git(g) = parse_source("github.com/opskit/aws-common@v1.4.2").unwrap() else {
            panic!()
        };
        assert_eq!(g.url, "https://github.com/opskit/aws-common");
        assert_eq!(g.rev, "v1.4.2");
        assert_eq!(g.subpath, None);
    }

    #[test]
    fn parses_subpath_and_yaml_file() {
        let Source::Git(g) = parse_source("github.com/org/repo//modules/aws-common@abc").unwrap()
        else {
            panic!()
        };
        assert_eq!(g.url, "https://github.com/org/repo");
        assert_eq!(g.subpath.as_deref(), Some("modules/aws-common"));
    }

    #[test]
    fn parses_explicit_git_url_with_scheme() {
        let Source::Git(g) =
            parse_source("git::ssh://git@corp.example/ops.git//common@v2").unwrap()
        else {
            panic!()
        };
        assert_eq!(g.url, "ssh://git@corp.example/ops.git");
        assert_eq!(g.subpath.as_deref(), Some("common"));
        assert_eq!(g.rev, "v2");
    }

    #[test]
    fn unpinned_git_source_errors() {
        let err = parse_source("github.com/org/repo").unwrap_err().to_string();
        assert!(err.contains("must be pinned"), "got: {err}");
    }

    #[test]
    fn local_paths_pass_through() {
        assert!(matches!(parse_source("./tools").unwrap(), Source::Local));
    }

    #[test]
    fn unrecognized_source_errors() {
        let err = parse_source("not-a-source").unwrap_err().to_string();
        assert!(err.contains("not recognized"), "got: {err}");
    }

    #[test]
    fn subpath_escape_is_rejected() {
        let err = parse_source("github.com/org/repo//../../etc@v1")
            .unwrap_err()
            .to_string();
        assert!(err.contains("without `..`"), "got: {err}");
    }

    #[test]
    fn full_sha_detection() {
        assert!(is_full_sha(&"a".repeat(40)));
        assert!(!is_full_sha("abc123"));
        assert!(!is_full_sha(&"z".repeat(40)));
    }
}
