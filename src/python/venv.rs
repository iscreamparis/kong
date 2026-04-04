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
        write_activation_scripts_windows(&venv)?;
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
        write_activation_scripts_unix(&venv)?;
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

/// Generate `Scripts\Activate.ps1` and `Scripts\activate.bat` inside the venv (Windows).
#[cfg(windows)]
fn write_activation_scripts_windows(venv: &Path) -> Result<()> {
    let scripts = venv.join("Scripts");
    let venv_abs = venv
        .canonicalize()
        .unwrap_or_else(|_| venv.to_path_buf());
    let venv_str = venv_abs.to_string_lossy();

    // ── Activate.ps1 ────────────────────────────────────────────────────────
    let ps1_path = scripts.join("Activate.ps1");
    if !ps1_path.exists() {
        let ps1 = format!(
            r#"# KONG-generated Python venv activation script
$script:VENV = "{venv_str}"

function global:deactivate([switch]$NonDestructive) {{
    if (Test-Path Env:_KONG_OLD_PATH) {{
        $env:PATH = $env:_KONG_OLD_PATH
        Remove-Item Env:_KONG_OLD_PATH -ErrorAction SilentlyContinue
    }}
    if (Test-Path Function:_KONG_OLD_PROMPT) {{
        Copy-Item Function:_KONG_OLD_PROMPT Function:prompt
        Remove-Item Function:_KONG_OLD_PROMPT -ErrorAction SilentlyContinue
    }}
    Remove-Item Env:VIRTUAL_ENV -ErrorAction SilentlyContinue
    Remove-Item Env:VIRTUAL_ENV_PROMPT -ErrorAction SilentlyContinue
    if (!$NonDestructive) {{ Remove-Item Function:deactivate -ErrorAction SilentlyContinue }}
}}

deactivate -NonDestructive

$env:VIRTUAL_ENV = $script:VENV
$env:VIRTUAL_ENV_PROMPT = ".venv"
$env:_KONG_OLD_PATH = $env:PATH
$env:PATH = "$env:VIRTUAL_ENV\Scripts;$env:PATH"

Copy-Item Function:prompt Function:_KONG_OLD_PROMPT -ErrorAction SilentlyContinue
function global:prompt {{
    Write-Host -NoNewline -ForegroundColor Green "(.venv) "
    & $Function:_KONG_OLD_PROMPT
}}
"#
        );
        std::fs::write(&ps1_path, ps1)
            .with_context(|| format!("failed to write {}", ps1_path.display()))?;
        debug!("Wrote Activate.ps1");
    }

    // ── activate.bat ────────────────────────────────────────────────────────
    let bat_path = scripts.join("activate.bat");
    if !bat_path.exists() {
        let bat = format!(
            "@echo off\r\nset \"VIRTUAL_ENV={venv_str}\"\r\nset \"PATH=%VIRTUAL_ENV%\\Scripts;%PATH%\"\r\n"
        );
        std::fs::write(&bat_path, bat)
            .with_context(|| format!("failed to write {}", bat_path.display()))?;
        debug!("Wrote activate.bat");
    }

    Ok(())
}

/// Generate `bin/activate` POSIX shell script inside the venv (Unix).
#[cfg(not(windows))]
fn write_activation_scripts_unix(venv: &Path) -> Result<()> {
    let bin = venv.join("bin");
    let venv_abs = venv
        .canonicalize()
        .unwrap_or_else(|_| venv.to_path_buf());
    let venv_str = venv_abs.to_string_lossy();

    let activate_path = bin.join("activate");
    if !activate_path.exists() {
        let script = format!(
            r#"# KONG-generated Python venv activation script
VIRTUAL_ENV="{venv_str}"
export VIRTUAL_ENV

_KONG_OLD_PATH="$PATH"
PATH="$VIRTUAL_ENV/bin:$PATH"
export PATH

_KONG_OLD_PS1="${{PS1:-}}"
PS1="(.venv) ${{PS1:-}}"
export PS1

deactivate() {{
    PATH="$_KONG_OLD_PATH"
    export PATH
    PS1="$_KONG_OLD_PS1"
    export PS1
    unset VIRTUAL_ENV _KONG_OLD_PATH _KONG_OLD_PS1
    unset -f deactivate
}}
"#
        );
        std::fs::write(&activate_path, &script)
            .with_context(|| format!("failed to write {}", activate_path.display()))?;
        // Make it executable
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&activate_path)?.permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&activate_path, perms)?;
        }
        debug!("Wrote bin/activate");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    // Venv tests require tempdir + store fixtures — see kong-test skill
}
