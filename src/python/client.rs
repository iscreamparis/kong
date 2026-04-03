use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use tracing::{debug, info};

use crate::download::{self, FileInfo};

/// Metadata from PyPI JSON API.
#[derive(Debug, Deserialize)]
struct PypiPackageInfo {
    releases: std::collections::HashMap<String, Vec<PypiFileEntry>>,
}

#[derive(Debug, Deserialize)]
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

/// Fetch metadata from PyPI, download the best wheel, and extract to store.
pub fn fetch_and_download(name: &str, version: &str, store_path: &Path) -> Result<FileInfo> {
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

    Ok(FileInfo {
        hash: result.hash,
        url: file.url.clone(),
    })
}

fn select_best_file(files: &[PypiFileEntry]) -> Option<&PypiFileEntry> {
    // 1. py3-none-any wheel
    if let Some(f) = files.iter().find(|f| {
        f.packagetype == "bdist_wheel" && f.filename.contains("py3-none-any")
    }) {
        return Some(f);
    }

    // 2. Any wheel matching current platform
    let platform_tag = current_platform_tag();
    if let Some(f) = files.iter().find(|f| {
        f.packagetype == "bdist_wheel" && f.filename.contains(&platform_tag)
    }) {
        return Some(f);
    }

    // 3. Any wheel
    if let Some(f) = files.iter().find(|f| f.packagetype == "bdist_wheel") {
        return Some(f);
    }

    // 4. Source dist
    files.iter().find(|f| f.packagetype == "sdist")
}

fn current_platform_tag() -> String {
    // Delegate to the single canonical implementation in config
    crate::config::platform_tag()
}

#[cfg(test)]
mod tests {
    // Tests require recorded JSON fixtures — see kong-test skill
}
