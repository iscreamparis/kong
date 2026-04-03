use std::path::Path;

use anyhow::Result;
use tracing::{debug, info};

use crate::download::{self, FileInfo};

/// Download a crate from crates.io and extract to the store.
pub fn fetch_and_download(name: &str, version: &str, store_path: &Path) -> Result<FileInfo> {
    let url = format!(
        "https://static.crates.io/crates/{name}/{name}-{version}.crate"
    );
    debug!(url = %url, "Downloading crate");

    let tmp = tempfile::TempDir::new()?;
    let result = download::download_and_verify(&url, tmp.path(), None)?;

    // .crate files are tar.gz — extract to store
    crate::extract::extract_targz(&result.path, store_path)?;
    crate::store::write_verified_marker(store_path, &result.hash)?;

    info!(crate_name = %name, version = %version, "Crate stored");

    Ok(FileInfo {
        hash: result.hash,
        url,
    })
}

#[cfg(test)]
mod tests {
    // Tests require recorded fixtures — see kong-test skill
}
