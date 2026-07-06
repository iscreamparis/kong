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
///
/// Directories are always created via `create_dir_all` (OS-default mode,
/// 0o755 on Unix) rather than preserving the tar-recorded mode.  Some npm
/// tarballs (e.g. pngjs 5.0.0) record directory entries with mode 0o666 — no
/// execute/traverse bit.  `Archive::unpack` would preserve that verbatim,
/// making those directories non-traversable and causing any subsequent
/// `hard_link` into them to fail with EACCES (os error 13).  npm, pnpm, and
/// yarn all normalise directory modes on extraction for the same reason; kong
/// now does the same.  File modes are preserved as-is via `entry.unpack` so
/// that executable bits on scripts and binaries are not disturbed.
pub fn extract_targz(archive_path: &Path, dest: &Path) -> Result<()> {
    debug!(src = %archive_path.display(), dst = %dest.display(), "Extracting tar.gz");

    let file = std::fs::File::open(archive_path)
        .with_context(|| format!("failed to open archive: {}", archive_path.display()))?;
    let decompressed = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(decompressed);

    std::fs::create_dir_all(dest)?;

    for entry in archive
        .entries()
        .with_context(|| format!("failed to read archive entries: {}", archive_path.display()))?
    {
        let mut entry = entry
            .with_context(|| format!("corrupt archive entry in: {}", archive_path.display()))?;
        let raw_path = entry.path()?.to_path_buf();
        let out = dest.join(&raw_path);

        if entry.header().entry_type().is_dir() {
            // Use create_dir_all so the OS assigns a traversable mode (0o755
            // on Unix) rather than inheriting whatever the tar entry recorded.
            std::fs::create_dir_all(&out)
                .with_context(|| format!("failed to create dir: {}", out.display()))?;
        } else {
            if let Some(p) = out.parent() {
                std::fs::create_dir_all(p)
                    .with_context(|| format!("failed to create parent dir: {}", p.display()))?;
            }
            entry
                .unpack(&out)
                .with_context(|| format!("failed to unpack entry: {}", out.display()))?;
        }
    }

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

/// Extract a .tar.gz, stripping the first N path components.
/// Used for Homebrew bottles which have `<name>/<version>/bin/...` structure (strip 2).
///
/// Hard links are handled in a deferred second pass: during the first pass,
/// hard-link entries whose targets haven't been extracted yet are recorded.
/// After all regular files are extracted, the deferred hard links are retried.
pub fn extract_targz_strip(archive_path: &Path, dest: &Path, strip: usize) -> Result<()> {
    debug!(src = %archive_path.display(), dst = %dest.display(), strip, "Extracting tar.gz (strip-N)");

    let file = std::fs::File::open(archive_path)
        .with_context(|| format!("failed to open archive: {}", archive_path.display()))?;
    let decompressed = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(decompressed);

    std::fs::create_dir_all(dest)?;

    // Deferred hard links: (output_path, link_target_path)
    let mut deferred_hardlinks: Vec<(std::path::PathBuf, std::path::PathBuf)> = Vec::new();

    for entry in archive.entries()? {
        let mut entry = entry?;
        let raw_path = entry.path()?.to_path_buf();
        let stripped: std::path::PathBuf = raw_path.components().skip(strip).collect();
        if stripped.as_os_str().is_empty() {
            continue;
        }
        let out = dest.join(&stripped);
        let etype = entry.header().entry_type();
        if etype.is_dir() {
            std::fs::create_dir_all(&out)?;
        } else if etype.is_hard_link() {
            // Hard links: strip the link target path the same way
            if let Some(link_target) = entry.link_name()? {
                let stripped_target: std::path::PathBuf =
                    link_target.components().skip(strip).collect();
                let target_out = dest.join(&stripped_target);
                if target_out.exists() {
                    if let Some(p) = out.parent() {
                        std::fs::create_dir_all(p)?;
                    }
                    std::fs::hard_link(&target_out, &out)
                        .or_else(|_| std::fs::copy(&target_out, &out).map(|_| ()))?;
                } else {
                    // Target not yet extracted — defer to second pass
                    deferred_hardlinks.push((out, target_out));
                }
            }
        } else if etype.is_symlink() {
            // Symlinks: unpack normally (target is relative, no stripping needed)
            if let Some(p) = out.parent() {
                std::fs::create_dir_all(p)?;
            }
            entry.unpack(&out)?;
        } else {
            if let Some(p) = out.parent() {
                std::fs::create_dir_all(p)?;
            }
            entry.unpack(&out)?;
        }
    }

    // Second pass: retry deferred hard links now that all files are extracted
    for (out, target_out) in deferred_hardlinks {
        if let Some(p) = out.parent() {
            std::fs::create_dir_all(p)?;
        }
        if target_out.exists() {
            std::fs::hard_link(&target_out, &out)
                .or_else(|_| std::fs::copy(&target_out, &out).map(|_| ()))
                .with_context(|| {
                    format!(
                        "failed to create hard link {} → {}",
                        out.display(),
                        target_out.display()
                    )
                })?;
        } else {
            debug!(
                link = %out.display(),
                target = %target_out.display(),
                "Skipping hard link: target not found after full extraction"
            );
        }
    }

    info!(dest = %dest.display(), "tar.gz extracted (strip-{})", strip);
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
    use super::*;

    /// Build an in-memory .tar.gz that reproduces the pngjs pattern:
    /// a directory entry with mode 0o666 (no execute/traverse bit) containing
    /// a regular file.
    fn make_tar_gz_bad_dir_mode() -> Vec<u8> {
        let buf = Vec::new();
        let enc = flate2::write::GzEncoder::new(buf, flate2::Compression::default());
        let mut ar = tar::Builder::new(enc);

        // Directory entry with bad mode — no execute bit on any class
        let mut hdr = tar::Header::new_gnu();
        hdr.set_path("baddir/").unwrap();
        hdr.set_mode(0o666);
        hdr.set_entry_type(tar::EntryType::Directory);
        hdr.set_size(0);
        hdr.set_mtime(0);
        hdr.set_uid(0);
        hdr.set_gid(0);
        hdr.set_cksum();
        ar.append(&hdr, std::io::empty()).unwrap();

        // Regular file inside the bad-mode directory
        let content: &[u8] = b"hello from bad-mode dir";
        let mut hdr = tar::Header::new_gnu();
        hdr.set_path("baddir/file.txt").unwrap();
        hdr.set_mode(0o644);
        hdr.set_entry_type(tar::EntryType::Regular);
        hdr.set_size(content.len() as u64);
        hdr.set_mtime(0);
        hdr.set_uid(0);
        hdr.set_gid(0);
        hdr.set_cksum();
        ar.append(&hdr, content).unwrap();

        ar.into_inner().unwrap().finish().unwrap()
    }

    /// Regression test for the pngjs-style "directory with mode 0o666" bug.
    ///
    /// Verifies that:
    ///  1. `extract_targz` succeeds (no EACCES at extraction time).
    ///  2. The extracted directory is owner-traversable (execute bit set) on
    ///     Unix — regardless of the mode recorded in the tar entry.
    ///  3. The file inside the directory is reachable and has the right content,
    ///     proving that a subsequent `hard_link` would not fail with EACCES.
    #[test]
    fn extract_targz_normalises_bad_directory_mode() {
        let tmp = tempfile::TempDir::new().unwrap();

        // Write the crafted archive to a temp file (extract_targz takes a Path)
        let archive_path = tmp.path().join("bad_dir_mode.tar.gz");
        std::fs::write(&archive_path, make_tar_gz_bad_dir_mode()).unwrap();

        let dest = tmp.path().join("extracted");
        extract_targz(&archive_path, &dest).expect("extract_targz must succeed");

        // The file inside the formerly-bad-mode directory must be reachable
        let file_path = dest.join("baddir").join("file.txt");
        assert!(
            file_path.exists(),
            "file inside bad-mode dir must exist after extraction"
        );
        assert_eq!(
            std::fs::read_to_string(&file_path).unwrap(),
            "hello from bad-mode dir",
            "file content must be intact"
        );

        // On Unix: the directory must now have the owner execute/traverse bit set.
        // This is what was missing before the fix, and what caused EACCES on
        // hard_link.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir_path = dest.join("baddir");
            let mode = std::fs::metadata(&dir_path).unwrap().permissions().mode();
            assert!(
                mode & 0o100 != 0,
                "owner execute (traverse) bit must be set on extracted directory \
                 (actual mode: {:#o}, tar-recorded mode was 0o666)",
                mode
            );
        }
    }

    /// Verify that a well-formed tar.gz (normal 0o755 dirs) still extracts correctly.
    #[test]
    fn extract_targz_normal_archive_works() {
        let tmp = tempfile::TempDir::new().unwrap();

        // Build a normal archive
        let buf = Vec::new();
        let enc = flate2::write::GzEncoder::new(buf, flate2::Compression::default());
        let mut ar = tar::Builder::new(enc);

        let mut hdr = tar::Header::new_gnu();
        hdr.set_path("pkg/").unwrap();
        hdr.set_mode(0o755);
        hdr.set_entry_type(tar::EntryType::Directory);
        hdr.set_size(0);
        hdr.set_mtime(0);
        hdr.set_uid(0);
        hdr.set_gid(0);
        hdr.set_cksum();
        ar.append(&hdr, std::io::empty()).unwrap();

        let content: &[u8] = b"normal content";
        let mut hdr = tar::Header::new_gnu();
        hdr.set_path("pkg/index.js").unwrap();
        hdr.set_mode(0o644);
        hdr.set_entry_type(tar::EntryType::Regular);
        hdr.set_size(content.len() as u64);
        hdr.set_mtime(0);
        hdr.set_uid(0);
        hdr.set_gid(0);
        hdr.set_cksum();
        ar.append(&hdr, content).unwrap();

        let bytes = ar.into_inner().unwrap().finish().unwrap();

        let archive_path = tmp.path().join("normal.tar.gz");
        std::fs::write(&archive_path, bytes).unwrap();

        let dest = tmp.path().join("out");
        extract_targz(&archive_path, &dest).expect("normal archive must extract cleanly");

        assert_eq!(
            std::fs::read_to_string(dest.join("pkg").join("index.js")).unwrap(),
            "normal content"
        );
    }
}
