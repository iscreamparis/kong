/// Download the Rust toolchain (rustc + cargo + rust-std) from static.rust-lang.org.
/// Stores it at `<store>/rust/toolchain/<version>/` and returns paths to cargo and rustc.
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tracing::{debug, info, warn};

const CHANNEL_URL: &str = "https://static.rust-lang.org/dist/channel-rust-stable.toml";

/// A resolved Rust toolchain in the store.
pub struct RustRuntime {
    pub version: String,        // e.g. "1.86.0"
    pub store_path: String,     // relative, e.g. "rust/toolchain/1.86.0"
    pub _cargo_exe: PathBuf,    // absolute path to cargo.exe / cargo
    pub _rustc_exe: PathBuf,    // absolute path to rustc.exe / rustc
}

/// Ensure the stable Rust toolchain is present in the store.
/// Downloads cargo, rustc, and rust-std components and merges them into one directory.
///
/// Offline fallback: when the stable channel manifest is unreachable (network blocked)
/// or the newer stable cannot be downloaded (read-only store), the newest toolchain
/// already in the store is used instead, so Rust projects keep working offline.
pub fn ensure_runtime(store_root: &Path) -> Result<RustRuntime> {
    ensure_runtime_with(store_root, resolve_components)
}

/// `ensure_runtime` with the component resolver injected, so tests can simulate an
/// unreachable channel manifest without any network.
fn ensure_runtime_with(
    store_root: &Path,
    resolve: impl FnOnce(&str) -> Result<(String, String, String, String)>,
) -> Result<RustRuntime> {
    let target = host_target();
    let (version, cargo_url, rustc_url, std_url) = match resolve(&target) {
        Ok(components) => components,
        Err(e) => {
            if let Some(rt) = newest_store_toolchain(store_root) {
                warn!(version = %rt.version, error = %format!("{e:#}"),
                    "Rust channel unreachable, using newest toolchain in store");
                return Ok(rt);
            }
            return Err(e).context(
                "Rust channel unreachable and no toolchain in the store \
                 (channel manifest: static.rust-lang.org)",
            );
        }
    };

    let store_path = format!("rust/toolchain/{version}");
    let toolchain_dir = store_root.join(&store_path);

    if toolchain_dir.exists()
        && cargo_exe_in(&toolchain_dir).is_some()
        && rustc_exe_in(&toolchain_dir).is_some()
    {
        debug!(version = %version, "Rust toolchain already in store");
    } else {
        info!(version = %version, target = %target, "Downloading Rust toolchain");
        let downloaded = (|| -> Result<()> {
            std::fs::create_dir_all(&toolchain_dir)?;
            download_component(&cargo_url, &toolchain_dir, "cargo")
                .context("failed to download cargo component")?;
            download_component(&rustc_url, &toolchain_dir, "rustc")
                .context("failed to download rustc component")?;
            download_component(&std_url, &toolchain_dir, "rust-std")
                .context("failed to download rust-std component")?;
            Ok(())
        })();
        if let Err(e) = downloaded {
            if let Some(rt) = newest_store_toolchain(store_root) {
                warn!(version = %rt.version, wanted = %version, error = %format!("{e:#}"),
                    "Rust toolchain {version} download failed — using the toolchain already in the store");
                return Ok(rt);
            }
            return Err(e).with_context(|| {
                format!("cannot download Rust toolchain {version} from static.rust-lang.org \
                         and no toolchain is in the store")
            });
        }
    }

    let cargo_exe = cargo_exe_in(&toolchain_dir)
        .with_context(|| format!("cargo not found after extraction in {}", toolchain_dir.display()))?;
    let rustc_exe = rustc_exe_in(&toolchain_dir)
        .with_context(|| format!("rustc not found after extraction in {}", toolchain_dir.display()))?;

    info!(version = %version, cargo = %cargo_exe.display(), "Rust toolchain ready");
    Ok(RustRuntime { version, store_path, _cargo_exe: cargo_exe, _rustc_exe: rustc_exe })
}

/// The newest complete toolchain (both cargo and rustc present) under
/// `<store>/rust/toolchain/`, compared numerically per dot component
/// (1.90.0 > 1.9.0 > 1.85.0). Directories whose name is not a semver `X.Y.Z` are ignored.
/// Never uses the network: this is the offline fallback of `ensure_runtime`.
pub fn newest_store_toolchain(store_root: &Path) -> Option<RustRuntime> {
    let base = store_root.join("rust").join("toolchain");
    let mut best: Option<(Vec<u64>, String, PathBuf, PathBuf)> = None;
    for entry in std::fs::read_dir(&base).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(key) = numeric_version(&name) else { continue };
        let dir = entry.path();
        let (Some(cargo), Some(rustc)) = (cargo_exe_in(&dir), rustc_exe_in(&dir)) else { continue };
        if best.as_ref().map_or(true, |(k, ..)| key > *k) {
            best = Some((key, name, cargo, rustc));
        }
    }
    best.map(|(_, version, cargo, rustc)| RustRuntime {
        store_path: format!("rust/toolchain/{version}"),
        version,
        _cargo_exe: cargo,
        _rustc_exe: rustc,
    })
}

/// "1.90.0" -> [1, 90, 0]; None unless the name is exactly three numeric components.
fn numeric_version(name: &str) -> Option<Vec<u64>> {
    let parts: Option<Vec<u64>> = name.split('.').map(|c| c.parse::<u64>().ok()).collect();
    parts.filter(|p| p.len() == 3)
}

/// Ensure the std library for an extra compilation `target` is merged into the
/// Kong-managed toolchain `runtime`.
///
/// `ensure_runtime` only installs rust-std for the host. Git-dependency build steps
/// (e.g. a wasm32 package) need extra std libraries (shared §4), so the matching
/// `rust-std` component of the same toolchain version is downloaded once and merged
/// into the toolchain directory. No network is used when it is already present.
pub fn ensure_target(store_root: &Path, runtime: &RustRuntime, target: &str) -> Result<()> {
    let toolchain_dir = store_root.join(&runtime.store_path);
    let lib_dir = toolchain_dir.join("lib").join("rustlib").join(target).join("lib");
    if lib_dir.is_dir() {
        debug!(version = %runtime.version, target = %target, "Rust target already in toolchain");
        return Ok(());
    }

    info!(version = %runtime.version, target = %target,
        "Adding Rust target {target} to toolchain {}", runtime.version);
    let url = format!("https://static.rust-lang.org/dist/channel-rust-{}.toml", runtime.version);
    let manifest = fetch_manifest(&url)?;
    let std_url = component_url(&manifest, "rust-std", target)?;
    std::fs::create_dir_all(&toolchain_dir)?;
    download_component(&std_url, &toolchain_dir, "rust-std")
        .with_context(|| format!("failed to download rust-std for target '{target}'"))?;
    Ok(())
}

// ── Internal helpers ─────────────────────────────────────────────────────────

/// Fetch and parse channel-rust-stable.toml to get component download URLs.
fn resolve_components(target: &str) -> Result<(String, String, String, String)> {
    info!("Fetching Rust stable channel manifest");
    let manifest = fetch_manifest(CHANNEL_URL)?;

    // manifest["pkg"]["rustc"]["version"] => "1.86.0 (05f9846f8 2025-03-31)"
    let version_str = manifest
        .get("pkg").and_then(|p| p.get("rustc"))
        .and_then(|r| r.get("version"))
        .and_then(|v| v.as_str())
        .context("cannot find rustc version in channel manifest")?;
    let version = version_str
        .split_whitespace()
        .next()
        .context("malformed rustc version string")?
        .to_string();

    let cargo_url = component_url(&manifest, "cargo", target)?;
    let rustc_url = component_url(&manifest, "rustc", target)?;
    let std_url   = component_url(&manifest, "rust-std", target)?;

    debug!(version = %version, cargo = %cargo_url, rustc = %rustc_url, std = %std_url, "Resolved Rust component URLs");
    Ok((version, cargo_url, rustc_url, std_url))
}

/// Fetch and parse a Rust channel manifest (`channel-rust-*.toml`).
fn fetch_manifest(url: &str) -> Result<toml::Value> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("kong-dependency-manager/0.1")
        .build()?;

    let text = client
        .get(url)
        .send()
        .context("failed to fetch Rust channel manifest")?
        .text()
        .context("failed to read Rust channel manifest")?;

    text.parse().context("failed to parse channel manifest TOML")
}

/// Extract a component's tar.gz download URL from the channel manifest.
fn component_url(manifest: &toml::Value, component: &str, target: &str) -> Result<String> {
    manifest
        .get("pkg")
        .and_then(|p| p.get(component))
        .and_then(|c| c.get("target"))
        .and_then(|t| t.get(target))
        .and_then(|t| t.get("url"))
        .and_then(|u| u.as_str())
        .map(|s| s.to_string())
        .with_context(|| format!("component '{component}' not found for target '{target}'"))
}

/// Download a component tar.gz and merge its files into `dest`.
/// Each component tarball has a top-level directory like `cargo-1.86.0-x86_64-pc-windows-msvc/`
/// containing a `cargo/` subtree. We merge `cargo/{bin,lib}` directly into `dest`.
fn download_component(url: &str, dest: &Path, component: &str) -> Result<()> {
    info!(component = %component, url = %url, "Downloading Rust component");
    let tmp = tempfile::TempDir::new()?;
    let result = crate::download::download_and_verify(url, tmp.path(), None)?;

    let file = std::fs::File::open(&result.path)?;
    let decompressed = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(decompressed);

    // Each component tarball has structure:
    //   <component>-<ver>-<target>/   ← outer wrapper (strip 1)
    //     <inner>/                    ← component name dir (cargo/, rustc/, rust-std-<target>/)
    //       bin/  lib/  ...          ← actual files we want
    // We strip 2 components so files land at dest/{bin,lib,...}
    for entry in archive.entries()? {
        let mut entry = entry?;
        let raw_path = entry.path()?.to_path_buf();
        // Strip first 2 path components (outer wrapper + inner component dir)
        let stripped: PathBuf = raw_path.components().skip(2).collect();
        if stripped.as_os_str().is_empty() {
            continue;
        }
        // Skip installer metadata files
        let first = stripped.components().next()
            .map(|c| c.as_os_str().to_string_lossy().to_string())
            .unwrap_or_default();
        if first == "manifest.in" || first == "rust-installer-version" || first == "components" {
            continue;
        }
        let out = dest.join(&stripped);
        if entry.header().entry_type().is_dir() {
            std::fs::create_dir_all(&out)?;
        } else {
            if let Some(p) = out.parent() {
                std::fs::create_dir_all(p)?;
            }
            let mut f = std::fs::File::create(&out)?;
            std::io::copy(&mut entry, &mut f)?;
            // Preserve executable bit on Unix
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Ok(mode) = entry.header().mode() {
                    if mode & 0o111 != 0 {
                        let perms = std::fs::Permissions::from_mode(0o755);
                        let _ = std::fs::set_permissions(&out, perms);
                    }
                }
            }
        }
    }

    debug!(component = %component, dest = %dest.display(), "Component extracted");
    Ok(())
}

/// The Rust target triple for the current platform (the host toolchain's triple).
pub fn host_target() -> String {
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    return "x86_64-pc-windows-msvc".to_string();

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    return "aarch64-apple-darwin".to_string();

    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    return "x86_64-apple-darwin".to_string();

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    return "x86_64-unknown-linux-gnu".to_string();

    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    return "aarch64-unknown-linux-gnu".to_string();
}

pub fn cargo_exe_in(dir: &Path) -> Option<PathBuf> {
    #[cfg(windows)]
    let name = "bin/cargo.exe";
    #[cfg(not(windows))]
    let name = "bin/cargo";
    let p = dir.join(name);
    p.exists().then_some(p)
}

pub fn rustc_exe_in(dir: &Path) -> Option<PathBuf> {
    #[cfg(windows)]
    let name = "bin/rustc.exe";
    #[cfg(not(windows))]
    let name = "bin/rustc";
    let p = dir.join(name);
    p.exists().then_some(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_target_is_nonempty() {
        assert!(!host_target().is_empty());
    }

    #[test]
    fn ensure_target_is_noop_when_present() {
        let store = tempfile::TempDir::new().unwrap();
        let store_path = "rust/toolchain/1.0.0".to_string();
        let toolchain = store.path().join(&store_path);
        std::fs::create_dir_all(toolchain.join("lib/rustlib/wasm32-unknown-unknown/lib")).unwrap();
        let runtime = RustRuntime {
            version: "1.0.0".to_string(),
            store_path,
            _cargo_exe: toolchain.join("bin/cargo"),
            _rustc_exe: toolchain.join("bin/rustc"),
        };
        // The version 1.0.0 manifest is never fetched: the target dir is already there.
        ensure_target(store.path(), &runtime, "wasm32-unknown-unknown").unwrap();
    }

    /// Create a fake toolchain dir `rust/toolchain/<version>` with the given bin files.
    fn seed_toolchain(store: &Path, version: &str, cargo: bool, rustc: bool) {
        let bin = store.join("rust/toolchain").join(version).join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let ext = if cfg!(windows) { ".exe" } else { "" };
        if cargo {
            std::fs::write(bin.join(format!("cargo{ext}")), b"").unwrap();
        }
        if rustc {
            std::fs::write(bin.join(format!("rustc{ext}")), b"").unwrap();
        }
    }

    fn failing_resolver(_: &str) -> Result<(String, String, String, String)> {
        anyhow::bail!("simulated: channel manifest unreachable")
    }

    #[test]
    fn store_scan_picks_newest_numeric_version() {
        let store = tempfile::TempDir::new().unwrap();
        seed_toolchain(store.path(), "1.85.0", true, true);
        seed_toolchain(store.path(), "1.9.0", true, true);
        seed_toolchain(store.path(), "1.90.0", true, true);
        seed_toolchain(store.path(), "1.99.0", true, false); // missing bin/rustc
        std::fs::create_dir_all(store.path().join("rust/toolchain/stable/bin")).unwrap();
        let rt = newest_store_toolchain(store.path()).unwrap();
        assert_eq!(rt.version, "1.90.0");
        assert_eq!(rt.store_path, "rust/toolchain/1.90.0");
        assert!(rt._rustc_exe.exists());
    }

    #[test]
    fn store_scan_compares_numerically_and_skips_non_semver() {
        let store = tempfile::TempDir::new().unwrap();
        seed_toolchain(store.path(), "1.9.0", true, true);
        seed_toolchain(store.path(), "1.10.0", true, true);
        seed_toolchain(store.path(), "nightly", true, true);
        seed_toolchain(store.path(), "2.0", true, true); // not X.Y.Z
        let rt = newest_store_toolchain(store.path()).unwrap();
        assert_eq!(rt.version, "1.10.0");
    }

    #[test]
    fn store_scan_skips_version_without_cargo() {
        let store = tempfile::TempDir::new().unwrap();
        seed_toolchain(store.path(), "1.80.0", true, true);
        seed_toolchain(store.path(), "1.81.0", false, true); // missing bin/cargo
        let rt = newest_store_toolchain(store.path()).unwrap();
        assert_eq!(rt.version, "1.80.0");
    }

    #[test]
    fn store_scan_empty_store_is_none() {
        let store = tempfile::TempDir::new().unwrap();
        assert!(newest_store_toolchain(store.path()).is_none());
    }

    #[test]
    fn unreachable_manifest_falls_back_to_store_toolchain() {
        let store = tempfile::TempDir::new().unwrap();
        seed_toolchain(store.path(), "1.85.0", true, true);
        let rt = ensure_runtime_with(store.path(), failing_resolver).unwrap();
        assert_eq!(rt.version, "1.85.0");
        assert!(rt._cargo_exe.exists());
    }

    #[test]
    fn unreachable_manifest_with_empty_store_is_an_error() {
        let store = tempfile::TempDir::new().unwrap();
        let Err(err) = ensure_runtime_with(store.path(), failing_resolver) else {
            panic!("expected an error with an empty store");
        };
        let msg = format!("{err:#}");
        assert!(msg.contains("Rust channel unreachable and no toolchain in the store"), "{msg}");
        assert!(msg.contains("static.rust-lang.org"), "{msg}");
        assert!(msg.contains("simulated"), "{msg}");
    }
}
