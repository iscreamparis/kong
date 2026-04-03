/// Download the Node.js runtime from the official distribution server.
/// Stores it at `<store>/node/runtime/<version>/` and returns the path to node.exe.
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use tracing::{debug, info};

const NODE_INDEX: &str = "https://nodejs.org/dist/index.json";

/// A resolved Node runtime in the store.
pub struct NodeRuntime {
    pub version: String,        // e.g. "22.11.0"
    pub store_path: String,     // relative, e.g. "node/runtime/22.11.0"
    pub node_exe: PathBuf,      // absolute path to node.exe / node
    pub npm_exe: PathBuf,       // absolute path to npm / npm.cmd
}

/// Ensure the requested Node version is present in the store.
/// `requested` can be "22", "20", "lts", "latest", or a full version like "22.11.0".
pub fn ensure_runtime(store_root: &Path, requested: &str) -> Result<NodeRuntime> {
    let (tarball_url, version) = resolve_release(requested)?;

    let store_path = format!("node/runtime/{version}");
    let runtime_dir = store_root.join(&store_path);

    if runtime_dir.exists() && node_exe_in(&runtime_dir).is_some() {
        debug!(version = %version, "Node runtime already in store");
    } else {
        info!(version = %version, "Downloading Node.js runtime");
        download_and_extract(&tarball_url, &runtime_dir)?;
    }

    let node_exe = node_exe_in(&runtime_dir)
        .with_context(|| format!("node executable not found after extraction in {}", runtime_dir.display()))?;
    let npm_exe = npm_exe_in(&runtime_dir)
        .with_context(|| format!("npm not found after extraction in {}", runtime_dir.display()))?;

    info!(version = %version, exe = %node_exe.display(), "Node.js runtime ready");
    Ok(NodeRuntime { version, store_path, node_exe, npm_exe })
}

// ── Internal helpers ────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct NodeRelease {
    version: String,    // "v22.11.0"
    lts: serde_json::Value, // false | "Jod" (LTS codename)
}

fn resolve_release(requested: &str) -> Result<(String, String)> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("kong-dependency-manager/0.1")
        .build()?;

    let releases: Vec<NodeRelease> = client
        .get(NODE_INDEX)
        .send()
        .context("failed to reach nodejs.org dist index")?
        .json()
        .context("failed to parse Node.js release index JSON")?;

    let normalized = requested.trim().to_lowercase();

    let chosen = releases.iter().find(|r| {
        let ver = r.version.trim_start_matches('v'); // "22.11.0"
        let is_lts = r.lts.is_string();

        match normalized.as_str() {
            "" | "lts" | "latest" => is_lts,
            _ => {
                // "22" → match major
                // "22.11" → match major.minor
                // "22.11.0" → exact
                ver == normalized
                    || ver.starts_with(&format!("{normalized}."))
                    || ver.starts_with(&format!("{normalized}-"))
            }
        }
    });

    let release = chosen.with_context(|| {
        format!("No Node.js release found matching '{requested}'")
    })?;

    let version = release.version.trim_start_matches('v').to_string();
    let url = build_download_url(&release.version, &version);
    debug!(url = %url, version = %version, "Selected Node.js release");
    Ok((url, version))
}

fn build_download_url(v_version: &str, version: &str) -> String {
    #[cfg(windows)]
    return format!("https://nodejs.org/dist/{v_version}/node-{v_version}-win-x64.zip");

    #[cfg(target_os = "macos")]
    {
        let arch = if cfg!(target_arch = "aarch64") { "arm64" } else { "x64" };
        return format!("https://nodejs.org/dist/{v_version}/node-v{version}-darwin-{arch}.tar.gz");
    }

    #[cfg(target_os = "linux")]
    return format!("https://nodejs.org/dist/{v_version}/node-v{version}-linux-x64.tar.gz");
}

fn download_and_extract(url: &str, dest: &Path) -> Result<()> {
    info!(url = %url, "Downloading Node.js runtime archive");
    let tmp = tempfile::TempDir::new()?;
    let result = crate::download::download_and_verify(url, tmp.path(), None)?;
    info!(size = result.path.metadata().map(|m| m.len()).unwrap_or(0), "Downloaded Node.js runtime");

    std::fs::create_dir_all(dest)?;

    #[cfg(windows)]
    {
        // Node Windows ships as a zip: node-vX.Y.Z-win-x64/...
        extract_zip_strip1(&result.path, dest)
            .context("failed to extract Node.js zip")?;
    }

    #[cfg(not(windows))]
    {
        // tar.gz with leading node-vX.Y.Z-linux-x64/ directory
        crate::extract::extract_targz_strip1(&result.path, dest)
            .context("failed to extract Node.js tar.gz")?;
    }

    info!(dest = %dest.display(), "Node.js runtime extracted");
    Ok(())
}

#[cfg(windows)]
fn extract_zip_strip1(archive_path: &Path, dest: &Path) -> Result<()> {
    let file = std::fs::File::open(archive_path)?;
    let mut archive = zip::ZipArchive::new(file)?;

    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let raw = match entry.enclosed_name() {
            Some(p) => p.to_path_buf(),
            None => continue,
        };
        // Strip first path component (e.g. "node-v22.11.0-win-x64/")
        let stripped: PathBuf = raw.components().skip(1).collect();
        if stripped.as_os_str().is_empty() {
            continue;
        }
        let out = dest.join(&stripped);
        if entry.is_dir() {
            std::fs::create_dir_all(&out)?;
        } else {
            if let Some(p) = out.parent() {
                std::fs::create_dir_all(p)?;
            }
            let mut f = std::fs::File::create(&out)?;
            std::io::copy(&mut entry, &mut f)?;
        }
    }
    Ok(())
}

/// Find node.exe / node inside the extracted runtime directory.
pub fn node_exe_in(runtime_dir: &Path) -> Option<PathBuf> {
    #[cfg(windows)]
    let candidates = ["node.exe"];
    #[cfg(not(windows))]
    let candidates = ["bin/node"];

    for rel in &candidates {
        let p = runtime_dir.join(rel);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

pub fn npm_exe_in(runtime_dir: &Path) -> Option<PathBuf> {
    #[cfg(windows)]
    let candidates = ["npm.cmd", "npm"];
    #[cfg(not(windows))]
    let candidates = ["bin/npm"];

    for rel in &candidates {
        let p = runtime_dir.join(rel);
        if p.exists() {
            return Some(p);
        }
    }
    None
}
