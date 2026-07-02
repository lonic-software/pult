use anyhow::Result;

use crate::fetch;
use crate::resolver::{PinInfo, Resolved};

/// `pult includes verify` — the CI guard. Resolution already succeeded (schema,
/// contracts, sha256 pins all checked), so what's left is the one thing that
/// can drift without any local file changing: a moved or deleted git tag.
/// Exit 0 = everything holds; 1 = drift or an unreachable remote.
pub fn run(resolved: &Resolved) -> Result<i32> {
    if resolved.pins.is_empty() {
        println!("no includes to verify");
        return Ok(0);
    }
    let mut failed = false;
    for pin in &resolved.pins {
        match pin {
            PinInfo::Local { source } => {
                println!("ok     {source} — local, resolves");
            }
            PinInfo::Git {
                source, rev_kind, ..
            } if rev_kind == "sha" => {
                println!("ok     {source} — immutable commit pin");
            }
            PinInfo::Git {
                source,
                url,
                rev,
                resolved_sha,
                ..
            } => match fetch::remote_tag_sha(url, rev) {
                Ok(Some(remote)) if remote == *resolved_sha => {
                    println!("ok     {source} — tag {rev} still at {}", short(&remote));
                }
                Ok(Some(remote)) => {
                    failed = true;
                    eprintln!(
                        "drift  {source} — tag {rev} MOVED on the remote: cached {} but remote is {}. \
                         Investigate the re-tag; if legitimate, clear the cache entry and re-trust.",
                        short(resolved_sha),
                        short(&remote)
                    );
                }
                Ok(None) => {
                    failed = true;
                    eprintln!("drift  {source} — tag {rev} no longer exists on the remote");
                }
                Err(e) => {
                    failed = true;
                    eprintln!("error  {source} — {e:#}");
                }
            },
        }
    }
    Ok(if failed { 1 } else { 0 })
}

fn short(sha: &str) -> &str {
    &sha[..10.min(sha.len())]
}
