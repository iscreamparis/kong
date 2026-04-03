use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tracing::{debug, info, warn};

use crate::config::{KongRules, PythonSection};
use crate::link;

/// Build a Python .venv from the kong store.
/// Uses the kong-managed Python runtime recorded in `rules.runtimes.python`.
pub fn build_venv(project_dir: &Path, python: &PythonSection, store_root: &Path, rules: &KongRules) -> Result<()> {
    let venv = project_dir.join(".venv");

    // ── site-packages path (version-agnostic on Windows, version-tagged on Unix) ──
    #[cfg(windows)]
    let site_packages = venv.join("Lib").join("site-packages");
    #[cfg(not(windows))]
    let site_packages = {
        // Use "python3.X" from the actual runtime version
        let maj_min = major_minor(&python.version);
        venv.join("lib").join(format!("python{maj_min}")).join("site-packages")
    };
    std::fs::create_dir_all(&site_packages)
        .with_context(|| format!("failed to create site-packages at {}", site_packages.display()))?;

    // ── Locate kong-managed python executable ───────────────────────────────
    let kong_python = kong_python_exe(store_root, rules);
    if kong_python.is_none() {
        warn!("Kong-managed Python runtime not found in store; .venv may be incomplete");
    }

    // ── pyvenv.cfg ──────────────────────────────────────────────────────────
    let cfg_path = venv.join("pyvenv.cfg");
    let python_home = kong_python
        .as_ref()
        .and_then(|e| e.parent())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "".to_string());
    let cfg_content = format!(
        "home = {python_home}\ninclude-system-site-packages = false\nversion = {}\n",
        python.version
    );
    std::fs::write(&cfg_path, &cfg_content)
        .with_context(|| format!("failed to write pyvenv.cfg at {}", cfg_path.display()))?;
    debug!("Wrote pyvenv.cfg");

    // ── Scripts / bin directory with python executable ───────────────────────
    #[cfg(windows)]
    {
        let scripts = venv.join("Scripts");
        std::fs::create_dir_all(&scripts)?;
        if let Some(ref src_exe) = kong_python {
            copy_or_link(src_exe, &scripts.join("python.exe"))?;
            // Also pythonw.exe if present alongside python.exe
            let src_w = src_exe.with_file_name("pythonw.exe");
            if src_w.exists() {
                copy_or_link(&src_w, &scripts.join("pythonw.exe"))?;
            }
        }
    }
    #[cfg(not(windows))]
    {
        let bin = venv.join("bin");
        std::fs::create_dir_all(&bin)?;
        if let Some(ref src_exe) = kong_python {
            let dst = bin.join("python3");
            if !dst.exists() {
                std::os::unix::fs::symlink(src_exe, &dst)?;
            }
            let dst2 = bin.join("python");
            if !dst2.exists() {
                std::os::unix::fs::symlink(src_exe, &dst2)?;
            }
        }
    }

    // ── Link packages from store into site-packages ──────────────────────────
    for pkg in &python.packages {
        let src = store_root.join(&pkg.store_path);
        if !src.exists() {
            warn!(pkg = %pkg.name, "Store path missing, skipping: {}", src.display());
            continue;
        }
        link::link_package(&src, &site_packages)?;
        debug!(pkg = %pkg.name, "Linked into site-packages");
    }

    info!("Python .venv ready at {}", venv.display());
    Ok(())
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Locate the python executable from the kong-managed runtime recorded in rules.
fn kong_python_exe(store_root: &Path, rules: &KongRules) -> Option<PathBuf> {
    let runtime_path = rules
        .runtimes
        .as_ref()?
        .python
        .as_ref()?
        .store_path
        .as_str();
    let runtime_dir = store_root.join(runtime_path);
    crate::python::runtime::python_exe_in(&runtime_dir)
}

/// Copy src → dst if they are on different volumes, otherwise hard-link.
fn copy_or_link(src: &Path, dst: &Path) -> Result<()> {
    if dst.exists() {
        return Ok(());
    }
    if std::fs::hard_link(src, dst).is_err() {
        std::fs::copy(src, dst)
            .with_context(|| format!("failed to copy {} → {}", src.display(), dst.display()))?;
    }
    Ok(())
}

fn major_minor(version: &str) -> String {
    let mut parts = version.splitn(3, '.');
    let major = parts.next().unwrap_or("3");
    let minor = parts.next().unwrap_or("0");
    format!("{major}.{minor}")
}

#[cfg(test)]
mod tests {
    // Venv tests require tempdir + store fixtures — see kong-test skill
}
