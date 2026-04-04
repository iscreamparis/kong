use std::path::Path;

use anyhow::{bail, Context, Result};
use tracing::{debug, info};

/// Extract a .whl (zip) archive to a destination directory.
pub fn extract_wheel(archive_path: &Path, dest: &Path) -> Result<()> {
    debug!(src = %archive_path.display(), dst = %dest.display(), "Extracting wheel (zip)");

    let file = std::fs::File::open(archive_path)
        .with_context(|| format!("failed to open archive: {}", archive_path.display()))?;
    let mut archive = zip::ZipArchive::new(file)
        .with_context(|| format!("invalid zip archive: {}", archive_path.display()))?;

    std::fs::create_dir_all(dest)?;

    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let enclosed = entry.enclosed_name().context("invalid zip entry name")?;

        // Skip stray .whl files that some wheels embed as zero-byte markers
        if enclosed.extension().and_then(|e| e.to_str()) == Some("whl") {
            debug!(name = %enclosed.display(), "Skipping .whl entry inside wheel");
            continue;
        }

        let out_path = dest.join(&enclosed);

        if entry.is_dir() {
            std::fs::create_dir_all(&out_path)?;
        } else {
            if let Some(parent) = out_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut out_file = std::fs::File::create(&out_path)?;
            std::io::copy(&mut entry, &mut out_file)?;
        }
    }

    info!(dest = %dest.display(), entries = archive.len(), "Wheel extracted");
    Ok(())
}

/// Extract a .tgz / .tar.gz / .crate archive to a destination directory.
pub fn extract_targz(archive_path: &Path, dest: &Path) -> Result<()> {
    debug!(src = %archive_path.display(), dst = %dest.display(), "Extracting tar.gz");

    let file = std::fs::File::open(archive_path)
        .with_context(|| format!("failed to open archive: {}", archive_path.display()))?;
    let decompressed = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(decompressed);

    std::fs::create_dir_all(dest)?;
    archive
        .unpack(dest)
        .with_context(|| format!("failed to extract: {}", archive_path.display()))?;

    info!(dest = %dest.display(), "tar.gz extracted");
    Ok(())
}

/// Extract a .tar.gz, stripping the first path component (like `tar --strip-components=1`).
pub fn extract_targz_strip1(archive_path: &Path, dest: &Path) -> Result<()> {
    #![allow(dead_code)]
    debug!(src = %archive_path.display(), dst = %dest.display(), "Extracting tar.gz (strip-1)");

    let file = std::fs::File::open(archive_path)
        .with_context(|| format!("failed to open archive: {}", archive_path.display()))?;
    let decompressed = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(decompressed);

    std::fs::create_dir_all(dest)?;

    for entry in archive.entries()? {
        let mut entry = entry?;
        let raw_path = entry.path()?.to_path_buf();
        let stripped: std::path::PathBuf = raw_path.components().skip(1).collect();
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

    info!(dest = %dest.display(), "tar.gz extracted (strip-1)");
    Ok(())
}

/// Auto-detect archive type and extract.
pub fn extract(archive_path: &Path, dest: &Path) -> Result<()> {
    let name = archive_path
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();

    if name.ends_with(".whl") || name.ends_with(".zip") {
        extract_wheel(archive_path, dest)
    } else if name.ends_with(".tgz")
        || name.ends_with(".tar.gz")
        || name.ends_with(".crate")
    {
        extract_targz(archive_path, dest)
    } else {
        bail!("unsupported archive format: {}", archive_path.display());
    }
}

#[cfg(test)]
mod tests {
    // Extraction tests require real archive fixtures — see kong-test skill
}
