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

/// Create a hard link for a file, falling back to copy on cross-device errors.
pub fn link_file(src: &Path, dst: &Path) -> Result<()> {
    debug!(src = %src.display(), dst = %dst.display(), "Hard link file");
    match std::fs::hard_link(src, dst) {
        Ok(()) => Ok(()),
        Err(e) if e.raw_os_error() == Some(17) => {
            // os error 17 = ERROR_NOT_SAME_DEVICE — store and project are on
            // different drives; fall back to file copy.
            debug!(src = %src.display(), "Cross-drive: falling back to copy");
            std::fs::copy(src, dst)
                .with_context(|| format!("copy fallback failed: {}", src.display()))?;
            Ok(())
        }
        Err(e) => Err(e).with_context(|| format!("hard_link failed: {}", src.display())),
    }
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

/// Create junctions from the project directory into the RULEZ env dir so that
/// tools (Vite, Node.js, pip) can find `node_modules` and `.venv` by walking
/// up the filesystem from the project root — no `NODE_PATH` env hacks needed.
pub fn create_project_junctions(
    project_dir: &Path,
    env_dir: &Path,
    rules: &crate::config::KongRules,
) -> Result<()> {
    if rules.node.is_some() {
        let src = env_dir.join("node_modules");
        let dst = project_dir.join("node_modules");
        if src.exists() {
            // If dst is a real (non-junction) directory, remove it only when it
            // contains nothing but Vite's auto-created cache (.vite).
            if dst.exists() && !junction::exists(&dst).unwrap_or(false) {
                let vite_only = std::fs::read_dir(&dst)
                    .map(|entries| {
                        let names: Vec<_> = entries
                            .filter_map(|e| e.ok())
                            .map(|e| e.file_name())
                            .collect();
                        names.len() <= 1
                            && names.iter().all(|n| n == ".vite")
                    })
                    .unwrap_or(false);
                if vite_only {
                    std::fs::remove_dir_all(&dst).with_context(|| {
                        format!("failed to remove Vite cache dir {}", dst.display())
                    })?;
                    debug!(path = %dst.display(), "Removed Vite-only node_modules before junction");
                }
            }
            if !dst.exists() {
                link_dir(&src, &dst)
                    .with_context(|| format!("failed to junction node_modules into project dir"))?;
                debug!(dst = %dst.display(), "Junctioned node_modules into project dir");
            }
        }
    }
    if rules.python.is_some() {
        let src = env_dir.join(".venv");
        let dst = project_dir.join(".venv");
        if src.exists() && !dst.exists() {
            link_dir(&src, &dst)
                .with_context(|| format!("failed to junction .venv into project dir"))?;
            debug!(dst = %dst.display(), "Junctioned .venv into project dir");
        }
    }
    Ok(())
}

/// Remove project-dir junctions created by `create_project_junctions`.
/// Only removes the junction entry itself — not the target contents in RULEZ.
pub fn clean_project_junctions(project_dir: &Path) -> Result<()> {
    for name in &["node_modules", ".venv"] {
        let p = project_dir.join(name);
        if p.exists() {
            #[cfg(windows)]
            {
                // Only delete the reparse point if it is actually a junction.
                if junction::exists(&p).unwrap_or(false) {
                    junction::delete(&p)
                        .with_context(|| format!("failed to delete junction {}", p.display()))?;
                    debug!(path = %p.display(), "Junction removed");
                } else {
                    debug!(path = %p.display(), "Not a junction — leaving real directory in place");
                }
            }
            #[cfg(unix)]
            {
                if p.is_symlink() {
                    std::fs::remove_file(&p)?;
                }
            }
        }
    }
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

    let rust_toolchain = project_dir.join(".rust-toolchain");
    if rust_toolchain.exists() {
        debug!(path = %rust_toolchain.display(), "Removing .rust-toolchain");
        remove_dir_all_robust(&rust_toolchain)?;
    }

    Ok(())
}

/// Remove a single file robustly: clears read-only attribute and retries on
/// transient locks (e.g. Windows Defender scanning the file, os error 32).
#[allow(dead_code)]
fn remove_file_robust(p: &Path) -> Result<()> {
    for attempt in 0..5u32 {
        match std::fs::remove_file(p) {
            Ok(()) => return Ok(()),
            Err(e) => {
                // os error 5  = Access Denied (read-only) — clear flag once
                // os error 32 = Sharing Violation (file locked) — wait & retry
                match e.raw_os_error() {
                    Some(5) => {
                        let mut perms = std::fs::metadata(p)
                            .with_context(|| format!("cannot read metadata: {}", p.display()))?
                            .permissions();
                        perms.set_readonly(false);
                        std::fs::set_permissions(p, perms)
                            .with_context(|| format!("cannot clear read-only: {}", p.display()))?;
                        // retry immediately
                    }
                    Some(32) if attempt < 4 => {
                        std::thread::sleep(std::time::Duration::from_millis(500 * (attempt + 1) as u64));
                    }
                    _ => {
                        return Err(e).with_context(|| format!("cannot delete: {}", p.display()));
                    }
                }
            }
        }
    }
    std::fs::remove_file(p).with_context(|| format!("cannot delete (gave up): {}", p.display()))
}

/// Robustly remove a directory that may contain junctions, symlinks, or
/// read-only hard-linked files.
///
/// Strategy: rename the directory out of the way first so the original path
/// is immediately free (VS Code file-watchers hold handles to directories but
/// the rename succeeds on NTFS because handles are inode-based). Then delete
/// the renamed copy, retrying as needed for transient AV/watcher locks.
fn remove_dir_all_robust(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }

    // First pass: remove NTFS junctions so remove_dir_all won't follow them.
    #[cfg(windows)]
    remove_junctions_recursive(path)?;

    #[cfg(windows)]
    {
        // Rename to a temp path so the target path is freed immediately.
        let temp = path.with_file_name(format!(
            "{}.__kong_delete__",
            path.file_name().unwrap_or_default().to_string_lossy()
        ));
        // If a previous failed clean left a temp dir, remove it first.
        if temp.exists() {
            let _ = remove_dir_windows(&temp);
        }
        if let Ok(()) = std::fs::rename(path, &temp) {
            return remove_dir_windows(&temp);
        }
        // Rename failed (VS Code file-watcher holds handle without FILE_SHARE_DELETE).
        // Fall back to shell rd which uses different API flags.
        return remove_dir_windows(path);
    }

    #[cfg(not(windows))]
    std::fs::remove_dir_all(path)
        .with_context(|| format!("cannot remove: {}", path.display()))
}

/// Delete a directory tree on Windows. Tries Rust-native remove_dir_all first,
/// then falls back to `cmd /c rd /s /q` which uses different API flags and
/// can succeed when .NET/Rust is blocked by VS Code file-watcher handles.
#[cfg(windows)]
fn remove_dir_windows(path: &Path) -> Result<()> {
    // Fast path: Rust-native with a couple of retries.
    for attempt in 0..3u32 {
        match std::fs::remove_dir_all(path) {
            Ok(()) => return Ok(()),
            Err(e) if (e.raw_os_error() == Some(32) || e.raw_os_error() == Some(5)) && attempt < 2 => {
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
            Err(_) => break,
        }
    }
    // Fallback: cmd rd — uses CreateFile with FILE_FLAG_DELETE_ON_CLOSE internally
    let status = std::process::Command::new("cmd")
        .args(["/c", "rd", "/s", "/q", &path.to_string_lossy()])
        .status()
        .with_context(|| format!("failed to spawn rd for: {}", path.display()))?;
    if status.success() || !path.exists() {
        Ok(())
    } else {
        anyhow::bail!("rd /s /q failed for: {}", path.display())
    }
}

/// Walk the tree and remove all NTFS junctions before WalkDir tries to follow them.
#[cfg(windows)]
fn remove_junctions_recursive(path: &Path) -> Result<()> {
    let entries = match std::fs::read_dir(path) {
        Ok(e) => e,
        Err(_) => return Ok(()), // can't read, skip
    };
    for entry in entries {
        let entry = entry?;
        let p = entry.path();
        // Use file_type from DirEntry (doesn't follow the junction target)
        let ft = entry.file_type()?;
        if ft.is_dir() || ft.is_symlink() {
            if junction::exists(&p).unwrap_or(false) {
                // Junction — remove it directly (don't recurse into target)
                std::fs::remove_dir(&p)
                    .with_context(|| format!("cannot remove junction: {}", p.display()))?;
            } else {
                // Real directory — recurse
                remove_junctions_recursive(&p)?;
            }
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
