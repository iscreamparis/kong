/// Download a standalone Python runtime from python-build-standalone.
/// Stores it at `<store>/python/runtime/<version>/` and returns the path to python.exe.
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use tracing::{debug, info};

const PBS_RELEASES_API: &str =
    "https://api.github.com/repos/indygreg/python-build-standalone/releases";

/// A resolved Python runtime in the store.
pub struct PythonRuntime {
    pub version: String,
    pub store_path: String,     // relative, e.g. "python/runtime/3.12.9"
    pub _python_exe: PathBuf,   // absolute path to python.exe / python3
}

/// Ensure the requested Python major.minor is present in the store.
/// `requested` can be "3.12", "3.11", etc. Pass "" / "latest" to pick the newest.
pub fn ensure_runtime(store_root: &Path, requested: &str) -> Result<PythonRuntime> {
    let (asset_url, version) = resolve_asset(requested)?;

    let store_path = format!("python/runtime/{version}");
    let runtime_dir = store_root.join(&store_path);

    if runtime_dir.exists() && python_exe_in(&runtime_dir).is_some() {
        debug!(version = %version, "Python runtime already in store");
    } else {
        info!(version = %version, "Downloading Python runtime");
        download_and_extract(&asset_url, &runtime_dir)?;
    }

    let exe = python_exe_in(&runtime_dir)
        .with_context(|| format!("python executable not found after extraction in {}", runtime_dir.display()))?;

    info!(version = %version, exe = %exe.display(), "Python runtime ready");
    Ok(PythonRuntime { version, store_path, _python_exe: exe })
}

// ── Internal helpers ────────────────────────────────────────────────────────

/// Fetch the GitHub releases list and find the best asset for this platform.
fn resolve_asset(requested: &str) -> Result<(String, String)> {
    let platform_suffix = platform_asset_suffix();

    let client = reqwest::blocking::Client::builder()
        .user_agent("kong-dependency-manager/0.1")
        .build()?;

    // Fetch the first page of releases (most recent first)
    let releases: Vec<GhRelease> = client
        .get(PBS_RELEASES_API)
        .query(&[("per_page", "10")])
        .send()
        .context("failed to reach GitHub API for python-build-standalone")?
        .json()
        .context("failed to parse GitHub releases JSON")?;

    for release in &releases {
        for asset in &release.assets {
            if !asset.name.ends_with(".tar.gz") {
                continue;
            }
            if !asset.name.contains("install_only") {
                continue;
            }
            if !asset.name.contains(&platform_suffix) {
                continue;
            }
            // Extract version from filename: cpython-3.12.9+20250101-x86_64-...
            let version = extract_version(&asset.name)?;
            if !requested.is_empty() && requested != "latest" {
                // Check major.minor match
                let major_minor = requested.trim_start_matches("python").trim().to_string();
                if !version.starts_with(&major_minor) {
                    continue;
                }
            }
            debug!(asset = %asset.name, version = %version, "Selected Python asset");
            return Ok((asset.browser_download_url.clone(), version));
        }
    }

    bail!(
        "No suitable Python runtime found for platform '{platform_suffix}' (requested: '{requested}'). \
         Check https://github.com/indygreg/python-build-standalone/releases"
    )
}

/// Download the tar.gz and extract into `dest`.
fn download_and_extract(url: &str, dest: &Path) -> Result<()> {
    info!(url = %url, "Downloading Python runtime archive");
    let tmp = tempfile::TempDir::new()?;
    let result = crate::download::download_and_verify(url, tmp.path(), None)?;
    info!(size = result.path.metadata().map(|m| m.len()).unwrap_or(0), "Downloaded Python runtime");

    std::fs::create_dir_all(dest)?;

    // python-build-standalone tarballs contain a top-level `python/` directory
    let file = std::fs::File::open(&result.path)?;
    let decompressed = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(decompressed);

    // Extract, stripping the first component ("python/") so files land directly in dest
    for entry in archive.entries()? {
        let mut entry = entry?;
        let raw_path = entry.path()?.to_path_buf();
        let stripped = raw_path
            .components()
            .skip(1)
            .collect::<std::path::PathBuf>();
        if stripped.as_os_str().is_empty() {
            continue;
        }
        let out = dest.join(&stripped);
        if entry.header().entry_type().is_dir() {
            std::fs::create_dir_all(&out)?;
        } else {
            if let Some(p) = out.parent() {
                std::fs::create_dir_all(p)?;
            }
            entry.unpack(&out)?;
        }
    }

    info!(dest = %dest.display(), "Python runtime extracted");
    Ok(())
}

/// Find python.exe / python3 inside the extracted runtime directory.
pub fn python_exe_in(runtime_dir: &Path) -> Option<PathBuf> {
    #[cfg(windows)]
    let candidates = ["python.exe", "Scripts/python.exe"];
    #[cfg(not(windows))]
    let candidates = ["bin/python3", "bin/python"];

    for rel in &candidates {
        let p = runtime_dir.join(rel);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

fn extract_version(filename: &str) -> Result<String> {
    // e.g. "cpython-3.12.9+20250101-x86_64-pc-windows-msvc-install_only.tar.gz"
    let rest = filename
        .strip_prefix("cpython-")
        .context("unexpected asset filename format")?;
    let version = rest
        .split('+')
        .next()
        .context("could not parse version from filename")?
        .to_string();
    Ok(version)
}

fn platform_asset_suffix() -> String {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => "x86_64-pc-windows-msvc-install_only".to_string(),
        ("linux", "x86_64")   => "x86_64-unknown-linux-gnu-install_only".to_string(),
        ("macos", "x86_64")   => "x86_64-apple-darwin-install_only".to_string(),
        ("macos", "aarch64")  => "aarch64-apple-darwin-install_only".to_string(),
        (os, arch) => format!("{arch}-unknown-{os}-install_only"),
    }
}

// ── GitHub API types ────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct GhRelease {
    assets: Vec<GhAsset>,
}

#[derive(Debug, Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
}
