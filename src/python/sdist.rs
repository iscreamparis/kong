//! Install a pure-Python **sdist** (source distribution) into the kong store.
//!
//! kong's store layout for a Python package is a flat importable tree that
//! `link::link_package` materializes into a venv's `site-packages` verbatim:
//! top-level modules (`foo.py`), top-level packages (`foo/__init__.py`), and
//! an optional `*.dist-info/` directory (read by `entry_points` for console
//! scripts and by `config::read_transitive_from_store` for cached transitive
//! deps). A **wheel** already ships exactly this layout, so kong just unzips it.
//!
//! An **sdist** does NOT: a `.tar.gz` sdist extracts as
//! `{name}-{version}/setup.py`, `{name}-{version}/{pkg}/…` — the importable
//! modules live one level down under a versioned root, so a plain extract into
//! the store leaves nothing importable at the store root (the bug this fixes).
//!
//! This module extracts the sdist to a temp dir, locates the importable
//! top-level modules/packages (handling the flat layout AND the `src/` layout),
//! copies them to the store root so the store matches what a wheel would have
//! produced, and synthesizes a minimal `{name}-{version}.dist-info/METADATA`
//! (carrying Name/Version + the `Requires-Dist` lines) so cached transitive-dep
//! reads keep working.
//!
//! It is **pure-Python only by design**: the sdist-only universe is legacy
//! pure-Python packages (compiled packages ship wheels). A sdist that carries C
//! sources / an `ext_modules` build cannot be built without a toolchain — we
//! detect that and FAIL LOUDLY rather than half-install. `setup.py` is NEVER
//! executed.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use tracing::{debug, info, warn};

/// File extensions that signal a compiled (non-pure-Python) sdist. If any source
/// file with one of these extensions is present, kong cannot build it without a
/// C/C++/Cython toolchain, so we refuse loudly instead of shipping a broken,
/// import-incomplete package.
const COMPILED_SOURCE_EXTS: &[&str] = &["c", "cpp", "cc", "cxx", "pyx", "pxd"];

/// Install a pure-Python sdist archive (already downloaded to `archive_path`)
/// into `store_path` (the package's store directory, e.g.
/// `.../python/libs/{name}-{version}-{py_tag}-{platform}`).
///
/// `name`/`version` drive the synthesized dist-info and error messages.
/// `requires_dist` is the parent's `Requires-Dist` list (from PyPI metadata),
/// written into the synthesized METADATA so a later cache hit can still resolve
/// transitive deps from the store.
pub fn install_sdist(
    archive_path: &Path,
    store_path: &Path,
    name: &str,
    version: &str,
    requires_dist: &[String],
) -> Result<()> {
    info!(pkg = %name, ver = %version, "Installing pure-Python sdist");

    // 1. Extract the sdist to a temp dir.
    let tmp = tempfile::TempDir::new().context("failed to create temp dir for sdist extract")?;
    crate::extract::extract(archive_path, tmp.path())
        .with_context(|| format!("failed to extract sdist for {name}=={version}"))?;

    // 2. Locate the single `{name}-{version}/` source root. Most sdists have a
    //    single top-level directory; if there are several, prefer one that looks
    //    like the project root (contains setup.py / setup.cfg / pyproject.toml),
    //    else the lone directory.
    let root = locate_sdist_root(tmp.path())
        .with_context(|| format!("could not locate sdist source root for {name}=={version}"))?;
    debug!(root = %root.display(), "Located sdist source root");

    // 3. Refuse compiled sdists loudly — kong has no build toolchain.
    if let Some(offender) = find_compiled_source(&root) {
        bail!(
            "package '{name}=={version}' is a compiled sdist (found '{}') — kong installs \
             pure-Python sdists only and has no C/build toolchain. Use a version that \
             publishes a wheel for this platform, or install a build toolchain.",
            offender.display()
        );
    }

    // 4. Determine the import base: a `src/` layout puts modules under
    //    `{root}/src/`, otherwise they sit directly under `{root}`.
    let import_base = pick_import_base(&root);
    debug!(base = %import_base.display(), "Import base for sdist");

    // 5. Collect the top-level importable modules/packages.
    let items = collect_importable(&import_base);
    if items.is_empty() {
        bail!(
            "package '{name}=={version}' sdist has no importable top-level module or package \
             under {} — kong cannot install it (not a simple pure-Python layout)",
            import_base.display()
        );
    }

    // 6. Copy them into the store root so the store matches a wheel extract.
    std::fs::create_dir_all(store_path)
        .with_context(|| format!("failed to create store dir {}", store_path.display()))?;
    for item in &items {
        let dst = store_path.join(item.file_name().context("import item has no file name")?);
        copy_into_store(item, &dst)
            .with_context(|| format!("failed to copy {} into store", item.display()))?;
        debug!(item = %item.display(), "Copied importable item into store");
    }

    // 7. Synthesize a minimal dist-info so console-script discovery and cached
    //    transitive-dep reads (config::read_transitive_from_store) keep working.
    write_dist_info(store_path, name, version, requires_dist)
        .with_context(|| format!("failed to write dist-info for {name}=={version}"))?;

    info!(
        pkg = %name,
        ver = %version,
        items = items.len(),
        "Pure-Python sdist installed into store"
    );
    Ok(())
}

/// Find the source root inside an extracted sdist temp dir.
///
/// A canonical sdist has exactly one top-level directory (`{name}-{version}/`).
/// We tolerate stray files at the top level (some sdists include a `PKG-INFO`
/// sibling). If multiple directories exist, prefer the one containing a project
/// marker (`setup.py`/`setup.cfg`/`pyproject.toml`); otherwise fall back to the
/// only directory, or the temp dir itself if the modules sit at the top already.
fn locate_sdist_root(extract_dir: &Path) -> Result<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(extract_dir)
        .with_context(|| format!("failed to read extracted sdist {}", extract_dir.display()))?
    {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            dirs.push(entry.path());
        }
    }

    // Prefer a directory that carries a project marker.
    if let Some(marked) = dirs.iter().find(|d| has_project_marker(d)) {
        return Ok(marked.clone());
    }
    match dirs.len() {
        1 => Ok(dirs.into_iter().next().unwrap()),
        0 => {
            // No subdir — modules may already be at the top level.
            Ok(extract_dir.to_path_buf())
        }
        _ => {
            // Ambiguous: no marker and several dirs. Pick the first deterministically
            // (sorted) so the choice is stable, and warn.
            dirs.sort();
            warn!(
                "sdist has multiple top-level dirs and no project marker; using {}",
                dirs[0].display()
            );
            Ok(dirs.into_iter().next().unwrap())
        }
    }
}

/// Does this directory contain a Python project marker file?
fn has_project_marker(dir: &Path) -> bool {
    ["setup.py", "setup.cfg", "pyproject.toml"]
        .iter()
        .any(|m| dir.join(m).is_file())
}

/// Choose the directory that holds the importable modules: a `src/` layout if
/// present (and non-empty), otherwise the root itself.
fn pick_import_base(root: &Path) -> PathBuf {
    let src = root.join("src");
    if src.is_dir() {
        // Only treat it as a src-layout if `src/` actually contains importable
        // items (a package dir or a top-level module), not just data files.
        if !collect_importable(&src).is_empty() {
            return src;
        }
    }
    root.to_path_buf()
}

/// Collect the top-level importable items directly under `base`:
///   - directories that contain an `__init__.py` (regular packages), and
///   - top-level `*.py` modules (excluding `setup.py`, which is build tooling).
///
/// This is the robust heuristic the task prefers over executing setup.py:
/// it captures the overwhelmingly common pure-Python layouts without parsing
/// metadata.
fn collect_importable(base: &Path) -> Vec<PathBuf> {
    let mut items = Vec::new();
    let rd = match std::fs::read_dir(base) {
        Ok(rd) => rd,
        Err(_) => return items,
    };
    for entry in rd.flatten() {
        let path = entry.path();
        let ft = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        if ft.is_dir() {
            if path.join("__init__.py").is_file() {
                items.push(path);
            }
        } else if ft.is_file() {
            let fname = entry.file_name();
            let fname = fname.to_string_lossy();
            // Build/tooling scripts that live at the root are not importable
            // package modules — skip them.
            if fname == "setup.py" || fname == "conftest.py" {
                continue;
            }
            if fname.ends_with(".py") {
                items.push(path);
            }
        }
    }
    items.sort();
    items
}

/// Walk the source root looking for a compiled-language source file. Returns the
/// first offender found (relative-ish path for the error message), or None for a
/// pure-Python sdist.
fn find_compiled_source(root: &Path) -> Option<PathBuf> {
    for entry in walkdir::WalkDir::new(root).into_iter().flatten() {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            let ext = ext.to_ascii_lowercase();
            if COMPILED_SOURCE_EXTS.contains(&ext.as_str()) {
                return Some(path.to_path_buf());
            }
        }
    }
    None
}

/// Copy a file or directory tree from `src` into `dst` (recursive for dirs).
fn copy_into_store(src: &Path, dst: &Path) -> Result<()> {
    if src.is_dir() {
        copy_dir_recursive(src, dst)
    } else {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(src, dst)
            .with_context(|| format!("copy {} → {}", src.display(), dst.display()))?;
        Ok(())
    }
}

/// Recursively copy a directory tree.
fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)
                .with_context(|| format!("copy {} → {}", from.display(), to.display()))?;
        }
    }
    Ok(())
}

/// Write a minimal `{name}-{version}.dist-info/METADATA` into the store so that:
///   - `entry_points` console-script discovery has a dist-info to look in
///     (there is no entry_points.txt for a heuristic install — that's fine,
///     it's scanned tolerantly), and
///   - `config::read_transitive_from_store` can re-derive `Requires-Dist` on a
///     cache hit (same data PyPI returned, persisted next to the package).
fn write_dist_info(store_path: &Path, name: &str, version: &str, requires_dist: &[String]) -> Result<()> {
    let dist_info = store_path.join(format!("{name}-{version}.dist-info"));
    std::fs::create_dir_all(&dist_info)
        .with_context(|| format!("failed to create {}", dist_info.display()))?;

    let mut metadata = format!(
        "Metadata-Version: 2.1\nName: {name}\nVersion: {version}\n"
    );
    for req in requires_dist {
        metadata.push_str("Requires-Dist: ");
        metadata.push_str(req.trim());
        metadata.push('\n');
    }
    std::fs::write(dist_info.join("METADATA"), metadata)
        .with_context(|| format!("failed to write METADATA in {}", dist_info.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Build a synthetic pure-Python sdist tarball with the canonical
    /// `{name}-{version}/` root containing a top-level module and a package dir.
    /// Returns (tempdir holding the tarball, tarball path).
    fn make_sdist_tarball(
        layout: &[(&str, &str)],
        archive_name: &str,
    ) -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::TempDir::new().unwrap();
        // Stage the file tree, then tar+gzip it.
        let stage = tmp.path().join("stage");
        for (rel, contents) in layout {
            let p = stage.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, contents).unwrap();
        }
        let archive_path = tmp.path().join(archive_name);
        let tar_gz = fs::File::create(&archive_path).unwrap();
        let enc = flate2::write::GzEncoder::new(tar_gz, flate2::Compression::default());
        let mut builder = tar::Builder::new(enc);
        // Append the staged tree under its relative paths.
        builder.append_dir_all(".", &stage).unwrap();
        builder.into_inner().unwrap().finish().unwrap();
        (tmp, archive_path)
    }

    #[test]
    fn installs_flat_pure_python_layout() {
        // {name}-{version}/ with a top-level module AND a package dir.
        let (_tmp, archive) = make_sdist_tarball(
            &[
                ("sgmllib3k-1.0.0/setup.py", "from setuptools import setup\nsetup()\n"),
                ("sgmllib3k-1.0.0/PKG-INFO", "Metadata\n"),
                ("sgmllib3k-1.0.0/sgmllib.py", "# top-level module\nclass SGMLParser: pass\n"),
                ("sgmllib3k-1.0.0/helper/__init__.py", "# a package\n"),
                ("sgmllib3k-1.0.0/helper/util.py", "X = 1\n"),
            ],
            "sgmllib3k-1.0.0.tar.gz",
        );

        let store = tempfile::TempDir::new().unwrap();
        let store_path = store.path().join("python/libs/sgmllib3k-1.0.0-cp310-any");
        install_sdist(&archive, &store_path, "sgmllib3k", "1.0.0", &[]).unwrap();

        // The importable items must land at the STORE ROOT (not under a versioned dir).
        assert!(store_path.join("sgmllib.py").is_file(), "top-level module at store root");
        assert!(store_path.join("helper/__init__.py").is_file(), "package at store root");
        assert!(store_path.join("helper/util.py").is_file());
        // setup.py / PKG-INFO are NOT importable — must not be copied.
        assert!(!store_path.join("setup.py").exists(), "setup.py must not be installed");
        assert!(!store_path.join("PKG-INFO").exists(), "PKG-INFO must not be installed");
        // dist-info synthesized.
        assert!(
            store_path.join("sgmllib3k-1.0.0.dist-info/METADATA").is_file(),
            "synthesized METADATA"
        );
        let md = fs::read_to_string(store_path.join("sgmllib3k-1.0.0.dist-info/METADATA")).unwrap();
        assert!(md.contains("Name: sgmllib3k"));
        assert!(md.contains("Version: 1.0.0"));
    }

    #[test]
    fn installs_src_layout() {
        // {name}-{version}/src/{pkg}/__init__.py — modules under src/.
        let (_tmp, archive) = make_sdist_tarball(
            &[
                ("mypkg-2.3.0/setup.cfg", "[metadata]\nname = mypkg\n"),
                ("mypkg-2.3.0/pyproject.toml", "[build-system]\n"),
                ("mypkg-2.3.0/src/mypkg/__init__.py", "VERSION = '2.3.0'\n"),
                ("mypkg-2.3.0/src/mypkg/core.py", "def go(): return 42\n"),
                ("mypkg-2.3.0/tests/test_core.py", "# tests, not shipped\n"),
            ],
            "mypkg-2.3.0.tar.gz",
        );

        let store = tempfile::TempDir::new().unwrap();
        let store_path = store.path().join("python/libs/mypkg-2.3.0-cp310-any");
        install_sdist(&archive, &store_path, "mypkg", "2.3.0", &["requests>=2".to_string()]).unwrap();

        // src/ layout: the PACKAGE (not the `src` dir) lands at the store root.
        assert!(store_path.join("mypkg/__init__.py").is_file(), "package from src/ at store root");
        assert!(store_path.join("mypkg/core.py").is_file());
        assert!(!store_path.join("src").exists(), "the src/ wrapper must not be copied");
        assert!(!store_path.join("tests").exists(), "tests dir must not be installed");
        // Requires-Dist persisted for cache-hit transitive resolution.
        let md = fs::read_to_string(store_path.join("mypkg-2.3.0.dist-info/METADATA")).unwrap();
        assert!(md.contains("Requires-Dist: requests>=2"), "metadata: {md}");
    }

    #[test]
    fn rejects_compiled_sdist_loudly() {
        // A sdist carrying C sources must fail with a clear, named error.
        let (_tmp, archive) = make_sdist_tarball(
            &[
                ("cext-1.0.0/setup.py", "setup()\n"),
                ("cext-1.0.0/cext/__init__.py", "\n"),
                ("cext-1.0.0/cext/_speed.c", "int main(){return 0;}\n"),
            ],
            "cext-1.0.0.tar.gz",
        );

        let store = tempfile::TempDir::new().unwrap();
        let store_path = store.path().join("python/libs/cext-1.0.0-cp310-any");
        let err = install_sdist(&archive, &store_path, "cext", "1.0.0", &[]).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("cext==1.0.0"), "error names the package: {msg}");
        assert!(msg.contains("compiled sdist"), "error explains why: {msg}");
    }

    #[test]
    fn collect_importable_skips_setup_and_non_py() {
        let tmp = tempfile::TempDir::new().unwrap();
        let base = tmp.path();
        fs::write(base.join("setup.py"), "").unwrap();
        fs::write(base.join("conftest.py"), "").unwrap();
        fs::write(base.join("README.md"), "").unwrap();
        fs::write(base.join("mod.py"), "").unwrap();
        fs::create_dir_all(base.join("pkg")).unwrap();
        fs::write(base.join("pkg/__init__.py"), "").unwrap();
        // A directory without __init__.py is not an importable (regular) package.
        fs::create_dir_all(base.join("data")).unwrap();
        fs::write(base.join("data/file.txt"), "").unwrap();

        let items: Vec<String> = collect_importable(base)
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(items.contains(&"mod.py".to_string()));
        assert!(items.contains(&"pkg".to_string()));
        assert!(!items.contains(&"setup.py".to_string()));
        assert!(!items.contains(&"conftest.py".to_string()));
        assert!(!items.contains(&"README.md".to_string()));
        assert!(!items.contains(&"data".to_string()));
    }
}
