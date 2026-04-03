use std::path::Path;

use anyhow::{Context, Result};
use tracing::debug;
use walkdir::WalkDir;

/// Link all files from `src` into `dst` using hard links (files) and real
/// directories.  Walks the tree recursively so every file in `dst` ends up
/// with its own directory entry inside the project — `__dirname` and module
/// resolution therefore stay inside the project tree, not the store.
///
/// This is the right approach for Node packages: junctioning the whole
/// package directory works on disk but causes Node.js to follow the junction
/// transparently and resolve `__dirname` to the store path, which breaks
/// `require()` resolution for peer / sibling deps.
pub fn link_package(src: &Path, dst: &Path) -> Result<()> {
    debug!(src = %src.display(), dst = %dst.display(), "Linking package (deep)");
    std::fs::create_dir_all(dst)?;

    for entry in WalkDir::new(src).min_depth(1) {
        let entry = entry?;
        let rel = entry.path().strip_prefix(src)
            .with_context(|| format!("strip_prefix failed for {}", entry.path().display()))?;
        let dst_path = dst.join(rel);

        if dst_path.exists() {
            continue;
        }

        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&dst_path)?;
        } else {
            if let Some(p) = dst_path.parent() {
                std::fs::create_dir_all(p)?;
            }
            link_file(entry.path(), &dst_path)
                .with_context(|| format!("failed to link file: {}", entry.path().display()))?;
        }
    }

    Ok(())
}

/// Create a hard link for a file.
pub fn link_file(src: &Path, dst: &Path) -> Result<()> {
    debug!(src = %src.display(), dst = %dst.display(), "Hard link file");
    std::fs::hard_link(src, dst)?;
    Ok(())
}

/// Create a directory junction (Windows) or symlink (Unix).
#[cfg(windows)]
pub fn link_dir(src: &Path, dst: &Path) -> Result<()> {
    debug!(src = %src.display(), dst = %dst.display(), "Junction dir");
    junction::create(src, dst)
        .with_context(|| format!("junction failed: {} -> {}", src.display(), dst.display()))?;
    Ok(())
}

#[cfg(unix)]
pub fn link_dir(src: &Path, dst: &Path) -> Result<()> {
    debug!(src = %src.display(), dst = %dst.display(), "Symlink dir");
    std::os::unix::fs::symlink(src, dst)?;
    Ok(())
}

/// Remove virtual environments created by `kong use`.
pub fn clean_environments(project_dir: &Path) -> Result<()> {
    let venv = project_dir.join(".venv");
    if venv.exists() {
        debug!(path = %venv.display(), "Removing .venv");
        remove_dir_all_robust(&venv)?;
    }

    let node_modules = project_dir.join("node_modules");
    if node_modules.exists() {
        debug!(path = %node_modules.display(), "Removing node_modules");
        remove_dir_all_robust(&node_modules)?;
    }

    let cargo_config = project_dir.join(".cargo").join("config.toml");
    if cargo_config.exists() {
        debug!(path = %cargo_config.display(), "Removing .cargo/config.toml");
        std::fs::remove_file(&cargo_config)?;
    }

    Ok(())
}

/// Robustly remove a directory that may contain junctions/symlinks.
fn remove_dir_all_robust(path: &Path) -> Result<()> {
    // On Windows, junctions should be removed with remove_dir (not recursed into)
    for entry in WalkDir::new(path).contents_first(true) {
        let entry = entry?;
        let p = entry.path();

        #[cfg(windows)]
        {
            if p.is_dir() {
                if junction::exists(p).unwrap_or(false) {
                    std::fs::remove_dir(p)?;
                    continue;
                }
            }
        }

        if p.is_dir() {
            std::fs::remove_dir(p)?;
        } else {
            std::fs::remove_file(p)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hard_link_file_works() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("source.txt");
        std::fs::write(&src, "hello kong").unwrap();

        let dst = tmp.path().join("link.txt");
        link_file(&src, &dst).unwrap();

        assert_eq!(std::fs::read_to_string(&dst).unwrap(), "hello kong");
    }

    #[test]
    fn link_file_idempotent_via_link_package() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src_dir = tmp.path().join("src_pkg");
        std::fs::create_dir(&src_dir).unwrap();
        std::fs::write(src_dir.join("file.txt"), "data").unwrap();

        let dst_dir = tmp.path().join("dst_pkg");
        link_package(&src_dir, &dst_dir).unwrap();
        // Run again — should not error
        link_package(&src_dir, &dst_dir).unwrap();

        assert_eq!(
            std::fs::read_to_string(dst_dir.join("file.txt")).unwrap(),
            "data"
        );
    }
}
