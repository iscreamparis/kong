//! `kong import`, `kong solidify`, `kong eject` — project migration commands.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tracing::{debug, info, warn};
use walkdir::WalkDir;

use crate::config::{self, KongRules, NodeSection, PackageEntry, PythonSection, RuntimeEntry};
use crate::link;
use crate::store;

// ── kong import ─────────────────────────────────────────────────────────────

/// Convert an existing project (with an installed venv + `node_modules`) to the
/// KONG way by **adopting** the already-installed packages byte-for-byte.
///
/// This is a COPY-ADOPT, not a re-resolve: for every ecosystem that already has
/// an installed environment, we copy the exact installed distributions (incl.
/// native `.so` / `.node` binaries) into the content-addressed store and build
/// `kong.rules` from that installed set — nothing is re-resolved or re-downloaded
/// from PyPI/npm.  Only ecosystems with NO installed environment fall back to the
/// manifest-driven resolver (`generate_rules`) so a genuinely-missing dep can
/// still be fetched.
pub fn import_project(project_dir: &Path) -> Result<()> {
    let store_root = store::store_root()?;
    let project_name = project_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "project".to_string());

    info!(project = %project_name, "Importing existing project into KONG");

    // ── 1. Adopt installed environments: copy packages into the store and
    //    build sections directly from what is installed (no re-resolve). ──
    let adopted_python = adopt_python(project_dir, &store_root)?;
    let adopted_node = adopt_node(project_dir, &store_root)?;

    // ── 2. Build kong.rules. Start from the manifest-driven resolver so
    //    scripts / services / brew / rust are still discovered, then OVERRIDE
    //    the python/node sections with the adopted (copied) sets when present.
    //    For an ecosystem we adopted, the resolver must NOT run (it would
    //    re-download); so we hide that ecosystem's manifest from the resolver.
    info!("Generating kong.rules");
    let mut rules = generate_rules_skipping(
        project_dir,
        adopted_python.is_some(),
        adopted_node.is_some(),
    )?;

    if let Some((section, runtime)) = adopted_python {
        info!(count = section.packages.len(), "Python packages adopted from install (copied, not re-downloaded)");
        rules.python = Some(section);
        // Ensure the runtimes section advertises the adopted runtime so
        // build_venv can locate a python executable.
        ensure_python_runtime_entry(&mut rules, runtime);
    }
    if let Some(section) = adopted_node {
        info!(count = section.packages.len(), "Node packages adopted from install (copied, not re-downloaded)");
        rules.node = Some(section);
    }

    let rules_path = project_dir.join("kong.rules");
    config::write_rules(&rules, &rules_path)?;
    info!(path = %rules_path.display(), "kong.rules written");

    // ── 3. Remove the local installed environments now that they live in the
    //    store; they will be recreated as links into the RULEZ env. ──────
    for venv in discover_venvs(project_dir) {
        if venv.exists() && !venv.is_symlink() {
            info!(path = %venv.display(), "Removing local venv (packages are now in the store)");
            link::remove_dir_all_robust(&venv)?;
        }
    }
    let nm = project_dir.join("node_modules");
    if nm.exists() && !nm.is_symlink() {
        info!("Removing local node_modules (packages are now in the store)");
        link::remove_dir_all_robust(&nm)?;
    }

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

/// Run the manifest-driven resolver, but optionally hide the python / node
/// manifests so already-adopted ecosystems are NOT re-resolved/re-downloaded.
///
/// KONG stays general: we don't special-case any project.  We copy the project
/// into a temp dir minus the manifests we want skipped, run `generate_rules`
/// there, then return its non-python/node parts (scripts/services/brew/rust).
/// When nothing is skipped this is just `generate_rules(project_dir)`.
fn generate_rules_skipping(
    project_dir: &Path,
    skip_python: bool,
    skip_node: bool,
) -> Result<KongRules> {
    if !skip_python && !skip_node {
        return config::generate_rules(project_dir, false);
    }

    // Build a shadow project dir: hard-link/copy everything except the
    // manifests for the ecosystems we already adopted, so the resolver sees
    // no deps there and skips network resolution for them.
    let shadow = tempfile::TempDir::new().context("failed to create shadow project dir")?;
    let python_manifests = [
        "requirements.txt", "requirements.lock", "pyproject.toml",
        "Pipfile", "Pipfile.lock", "poetry.lock", "setup.py", "setup.cfg",
        "uv.lock",
    ];
    let node_manifests = ["package.json", "package-lock.json", "yarn.lock", "pnpm-lock.yaml"];

    for entry in std::fs::read_dir(project_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        // Never copy heavy/irrelevant trees into the shadow.
        if name_str == ".venv" || name_str == "venv" || name_str == "node_modules"
            || name_str == ".git" || name_str == "kong.rules"
        {
            continue;
        }
        if skip_python && python_manifests.contains(&name_str.as_ref()) {
            continue;
        }
        if skip_node && node_manifests.contains(&name_str.as_ref()) {
            continue;
        }
        let dst = shadow.path().join(&name);
        // Classify by lstat: a real directory recurses; a symlink (even one to
        // a directory, e.g. a stray `lib64`) is recreated, not walked; a
        // regular file is copied.
        let meta = std::fs::symlink_metadata(entry.path())?;
        if meta.is_dir() {
            copy_dir_recursive(&entry.path(), &dst)?;
        } else {
            copy_entry(&entry.path(), &dst)?;
        }
    }

    config::generate_rules(shadow.path(), false)
}

/// Make sure `rules.runtimes.python` points at a usable runtime so `build_venv`
/// can find a python executable.  We reuse the resolver's runtime entry when it
/// already produced one (same flow as a normal `kong use`); otherwise we ensure
/// a kong-managed runtime matching the adopted venv's major.minor.
fn ensure_python_runtime_entry(rules: &mut KongRules, adopted: RuntimeEntry) {
    let already = rules
        .runtimes
        .as_ref()
        .and_then(|r| r.python.as_ref())
        .is_some();
    if already {
        return;
    }
    let runtimes = rules.runtimes.get_or_insert_with(|| config::RuntimeSection {
        python: None,
        node: None,
        rust: None,
    });
    runtimes.python = Some(adopted);
}

// ── Python copy-adopt ────────────────────────────────────────────────────────

/// Discover candidate venv directories inside the project (project-agnostic:
/// any dir containing a `pyvenv.cfg`).  Common names are checked first, then a
/// shallow scan picks up anything else.
fn discover_venvs(project_dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut seen = std::collections::HashSet::new();

    let consider = |p: PathBuf, found: &mut Vec<PathBuf>, seen: &mut std::collections::HashSet<PathBuf>| {
        if p.join("pyvenv.cfg").exists() && seen.insert(p.clone()) {
            found.push(p);
        }
    };

    for name in [".venv", "venv", "env", ".env"] {
        consider(project_dir.join(name), &mut found, &mut seen);
    }
    // Shallow scan for any other venv-shaped dir at the project root.
    if let Ok(rd) = std::fs::read_dir(project_dir) {
        for entry in rd.flatten() {
            if entry.path().is_dir() {
                consider(entry.path(), &mut found, &mut seen);
            }
        }
    }
    found
}

/// Copy each installed Python distribution from the project's venv into the
/// store byte-for-byte (incl. native `.so`/`.pyd`), and build a `PythonSection`
/// describing the adopted set.  Returns `None` if no installed venv is found
/// (so the caller falls back to the resolver).
fn adopt_python(project_dir: &Path, store_root: &Path) -> Result<Option<(PythonSection, RuntimeEntry)>> {
    let venv = match discover_venvs(project_dir).into_iter().next() {
        Some(v) => v,
        None => return Ok(None),
    };
    let site_packages = match find_site_packages(&venv) {
        Ok(sp) => sp,
        Err(e) => {
            warn!("venv at {} has no site-packages: {e}", venv.display());
            return Ok(None);
        }
    };

    // Detect the installed Python version → compute the ABI / platform tags.
    // The store dir name MUST match what the resolver/build_venv expect so the
    // linker finds the entry: python/libs/{name}-{ver}-{cpXY}-{platform}.
    let py_version = read_pyvenv_version(&venv).unwrap_or_else(|| "3.12.0".to_string());
    let py_tag = short_python_tag(&py_version);
    let platform = config::platform_tag();

    info!(venv = %venv.display(), version = %py_version, "Adopting installed Python packages");

    let mut packages = Vec::new();
    for entry in std::fs::read_dir(&site_packages)? {
        let entry = entry?;
        let name_os = entry.file_name();
        let name_str = name_os.to_string_lossy();
        if !name_str.ends_with(".dist-info") || !entry.path().is_dir() {
            continue;
        }

        let metadata_path = entry.path().join("METADATA");
        let (pkg_name, pkg_version) = match read_dist_info_metadata(&metadata_path) {
            Some(nv) => nv,
            None => continue,
        };

        // Skip venv internals — they're provided by the kong runtime.
        let lower = pkg_name.to_lowercase();
        if lower == "pip" || lower == "setuptools" || lower == "wheel" {
            continue;
        }

        let store_path_rel = format!(
            "python/libs/{}-{}-{}-{}",
            pkg_name, pkg_version, py_tag, platform
        );
        let full_store_path = store_root.join(&store_path_rel);

        // Copy byte-for-byte unless already present (idempotent re-import).
        if full_store_path.exists() {
            debug!(pkg = %pkg_name, "Already in store, reusing");
        } else {
            info!(pkg = %pkg_name, ver = %pkg_version, "Copying installed package to store");
            std::fs::create_dir_all(&full_store_path)?;
            copy_installed_python_dist(&site_packages, &entry.path(), &pkg_name, &full_store_path)?;
            store::write_verified_marker(&full_store_path, "imported")?;
        }

        packages.push(PackageEntry {
            name: pkg_name,
            version: pkg_version,
            hash: None,
            store_path: store_path_rel,
            source_url: None,
        });
    }

    if packages.is_empty() {
        return Ok(None);
    }

    let section = PythonSection {
        version: py_version.clone(),
        platform,
        packages,
    };
    // Reference a kong-managed runtime of the same major.minor as the install,
    // so the ABI of the copied native modules matches the python we link.
    let runtime = match crate::python::runtime::ensure_runtime(store_root, &major_minor(&py_version)) {
        Ok(rt) => RuntimeEntry { version: rt.version, store_path: rt.store_path },
        Err(e) => {
            warn!("could not ensure kong python runtime for {py_version}: {e}; \
                   build_venv will look for an existing runtime in rules");
            RuntimeEntry { version: py_version, store_path: String::new() }
        }
    };
    Ok(Some((section, runtime)))
}

/// Copy ONE installed distribution from site-packages into `store_dir`.
///
/// Uses the `.dist-info/RECORD` manifest (the authoritative file list, which
/// includes native `.so`/`.pyd`/data files) when present so we copy exactly
/// what was installed; falls back to copying the `.dist-info` dir + the guessed
/// package dir when RECORD is missing (editable/legacy installs).
fn copy_installed_python_dist(
    site_packages: &Path,
    dist_info_dir: &Path,
    pkg_name: &str,
    store_dir: &Path,
) -> Result<()> {
    let record_files = read_record_files(&dist_info_dir.join("RECORD"));
    if record_files.is_empty() {
        copy_dist_info_and_package(site_packages, dist_info_dir, pkg_name, store_dir)?;
        return Ok(());
    }
    for rel_path in &record_files {
        // RECORD paths are relative to site-packages. Skip entries that escape
        // it (e.g. "../../Scripts/foo.exe") — those are console scripts the
        // kong venv regenerates, not package payload.
        if rel_path.starts_with("..") || Path::new(rel_path).is_absolute() {
            continue;
        }
        let src = site_packages.join(rel_path);
        let dst = store_dir.join(rel_path);
        // Use lstat (via copy_entry): a RECORD entry may be a symlink, and we
        // must not follow it into a "neither file nor symlink-to-file" error.
        if std::fs::symlink_metadata(&src).is_err() {
            continue; // RECORD lists a file that isn't there — skip.
        }
        copy_entry(&src, &dst)?;
    }
    Ok(())
}

// ── Node copy-adopt ──────────────────────────────────────────────────────────

/// Copy each installed Node package from `node_modules/` into the store
/// byte-for-byte (incl. native `.node` addons) and build a `NodeSection`.
/// Returns `None` when there is no installed `node_modules` (caller falls back
/// to the resolver).
fn adopt_node(project_dir: &Path, store_root: &Path) -> Result<Option<NodeSection>> {
    let nm = project_dir.join("node_modules");
    if !nm.exists() || nm.is_symlink() {
        return Ok(None);
    }

    let mut packages = Vec::new();
    for entry in std::fs::read_dir(&nm)? {
        let entry = entry?;
        let name_str = entry.file_name().to_string_lossy().to_string();

        // Skip dotfiles, .bin, lockfiles, hoisting metadata.
        if name_str.starts_with('.') || name_str == ".bin" || name_str == ".package-lock.json" {
            continue;
        }

        if name_str.starts_with('@') {
            if !entry.path().is_dir() {
                continue;
            }
            for sub in std::fs::read_dir(entry.path())? {
                let sub = sub?;
                let scoped_name = format!("{}/{}", name_str, sub.file_name().to_string_lossy());
                if let Some(pkg) = adopt_single_node_package(&nm, &scoped_name, store_root)? {
                    packages.push(pkg);
                }
            }
        } else if let Some(pkg) = adopt_single_node_package(&nm, &name_str, store_root)? {
            packages.push(pkg);
        }
    }

    if packages.is_empty() {
        return Ok(None);
    }
    Ok(Some(NodeSection { packages }))
}

/// Copy one installed Node package into the store under the `package/`
/// sub-layout `build_node_modules` expects, and return its `PackageEntry`.
fn adopt_single_node_package(
    nm_dir: &Path,
    pkg_name: &str,
    store_root: &Path,
) -> Result<Option<PackageEntry>> {
    let pkg_dir = nm_dir.join(pkg_name);
    let pkg_json_path = pkg_dir.join("package.json");
    if !pkg_json_path.exists() {
        return Ok(None);
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
        debug!(pkg = %name, "Already in store, reusing");
    } else {
        // npm tarballs extract with a `package/` subdirectory convention;
        // replicate it so build_node_modules finds the content root.
        let store_pkg_dir = full_store_path.join("package");
        info!(pkg = %name, ver = %version, "Copying installed package to store");
        std::fs::create_dir_all(&store_pkg_dir)?;
        copy_dir_recursive(&pkg_dir, &store_pkg_dir)?;
        store::write_verified_marker(&full_store_path, "imported")?;
    }

    Ok(Some(PackageEntry {
        name,
        version,
        hash: None,
        store_path: store_path_rel,
        source_url: None,
    }))
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
///
/// A solidified venv must be **truly standalone** — i.e. survive the store
/// being deleted. So besides copying site-packages, we also copy the
/// python-build-standalone runtime into the project (`.venv/runtime/`) and
/// repoint `bin/python`(`Scripts\python.exe`) + `pyvenv.cfg home` at that
/// local copy. python-build-standalone runtimes are relocatable, so a
/// `python -c "import sys; print(sys.executable)"` then resolves entirely
/// inside the project with the store gone. We also (re)generate
/// `[console_scripts]` launchers pointing at the local python.
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

    // ── Localize the runtime: copy it into the project so bin/python no longer
    //    points into the (deletable) store. ──────────────────────────────────
    let local_runtime = venv_dir.join("runtime");
    let local_python = localize_runtime(store_root, rules, &local_runtime)?;
    if local_python.is_none() {
        warn!("kong python runtime not found in store; solidified .venv will not be standalone");
    }

    // pyvenv.cfg `home` → the LOCAL runtime's bin dir (store-independent).
    let python_home = local_python
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

    // bin/Scripts with python executable → point at the LOCAL runtime.
    #[cfg(not(windows))]
    {
        let bin = venv_dir.join("bin");
        std::fs::create_dir_all(&bin)?;
        if let Some(ref src_exe) = local_python {
            for name in &["python3", "python"] {
                let dst = bin.join(name);
                if dst.exists() || dst.is_symlink() {
                    std::fs::remove_file(&dst).ok();
                }
                // Relative symlink into the project's own runtime/ — so the
                // link target moves with the project and never touches the store.
                let rel = pathdiff_relative(&dst, src_exe).unwrap_or_else(|| src_exe.clone());
                std::os::unix::fs::symlink(&rel, &dst)?;
            }
        }
    }
    #[cfg(windows)]
    {
        let scripts = venv_dir.join("Scripts");
        std::fs::create_dir_all(&scripts)?;
        if let Some(ref src_exe) = local_python {
            let _ = std::fs::copy(src_exe, scripts.join("python.exe"));
            let src_w = src_exe.with_file_name("pythonw.exe");
            if src_w.exists() {
                let _ = std::fs::copy(&src_w, scripts.join("pythonw.exe"));
            }
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

    // Generate [console_scripts] launchers pointing at the venv's local python.
    {
        #[cfg(windows)]
        let (bin_dir, venv_python) =
            (venv_dir.join("Scripts"), venv_dir.join("Scripts").join("python.exe"));
        #[cfg(not(windows))]
        let (bin_dir, venv_python) =
            (venv_dir.join("bin"), venv_dir.join("bin").join("python"));
        let n = crate::python::entry_points::generate_console_scripts(
            &site_packages,
            &bin_dir,
            &venv_python,
        )?;
        if n > 0 {
            info!(count = n, "  ✓ Generated {n} console-script launcher(s)");
        }
    }

    info!("  ✓ Runtime copied locally (.venv/runtime) — standalone, +runtime-size disk cost");
    Ok(())
}

/// Copy the kong-managed python-build-standalone runtime from the store into
/// `local_runtime`, and return the path to the LOCAL python executable inside
/// it. python-build-standalone is relocatable, so the copy runs standalone.
/// Returns `Ok(None)` when no runtime is recorded / found in the store.
fn localize_runtime(
    store_root: &Path,
    rules: &KongRules,
    local_runtime: &Path,
) -> Result<Option<PathBuf>> {
    let store_exe = match kong_python_exe(store_root, rules) {
        Some(e) => e,
        None => return Ok(None),
    };
    // The store runtime base dir = rules.runtimes.python.store_path.
    let rt = rules
        .runtimes
        .as_ref()
        .and_then(|r| r.python.as_ref())
        .map(|p| store_root.join(&p.store_path));
    let store_base = match rt {
        Some(b) if b.is_dir() => b,
        // Fallback: the runtime base is an ancestor of the exe.
        _ => store_exe
            .ancestors()
            .find(|a| python_exe_in_runtime(a).is_some())
            .map(|a| a.to_path_buf())
            .unwrap_or_else(|| store_exe.clone()),
    };

    if local_runtime.exists() {
        link::remove_dir_all_robust(local_runtime).ok();
    }
    copy_dir_recursive(&store_base, local_runtime)?;

    // Recompute the exe path inside the LOCAL copy by mirroring its relative
    // position from the store base.
    let rel_exe = store_exe
        .strip_prefix(&store_base)
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|_| {
            // store_exe wasn't under store_base (unexpected) — probe the copy.
            python_exe_in_runtime(local_runtime)
                .and_then(|e| e.strip_prefix(local_runtime).ok().map(|p| p.to_path_buf()))
                .unwrap_or_else(|| PathBuf::from("bin/python3"))
        });
    Ok(Some(local_runtime.join(rel_exe)))
}

/// Find python.exe / python3 inside a runtime dir (flat or nested `python/`).
fn python_exe_in_runtime(base: &Path) -> Option<PathBuf> {
    #[cfg(windows)]
    let candidates = ["python.exe", "python/python.exe"];
    #[cfg(not(windows))]
    let candidates = ["bin/python3", "bin/python", "python/bin/python3"];
    for rel in &candidates {
        let p = base.join(rel);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

/// Compute a relative path FROM the directory containing `link` TO `target`.
/// Minimal pure-std implementation (avoids a new dependency).
#[cfg(not(windows))]
fn pathdiff_relative(link: &Path, target: &Path) -> Option<PathBuf> {
    let base = link.parent()?;
    let base_c: Vec<_> = base.components().collect();
    let tgt_c: Vec<_> = target.components().collect();
    let mut i = 0;
    while i < base_c.len() && i < tgt_c.len() && base_c[i] == tgt_c[i] {
        i += 1;
    }
    let mut rel = PathBuf::new();
    for _ in i..base_c.len() {
        rel.push("..");
    }
    for c in &tgt_c[i..] {
        rel.push(c.as_os_str());
    }
    if rel.as_os_str().is_empty() {
        None
    } else {
        Some(rel)
    }
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

/// Recursively copy a directory tree, robustly handling every entry kind a real
/// venv / site-packages / node_modules tree contains.
///
/// `WalkDir` does NOT follow symlinks by default, so a symlink (whatever its
/// target) is yielded as a leaf and we recreate it verbatim — we never descend
/// through it (no infinite loop on a self-referential link like `lib64 -> lib`).
/// Each entry is classified by `symlink_metadata` (lstat) so a symlink is
/// detected BEFORE its target is followed.
fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in WalkDir::new(src).min_depth(1) {
        let entry = entry?;
        let rel = entry.path().strip_prefix(src)?;
        let dst_path = dst.join(rel);
        copy_entry(entry.path(), &dst_path)?;
    }
    Ok(())
}

/// Copy a single filesystem entry from `src` to `dst`, classifying it by lstat
/// (`symlink_metadata`) so symlinks are detected before their target is
/// followed.  Handles: directories (created), symlinks of any kind (recreated
/// as a symlink to the same target — dir or file), broken/dangling symlinks
/// (skipped with a debug log), and regular files (byte-for-byte copy).
///
/// Idempotent: an entry that already exists at `dst` is left untouched.
fn copy_entry(src: &Path, dst: &Path) -> Result<()> {
    // lstat: does NOT follow the link, so a symlink reports as a symlink.
    let meta = match std::fs::symlink_metadata(src) {
        Ok(m) => m,
        // Race / vanished entry — nothing to copy.
        Err(_) => return Ok(()),
    };

    if meta.file_type().is_symlink() {
        // A symlink of ANY kind (to a dir like `lib64 -> lib`, to a file, or
        // dangling). Recreate it verbatim rather than copying its target.
        if dst.symlink_metadata().is_ok() {
            return Ok(()); // already present (idempotent re-import)
        }
        let target = match std::fs::read_link(src) {
            Ok(t) => t,
            Err(e) => {
                debug!(path = %src.display(), err = %e, "Unreadable symlink, skipping");
                return Ok(());
            }
        };
        // Dangling/broken symlink: keep going (don't abort the whole import).
        // We recreate it anyway so the tree stays faithful, but a recreate
        // failure on a dangling link is non-fatal.
        let points_to_dir = std::fs::metadata(src).map(|m| m.is_dir()).unwrap_or(false);
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        match symlink_any(&target, dst, points_to_dir) {
            Ok(()) => {}
            Err(e) => {
                debug!(
                    path = %src.display(),
                    target = %target.display(),
                    err = %e,
                    "Could not recreate symlink (likely dangling/unsupported), skipping"
                );
            }
        }
        return Ok(());
    }

    if meta.is_dir() {
        std::fs::create_dir_all(dst)?;
        return Ok(());
    }

    // Regular file (incl. native .so/.pyd/.node) — byte-for-byte.
    if dst.exists() {
        return Ok(()); // idempotent
    }
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(src, dst)
        .with_context(|| format!("failed to copy {}", src.display()))?;
    Ok(())
}

/// Create a symlink at `dst` pointing at `target`. On Unix a single syscall
/// handles both file and dir targets; on Windows the kind matters, so we pick
/// `symlink_dir` / `symlink_file` from whether the target resolves to a dir.
#[cfg(unix)]
fn symlink_any(target: &Path, dst: &Path, _points_to_dir: bool) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, dst)
}

#[cfg(windows)]
fn symlink_any(target: &Path, dst: &Path, points_to_dir: bool) -> std::io::Result<()> {
    if points_to_dir {
        std::os::windows::fs::symlink_dir(target, dst)
    } else {
        std::os::windows::fs::symlink_file(target, dst)
    }
}

/// Convert "3.12.9" → "cp312"
fn short_python_tag(full_version: &str) -> String {
    let mut parts = full_version.splitn(3, '.');
    let major = parts.next().unwrap_or("3");
    let minor = parts.next().unwrap_or("0");
    format!("cp{major}{minor}")
}

/// Convert "3.12.9" → "3.12"
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a fake site-packages with one distribution that has a native
    /// extension recorded in RECORD, then assert copy-adopt reproduces every
    /// file (incl. the `.so`) byte-for-byte under the store entry.
    #[test]
    fn copy_installed_python_dist_preserves_native_so() {
        let tmp = tempfile::TempDir::new().unwrap();
        let site = tmp.path().join("site-packages");
        let pkg = site.join("pydantic_core");
        std::fs::create_dir_all(&pkg).unwrap();
        // A native extension module — the exact thing re-download breaks.
        let so_bytes: &[u8] = &[0x7f, b'E', b'L', b'F', 1, 2, 3, 4];
        std::fs::write(pkg.join("_pydantic_core.so"), so_bytes).unwrap();
        std::fs::write(pkg.join("__init__.py"), b"# pkg").unwrap();

        let dist_info = site.join("pydantic_core-2.0.0.dist-info");
        std::fs::create_dir_all(&dist_info).unwrap();
        std::fs::write(
            dist_info.join("METADATA"),
            "Name: pydantic_core\nVersion: 2.0.0\n",
        )
        .unwrap();
        std::fs::write(
            dist_info.join("RECORD"),
            "pydantic_core/__init__.py,,\n\
             pydantic_core/_pydantic_core.so,,\n\
             pydantic_core-2.0.0.dist-info/METADATA,,\n\
             pydantic_core-2.0.0.dist-info/RECORD,,\n",
        )
        .unwrap();

        let store_dir = tmp.path().join("store_entry");
        std::fs::create_dir_all(&store_dir).unwrap();
        copy_installed_python_dist(&site, &dist_info, "pydantic_core", &store_dir).unwrap();

        // The native .so must be present and identical.
        let copied_so = store_dir.join("pydantic_core").join("_pydantic_core.so");
        assert!(copied_so.exists(), "native .so was not copied");
        assert_eq!(std::fs::read(&copied_so).unwrap(), so_bytes, ".so bytes differ");
        // The dist-info METADATA travelled along too.
        assert!(store_dir
            .join("pydantic_core-2.0.0.dist-info")
            .join("METADATA")
            .exists());
    }

    /// `localize_runtime` copies a (fake) store runtime into the project and
    /// returns a python exe path INSIDE the project — never pointing at the
    /// store. After the store dir is deleted, the local exe must still exist.
    #[test]
    fn localize_runtime_copies_into_project_and_is_store_independent() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store_root = tmp.path().join("store");
        // Fake python-build-standalone runtime in the store.
        let rt_rel = "python/runtime/3.12.9";
        let rt_base = store_root.join(rt_rel);
        #[cfg(windows)]
        let exe_rel = "python.exe";
        #[cfg(not(windows))]
        let exe_rel = "bin/python3";
        let store_exe = rt_base.join(exe_rel);
        std::fs::create_dir_all(store_exe.parent().unwrap()).unwrap();
        std::fs::write(&store_exe, b"#!/fake/python\n").unwrap();
        // A sibling file to prove the whole runtime tree is copied.
        std::fs::write(rt_base.join("LICENSE"), b"license").unwrap();

        let rules = KongRules {
            version: 1,
            project: "p".into(),
            generated: "now".into(),
            runtimes: Some(config::RuntimeSection {
                python: Some(config::RuntimeEntry {
                    version: "3.12.9".into(),
                    store_path: rt_rel.into(),
                }),
                node: None,
                rust: None,
            }),
            python: None,
            node: None,
            rust: None,
            brew: None,
            scripts: Default::default(),
            services: Vec::new(),
        };

        let project = tmp.path().join("proj");
        let local_runtime = project.join(".venv").join("runtime");
        let local_exe = localize_runtime(&store_root, &rules, &local_runtime)
            .unwrap()
            .expect("should return a local python exe");

        // The returned exe is inside the PROJECT, not the store.
        assert!(local_exe.starts_with(&project), "exe not in project: {}", local_exe.display());
        assert!(!local_exe.starts_with(&store_root), "exe still in store: {}", local_exe.display());
        assert!(local_exe.exists(), "local exe missing");
        assert!(local_runtime.join("LICENSE").exists(), "runtime tree not fully copied");

        // Delete the store entirely — the local runtime must survive.
        link::remove_dir_all_robust(&store_root).unwrap();
        assert!(local_exe.exists(), "local exe vanished after store removal — not standalone");
    }

    /// RECORD paths that escape site-packages (console scripts) are not copied.
    #[test]
    fn copy_installed_python_dist_skips_escaping_record_paths() {
        let tmp = tempfile::TempDir::new().unwrap();
        let site = tmp.path().join("site-packages");
        let pkg = site.join("widget");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("__init__.py"), b"x").unwrap();
        let dist_info = site.join("widget-1.0.dist-info");
        std::fs::create_dir_all(&dist_info).unwrap();
        std::fs::write(dist_info.join("METADATA"), "Name: widget\nVersion: 1.0\n").unwrap();
        std::fs::write(
            dist_info.join("RECORD"),
            "widget/__init__.py,,\n../../Scripts/widget.exe,,\n",
        )
        .unwrap();

        let store_dir = tmp.path().join("store_entry");
        std::fs::create_dir_all(&store_dir).unwrap();
        copy_installed_python_dist(&site, &dist_info, "widget", &store_dir).unwrap();

        assert!(store_dir.join("widget").join("__init__.py").exists());
        // The escaping path must NOT have created anything outside store_dir.
        assert!(!store_dir.join("..").join("..").join("Scripts").join("widget.exe").exists());
    }

    /// Falling back to dist-info+package copy when RECORD is absent still
    /// brings the package contents.
    #[test]
    fn copy_installed_python_dist_without_record_falls_back() {
        let tmp = tempfile::TempDir::new().unwrap();
        let site = tmp.path().join("site-packages");
        let pkg = site.join("my_pkg");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("mod.py"), b"code").unwrap();
        let dist_info = site.join("my_pkg-3.0.dist-info");
        std::fs::create_dir_all(&dist_info).unwrap();
        std::fs::write(dist_info.join("METADATA"), "Name: my-pkg\nVersion: 3.0\n").unwrap();
        // No RECORD on purpose.

        let store_dir = tmp.path().join("store_entry");
        std::fs::create_dir_all(&store_dir).unwrap();
        copy_installed_python_dist(&site, &dist_info, "my-pkg", &store_dir).unwrap();

        assert!(store_dir.join("my_pkg").join("mod.py").exists());
        assert!(store_dir.join("my_pkg-3.0.dist-info").join("METADATA").exists());
    }

    #[test]
    fn discover_venvs_finds_nonstandard_name_by_pyvenv_cfg() {
        let tmp = tempfile::TempDir::new().unwrap();
        // CRM-style: the env dir is "venv", not ".venv".
        let venv = tmp.path().join("venv");
        std::fs::create_dir_all(&venv).unwrap();
        std::fs::write(venv.join("pyvenv.cfg"), "version = 3.10.4\n").unwrap();
        // A decoy dir without pyvenv.cfg must be ignored.
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();

        let found = discover_venvs(tmp.path());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0], venv);
    }

    /// Create a symlink for the test, returning false if the host can't make
    /// one (Windows without Developer Mode). On Unix this always succeeds.
    #[cfg(unix)]
    fn try_symlink(target: &Path, link: &Path, _is_dir: bool) -> bool {
        std::os::unix::fs::symlink(target, link).is_ok()
    }
    #[cfg(windows)]
    fn try_symlink(target: &Path, link: &Path, is_dir: bool) -> bool {
        if is_dir {
            std::os::windows::fs::symlink_dir(target, link).is_ok()
        } else {
            std::os::windows::fs::symlink_file(target, link).is_ok()
        }
    }

    /// `copy_dir_recursive` must reproduce a real venv tree: regular files and a
    /// native `.so` byte-for-byte, a `lib64 -> lib` DIRECTORY symlink recreated
    /// as a symlink (not copied as a file, not infinite-looped), a
    /// symlink-to-file recreated, and a dangling symlink that does NOT abort the
    /// whole copy.
    #[test]
    fn copy_dir_recursive_handles_dir_symlinks_and_dangling() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("venv");
        let lib = src.join("lib");
        std::fs::create_dir_all(&lib).unwrap();

        // Regular text file.
        std::fs::write(src.join("pyvenv.cfg"), b"version = 3.10.4\n").unwrap();
        // Native extension — bytes must be preserved exactly.
        let so_bytes: &[u8] = &[0x7f, b'E', b'L', b'F', 9, 8, 7, 6];
        std::fs::write(lib.join("_native.so"), so_bytes).unwrap();
        // A nested subdir with a file (recursion).
        std::fs::write(lib.join("module.py"), b"print(1)").unwrap();

        // `lib64 -> lib` directory symlink (relative target, points back inside
        // the tree — must not infinite-loop).
        let dir_symlink_made = try_symlink(Path::new("lib"), &src.join("lib64"), true);
        // symlink-to-file: `cfg-alias -> pyvenv.cfg`.
        let file_symlink_made = try_symlink(Path::new("pyvenv.cfg"), &src.join("cfg-alias"), false);
        // Dangling symlink: target does not exist.
        let dangling_made = try_symlink(Path::new("does-not-exist"), &src.join("dangling"), false);

        let dst = tmp.path().join("copy");
        // The whole copy must succeed even with the dangling link present.
        copy_dir_recursive(&src, &dst).expect("copy_dir_recursive must not abort");

        // Regular file + native .so copied byte-identical.
        assert_eq!(std::fs::read(dst.join("pyvenv.cfg")).unwrap(), b"version = 3.10.4\n");
        let copied_so = dst.join("lib").join("_native.so");
        assert!(copied_so.exists(), "native .so missing");
        assert_eq!(std::fs::read(&copied_so).unwrap(), so_bytes, ".so bytes differ");
        assert!(dst.join("lib").join("module.py").exists());

        if dir_symlink_made {
            let copied_lib64 = dst.join("lib64");
            let lmeta = std::fs::symlink_metadata(&copied_lib64)
                .expect("lib64 should exist in the copy");
            assert!(
                lmeta.file_type().is_symlink(),
                "lib64 must be recreated as a symlink, not copied as a dir/file"
            );
            // It still resolves to the directory it pointed at.
            assert_eq!(std::fs::read_link(&copied_lib64).unwrap(), Path::new("lib"));
        }
        if file_symlink_made {
            let alias = dst.join("cfg-alias");
            assert!(
                std::fs::symlink_metadata(&alias).unwrap().file_type().is_symlink(),
                "cfg-alias must be a symlink in the copy"
            );
        }
        if dangling_made {
            // The dangling link was either recreated (as a still-dangling link)
            // or cleanly skipped; either way the copy did not error above.
            // Nothing more to assert beyond the copy having succeeded.
        }
    }
}
