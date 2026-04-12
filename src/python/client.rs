use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use tracing::{debug, info};

use crate::download::{self, FileInfo};

/// Metadata from PyPI JSON API.
#[derive(Debug, Deserialize)]
struct PypiPackageInfo {
    info: PypiInfo,
    releases: std::collections::HashMap<String, Vec<PypiFileEntry>>,
}

#[derive(Debug, Deserialize)]
struct PypiInfo {
    requires_dist: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct PypiFileEntry {
    filename: String,
    url: String,
    digests: PypiDigests,
    #[serde(default)]
    packagetype: String,
    #[serde(default)]
    requires_python: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PypiDigests {
    sha256: String,
}

/// A resolved transitive dependency (name + exact version from PyPI).
#[derive(Debug, Clone)]
pub struct TransitiveDep {
    pub name: String,
    pub version: String,
}

/// Fetch metadata from PyPI, download the best wheel, and extract to store.
/// Returns the file info plus any transitive dependencies found in wheel METADATA.
pub fn fetch_and_download(name: &str, version: &str, store_path: &Path) -> Result<(FileInfo, Vec<TransitiveDep>)> {
    let url = format!("https://pypi.org/pypi/{name}/json");
    debug!(url = %url, "Fetching PyPI metadata");

    let response = reqwest::blocking::get(&url)
        .with_context(|| format!("failed to fetch PyPI metadata for {name}"))?;

    if response.status() == reqwest::StatusCode::NOT_FOUND {
        bail!("package '{name}' not found on PyPI");
    }

    let info: PypiPackageInfo = response
        .json()
        .with_context(|| format!("failed to parse PyPI response for {name}"))?;

    let files = info
        .releases
        .get(version)
        .with_context(|| format!("version '{version}' not found for '{name}' on PyPI"))?;

    // Select best file: prefer wheel (py3-none-any), then any wheel, then sdist
    let file = select_best_file(files)
        .with_context(|| format!("no suitable file for {name}=={version}"))?;

    info!(filename = %file.filename, "Selected: {}", file.filename);

    // Download to a temp directory, then extract
    let tmp = tempfile::TempDir::new()?;
    let result = download::download_and_verify(
        &file.url,
        tmp.path(),
        Some(&file.digests.sha256),
    )?;

    // Extract to store
    crate::extract::extract(&result.path, store_path)?;
    crate::store::write_verified_marker(store_path, &result.hash)?;

    // Collect transitive deps from PyPI info.requires_dist (more reliable than
    // reading the extracted METADATA file — same data, no filesystem walk).
    let transitive = parse_requires_dist(info.info.requires_dist.as_deref().unwrap_or(&[]));

    Ok((FileInfo {
        hash: result.hash,
        url: file.url.clone(),
    }, transitive))
}

/// Resolve the latest version of a package from PyPI (used for transitive deps
/// that have no pinned version in the manifest or lockfile).
pub fn resolve_latest_version(name: &str) -> Result<String> {
    #[derive(Deserialize)]
    struct Info { version: String }
    #[derive(Deserialize)]
    struct Response { info: Info }

    let url = format!("https://pypi.org/pypi/{name}/json");
    let resp: Response = reqwest::blocking::get(&url)
        .with_context(|| format!("failed to fetch PyPI metadata for {name}"))?
        .json()
        .with_context(|| format!("failed to parse PyPI response for {name}"))?;
    Ok(resp.info.version)
}

/// Parse `Requires-Dist` lines — public alias so config.rs can call it for
/// already-cached packages.
pub fn parse_requires_dist_pub(entries: &[String]) -> Vec<TransitiveDep> {
    parse_requires_dist(entries)
}

/// Parse `Requires-Dist` lines from PyPI `info.requires_dist`.
/// Skips extras (conditional deps like `extra == "async"`) and environment
/// markers that would exclude this platform. Returns bare name + resolved version.
fn parse_requires_dist(entries: &[String]) -> Vec<TransitiveDep> {
    let mut deps = Vec::new();
    for entry in entries {
        // Skip anything with "extra ==" — those are optional deps
        if entry.contains("extra ==") || entry.contains("extra==") {
            continue;
        }
        // Strip environment markers (semicolon and after)
        let spec = if let Some(idx) = entry.find(';') {
            entry[..idx].trim()
        } else {
            entry.trim()
        };
        // Parse "Name>=version,<other" — take the name part only
        let name_end = spec.find(|c: char| !c.is_alphanumeric() && c != '-' && c != '_' && c != '.')
            .unwrap_or(spec.len());
        let dep_name = spec[..name_end].trim().to_string();
        if dep_name.is_empty() {
            continue;
        }
        // Extract minimum version from first specifier like ">=3.1" or "==3.1.0"
        let version_str = &spec[name_end..].trim();
        if let Some(ver) = extract_min_version(version_str) {
            deps.push(TransitiveDep { name: dep_name, version: ver });
        } else {
            // No pin — will be resolved to latest
            deps.push(TransitiveDep { name: dep_name, version: String::new() });
        }
    }
    deps
}

/// Extract a concrete version from a specifier like "==3.1.0" → "3.1.0".
/// Only returns exact pins (==). For >= / ~= / etc. returns None so the
/// caller can resolve the latest satisfying version via the PyPI API instead
/// of using a truncated version string like "3.1" that doesn't exist on PyPI.
fn extract_min_version(spec: &str) -> Option<String> {
    // Only return exact pins — avoid returning "3.1" for ">=3.1" which breaks
    // PyPI lookups (the actual release would be "3.1.0", "3.1.3", etc.)
    for part in spec.split(',') {
        let part = part.trim();
        if let Some(v) = part.strip_prefix("==") {
            return Some(v.trim().to_string());
        }
    }
    None
}

fn select_best_file(files: &[PypiFileEntry]) -> Option<&PypiFileEntry> {
    // 1. py3-none-any wheel (pure Python — always compatible)
    if let Some(f) = files.iter().find(|f| {
        f.packagetype == "bdist_wheel" && f.filename.contains("py3-none-any")
    }) {
        return Some(f);
    }

    // 2. py2.py3-none-any wheel (dual-compat pure Python)
    if let Some(f) = files.iter().find(|f| {
        f.packagetype == "bdist_wheel" && f.filename.contains("py2.py3-none-any")
    }) {
        return Some(f);
    }

    // 3. Platform-specific wheel matching current arch
    let platform_tag = current_platform_tag();
    let arch_suffix = current_arch_suffix();

    // Exact platform tag match first
    if let Some(f) = files.iter().find(|f| {
        f.packagetype == "bdist_wheel" && f.filename.contains(&platform_tag)
    }) {
        return Some(f);
    }

    // On macOS, accept any macosx_*_<arch> wheel (different min OS versions are fine)
    if !arch_suffix.is_empty() {
        if let Some(f) = files.iter().find(|f| {
            f.packagetype == "bdist_wheel" && f.filename.contains(&arch_suffix)
        }) {
            return Some(f);
        }
    }

    // 4. Any wheel (last resort — may be wrong arch)
    if let Some(f) = files.iter().find(|f| f.packagetype == "bdist_wheel") {
        return Some(f);
    }

    // 5. Source dist
    files.iter().find(|f| f.packagetype == "sdist")
}

fn current_platform_tag() -> String {
    crate::config::platform_tag()
}

/// Returns the architecture-specific suffix to match in wheel filenames.
/// e.g. "arm64.whl" on Apple Silicon, "x86_64.whl" on Intel Mac, "" otherwise.
fn current_arch_suffix() -> String {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "arm64.whl".to_string(),
        ("macos", "x86_64")  => "x86_64.whl".to_string(),
        ("linux", "x86_64")  => "x86_64.whl".to_string(),
        ("linux", "aarch64") => "aarch64.whl".to_string(),
        ("windows", "x86_64") => "amd64.whl".to_string(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    // Tests require recorded JSON fixtures — see kong-test skill
}
