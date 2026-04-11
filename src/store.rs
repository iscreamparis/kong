use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

// ── Store root detection ────────────────────────────────────────────────────

pub fn store_root() -> Result<PathBuf> {
    // 1. Check env var (override for testing / custom setups)
    if let Ok(path) = std::env::var("KONG_STORE") {
        let p = PathBuf::from(path);
        std::fs::create_dir_all(&p)
            .with_context(|| format!("failed to create store at KONG_STORE={}", p.display()))?;
        return Ok(p);
    }

    // 2. Derive from a platform-appropriate location.
    //    Windows: next to kong.exe  (e.g. C:\kong\store\)
    //    macOS/Linux: ~/.local/share/kong/store  (XDG / macOS convention)
    let root = install_root()?.join("store");

    std::fs::create_dir_all(&root)
        .with_context(|| format!("failed to create store at {}", root.display()))?;
    Ok(root)
}

/// Returns the kong data directory root.
/// Windows: parent of kong.exe (e.g. `C:\kong`)
/// macOS/Linux: `~/.local/share/kong` (XDG data dir)
pub fn install_root() -> Result<PathBuf> {
    #[cfg(windows)]
    {
        let exe = std::env::current_exe()
            .context("could not determine kong executable path")?;
        Ok(exe.parent().context("kong executable has no parent directory")?.to_path_buf())
    }
    #[cfg(not(windows))]
    {
        let base = dirs::data_local_dir()
            .context("could not determine local data directory (~/.local/share on Linux, ~/Library/Application Support on macOS)")?;
        Ok(base.join("kong"))
    }
}

/// Returns the RULEZ directory for a named project:
///   `<install_root>/RULEZ/<project_name>/`
///
/// All virtual environments (`.venv`, `node_modules`, `.rust-toolchain`) are
/// created here instead of inside the project tree.  Because the RULEZ dir
/// sits on the same drive as the store, hard links always work even when the
/// project lives on a different drive.
pub fn rulez_dir(project_name: &str) -> Result<PathBuf> {
    let path = install_root()?.join("RULEZ").join(project_name);
    std::fs::create_dir_all(&path)
        .with_context(|| format!("failed to create RULEZ dir at {}", path.display()))?;
    Ok(path)
}

// ── Store path helpers ──────────────────────────────────────────────────────

#[allow(dead_code)]
pub fn python_store_path(
    store_root: &Path,
    name: &str,
    version: &str,
    python_tag: &str,
    platform_tag: &str,
) -> PathBuf {
    store_root
        .join("python")
        .join("libs")
        .join(format!("{name}-{version}-{python_tag}-{platform_tag}"))
}

#[allow(dead_code)]
pub fn node_store_path(store_root: &Path, name: &str, version: &str) -> PathBuf {
    store_root
        .join("node")
        .join("libs")
        .join(format!("{name}-{version}"))
}

#[allow(dead_code)]
pub fn rust_store_path(store_root: &Path, name: &str, version: &str) -> PathBuf {
    store_root
        .join("rust")
        .join("crates")
        .join(format!("{name}-{version}"))
}

// ── Verified marker ─────────────────────────────────────────────────────────

const VERIFIED_MARKER: &str = ".kong-verified";

pub fn is_verified(store_path: &Path) -> bool {
    store_path.join(VERIFIED_MARKER).exists()
}

pub fn write_verified_marker(store_path: &Path, hash: &str) -> Result<()> {
    let marker = store_path.join(VERIFIED_MARKER);
    let content = format!("hash={hash}\nverified={}\n", chrono::Utc::now().to_rfc3339());
    std::fs::write(&marker, content)
        .with_context(|| format!("failed to write marker: {}", marker.display()))?;
    Ok(())
}

// ── Doctor report ───────────────────────────────────────────────────────────

pub struct DoctorReport {
    pub store_path: PathBuf,
    pub store_exists: bool,
    pub python_count: usize,
    pub node_count: usize,
    pub rust_count: usize,
    pub issues: Vec<String>,
}

impl DoctorReport {
    pub fn print(&self) {
        if self.store_exists {
            println!("  ✓ Store: {} ({} Python, {} Node, {} Rust packages)",
                self.store_path.display(),
                self.python_count,
                self.node_count,
                self.rust_count,
            );
        } else {
            println!("  ✗ Store: {} (not found)", self.store_path.display());
        }

        if self.issues.is_empty() {
            println!("  ✓ No issues found");
        } else {
            for issue in &self.issues {
                println!("  ✗ {issue}");
            }
        }
    }
}

pub fn doctor() -> Result<DoctorReport> {
    let root = store_root()?;
    let exists = root.exists();

    let mut python_count = 0;
    let mut node_count = 0;
    let mut rust_count = 0;
    let mut issues = Vec::new();

    if exists {
        let py_libs = root.join("python").join("libs");
        if py_libs.exists() {
            python_count = count_subdirs(&py_libs);
        }

        let node_libs = root.join("node").join("libs");
        if node_libs.exists() {
            node_count = count_subdirs(&node_libs);
        }

        let rust_crates = root.join("rust").join("crates");
        if rust_crates.exists() {
            rust_count = count_subdirs(&rust_crates);
        }

        // Check for unverified entries
        check_verified(&py_libs, &mut issues);
        check_verified(&node_libs, &mut issues);
        check_verified(&rust_crates, &mut issues);
    } else {
        issues.push("Store directory does not exist. Run 'kong rules' to initialize.".to_string());
    }

    Ok(DoctorReport {
        store_path: root,
        store_exists: exists,
        python_count,
        node_count,
        rust_count,
        issues,
    })
}

fn count_subdirs(path: &Path) -> usize {
    std::fs::read_dir(path)
        .map(|entries| entries.filter_map(|e| e.ok()).filter(|e| e.path().is_dir()).count())
        .unwrap_or(0)
}

fn check_verified(libs_dir: &Path, issues: &mut Vec<String>) {
    if !libs_dir.exists() {
        return;
    }
    if let Ok(entries) = std::fs::read_dir(libs_dir) {
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.is_dir() && !is_verified(&path) {
                issues.push(format!("Unverified package: {}", path.display()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_path_construction() {
        let root = PathBuf::from("/store");
        assert_eq!(
            python_store_path(&root, "requests", "2.31.0", "py3", "win_amd64"),
            PathBuf::from("/store/python/libs/requests-2.31.0-py3-win_amd64")
        );
        assert_eq!(
            node_store_path(&root, "express", "4.18.2"),
            PathBuf::from("/store/node/libs/express-4.18.2")
        );
        assert_eq!(
            rust_store_path(&root, "serde", "1.0.193"),
            PathBuf::from("/store/rust/crates/serde-1.0.193")
        );
    }
}
