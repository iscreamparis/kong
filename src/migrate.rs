//! `kong import`, `kong solidify`, `kong eject` — project migration commands.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tracing::{debug, info, warn};
use walkdir::WalkDir;

use crate::config::{self, KongRules};
use crate::link;
use crate::store;

// ── kong import ─────────────────────────────────────────────────────────────

/// Convert an existing project (with local `.venv`, `node_modules`) to the
/// KONG way.  Moves already-installed packages into the global store instead
/// of re-downloading them, then replaces the local copies with links.
pub fn import_project(project_dir: &Path) -> Result<()> {
    let store_root = store::store_root()?;
    let project_name = project_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "project".to_string());

    info!(project = %project_name, "Importing existing project into KONG");

    // ── 1. Pre-populate the store from local environments ───────────────
    //    This ensures generate_rules will find packages already in the store
    //    and skip downloading them from the internet.
    let py_imported = prepopulate_python_store(project_dir, &store_root)?;
    let node_imported = prepopulate_node_store(project_dir, &store_root)?;
    if py_imported > 0 {
        info!(count = py_imported, "Python packages moved to store");
    }
    if node_imported > 0 {
        info!(count = node_imported, "Node packages moved to store");
    }

    // ── 2. Remove local environments before generate_rules ──────────────
    //    generate_rules doesn't need .venv or node_modules — it reads
    //    manifests (requirements.txt, package.json, Cargo.toml) directly.
    let venv = project_dir.join(".venv");
    if venv.exists() && !venv.is_symlink() {
        info!("Removing local .venv (packages are now in the store)");
        link::remove_dir_all_robust(&venv)?;
    }
    let nm = project_dir.join("node_modules");
    if nm.exists() && !nm.is_symlink() {
        info!("Removing local node_modules (packages are now in the store)");
        link::remove_dir_all_robust(&nm)?;
    }

    // ── 3. Generate kong.rules (reuses full pipeline: manifests, brew,
    //    scripts, services — but skips downloads for pre-populated packages)
    info!("Generating kong.rules");
    let rules = config::generate_rules(project_dir, false)?;
    let rules_path = project_dir.join("kong.rules");
    config::write_rules(&rules, &rules_path)?;
    info!(path = %rules_path.display(), "kong.rules written");

    // ── 4. Set up RULEZ-based environments (like `kong use`) ────────────
    let env_dir = store::rulez_dir(&project_name)?;

    if let Some(ref py) = rules.python {
        crate::python::venv::build_venv(&env_dir, py, &store_root, &rules)?;
        info!("  ✓ Python .venv created");
    }
    if let Some(ref node) = rules.node {
        crate::node::modules::build_node_modules(&env_dir, node, &store_root)?;
        info!("  ✓ Node node_modules created");
    }
    if let Some(ref rs) = rules.rust {
        crate::rust_eco::source::configure_source_replacement(&env_dir, rs, &store_root, &rules)?;
        info!("  ✓ Rust source replacement configured");
    }
    if let Some(ref brew) = rules.brew {
        crate::brew::client::ensure_bottles_in_store(brew, &store_root)?;
    }
    link::create_project_junctions(project_dir, &env_dir, &rules)?;
    info!("  ✓ Project junctions created");

    store::register_project(&project_name, project_dir);
    info!("✓ Import complete for {}", project_name);
    Ok(())
}

/// Scan local `.venv/` site-packages and copy each package into the global
/// store.  Returns the number of packages imported.
fn prepopulate_python_store(project_dir: &Path, store_root: &Path) -> Result<usize> {
    let venv = project_dir.join(".venv");
    if !venv.exists() || venv.is_symlink() {
        return Ok(0);
    }

    let site_packages = match find_site_packages(&venv) {
        Ok(sp) => sp,
        Err(_) => return Ok(0),
    };

    // Detect Python version from pyvenv.cfg → compute tags
    let py_version = read_pyvenv_version(&venv).unwrap_or_else(|| "3.12.0".to_string());
    let py_tag = short_python_tag(&py_version);
    let platform = config::platform_tag();

    let mut count = 0;
    for entry in std::fs::read_dir(&site_packages)? {
        let entry = entry?;
        let name_os = entry.file_name();
        let name_str = name_os.to_string_lossy();
        if !name_str.ends_with(".dist-info") || !entry.path().is_dir() {
            continue;
        }

        // Read METADATA for name and version
        let metadata_path = entry.path().join("METADATA");
        let (pkg_name, pkg_version) = match read_dist_info_metadata(&metadata_path) {
            Some(nv) => nv,
            None => continue,
        };

        // Skip pip/setuptools/wheel — these are venv internals
        let lower = pkg_name.to_lowercase();
        if lower == "pip" || lower == "setuptools" || lower == "wheel" {
            continue;
        }

        let store_path_rel = format!(
            "python/libs/{}-{}-{}-{}",
            pkg_name, pkg_version, py_tag, platform
        );
        let full_store_path = store_root.join(&store_path_rel);

        if full_store_path.exists() {
            debug!(pkg = %pkg_name, "Already in store, skipping");
            continue;
        }

        // Read RECORD for the file list
        let record_path = entry.path().join("RECORD");
        let record_files = read_record_files(&record_path);

        info!(pkg = %pkg_name, ver = %pkg_version, "Importing to store");
        std::fs::create_dir_all(&full_store_path)?;

        if record_files.is_empty() {
            copy_dist_info_and_package(
                &site_packages,
                &entry.path(),
                &pkg_name,
                &full_store_path,
            )?;
        } else {
            for rel_path in &record_files {
                let src = site_packages.join(rel_path);
                let dst = full_store_path.join(rel_path);
                if src.exists() {
                    if let Some(parent) = dst.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    if src.is_dir() {
                        std::fs::create_dir_all(&dst)?;
                    } else {
                        std::fs::copy(&src, &dst)?;
                    }
                }
            }
        }

        store::write_verified_marker(&full_store_path, "imported")?;
        count += 1;
    }

    Ok(count)
}

/// Scan local `node_modules/` and copy each package into the global store.
/// Returns the number of packages imported.
fn prepopulate_node_store(project_dir: &Path, store_root: &Path) -> Result<usize> {
    let nm = project_dir.join("node_modules");
    if !nm.exists() || nm.is_symlink() {
        return Ok(0);
    }

    let mut count = 0;

    for entry in std::fs::read_dir(&nm)? {
        let entry = entry?;
        let name_str = entry.file_name().to_string_lossy().to_string();

        // Skip dotfiles, .bin, lockfiles
        if name_str.starts_with('.') || name_str == ".bin" || name_str == ".package-lock.json" {
            continue;
        }

        if name_str.starts_with('@') {
            // Scoped package — one level deeper
            if !entry.path().is_dir() {
                continue;
            }
            for sub in std::fs::read_dir(entry.path())? {
                let sub = sub?;
                let scoped_name = format!("{}/{}", name_str, sub.file_name().to_string_lossy());
                if import_single_node_package(&nm, &scoped_name, store_root)? {
                    count += 1;
                }
            }
        } else if import_single_node_package(&nm, &name_str, store_root)? {
            count += 1;
        }
    }

    Ok(count)
}

/// Copy a single Node package from node_modules/ into the store.
/// Returns true if the package was newly imported.
fn import_single_node_package(
    nm_dir: &Path,
    pkg_name: &str,
    store_root: &Path,
) -> Result<bool> {
    let pkg_dir = nm_dir.join(pkg_name);
    let pkg_json_path = pkg_dir.join("package.json");
    if !pkg_json_path.exists() {
        return Ok(false);
    }

    let content = std::fs::read_to_string(&pkg_json_path)?;
    let v: serde_json::Value = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse {}", pkg_json_path.display()))?;

    let name = v.get("name")
        .and_then(|n| n.as_str())
        .unwrap_or(pkg_name)
        .to_string();
    let version = v.get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("0.0.0")
        .to_string();

    let safe_name = name.replace('/', "+");
    let store_path_rel = format!("node/libs/{}-{}", safe_name, version);
    let full_store_path = store_root.join(&store_path_rel);

    if full_store_path.exists() {
        debug!(pkg = %name, "Already in store, skipping");
        return Ok(false);
    }

    // npm tarballs extract with a `package/` subdirectory convention;
    // replicate that layout so build_node_modules sees what it expects.
    let store_pkg_dir = full_store_path.join("package");
    info!(pkg = %name, ver = %version, "Importing to store");
    std::fs::create_dir_all(&store_pkg_dir)?;
    copy_dir_recursive(&pkg_dir, &store_pkg_dir)?;
    store::write_verified_marker(&full_store_path, "imported")?;

    Ok(true)
}

// ── kong solidify ───────────────────────────────────────────────────────────

/// Copy packages from the store into real local directories so the project
/// works without KONG.
pub fn solidify_project(project_dir: &Path) -> Result<()> {
    let rules_path = project_dir.join("kong.rules");
    let rules = config::read_rules(&rules_path)
        .context("kong.rules not found — is this a KONG-managed project?")?;
    let store_root = store::store_root()?;
    let project_name = project_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "project".to_string());

    info!(project = %project_name, "Solidifying project (copying from store to local)");

    // ── 1. Remove existing junctions/symlinks ───────────────────────────
    link::clean_project_junctions(project_dir)?;

    // ── 2. Rebuild real local environments with copied files ────────────
    if let Some(ref py) = rules.python {
        let venv = project_dir.join(".venv");
        solidify_python(&venv, py, &store_root, &rules)?;
        info!("  ✓ Python .venv solidified (real local copy)");
    }

    if let Some(ref node) = rules.node {
        let nm = project_dir.join("node_modules");
        solidify_node(&nm, node, &store_root)?;
        info!("  ✓ Node node_modules solidified (real local copy)");
    }

    info!("✓ Solidify complete for {}. Project now works without KONG.", project_name);
    Ok(())
}

/// Create a real (non-linked) .venv by copying packages from the store.
fn solidify_python(
    venv_dir: &Path,
    python: &config::PythonSection,
    store_root: &Path,
    rules: &KongRules,
) -> Result<()> {
    // Remove existing .venv (it's a symlink to RULEZ)
    remove_link_or_dir(venv_dir)?;

    // site-packages path
    #[cfg(windows)]
    let site_packages = venv_dir.join("Lib").join("site-packages");
    #[cfg(not(windows))]
    let site_packages = {
        let maj_min = major_minor(&python.version);
        venv_dir.join("lib").join(format!("python{maj_min}")).join("site-packages")
    };
    std::fs::create_dir_all(&site_packages)?;

    // pyvenv.cfg
    let kong_python = kong_python_exe(store_root, rules);
    let python_home = kong_python
        .as_ref()
        .and_then(|e| e.parent())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    std::fs::write(
        venv_dir.join("pyvenv.cfg"),
        format!(
            "home = {python_home}\ninclude-system-site-packages = false\nversion = {}\n",
            python.version
        ),
    )?;

    // bin/Scripts with python executable
    #[cfg(not(windows))]
    {
        let bin = venv_dir.join("bin");
        std::fs::create_dir_all(&bin)?;
        if let Some(ref src_exe) = kong_python {
            for name in &["python3", "python"] {
                let dst = bin.join(name);
                if !dst.exists() {
                    std::os::unix::fs::symlink(src_exe, &dst)?;
                }
            }
        }
    }
    #[cfg(windows)]
    {
        let scripts = venv_dir.join("Scripts");
        std::fs::create_dir_all(&scripts)?;
        if let Some(ref src_exe) = kong_python {
            let _ = std::fs::copy(src_exe, scripts.join("python.exe"));
        }
    }

    // Copy (not link) packages from store into site-packages
    for pkg in &python.packages {
        let src = store_root.join(&pkg.store_path);
        if !src.exists() {
            warn!(pkg = %pkg.name, "Store path missing, skipping");
            continue;
        }
        copy_dir_recursive(&src, &site_packages)?;
        debug!(pkg = %pkg.name, "Copied into .venv");
    }

    Ok(())
}

/// Create a real (non-linked) node_modules by copying packages from the store.
fn solidify_node(
    nm_dir: &Path,
    node: &config::NodeSection,
    store_root: &Path,
) -> Result<()> {
    remove_link_or_dir(nm_dir)?;
    std::fs::create_dir_all(nm_dir)?;

    for pkg in &node.packages {
        let src = store_root.join(&pkg.store_path);
        if !src.exists() {
            warn!(pkg = %pkg.name, "Store path missing, skipping");
            continue;
        }

        // npm tarballs unpack with a `package/` subdirectory
        let content_dir = if src.join("package").is_dir() {
            src.join("package")
        } else {
            src.clone()
        };

        let dst = nm_dir.join(&pkg.name);
        if pkg.name.contains('/') {
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent)?;
            }
        }

        copy_dir_recursive(&content_dir, &dst)?;
        debug!(pkg = %pkg.name, "Copied into node_modules");
    }

    Ok(())
}

// ── kong eject ──────────────────────────────────────────────────────────────

/// Remove all KONG artifacts from a project: RULEZ dir, junctions, and any
/// store packages used exclusively by this project.
pub fn eject_project(project_dir: &Path) -> Result<()> {
    let rules_path = project_dir.join("kong.rules");
    let rules = config::read_rules(&rules_path)
        .context("kong.rules not found — is this a KONG-managed project?")?;
    let store_root = store::store_root()?;
    let project_name = project_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "project".to_string());

    info!(project = %project_name, "Ejecting project from KONG");

    // ── 1. Build cross-project dep index to find exclusive deps ─────────
    let all_projects = discover_all_projects()?;
    let dep_index = build_dep_index(&all_projects);

    // ── 2. Collect store paths used only by this project ────────────────
    let mut exclusive_store_paths: Vec<String> = Vec::new();

    let all_packages = rules.python.as_ref().map(|s| &s.packages[..]).unwrap_or(&[])
        .iter()
        .chain(rules.node.as_ref().map(|s| &s.packages[..]).unwrap_or(&[]).iter())
        .chain(rules.rust.as_ref().map(|s| &s.packages[..]).unwrap_or(&[]).iter());

    for pkg in all_packages {
        let eco = ecosystem_for_store_path(&pkg.store_path);
        let key = (eco, pkg.name.clone(), pkg.version.clone());
        let user_count = dep_index.get(&key).map(|v| v.len()).unwrap_or(0);
        if user_count <= 1 {
            exclusive_store_paths.push(pkg.store_path.clone());
        }
    }

    // ── 3. Remove project junctions ─────────────────────────────────────
    link::clean_project_junctions(project_dir)?;
    info!("  ✓ Removed project junctions");

    // ── 4. Remove RULEZ directory ───────────────────────────────────────
    let env_dir = store::rulez_dir(&project_name)?;
    if env_dir.exists() {
        link::clean_environments(&env_dir)?;
        std::fs::remove_dir_all(&env_dir)
            .with_context(|| format!("failed to remove RULEZ dir: {}", env_dir.display()))?;
        info!("  ✓ Removed RULEZ directory");
    }

    // ── 5. Remove exclusive store entries ────────────────────────────────
    for sp in &exclusive_store_paths {
        let full = store_root.join(sp);
        if full.exists() {
            std::fs::remove_dir_all(&full)
                .with_context(|| format!("failed to remove store entry: {}", full.display()))?;
            debug!(path = %sp, "Removed exclusive store entry");
        }
    }
    if !exclusive_store_paths.is_empty() {
        info!("  ✓ Removed {} store entries used only by this project", exclusive_store_paths.len());
    }

    // ── 6. Remove kong.rules ────────────────────────────────────────────
    if rules_path.exists() {
        std::fs::remove_file(&rules_path)?;
        info!("  ✓ Removed kong.rules");
    }

    store::unregister_project(&project_name);
    info!("✓ Eject complete for {}", project_name);
    Ok(())
}

// ── Shared helpers ──────────────────────────────────────────────────────────

/// Discover all KONG-managed projects from the registry.
fn discover_all_projects() -> Result<Vec<(String, PathBuf)>> {
    let reg = store::read_registry();
    Ok(reg.into_iter()
        .filter(|(_, path)| path.join("kong.rules").exists())
        .collect())
}

fn ecosystem_for_store_path(store_path: &str) -> String {
    store_path.split('/').next().unwrap_or("unknown").to_string()
}

/// Build a deps → projects usage index.
fn build_dep_index(
    projects: &[(String, PathBuf)],
) -> std::collections::HashMap<(String, String, String), Vec<String>> {
    let mut index: std::collections::HashMap<(String, String, String), Vec<String>> =
        std::collections::HashMap::new();

    for (proj_name, proj_path) in projects {
        let rules = match config::read_rules(&proj_path.join("kong.rules")) {
            Ok(r) => r,
            Err(_) => continue,
        };

        if let Some(ref py) = rules.python {
            for pkg in &py.packages {
                index
                    .entry(("python".into(), pkg.name.clone(), pkg.version.clone()))
                    .or_default()
                    .push(proj_name.clone());
            }
        }
        if let Some(ref node) = rules.node {
            for pkg in &node.packages {
                index
                    .entry(("node".into(), pkg.name.clone(), pkg.version.clone()))
                    .or_default()
                    .push(proj_name.clone());
            }
        }
        if let Some(ref rs) = rules.rust {
            for pkg in &rs.packages {
                index
                    .entry(("rust".into(), pkg.name.clone(), pkg.version.clone()))
                    .or_default()
                    .push(proj_name.clone());
            }
        }
    }

    index
}

/// Remove a symlink/junction or a real directory.
fn remove_link_or_dir(path: &Path) -> Result<()> {
    if !path.exists() && !path.is_symlink() {
        return Ok(());
    }
    #[cfg(unix)]
    if path.is_symlink() {
        std::fs::remove_file(path)?;
        return Ok(());
    }
    #[cfg(windows)]
    if junction::exists(path).unwrap_or(false) {
        junction::delete(path)?;
        return Ok(());
    }
    if path.exists() {
        link::remove_dir_all_robust(path)?;
    }
    Ok(())
}

/// Find site-packages inside a .venv
fn find_site_packages(venv: &Path) -> Result<PathBuf> {
    #[cfg(not(windows))]
    {
        let lib = venv.join("lib");
        if lib.exists() {
            for entry in std::fs::read_dir(&lib)? {
                let entry = entry?;
                if entry.file_name().to_string_lossy().starts_with("python") {
                    let sp = entry.path().join("site-packages");
                    if sp.exists() {
                        return Ok(sp);
                    }
                }
            }
        }
    }
    #[cfg(windows)]
    {
        let sp = venv.join("Lib").join("site-packages");
        if sp.exists() {
            return Ok(sp);
        }
    }
    anyhow::bail!("Could not find site-packages in {}", venv.display())
}

/// Read the Python version from pyvenv.cfg
fn read_pyvenv_version(venv: &Path) -> Option<String> {
    let cfg = std::fs::read_to_string(venv.join("pyvenv.cfg")).ok()?;
    for line in cfg.lines() {
        if let Some(rest) = line.strip_prefix("version") {
            let ver = rest.trim_start_matches(|c: char| c == ' ' || c == '=').trim();
            if !ver.is_empty() {
                return Some(ver.to_string());
            }
        }
    }
    None
}

/// Read package name and version from a .dist-info/METADATA file.
fn read_dist_info_metadata(metadata_path: &Path) -> Option<(String, String)> {
    let content = std::fs::read_to_string(metadata_path).ok()?;
    let mut name = None;
    let mut version = None;
    for line in content.lines() {
        if let Some(n) = line.strip_prefix("Name: ") {
            name = Some(n.trim().to_string());
        }
        if let Some(v) = line.strip_prefix("Version: ") {
            version = Some(v.trim().to_string());
        }
        if name.is_some() && version.is_some() {
            break;
        }
    }
    Some((name?, version?))
}

/// Read file paths from a .dist-info/RECORD file.
fn read_record_files(record_path: &Path) -> Vec<String> {
    let content = match std::fs::read_to_string(record_path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    content
        .lines()
        .filter(|l| !l.is_empty())
        .filter_map(|line| {
            let path = line.split(',').next()?.trim();
            if path.is_empty() { None } else { Some(path.to_string()) }
        })
        .collect()
}

/// Copy a .dist-info directory and the guessed package directory.
fn copy_dist_info_and_package(
    site_packages: &Path,
    dist_info_dir: &Path,
    pkg_name: &str,
    store_dir: &Path,
) -> Result<()> {
    let di_name = dist_info_dir
        .file_name()
        .context("dist-info has no name")?;
    copy_dir_recursive(dist_info_dir, &store_dir.join(di_name))?;

    let normalized = pkg_name.to_lowercase().replace('-', "_");
    let pkg_dir = site_packages.join(&normalized);
    if pkg_dir.is_dir() {
        copy_dir_recursive(&pkg_dir, &store_dir.join(&normalized))?;
    } else {
        let py_file = site_packages.join(format!("{}.py", normalized));
        if py_file.exists() {
            std::fs::copy(&py_file, store_dir.join(format!("{}.py", normalized)))?;
        }
    }

    Ok(())
}

/// Recursively copy a directory tree.
fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in WalkDir::new(src).min_depth(1) {
        let entry = entry?;
        let rel = entry.path().strip_prefix(src)?;
        let dst_path = dst.join(rel);

        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&dst_path)?;
        } else if !dst_path.exists() {
            if let Some(parent) = dst_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(entry.path(), &dst_path)
                .with_context(|| format!("failed to copy {}", entry.path().display()))?;
        }
    }
    Ok(())
}

/// Convert "3.12.9" → "cp312"
fn short_python_tag(full_version: &str) -> String {
    let mut parts = full_version.splitn(3, '.');
    let major = parts.next().unwrap_or("3");
    let minor = parts.next().unwrap_or("0");
    format!("cp{major}{minor}")
}

/// Convert "3.12.9" → "3.12"
#[allow(dead_code)]
fn major_minor(full_version: &str) -> String {
    let mut parts = full_version.splitn(3, '.');
    let major = parts.next().unwrap_or("3");
    let minor = parts.next().unwrap_or("0");
    format!("{major}.{minor}")
}

/// Locate the python executable from the kong-managed runtime.
fn kong_python_exe(store_root: &Path, rules: &KongRules) -> Option<PathBuf> {
    let rt = rules.runtimes.as_ref()?.python.as_ref()?;
    let base = store_root.join(&rt.store_path);
    #[cfg(windows)]
    {
        // Try flat layout first (python.exe directly in runtime dir)
        let exe = base.join("python.exe");
        if exe.exists() { return Some(exe); }
        // Fallback: nested python/ subdirectory
        let exe2 = base.join("python").join("python.exe");
        if exe2.exists() { return Some(exe2); }
    }
    #[cfg(not(windows))]
    {
        let exe = base.join("python").join("bin").join("python3");
        if exe.exists() { return Some(exe); }
        let exe2 = base.join("bin").join("python3");
        if exe2.exists() { return Some(exe2); }
    }
    None
}
