/// Download the Rust toolchain (rustc + cargo + rust-std) from static.rust-lang.org.
/// Stores it at `<store>/rust/toolchain/<version>/` and returns paths to cargo and rustc.
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use tracing::{debug, info};

const CHANNEL_URL: &str = "https://static.rust-lang.org/dist/channel-rust-stable.toml";

/// A resolved Rust toolchain in the store.
pub struct RustRuntime {
    pub version: String,        // e.g. "1.86.0"
    pub store_path: String,     // relative, e.g. "rust/toolchain/1.86.0"
    pub cargo_exe: PathBuf,     // absolute path to cargo.exe / cargo
    pub rustc_exe: PathBuf,     // absolute path to rustc.exe / rustc
}

/// Ensure the stable Rust toolchain is present in the store.
/// Downloads cargo, rustc, and rust-std components and merges them into one directory.
pub fn ensure_runtime(store_root: &Path) -> Result<RustRuntime> {
    let target = rust_target();
    let (version, cargo_url, rustc_url, std_url) = resolve_components(&target)?;

    let store_path = format!("rust/toolchain/{version}");
    let toolchain_dir = store_root.join(&store_path);

    if toolchain_dir.exists()
        && cargo_exe_in(&toolchain_dir).is_some()
        && rustc_exe_in(&toolchain_dir).is_some()
    {
        debug!(version = %version, "Rust toolchain already in store");
    } else {
        info!(version = %version, target = %target, "Downloading Rust toolchain");
        std::fs::create_dir_all(&toolchain_dir)?;
        download_component(&cargo_url, &toolchain_dir, "cargo")
            .context("failed to download cargo component")?;
        download_component(&rustc_url, &toolchain_dir, "rustc")
            .context("failed to download rustc component")?;
        download_component(&std_url, &toolchain_dir, "rust-std")
            .context("failed to download rust-std component")?;
    }

    let cargo_exe = cargo_exe_in(&toolchain_dir)
        .with_context(|| format!("cargo not found after extraction in {}", toolchain_dir.display()))?;
    let rustc_exe = rustc_exe_in(&toolchain_dir)
        .with_context(|| format!("rustc not found after extraction in {}", toolchain_dir.display()))?;

    info!(version = %version, cargo = %cargo_exe.display(), "Rust toolchain ready");
    Ok(RustRuntime { version, store_path, cargo_exe, rustc_exe })
}

// ── Internal helpers ─────────────────────────────────────────────────────────

/// Fetch and parse channel-rust-stable.toml to get component download URLs.
fn resolve_components(target: &str) -> Result<(String, String, String, String)> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("kong-dependency-manager/0.1")
        .build()?;

    info!("Fetching Rust stable channel manifest");
    let text = client
        .get(CHANNEL_URL)
        .send()
        .context("failed to fetch Rust channel manifest")?
        .text()
        .context("failed to read Rust channel manifest")?;

    let manifest: toml::Value = text.parse().context("failed to parse channel manifest TOML")?;

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

/// The Rust target triple for the current platform.
fn rust_target() -> String {
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
    fn rust_target_is_nonempty() {
        assert!(!rust_target().is_empty());
    }
}
