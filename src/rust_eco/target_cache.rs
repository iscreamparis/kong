//! Shared Cargo build cache: a writable cache root, the deterministic key of a
//! shared `CARGO_TARGET_DIR`, and the key of the vendored registry directory.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use tracing::{debug, warn};

use crate::store;

/// Resolve the cache root: the first writable of `KONG_CACHE`, the store's
/// `cache/` and the system temp dir's `kong-cache/`.
///
/// The store may be read-only (the nomorechat sandbox mounts it so), while the
/// build cache must be written, hence the fallbacks.
pub fn cache_root() -> Result<PathBuf> {
    pick_writable(&default_candidates())
}

/// The candidate cache roots, in order of preference.
pub fn default_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(v) = std::env::var_os("KONG_CACHE") {
        if !v.is_empty() {
            candidates.push(PathBuf::from(v));
        }
    }
    match store::install_root() {
        Ok(root) => candidates.push(root.join("cache")),
        Err(e) => warn!(error = %e, "No kong install root, skipping the store cache candidate"),
    }
    candidates.push(std::env::temp_dir().join("kong-cache"));
    candidates
}

/// Return the first candidate that is a writable directory (created if
/// missing). Pure in its input so tests pass the list and never set env vars.
pub fn pick_writable(candidates: &[PathBuf]) -> Result<PathBuf> {
    let mut rejected = Vec::new();
    for (i, dir) in candidates.iter().enumerate() {
        match probe_writable(dir) {
            Ok(()) => {
                debug!(path = %dir.display(), "Using cache root");
                return Ok(dir.clone());
            }
            Err(e) => {
                if i + 1 < candidates.len() {
                    warn!(path = %dir.display(), reason = %format!("{e:#}"), "Cache root candidate not writable, trying the next one");
                }
                rejected.push(format!("  {}: {e:#}", dir.display()));
            }
        }
    }
    bail!(
        "No writable kong cache root.\n  \
         tried:\n{}\n  \
         hint:  set KONG_CACHE to a writable directory",
        rejected.join("\n")
    )
}

fn probe_writable(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let probe = dir.join(format!(".kong-write-probe-{}", std::process::id()));
    std::fs::write(&probe, b"")
        .with_context(|| format!("cannot create a file in {}", dir.display()))?;
    std::fs::remove_file(&probe)
        .with_context(|| format!("cannot remove {}", probe.display()))?;
    Ok(())
}

fn short_sha256(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    hex::encode(digest)[..16].to_string()
}

/// Key of the shared target dir: two checkouts of the same workspace at the
/// same lockfile, toolchain and host share one `CARGO_TARGET_DIR`.
///
/// Features are deliberately not part of the key: Cargo already separates
/// feature sets inside one target dir through its per-unit metadata hash, so
/// splitting by features would only duplicate the dependency builds.
pub fn target_key(workspace_members: &[String], lock_hash: &str, toolchain: &str, host: &str) -> String {
    short_sha256(&format!(
        "workspace={}\nlock={}\ntoolchain={}\nhost={}\n",
        workspace_members.join(","),
        lock_hash,
        toolchain,
        host
    ))
}

/// `<cache_root>/cargo-target/<key>`, created.
pub fn target_dir(cache_root: &Path, key: &str) -> Result<PathBuf> {
    let dir = cache_root.join("cargo-target").join(key);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("cannot create the shared target dir {}", dir.display()))?;
    Ok(dir)
}

/// Key of the vendored registry: sha256 over the sorted `<name> <version> <hash>`
/// lines, so the same crate set gives the same directory whatever its order.
pub fn vendor_key(packages: &[(String, String, String)]) -> String {
    let mut lines: Vec<String> = packages
        .iter()
        .map(|(name, version, hash)| format!("{name} {version} {hash}\n"))
        .collect();
    lines.sort();
    short_sha256(&lines.concat())
}

/// `<cache_root>/cargo-vendor/<vendor key>` (not created: the vendor builder
/// fills it atomically).
pub fn vendor_dir(cache_root: &Path, vendor_key: &str) -> PathBuf {
    cache_root.join("cargo-vendor").join(vendor_key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn members() -> Vec<String> {
        vec!["kong".to_string()]
    }

    #[test]
    fn key_is_stable() {
        let a = target_key(&members(), "abc", "1.80.0", "x86_64-unknown-linux-gnu");
        let b = target_key(&members(), "abc", "1.80.0", "x86_64-unknown-linux-gnu");
        assert_eq!(a, b);
        assert_eq!(a.len(), 16);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn key_changes_with_each_input() {
        let base = target_key(&members(), "abc", "1.80.0", "host-a");
        let others = [
            target_key(&["kong".into(), "other".into()], "abc", "1.80.0", "host-a"),
            target_key(&members(), "abd", "1.80.0", "host-a"),
            target_key(&members(), "abc", "1.81.0", "host-a"),
            target_key(&members(), "abc", "1.80.0", "host-b"),
        ];
        for other in others {
            assert_ne!(base, other);
        }
    }

    #[test]
    fn picker_skips_a_regular_file() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("not-a-dir");
        std::fs::write(&file, b"x").unwrap();
        let good = tmp.path().join("cache");
        let picked = pick_writable(&[file, good.clone()]).unwrap();
        assert_eq!(picked, good);
        assert!(good.is_dir());
    }

    #[test]
    fn picker_fails_when_nothing_is_writable() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("f");
        std::fs::write(&file, b"x").unwrap();
        assert!(pick_writable(&[file]).is_err());
    }

    #[test]
    fn target_dir_is_created_under_cargo_target() {
        let tmp = TempDir::new().unwrap();
        let dir = target_dir(tmp.path(), "0123456789abcdef").unwrap();
        assert!(dir.is_dir());
        assert_eq!(dir, tmp.path().join("cargo-target").join("0123456789abcdef"));
    }

    #[test]
    fn vendor_key_ignores_order() {
        let p = |n: &str, v: &str, h: &str| (n.to_string(), v.to_string(), h.to_string());
        let a = vec![p("serde", "1.0.0", "aa"), p("anyhow", "1.0.1", "bb")];
        let b = vec![p("anyhow", "1.0.1", "bb"), p("serde", "1.0.0", "aa")];
        assert_eq!(vendor_key(&a), vendor_key(&b));
        assert_ne!(vendor_key(&a), vendor_key(&[p("serde", "1.0.0", "aa")]));
        let tmp = TempDir::new().unwrap();
        assert_eq!(
            vendor_dir(tmp.path(), &vendor_key(&a)),
            tmp.path().join("cargo-vendor").join(vendor_key(&a))
        );
    }
}
