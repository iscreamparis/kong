use std::path::Path;

use anyhow::{bail, Context, Result};
use tracing::{debug, info};

use crate::config::{LocalPackageEntry, NodeSection};
use crate::link;

/// Build a flat node_modules structure from the store using deep hard links.
///
/// All packages land directly at `node_modules/<name>` (hoisted, like npm).
/// We use deep hard links rather than directory junctions so that Node's
/// module resolution sees project-local paths for every file's `__dirname`.
/// Junctions on Windows are transparent to path resolution, so a junction
/// pointing into the store would cause `require()` look-ups to walk the store
/// tree instead of the project tree — breaking peer-dep resolution.
///
/// `env_dir` is where node_modules is built (the RULEZ env; the project's own
/// `node_modules` is a junction to it). `project_dir` is the project itself:
/// local packages (`node.local`) are recorded relative to it and resolved
/// against it, then linked with a junction (Windows) / symlink (Unix) — the
/// same thing npm does for a `file:<dir>` dependency — never copied.
pub fn build_node_modules(
    env_dir: &Path,
    project_dir: &Path,
    node: &NodeSection,
    store_root: &Path,
) -> Result<()> {
    let nm = env_dir.join("node_modules");
    std::fs::create_dir_all(&nm)?;

    link_local_packages(&nm, project_dir, &node.local)?;

    for pkg in &node.packages {
        let src = store_root.join(&pkg.store_path);
        if !src.exists() {
            tracing::warn!(pkg = %pkg.name, "Store path missing, skipping: {}", src.display());
            continue;
        }

        let content_dir = package_content_root(&src);

        // Destination: node_modules/<name> (handle @scope/name)
        let top_level = nm.join(&pkg.name);
        // Was a LOCAL package on a previous `kong use` (now a registry one):
        // drop the link itself — never what it points at.
        if is_dir_link(&top_level) {
            remove_dir_link(&top_level)?;
        }
        if top_level.exists() {
            debug!(pkg = %pkg.name, "Already linked, skipping");
            continue;
        }

        // Create @scope/ parent if needed
        if pkg.name.contains('/') {
            if let Some(parent) = top_level.parent() {
                std::fs::create_dir_all(parent)?;
            }
        }

        link::link_package(&content_dir, &top_level)
            .with_context(|| format!("failed to link node package {}", pkg.name))?;

        debug!(pkg = %pkg.name, "Linked into node_modules");
    }

    info!("node_modules ready at {}", nm.display());

    // ── Create .bin/ shims for executables ───────────────────────────────────
    // npm creates node_modules/.bin/<name> → ../<pkg>/bin/script.js for every
    // "bin" entry in a package's package.json. We replicate that here.
    let bin_dir = nm.join(".bin");
    std::fs::create_dir_all(&bin_dir)?;

    // Bins of registry AND local packages: npm links both.
    let bin_owners = node.packages.iter().map(|p| p.name.as_str())
        .chain(node.local.iter().map(|l| l.name.as_str()));
    for pkg_name in bin_owners {
        let top_level = nm.join(pkg_name);
        if !top_level.exists() {
            continue;
        }
        let pkg_json_path = top_level.join("package.json");
        let content = match std::fs::read_to_string(&pkg_json_path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let v: serde_json::Value = match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let bin = match v.get("bin") {
            Some(b) => b,
            None => continue,
        };

        // "bin" can be a string (single binary named after the package) or an object.
        let entries: Vec<(String, String)> = if let Some(s) = bin.as_str() {
            let bin_name = pkg_name.split('/').last().unwrap_or(pkg_name).to_string();
            vec![(bin_name, s.to_string())]
        } else if let Some(obj) = bin.as_object() {
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        } else {
            continue
        };

        for (bin_name, rel_path) in entries {
            let shim = bin_dir.join(&bin_name);
            if shim.exists() {
                continue;
            }
            // Target is relative to node_modules/ root, e.g. ../vite/bin/vite.js
            let target = top_level.join(&rel_path);
            if !target.exists() {
                debug!(bin = %bin_name, "bin target missing, skipping");
                continue;
            }
            // Make target executable and symlink into .bin/
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Ok(meta) = std::fs::metadata(&target) {
                    let mut perms = meta.permissions();
                    perms.set_mode(perms.mode() | 0o111);
                    let _ = std::fs::set_permissions(&target, perms);
                }
                std::os::unix::fs::symlink(&target, &shim)
                    .with_context(|| format!("failed to create .bin/{bin_name} shim"))?;
            }
            #[cfg(windows)]
            {
                // On Windows npm creates .cmd wrappers; create a simple one.
                let target_str = target.to_string_lossy().replace('/', "\\");
                let cmd_path = bin_dir.join(format!("{bin_name}.cmd"));
                if !cmd_path.exists() {
                    std::fs::write(&cmd_path, format!("@node \"{target_str}\" %*\r\n"))?;
                }
            }
            debug!(bin = %bin_name, "Created .bin shim");
        }
    }

    Ok(())
}

/// The directory holding a stored npm package's `package.json`.
///
/// npm tarballs conventionally unpack to `package/`, but npm itself strips the
/// FIRST path component whatever its name — and some published tarballs use
/// another one (`@types/node@24.13.6` ships `node v24.13/…`). Linking the store
/// dir itself in that case produced `node_modules/@types/node/node v24.13/`,
/// with no package.json at the package root (TS2688 "Cannot find type
/// definition file for 'node'"). Resolution order: `package/`; else the single
/// top-level directory when it holds a package.json and the store dir holds no
/// package.json of its own; else the store dir (adopted / flat layouts).
pub fn package_content_root(src: &Path) -> std::path::PathBuf {
    let conventional = src.join("package");
    if conventional.is_dir() {
        return conventional;
    }
    if !src.join("package.json").exists() {
        if let Ok(rd) = std::fs::read_dir(src) {
            let entries: Vec<_> = rd
                .filter_map(|e| e.ok())
                .filter(|e| !e.file_name().to_string_lossy().starts_with(".kong"))
                .collect();
            if entries.len() == 1 {
                let only = entries[0].path();
                if only.is_dir() && only.join("package.json").is_file() {
                    return only;
                }
            }
        }
    }
    src.to_path_buf()
}

/// Link each local package at `node_modules/<name>` → its directory.
///
/// Idempotent: a link already pointing at the right directory is kept; one
/// pointing elsewhere (the dep moved) is replaced; a real directory left by a
/// previous registry install of that name is removed from the ENV (hard links
/// only — the store keeps its copy). The target itself is never written to.
pub fn link_local_packages(
    nm: &Path,
    project_dir: &Path,
    local: &[LocalPackageEntry],
) -> Result<()> {
    for lp in local {
        let target = lp.resolve(project_dir);
        if !target.is_dir() {
            bail!(
                "local Node dependency '{}' points at a directory that does not exist:\n  \
                 declared path : {} (kong.rules node.local, relative to {})\n  \
                 resolved to   : {}\n\
                 Restore the directory or fix the dependency in package.json, then re-run \
                 `kong rules`.",
                lp.name, lp.path, project_dir.display(), target.display()
            );
        }
        let dst = nm.join(&lp.name);
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }

        if is_dir_link(&dst) {
            if link_points_to(&dst, &target) {
                debug!(pkg = %lp.name, "Local link already correct");
                continue;
            }
            remove_dir_link(&dst)?;
        } else if dst.exists() {
            // A real directory inside the kong env (a previous registry install
            // of the same name, deep hard links). Safe to drop: it is the env's
            // own tree, not the store and not the local package.
            link::remove_dir_all_robust(&dst).with_context(|| {
                format!("failed to replace {} with a link to local package '{}'", dst.display(), lp.name)
            })?;
        }

        link::link_dir(&target, &dst).with_context(|| {
            format!(
                "failed to link local Node package '{}': {} -> {}",
                lp.name, dst.display(), target.display()
            )
        })?;
        info!(pkg = %lp.name, target = %target.display(), "Linked local package");
    }
    Ok(())
}

/// True if `p` is a directory junction (Windows) or a symlink (any OS).
fn is_dir_link(p: &Path) -> bool {
    let is_symlink = std::fs::symlink_metadata(p)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false);
    #[cfg(windows)]
    {
        is_symlink || junction::exists(p).unwrap_or(false)
    }
    #[cfg(not(windows))]
    {
        is_symlink
    }
}

/// Does the link at `link` resolve to the directory `target`?
fn link_points_to(link: &Path, target: &Path) -> bool {
    match (std::fs::canonicalize(link), std::fs::canonicalize(target)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Remove a junction/symlink ENTRY without touching what it points at.
fn remove_dir_link(p: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        if junction::exists(p).unwrap_or(false) {
            return junction::delete(p)
                .and_then(|_| std::fs::remove_dir(p))
                .with_context(|| format!("failed to remove junction {}", p.display()));
        }
        // A directory symlink on Windows is removed with remove_dir.
        std::fs::remove_dir(p)
            .or_else(|_| std::fs::remove_file(p))
            .with_context(|| format!("failed to remove link {}", p.display()))
    }
    #[cfg(not(windows))]
    {
        std::fs::remove_file(p).with_context(|| format!("failed to remove symlink {}", p.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(name: &str, path: &str) -> LocalPackageEntry {
        LocalPackageEntry { name: name.into(), version: "0.1.0".into(), path: path.into() }
    }

    /// Layout: <root>/vendor/atoms (the local package) and <root>/app (the
    /// project, declaring `file:../vendor/atoms`), env dir elsewhere.
    fn fixture() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
        let root = tempfile::TempDir::new().unwrap();
        let vendor = root.path().join("vendor").join("atoms");
        std::fs::create_dir_all(vendor.join("bin")).unwrap();
        std::fs::write(
            vendor.join("package.json"),
            r#"{"name":"@real3d/atoms","version":"0.1.0","bin":{"atoms-cli":"bin/cli.js"}}"#,
        ).unwrap();
        std::fs::write(vendor.join("bin").join("cli.js"), "console.log(1)").unwrap();
        std::fs::write(vendor.join("marker.txt"), "live source").unwrap();
        let app = root.path().join("app");
        std::fs::create_dir_all(&app).unwrap();
        let env = root.path().join("env");
        std::fs::create_dir_all(&env).unwrap();
        (root, vendor, app, env)
    }

    #[test]
    fn content_root_handles_non_package_tarball_root() {
        let t = tempfile::TempDir::new().unwrap();
        // Conventional layout.
        let a = t.path().join("a");
        std::fs::create_dir_all(a.join("package")).unwrap();
        std::fs::write(a.join(".kong-verified"), "x").unwrap();
        assert_eq!(package_content_root(&a), a.join("package"));
        // @types/node@24.13.6: top dir is "node v24.13".
        let b = t.path().join("b");
        std::fs::create_dir_all(b.join("node v24.13")).unwrap();
        std::fs::write(b.join("node v24.13").join("package.json"), "{}").unwrap();
        std::fs::write(b.join(".kong-verified"), "x").unwrap();
        assert_eq!(package_content_root(&b), b.join("node v24.13"));
        // Flat layout (package.json at the root) stays as-is.
        let c = t.path().join("c");
        std::fs::create_dir_all(c.join("lib")).unwrap();
        std::fs::write(c.join("package.json"), "{}").unwrap();
        assert_eq!(package_content_root(&c), c);
    }

    #[test]
    fn use_links_local_package_to_its_directory() {
        let (_root, vendor, app, env) = fixture();
        let node = NodeSection { packages: vec![], local: vec![local("@real3d/atoms", "../vendor/atoms")] };
        let store = tempfile::TempDir::new().unwrap();
        build_node_modules(&env, &app, &node, store.path()).unwrap();

        let dst = env.join("node_modules").join("@real3d").join("atoms");
        assert!(is_dir_link(&dst), "node_modules/@real3d/atoms must be a junction/symlink");
        assert_eq!(
            std::fs::canonicalize(&dst).unwrap(),
            std::fs::canonicalize(&vendor).unwrap(),
            "link must point at the local package directory"
        );
        // Live: a change in the source is visible through the link (not a copy).
        std::fs::write(vendor.join("marker.txt"), "edited").unwrap();
        assert_eq!(std::fs::read_to_string(dst.join("marker.txt")).unwrap(), "edited");
        // Nothing of it went to the store.
        assert_eq!(std::fs::read_dir(store.path()).unwrap().count(), 0);
        // Its bin got a shim, like npm does for a linked package.
        let bin = env.join("node_modules").join(".bin");
        #[cfg(windows)]
        assert!(bin.join("atoms-cli.cmd").exists());
        #[cfg(unix)]
        assert!(std::fs::symlink_metadata(bin.join("atoms-cli")).is_ok());
    }

    #[test]
    fn use_is_idempotent_and_repoints_a_moved_local_package() {
        let (root, vendor, app, env) = fixture();
        let store = tempfile::TempDir::new().unwrap();
        let node = NodeSection { packages: vec![], local: vec![local("@real3d/atoms", "../vendor/atoms")] };
        build_node_modules(&env, &app, &node, store.path()).unwrap();
        build_node_modules(&env, &app, &node, store.path()).unwrap(); // twice: no error

        // The dep moves to another checkout: the link is replaced, and the old
        // target is left intact (only the link entry is removed).
        let other = root.path().join("CRM_Atoms");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("package.json"), r#"{"name":"@real3d/atoms","version":"0.2.0"}"#).unwrap();
        let moved = NodeSection { packages: vec![], local: vec![local("@real3d/atoms", "../CRM_Atoms")] };
        build_node_modules(&env, &app, &moved, store.path()).unwrap();
        let dst = env.join("node_modules").join("@real3d").join("atoms");
        assert_eq!(
            std::fs::canonicalize(&dst).unwrap(),
            std::fs::canonicalize(&other).unwrap()
        );
        assert!(vendor.join("marker.txt").exists(), "old target must be untouched");
    }

    #[test]
    fn use_errors_clearly_when_local_target_is_missing() {
        let (_root, _vendor, app, env) = fixture();
        let store = tempfile::TempDir::new().unwrap();
        let node = NodeSection { packages: vec![], local: vec![local("@real3d/atoms", "../nowhere/atoms")] };
        let err = build_node_modules(&env, &app, &node, store.path()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("@real3d/atoms"), "{msg}");
        assert!(msg.contains("../nowhere/atoms"), "{msg}");
        assert!(msg.contains("does not exist"), "{msg}");
    }

    #[test]
    fn clean_environments_never_deletes_through_a_local_link() {
        let (_root, vendor, app, env) = fixture();
        let store = tempfile::TempDir::new().unwrap();
        let node = NodeSection { packages: vec![], local: vec![local("@real3d/atoms", "../vendor/atoms")] };
        build_node_modules(&env, &app, &node, store.path()).unwrap();
        link::clean_environments(&env, false).unwrap();
        assert!(!env.join("node_modules").exists());
        assert!(vendor.join("marker.txt").exists(), "local package source must survive a clean");
        assert!(vendor.join("package.json").exists());
    }
}
