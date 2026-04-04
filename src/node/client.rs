use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use tracing::{debug, info};

use crate::download::{self, FileInfo};

/// npm registry response for a specific version.
#[derive(Debug, Deserialize)]
struct NpmVersionInfo {
    dist: NpmDist,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct NpmDist {
    tarball: String,
    #[serde(default)]
    shasum: String,
    #[serde(default)]
    integrity: Option<String>,
}

/// Fetch metadata from npm, download tarball, and extract to store.
pub fn fetch_and_download(name: &str, version: &str, store_path: &Path) -> Result<FileInfo> {
    let url = format!("https://registry.npmjs.org/{name}/{version}");
    debug!(url = %url, "Fetching npm metadata");

    let response = reqwest::blocking::get(&url)
        .with_context(|| format!("failed to fetch npm metadata for {name}@{version}"))?;

    if response.status() == reqwest::StatusCode::NOT_FOUND {
        bail!("package '{name}@{version}' not found on npm");
    }

    let info: NpmVersionInfo = response
        .json()
        .with_context(|| format!("failed to parse npm response for {name}@{version}"))?;

    info!(tarball = %info.dist.tarball, "Downloading npm tarball");

    // Download tarball
    let tmp = tempfile::TempDir::new()?;
    let result = download::download_and_verify(&info.dist.tarball, tmp.path(), None)?;

    // Extract to store (npm tarballs contain a `package/` directory)
    crate::extract::extract_targz(&result.path, store_path)?;
    crate::store::write_verified_marker(store_path, &result.hash)?;

    Ok(FileInfo {
        hash: result.hash,
        url: info.dist.tarball,
    })
}

#[cfg(test)]
mod tests {
    // Tests require recorded JSON fixtures — see kong-test skill
}
