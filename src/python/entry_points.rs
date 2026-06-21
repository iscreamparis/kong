//! Generate `[console_scripts]` launchers for a kong-built Python venv.
//!
//! pip / the `venv` module create a small launcher in `bin/` (or `Scripts\`)
//! for every `[console_scripts]` entry point declared by an installed
//! distribution (e.g. `uvicorn`, `gunicorn`, `alembic`, `pip`). KONG's venv
//! builder only places `bin/python`, so those launchers were missing and
//! `ExecStart=.../.venv/bin/uvicorn` failed with `203/EXEC`.
//!
//! This module is fully **general**: it discovers entry points by scanning
//! every `*.dist-info/entry_points.txt` in site-packages — no package names
//! are hardcoded. It is invoked from BOTH the store-linked `.venv`
//! (`kong use`) AND the solidified `.venv` (`kong solidify`); the only
//! difference is which `bin/python` the launcher's shebang points at.

use std::path::Path;

use anyhow::{Context, Result};
use tracing::debug;

/// A `[console_scripts]` entry point: `name = module[:attr]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsoleScript {
    /// The launcher file name (e.g. `uvicorn`).
    pub name: String,
    /// The import target module (e.g. `uvicorn.main`).
    pub module: String,
    /// The dotted attribute/callable to invoke (e.g. `main`). `None` means the
    /// module itself is the callable target — we then call its `main`.
    pub attr: Option<String>,
}

/// Scan all `*.dist-info/entry_points.txt` files in `site_packages` and
/// generate a launcher in `bin_dir` for every `[console_scripts]` entry,
/// each using `venv_python` on its shebang.
///
/// On Windows, a `<name>.exe` cannot be produced without the distlib launcher
/// stub, so we additionally write a `<name>-script.py` + a `<name>.bat`
/// wrapper that invokes the venv python — runnable from a shell.
///
/// Returns the number of launchers written.
pub fn generate_console_scripts(
    site_packages: &Path,
    bin_dir: &Path,
    venv_python: &Path,
) -> Result<usize> {
    if !site_packages.is_dir() {
        return Ok(0);
    }
    std::fs::create_dir_all(bin_dir)
        .with_context(|| format!("failed to create bin dir {}", bin_dir.display()))?;

    let mut written = 0usize;
    for entry in std::fs::read_dir(site_packages)
        .with_context(|| format!("failed to read site-packages {}", site_packages.display()))?
    {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.ends_with(".dist-info") || !entry.path().is_dir() {
            continue;
        }
        let ep_path = entry.path().join("entry_points.txt");
        let content = match std::fs::read_to_string(&ep_path) {
            Ok(c) => c,
            Err(_) => continue, // no entry_points.txt → nothing to do
        };
        for script in parse_console_scripts(&content) {
            write_launcher(bin_dir, venv_python, &script)?;
            debug!(script = %script.name, "Generated console-script launcher");
            written += 1;
        }
    }
    Ok(written)
}

/// Parse the `[console_scripts]` section of an `entry_points.txt`.
///
/// Format (INI-like):
/// ```ini
/// [console_scripts]
/// uvicorn = uvicorn.main:main
/// alembic = alembic.config:main
/// somecli = somepkg.cli
/// ```
/// `gui_scripts` are treated identically (both produce runnable launchers).
pub fn parse_console_scripts(content: &str) -> Vec<ConsoleScript> {
    let mut scripts = Vec::new();
    let mut in_section = false;

    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            let sect = line[1..line.len() - 1].trim();
            in_section = sect.eq_ignore_ascii_case("console_scripts")
                || sect.eq_ignore_ascii_case("gui_scripts");
            continue;
        }
        if !in_section {
            continue;
        }
        let Some((name, target)) = line.split_once('=') else {
            continue;
        };
        let name = name.trim();
        // Strip any `[extras]` suffix on the target, e.g. `pkg.cli:main [foo]`.
        let target = target.trim();
        let target = target.split_whitespace().next().unwrap_or(target);
        if name.is_empty() || target.is_empty() {
            continue;
        }
        let (module, attr) = match target.split_once(':') {
            Some((m, a)) => (m.trim().to_string(), Some(a.trim().to_string())),
            None => (target.to_string(), None),
        };
        if module.is_empty() {
            continue;
        }
        scripts.push(ConsoleScript {
            name: name.to_string(),
            module,
            attr,
        });
    }
    scripts
}

/// Build the launcher Python source for a console script, matching the layout
/// pip/`venv` emit.
///
/// `module:attr` → `from module import attr; ... attr()`.
/// `module` only → import the module and call its `main`.
pub fn launcher_source(python_exe: &Path, script: &ConsoleScript) -> String {
    let shebang = python_exe.to_string_lossy().replace('\\', "/");
    let (import_line, call_target) = match &script.attr {
        Some(attr) => {
            // Support dotted attrs (e.g. `cli.main`): import the top symbol,
            // call the full dotted path — mirrors pip's generated launchers.
            let top = attr.split('.').next().unwrap_or(attr);
            (
                format!("from {} import {}", script.module, top),
                attr.clone(),
            )
        }
        None => (
            format!("import {}", script.module),
            format!("{}.main", script.module),
        ),
    };

    format!(
        "#!{shebang}\n\
         # -*- coding: utf-8 -*-\n\
         import re\n\
         import sys\n\
         {import_line}\n\
         if __name__ == '__main__':\n\
         \x20   sys.argv[0] = re.sub(r'(-script\\.pyw?|\\.exe)?$', '', sys.argv[0])\n\
         \x20   sys.exit({call_target}())\n"
    )
}

/// Write the launcher file(s) for one console script into `bin_dir`.
fn write_launcher(bin_dir: &Path, venv_python: &Path, script: &ConsoleScript) -> Result<()> {
    let source = launcher_source(venv_python, script);

    #[cfg(not(windows))]
    {
        let path = bin_dir.join(&script.name);
        std::fs::write(&path, source.as_bytes())
            .with_context(|| format!("failed to write launcher {}", path.display()))?;
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&path)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms)?;
    }

    #[cfg(windows)]
    {
        // No distlib stub available → emit a `<name>-script.py` carrying the
        // launcher source plus a `<name>.bat` shell wrapper that runs it with
        // the venv python. (A true `<name>.exe` would require bundling the
        // launcher binary; this pair is runnable from any shell.)
        let script_py = bin_dir.join(format!("{}-script.py", script.name));
        std::fs::write(&script_py, source.as_bytes())
            .with_context(|| format!("failed to write launcher {}", script_py.display()))?;

        let py = venv_python.to_string_lossy().replace('/', "\\");
        let bat = format!(
            "@echo off\r\n\"{py}\" \"%~dp0{}-script.py\" %*\r\n",
            script.name
        );
        let bat_path = bin_dir.join(format!("{}.bat", script.name));
        std::fs::write(&bat_path, bat.as_bytes())
            .with_context(|| format!("failed to write launcher {}", bat_path.display()))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_module_attr_form() {
        let txt = "[console_scripts]\nuvicorn = uvicorn.main:main\nalembic = alembic.config:main\n";
        let scripts = parse_console_scripts(txt);
        assert_eq!(scripts.len(), 2);
        assert_eq!(scripts[0].name, "uvicorn");
        assert_eq!(scripts[0].module, "uvicorn.main");
        assert_eq!(scripts[0].attr.as_deref(), Some("main"));
        assert_eq!(scripts[1].name, "alembic");
        assert_eq!(scripts[1].module, "alembic.config");
        assert_eq!(scripts[1].attr.as_deref(), Some("main"));
    }

    #[test]
    fn parse_module_only_form() {
        let txt = "[console_scripts]\nsomecli = somepkg.cli\n";
        let scripts = parse_console_scripts(txt);
        assert_eq!(scripts.len(), 1);
        assert_eq!(scripts[0].name, "somecli");
        assert_eq!(scripts[0].module, "somepkg.cli");
        assert_eq!(scripts[0].attr, None);
    }

    #[test]
    fn ignores_other_sections_and_extras() {
        let txt = "[metadata]\nfoo = bar\n\
                   [console_scripts]\npip = pip._internal.cli.main:main [extra]\n\
                   [gui_scripts]\nmygui = mygui.app:run\n\
                   [other]\nbaz = qux:y\n";
        let scripts = parse_console_scripts(txt);
        // console_scripts (pip) + gui_scripts (mygui) are both kept; [other] is not.
        assert_eq!(scripts.len(), 2);
        assert_eq!(scripts[0].name, "pip");
        assert_eq!(scripts[0].module, "pip._internal.cli.main");
        assert_eq!(scripts[0].attr.as_deref(), Some("main"));
        assert_eq!(scripts[1].name, "mygui");
        assert_eq!(scripts[1].attr.as_deref(), Some("run"));
    }

    #[test]
    fn launcher_has_shebang_and_call() {
        let script = ConsoleScript {
            name: "uvicorn".into(),
            module: "uvicorn.main".into(),
            attr: Some("main".into()),
        };
        let py = Path::new("/proj/.venv/bin/python");
        let src = launcher_source(py, &script);
        assert!(src.starts_with("#!/proj/.venv/bin/python\n"), "shebang: {src}");
        assert!(src.contains("from uvicorn.main import main"));
        assert!(src.contains("sys.exit(main())"));
    }

    #[test]
    fn launcher_module_only_calls_main() {
        let script = ConsoleScript {
            name: "somecli".into(),
            module: "somepkg.cli".into(),
            attr: None,
        };
        let src = launcher_source(Path::new("/p/.venv/bin/python"), &script);
        assert!(src.contains("import somepkg.cli"));
        assert!(src.contains("sys.exit(somepkg.cli.main())"));
    }

    #[test]
    fn generate_writes_executable_with_venv_shebang() {
        let tmp = tempfile::TempDir::new().unwrap();
        let site = tmp.path().join("site-packages");
        let di = site.join("uvicorn-0.30.0.dist-info");
        std::fs::create_dir_all(&di).unwrap();
        std::fs::write(
            di.join("entry_points.txt"),
            "[console_scripts]\nuvicorn = uvicorn.main:main\n",
        )
        .unwrap();
        // A dist-info with no entry_points.txt must be skipped silently.
        std::fs::create_dir_all(site.join("plain-1.0.dist-info")).unwrap();

        let bin = tmp.path().join("bin");
        let py = bin.join("python");
        let n = generate_console_scripts(&site, &bin, &py).unwrap();
        assert_eq!(n, 1);

        #[cfg(not(windows))]
        {
            let launcher = bin.join("uvicorn");
            assert!(launcher.exists());
            let body = std::fs::read_to_string(&launcher).unwrap();
            let expected_shebang = format!("#!{}\n", py.to_string_lossy().replace('\\', "/"));
            assert!(body.starts_with(&expected_shebang), "shebang: {body}");
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&launcher).unwrap().permissions().mode();
            assert!(mode & 0o111 != 0, "launcher should be executable, mode={mode:o}");
        }
        #[cfg(windows)]
        {
            assert!(bin.join("uvicorn-script.py").exists());
            assert!(bin.join("uvicorn.bat").exists());
            let body = std::fs::read_to_string(bin.join("uvicorn-script.py")).unwrap();
            assert!(body.contains("from uvicorn.main import main"));
        }
    }
}
