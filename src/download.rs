use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use tracing::{debug, info};

/// Result of a successful download: the temp file path and its SHA-256 hash.
pub struct DownloadResult {
    pub path: PathBuf,
    pub hash: String,
    pub url: String,
}

/// Returned to callers after fetch_and_download completes.
pub struct FileInfo {
    pub hash: String,
    pub url: String,
}

/// Download a URL to a temporary file, compute its SHA-256 hash, and optionally verify against
/// an expected hash. Returns the path to the temp file and the computed hash.
pub fn download_and_verify(
    url: &str,
    dest_dir: &Path,
    expected_hash: Option<&str>,
) -> Result<DownloadResult> {
    debug!(url = %url, "Downloading");

    std::fs::create_dir_all(dest_dir)
        .with_context(|| format!("failed to create directory: {}", dest_dir.display()))?;

    let client = reqwest::blocking::Client::builder()
        .user_agent("kong-dependency-manager/0.1")
        .timeout(std::time::Duration::from_secs(600)) // 10 min for large archives
        .connect_timeout(std::time::Duration::from_secs(30))
        .build()
        .context("failed to build HTTP client")?;

    let response = client
        .get(url)
        .send()
        .with_context(|| format!("HTTP request failed: {url}"))?;

    if !response.status().is_success() {
        bail!("download failed: HTTP {} for {url}", response.status());
    }

    let bytes = response
        .bytes()
        .with_context(|| format!("failed to read response body from {url}"))?;

    // Compute SHA-256
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let hash = hex::encode(hasher.finalize());

    // Verify hash if expected
    if let Some(expected) = expected_hash {
        if hash != expected {
            bail!(
                "hash mismatch for {url}: expected {expected}, got {hash}"
            );
        }
        debug!("Hash verified: {hash}");
    }

    // Write to temp file in dest_dir
    let file_name = url
        .rsplit('/')
        .next()
        .unwrap_or("download");
    let dest_path = dest_dir.join(file_name);

    let mut file = std::fs::File::create(&dest_path)
        .with_context(|| format!("failed to create file: {}", dest_path.display()))?;
    file.write_all(&bytes)?;

    info!(path = %dest_path.display(), size = bytes.len(), "Downloaded");

    Ok(DownloadResult {
        path: dest_path,
        hash,
        url: url.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_hash_is_hex() {
        let mut hasher = Sha256::new();
        hasher.update(b"hello");
        let hash = hex::encode(hasher.finalize());
        assert_eq!(hash.len(), 64);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
