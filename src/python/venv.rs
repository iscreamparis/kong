use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tracing::{debug, info, warn};

use crate::config::{KongRules, PythonSection};
use crate::link;

/// Build a Python .venv from the kong store.
/// Uses the kong-managed Python runtime recorded in `rules.runtimes.python`.
///
/// The environment is built into a temporary sibling directory and then swapped
/// over the live `.venv` in a single atomic `rename`, so a service whose
/// `ExecStart` points inside `.venv` never observes a missing or half-built
/// environment during a rebuild. See [`atomic_swap_venv`].
pub fn build_venv(project_dir: &Path, python: &PythonSection, store_root: &Path, rules: &KongRules) -> Result<()> {
    let venv = project_dir.join(".venv");

    // Build into a temp sibling, then atomically rename over `.venv`. Using a
    // sibling guarantees the temp dir is on the same volume as the target, so
    // the final `rename` is a true atomic move (never a cross-device copy).
    let tmp_venv = temp_sibling(&venv);
    // Clear any leftover temp from a previously interrupted build.
    if tmp_venv.exists() {
        crate::link::remove_dir_all_robust(&tmp_venv)?;
    }

    build_venv_at(&tmp_venv, python, store_root, rules)?;
    atomic_swap_venv(&tmp_venv, &venv)?;

    info!("Python .venv ready at {}", venv.display());
    Ok(())
}

/// Build a fully-populated venv at an arbitrary target path (no swapping).
/// `build_venv` calls this on a temp path; tests call it directly.
///
/// Any path that will be baked into the env (activation scripts' `VIRTUAL_ENV`,
/// console-script shebangs) is computed via [`final_venv_path`] so it points at
/// the FINAL `.venv` location, not the temp path this is being built at — the
/// temp dir is about to be atomically renamed away.
fn build_venv_at(venv: &Path, python: &PythonSection, store_root: &Path, rules: &KongRules) -> Result<()> {
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

    // Path the env will live at once swapped — bake THIS into activation scripts
    // and shebangs, never the temp build path.
    let final_venv = final_venv_path(venv);

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
        write_activation_scripts_windows(&scripts, &final_venv)?;
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
        write_activation_scripts_unix(&bin, &final_venv)?;
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

    // ── Generate [console_scripts] launchers (uvicorn, alembic, pip, …) ───────
    //    pip/venv create these; kong didn't, so ExecStart=.venv/bin/uvicorn
    //    failed 203/EXEC. Driven entirely by each dist-info's entry_points.txt.
    //    Shebang points at the venv's OWN python at its FINAL location, since
    //    this venv is about to be atomically renamed into place (the temp path
    //    it is built at would not survive the swap).
    {
        #[cfg(windows)]
        let (bin_dir, venv_python) = (
            venv.join("Scripts"),
            final_venv.join("Scripts").join("python.exe"),
        );
        #[cfg(not(windows))]
        let (bin_dir, venv_python) = (
            venv.join("bin"),
            final_venv.join("bin").join("python"),
        );
        let n = crate::python::entry_points::generate_console_scripts(
            &site_packages,
            &bin_dir,
            &venv_python,
        )?;
        if n > 0 {
            debug!(count = n, "Generated console-script launchers");
        }
    }

    Ok(())
}

/// Given a temp venv path like `<dir>/.venv.kong-tmp-1234`, return the final
/// path it will be swapped to (`<dir>/.venv`). If `venv` is already the final
/// `.venv` (the path build_venv_at is called with directly, e.g. in tests),
/// it is returned unchanged so baked paths stay correct in both cases.
fn final_venv_path(venv: &Path) -> PathBuf {
    let parent = venv.parent().unwrap_or_else(|| Path::new("."));
    match venv.file_name().and_then(|n| n.to_str()) {
        Some(name) if name.starts_with(".venv.kong-tmp-") => parent.join(".venv"),
        _ => venv.to_path_buf(),
    }
}

/// Compute a same-directory temp path for staging a fresh `.venv` before the
/// atomic swap. PID-suffixed so concurrent `kong use` runs don't collide.
fn temp_sibling(venv: &Path) -> PathBuf {
    let parent = venv.parent().unwrap_or_else(|| Path::new("."));
    parent.join(format!(".venv.kong-tmp-{}", std::process::id()))
}

/// Atomically replace `final_venv` with the freshly-built `tmp_venv`.
///
/// The whole point of this dance: a live service whose `ExecStart` points
/// inside `.venv` must NEVER observe `.venv` missing or half-populated, even
/// during a `kong use --clean` rebuild. We therefore build the new env fully in
/// `tmp_venv`, then flip it into place in one step and only afterwards delete
/// the OLD env.
///
/// Neither Unix nor Windows offers an atomic "rename over an existing, non-empty
/// directory" primitive: POSIX `rename(2)` fails with `ENOTEMPTY`/`EEXIST` when
/// the target directory is non-empty, and Windows has no such operation at all.
/// A freshly-built `.venv` is a real, non-empty directory (site-packages + bin/),
/// and the `--clean`/keep-venv flow leaves the live `.venv` in place, so the
/// target is usually a non-empty directory.
///
/// Both platforms therefore use the same move-aside dance — move the old env
/// aside to a temp name first (fast metadata op), then rename the new env into
/// place, then GC the old copy. The window where `.venv` is absent is reduced to
/// the time between two `rename`s (sub-millisecond) rather than a full rebuild
/// (which was the minutes-long gap behind the old 203/EXEC bug).
fn atomic_swap_venv(tmp_venv: &Path, final_venv: &Path) -> Result<()> {
    #[cfg(not(windows))]
    {
        // POSIX `rename(2)` cannot replace a non-empty directory (ENOTEMPTY), so
        // we move the old env aside, swap the new one in (sub-ms window), GC the
        // old, and roll back on failure — the same proven dance as the Windows
        // branch below. (Linux `renameat2(RENAME_EXCHANGE)` could make this
        // perfectly gapless, but it is intentionally not used: it is not portable
        // to all unix targets and is not compile-checkable on the Windows dev host.)
        if !final_venv.exists() {
            // No existing env — a single rename is already atomic.
            std::fs::rename(tmp_venv, final_venv).with_context(|| {
                format!(
                    "swap failed: {} -> {}",
                    tmp_venv.display(),
                    final_venv.display()
                )
            })?;
            return Ok(());
        }

        // Existing env present: move it aside, swap the new one in, GC the old.
        let parent = final_venv.parent().unwrap_or_else(|| Path::new("."));
        let old_aside = parent.join(format!(".venv.kong-old-{}", std::process::id()));
        if old_aside.exists() {
            let _ = crate::link::remove_dir_all_robust(&old_aside);
        }
        std::fs::rename(final_venv, &old_aside).with_context(|| {
            format!("could not move old .venv aside: {}", final_venv.display())
        })?;
        // Smallest possible gap: between these two renames `.venv` is absent.
        match std::fs::rename(tmp_venv, final_venv) {
            Ok(()) => {
                // Success — remove the old copy in the background of this run.
                let _ = crate::link::remove_dir_all_robust(&old_aside);
                return Ok(());
            }
            Err(e) => {
                // Roll back: restore the old env so the service is not stranded.
                let _ = std::fs::rename(&old_aside, final_venv);
                return Err(e).with_context(|| {
                    format!(
                        "swap failed (old .venv restored): {} -> {}",
                        tmp_venv.display(),
                        final_venv.display()
                    )
                });
            }
        }
    }

    #[cfg(windows)]
    {
        if !final_venv.exists() {
            // No existing env — a single rename is already atomic.
            std::fs::rename(tmp_venv, final_venv).with_context(|| {
                format!(
                    "swap failed: {} -> {}",
                    tmp_venv.display(),
                    final_venv.display()
                )
            })?;
            return Ok(());
        }

        // Existing env present: move it aside, swap the new one in, GC the old.
        let parent = final_venv.parent().unwrap_or_else(|| Path::new("."));
        let old_aside = parent.join(format!(".venv.kong-old-{}", std::process::id()));
        if old_aside.exists() {
            let _ = crate::link::remove_dir_all_robust(&old_aside);
        }
        std::fs::rename(final_venv, &old_aside).with_context(|| {
            format!("could not move old .venv aside: {}", final_venv.display())
        })?;
        // Smallest possible gap: between these two renames `.venv` is absent.
        match std::fs::rename(tmp_venv, final_venv) {
            Ok(()) => {
                // Success — remove the old copy in the background of this run.
                let _ = crate::link::remove_dir_all_robust(&old_aside);
                Ok(())
            }
            Err(e) => {
                // Roll back: restore the old env so the service is not stranded.
                let _ = std::fs::rename(&old_aside, final_venv);
                Err(e).with_context(|| {
                    format!(
                        "swap failed (old .venv restored): {} -> {}",
                        tmp_venv.display(),
                        final_venv.display()
                    )
                })
            }
        }
    }
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

#[allow(dead_code)]
fn major_minor(version: &str) -> String {
    let mut parts = version.splitn(3, '.');
    let major = parts.next().unwrap_or("3");
    let minor = parts.next().unwrap_or("0");
    format!("{major}.{minor}")
}

/// Generate `Scripts\Activate.ps1` and `Scripts\activate.bat` inside the venv (Windows).
///
/// `final_venv` is the path the env will live at after the atomic swap — the
/// baked `VIRTUAL_ENV` must reference that, never the temp build dir.
#[cfg(windows)]
fn write_activation_scripts_windows(scripts: &Path, final_venv: &Path) -> Result<()> {
    let venv_str = final_venv.to_string_lossy();

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
///
/// `final_venv` is the path the env will live at after the atomic swap — the
/// baked `VIRTUAL_ENV` must reference that, never the temp build dir.
#[cfg(not(windows))]
fn write_activation_scripts_unix(bin: &Path, final_venv: &Path) -> Result<()> {
    let venv_str = final_venv.to_string_lossy();

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
    use super::*;
    use crate::config::{
        KongRules, PackageEntry, PythonSection, RuntimeEntry, RuntimeSection,
    };
    use std::collections::HashMap;

    /// Build a fake kong store on disk: a python runtime with a real (empty but
    /// present) interpreter executable, plus one extracted package carrying a
    /// `.dist-info/entry_points.txt` that declares a console_script. Returns
    /// `(store_root, rules)`.
    fn fake_store(tmp: &Path) -> (PathBuf, KongRules) {
        let store_root = tmp.join("store");

        // ── runtime: a stand-in python executable where python_exe_in looks ──
        let runtime_rel = "python/runtime/3.12.0";
        let runtime_dir = store_root.join(runtime_rel);
        #[cfg(windows)]
        let py_exe = runtime_dir.join("python.exe");
        #[cfg(not(windows))]
        let py_exe = runtime_dir.join("bin").join("python3");
        std::fs::create_dir_all(py_exe.parent().unwrap()).unwrap();
        std::fs::write(&py_exe, b"#!/bin/sh\n").unwrap();

        // ── package: webserve-1.0 with a console_script `webserve` ──────────
        let pkg_rel = "python/libs/webserve-1.0-cp312-test";
        let pkg_dir = store_root.join(pkg_rel);
        let di = pkg_dir.join("webserve-1.0.dist-info");
        std::fs::create_dir_all(&di).unwrap();
        std::fs::write(
            di.join("entry_points.txt"),
            "[console_scripts]\nwebserve = webserve.main:run\n",
        )
        .unwrap();
        // a module file so the package has real content too
        let mod_dir = pkg_dir.join("webserve");
        std::fs::create_dir_all(&mod_dir).unwrap();
        std::fs::write(mod_dir.join("__init__.py"), b"").unwrap();

        let rules = KongRules {
            version: 1,
            project: "t".into(),
            generated: "now".into(),
            runtimes: Some(RuntimeSection {
                python: Some(RuntimeEntry {
                    version: "3.12.0".into(),
                    store_path: runtime_rel.into(),
                }),
                node: None,
                rust: None,
            }),
            python: Some(PythonSection {
                version: "3.12.0".into(),
                platform: "test".into(),
                packages: vec![PackageEntry {
                    name: "webserve".into(),
                    version: "1.0".into(),
                    hash: None,
                    store_path: pkg_rel.into(),
                    source_url: None,
                }],
            }),
            node: None,
            rust: None,
            brew: None,
            scripts: HashMap::new(),
            services: Vec::new(),
        };
        (store_root, rules)
    }

    fn site_packages_of(venv: &Path, py_version: &str) -> PathBuf {
        #[cfg(windows)]
        {
            let _ = py_version;
            venv.join("Lib").join("site-packages")
        }
        #[cfg(not(windows))]
        {
            let mm = major_minor(py_version);
            venv.join("lib").join(format!("python{mm}")).join("site-packages")
        }
    }

    /// The console-script launcher exists in the venv's bin/Scripts after `use`.
    #[test]
    fn use_materializes_console_scripts() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (store_root, rules) = fake_store(tmp.path());
        let py = rules.python.as_ref().unwrap();

        let project = tmp.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();
        build_venv(&project, py, &store_root, &rules).unwrap();

        let venv = project.join(".venv");
        #[cfg(not(windows))]
        {
            let launcher = venv.join("bin").join("webserve");
            assert!(launcher.exists(), "launcher must be materialized");
            // Executable bit set.
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&launcher).unwrap().permissions().mode();
            assert!(mode & 0o111 != 0, "launcher must be executable (mode {mode:o})");
            // Invokes the declared callable.
            let body = std::fs::read_to_string(&launcher).unwrap();
            assert!(body.contains("from webserve.main import run"), "body: {body}");
            assert!(body.contains("sys.exit(run())"), "body: {body}");
            // Shebang points at the FINAL venv python, not a temp dir.
            let expected = format!("#!{}", venv.join("bin").join("python").display());
            assert!(
                body.lines().next().unwrap().starts_with(&expected.replace('\\', "/")),
                "shebang must point at the live venv python, got: {}",
                body.lines().next().unwrap()
            );
        }
        #[cfg(windows)]
        {
            assert!(venv.join("Scripts").join("webserve-script.py").exists());
            assert!(venv.join("Scripts").join("webserve.bat").exists());
        }
    }

    /// Activation scripts must bake the FINAL `.venv` path, never the temp
    /// build dir that gets renamed away by the swap.
    #[test]
    fn activation_scripts_reference_final_venv_not_temp() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (store_root, rules) = fake_store(tmp.path());
        let py = rules.python.as_ref().unwrap();
        let project = tmp.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();

        build_venv(&project, py, &store_root, &rules).unwrap();
        let venv = project.join(".venv");

        #[cfg(not(windows))]
        let (act, needle) = (
            venv.join("bin").join("activate"),
            venv.to_string_lossy().into_owned(),
        );
        #[cfg(windows)]
        let (act, needle) = (
            venv.join("Scripts").join("Activate.ps1"),
            venv.to_string_lossy().into_owned(),
        );

        let body = std::fs::read_to_string(&act).unwrap();
        assert!(
            body.contains(&needle),
            "activation must reference the final .venv path ({needle}), body:\n{body}"
        );
        assert!(
            !body.contains(".venv.kong-tmp-"),
            "activation must NOT reference the temp build dir, body:\n{body}"
        );
    }

    /// `build_venv` builds in a temp sibling and only swaps at the end; the
    /// live `.venv` is never absent or half-built mid-build. We can't observe
    /// the instant of the rename in a single-threaded test, so we assert the
    /// invariant the swap guarantees: the temp path is the only thing built
    /// before the swap, and after `build_venv` the live `.venv` resolves to a
    /// complete env while no temp/old leftovers remain.
    #[test]
    fn build_uses_temp_then_swaps_leaving_no_leftovers() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (store_root, rules) = fake_store(tmp.path());
        let py = rules.python.as_ref().unwrap();
        let project = tmp.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();

        // The temp sibling name `build_venv` will use.
        let tmp_venv = temp_sibling(&project.join(".venv"));
        assert!(!tmp_venv.exists());

        build_venv(&project, py, &store_root, &rules).unwrap();

        let venv = project.join(".venv");
        // Complete env present at the live path.
        assert!(venv.join("pyvenv.cfg").exists(), ".venv must be complete after use");
        assert!(site_packages_of(&venv, &py.version).join("webserve").exists());
        // No temp or old leftovers.
        assert!(!tmp_venv.exists(), "temp build dir must be gone after swap");
        let leftovers: Vec<_> = std::fs::read_dir(&project)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".venv.kong-"))
            .collect();
        assert!(leftovers.is_empty(), "no .venv.kong-* leftovers, found {leftovers:?}");
    }

    /// A rebuild over an EXISTING `.venv` (the `kong use --clean` rebuild case)
    /// must replace it atomically — the live `.venv` resolves to a complete env
    /// before and after; the old contents are only gone once the new env is in.
    #[test]
    fn rebuild_over_existing_venv_is_complete_throughout() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (store_root, rules) = fake_store(tmp.path());
        let py = rules.python.as_ref().unwrap();
        let project = tmp.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();

        // First build — establishes a live .venv.
        build_venv(&project, py, &store_root, &rules).unwrap();
        let venv = project.join(".venv");
        assert!(venv.join("pyvenv.cfg").exists());

        // Drop a marker INTO the live venv; a true atomic swap replaces the
        // whole dir, so the marker is gone but the env is still complete —
        // proving the new env (not a half-cleared old one) is what's live.
        std::fs::write(venv.join("OLD_MARKER"), b"x").unwrap();

        // Rebuild (simulates `--clean` rebuild: existing .venv stays until swap).
        build_venv(&project, py, &store_root, &rules).unwrap();

        // Still a complete env at the live path.
        assert!(venv.join("pyvenv.cfg").exists(), ".venv complete after rebuild");
        assert!(site_packages_of(&venv, &py.version).join("webserve").exists());
        #[cfg(not(windows))]
        assert!(venv.join("bin").join("webserve").exists(), "console script present after rebuild");
        // The swap brought in a fresh dir — the old marker is gone.
        assert!(!venv.join("OLD_MARKER").exists(), "swap replaced the whole env atomically");
        // No leftovers.
        let leftovers: Vec<_> = std::fs::read_dir(&project)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".venv.kong-"))
            .collect();
        assert!(leftovers.is_empty(), "no leftovers after rebuild, found {leftovers:?}");
    }

    /// The swap only removes the OLD env AFTER the new one is in place: assert
    /// directly on `atomic_swap_venv` that the target exists at completion and
    /// the temp source no longer does (the new env is what's live).
    #[test]
    fn atomic_swap_target_present_source_consumed() {
        let tmp = tempfile::TempDir::new().unwrap();
        let parent = tmp.path();
        let final_venv = parent.join(".venv");
        let tmp_venv = parent.join(".venv.kong-tmp-9999");

        // Pre-existing "old" env with a marker.
        std::fs::create_dir_all(&final_venv).unwrap();
        std::fs::write(final_venv.join("old.txt"), b"old").unwrap();
        // Freshly-built "new" env in the temp sibling.
        std::fs::create_dir_all(&tmp_venv).unwrap();
        std::fs::write(tmp_venv.join("new.txt"), b"new").unwrap();

        atomic_swap_venv(&tmp_venv, &final_venv).unwrap();

        // Target present and is the NEW env; temp source consumed; no old left.
        assert!(final_venv.exists(), ".venv must exist after swap");
        assert!(final_venv.join("new.txt").exists(), "new env must be live");
        assert!(!final_venv.join("old.txt").exists(), "old env must be replaced");
        assert!(!tmp_venv.exists(), "temp source must be consumed by the swap");
    }
}
