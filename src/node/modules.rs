use std::collections::HashSet;
use std::path::Path;

use anyhow::{bail, Context, Result};
use tracing::{debug, info};

use crate::config::{GitPackageEntry, LocalPackageEntry, NodeSection, PackageEntry};
use crate::link;

/// Build node_modules from the store using deep hard links.
///
/// Top-level packages land at `node_modules/<name>` (hoisted, like npm); a
/// package with a `path` (a nested lockfile install such as
/// `node_modules/path-scurry/node_modules/lru-cache`) lands at that path, so it
/// keeps its own version next to the hoisted one.
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
    link_git_packages(&nm, &node.git, store_root)?;
    let git_names: HashSet<&str> = node.git.iter().map(|g| g.name.as_str()).collect();

    // Top-level entries FIRST (no `path`), whatever the order of the list: the
    // hoisted `node_modules/<name>` must be the lockfile's own top-level version,
    // never a nested one that happens to be listed earlier. Then the nested
    // installs, shallowest first so a parent exists before its own nested dir.
    let top: Vec<&PackageEntry> = node.packages.iter().filter(|p| p.path.is_none()).collect();
    let mut nested: Vec<&PackageEntry> =
        node.packages.iter().filter(|p| p.path.is_some()).collect();
    nested.sort_by_key(|p| {
        p.path
            .as_deref()
            .map_or(0, |s| s.matches("node_modules/").count())
    });

    let mut placed: HashSet<std::path::PathBuf> = HashSet::new();
    for pkg in top.into_iter().chain(nested) {
        let src = store_root.join(&pkg.store_path);
        if !src.exists() {
            tracing::warn!(pkg = %pkg.name, "Store path missing, skipping: {}", src.display());
            continue;
        }
        let content_dir = package_content_root(&src);

        let dest = match pkg.path.as_deref() {
            // Nested: <env>/node_modules/a/node_modules/b, exactly the lockfile key.
            Some(p) => {
                let rel = safe_rel_path(p).with_context(|| {
                    format!("invalid nested install path '{}' for {}", p, pkg.name)
                })?;
                let dest = env_dir.join(rel);
                // The parent package must be installed (it may have been skipped,
                // e.g. a platform-incompatible optional): never create a bare dir.
                let parent_key = &p[..p.rfind("/node_modules/").unwrap_or(0)];
                if !env_dir.join(parent_key).join("package.json").is_file() {
                    debug!(pkg = %pkg.name, path = %p, "Parent package not installed, skipping nested");
                    continue;
                }
                dest
            }
            None => nm.join(&pkg.name),
        };
        // A git dependency owns its `node_modules/<name>` link: the parser never
        // records a registry entry under that name, but an edited kong.rules
        // must not replace the link with a registry copy.
        if pkg.path.is_none() && git_names.contains(pkg.name.as_str()) {
            debug!(pkg = %pkg.name, "Git dependency of the same name, skipping registry entry");
            continue;
        }
        // Two entries for one location (old kong.rules with duplicate top-level
        // names): the first wins, as before — never flip-flop between them.
        if !placed.insert(dest.clone()) {
            debug!(pkg = %pkg.name, "Already linked, skipping");
            continue;
        }
        // Was a LOCAL package on a previous `kong use` (now a registry one):
        // drop the link itself — never what it points at.
        if is_dir_link(&dest) {
            remove_dir_link(&dest)?;
        }
        if dest.exists() {
            // Unreadable package.json: leave it alone (old behaviour).
            let wrong = installed_version(&dest).map_or(false, |v| v != pkg.version);
            if !wrong {
                debug!(pkg = %pkg.name, "Already linked, skipping");
                continue;
            }
            // Wrong version left by an older kong (nested version hoisted over
            // the top-level one). It is the env's own tree (hard links), so
            // removing it cannot touch the store.
            info!(pkg = %pkg.name, want = %pkg.version, at = %dest.display(), "Replacing stale install");
            link::remove_dir_all_robust(&dest)
                .with_context(|| format!("failed to replace stale {}", dest.display()))?;
        }

        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        link::link_package(&content_dir, &dest)
            .with_context(|| format!("failed to link node package {}", pkg.name))?;
        debug!(pkg = %pkg.name, at = %dest.display(), "Linked into node_modules");
    }

    info!("node_modules ready at {}", nm.display());

    // ── Create .bin/ shims for executables ───────────────────────────────────
    // npm creates node_modules/.bin/<name> → ../<pkg>/bin/script.js for every
    // "bin" entry in a package's package.json. We replicate that here.
    let bin_dir = nm.join(".bin");
    std::fs::create_dir_all(&bin_dir)?;

    // Bins of registry, local AND git packages: npm links all of them.
    let bin_owners = node
        .packages
        .iter()
        .map(|p| p.name.as_str())
        .chain(node.local.iter().map(|l| l.name.as_str()))
        .chain(node.git.iter().map(|g| g.name.as_str()));
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
            continue;
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

/// `version` field of `<dir>/package.json`, if readable.
fn installed_version(dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(dir.join("package.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    v.get("version")?.as_str().map(|s| s.to_string())
}

/// A lockfile key as a relative path that cannot escape the env.
fn safe_rel_path(p: &str) -> Result<std::path::PathBuf> {
    let mut out = std::path::PathBuf::new();
    for seg in p.split('/') {
        if seg.is_empty() || seg == "." || seg == ".." || seg.contains('\\') || seg.contains(':') {
            bail!("unsafe segment '{seg}'");
        }
        out.push(seg);
    }
    if !p.starts_with("node_modules/") {
        bail!("must start with node_modules/");
    }
    Ok(out)
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
                lp.name,
                lp.path,
                project_dir.display(),
                target.display()
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
                format!(
                    "failed to replace {} with a link to local package '{}'",
                    dst.display(),
                    lp.name
                )
            })?;
        }

        link::link_dir(&target, &dst).with_context(|| {
            format!(
                "failed to link local Node package '{}': {} -> {}",
                lp.name,
                dst.display(),
                target.display()
            )
        })?;
        info!(pkg = %lp.name, target = %target.display(), "Linked local package");
    }
    Ok(())
}

/// Link each git dependency at `node_modules/<name>` → its package directory
/// inside the store checkout (`git_link_target`).
///
/// The link goes to the REAL checkout path, not a copy: Node and Vite resolve
/// symlinks, so `require()` from the package walks up into the checkout's own
/// `node_modules` (installed by `kong rules`). Same idempotent rules as
/// `link_local_packages`: a correct link is kept, a stale one replaced, a real
/// env directory removed; the checkout itself is never written to.
pub fn link_git_packages(nm: &Path, git: &[GitPackageEntry], store_root: &Path) -> Result<()> {
    for g in git {
        crate::node::git::check_prepared(store_root, g)?;
        let target = crate::node::git::git_link_target(store_root, g);
        let dst = nm.join(&g.name);
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }

        if is_dir_link(&dst) {
            if link_points_to(&dst, &target) {
                debug!(pkg = %g.name, "Git link already correct");
                continue;
            }
            remove_dir_link(&dst)?;
        } else if dst.exists() {
            // The env's own tree (a previous registry or solidified install):
            // hard links only, the store keeps its copy.
            link::remove_dir_all_robust(&dst).with_context(|| {
                format!(
                    "failed to replace {} with a link to git dependency '{}'",
                    dst.display(),
                    g.name
                )
            })?;
        }

        link::link_dir(&target, &dst).with_context(|| {
            format!(
                "failed to link git dependency '{}': {} -> {}",
                g.name,
                dst.display(),
                target.display()
            )
        })?;
        info!(pkg = %g.name, commit = %&g.commit[..g.commit.len().min(12)], target = %target.display(), "Linked git dependency");
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
        LocalPackageEntry {
            name: name.into(),
            version: "0.1.0".into(),
            path: path.into(),
        }
    }

    /// Layout: <root>/vendor/atoms (the local package) and <root>/app (the
    /// project, declaring `file:../vendor/atoms`), env dir elsewhere.
    fn fixture() -> (
        tempfile::TempDir,
        std::path::PathBuf,
        std::path::PathBuf,
        std::path::PathBuf,
    ) {
        let root = tempfile::TempDir::new().unwrap();
        let vendor = root.path().join("vendor").join("atoms");
        std::fs::create_dir_all(vendor.join("bin")).unwrap();
        std::fs::write(
            vendor.join("package.json"),
            r#"{"name":"@real3d/atoms","version":"0.1.0","bin":{"atoms-cli":"bin/cli.js"}}"#,
        )
        .unwrap();
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
        let node = NodeSection {
            packages: vec![],
            local: vec![local("@real3d/atoms", "../vendor/atoms")],
            git: vec![],
        };
        let store = tempfile::TempDir::new().unwrap();
        build_node_modules(&env, &app, &node, store.path()).unwrap();

        let dst = env.join("node_modules").join("@real3d").join("atoms");
        assert!(
            is_dir_link(&dst),
            "node_modules/@real3d/atoms must be a junction/symlink"
        );
        assert_eq!(
            std::fs::canonicalize(&dst).unwrap(),
            std::fs::canonicalize(&vendor).unwrap(),
            "link must point at the local package directory"
        );
        // Live: a change in the source is visible through the link (not a copy).
        std::fs::write(vendor.join("marker.txt"), "edited").unwrap();
        assert_eq!(
            std::fs::read_to_string(dst.join("marker.txt")).unwrap(),
            "edited"
        );
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
        let node = NodeSection {
            packages: vec![],
            local: vec![local("@real3d/atoms", "../vendor/atoms")],
            git: vec![],
        };
        build_node_modules(&env, &app, &node, store.path()).unwrap();
        build_node_modules(&env, &app, &node, store.path()).unwrap(); // twice: no error

        // The dep moves to another checkout: the link is replaced, and the old
        // target is left intact (only the link entry is removed).
        let other = root.path().join("CRM_Atoms");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(
            other.join("package.json"),
            r#"{"name":"@real3d/atoms","version":"0.2.0"}"#,
        )
        .unwrap();
        let moved = NodeSection {
            packages: vec![],
            local: vec![local("@real3d/atoms", "../CRM_Atoms")],
            git: vec![],
        };
        build_node_modules(&env, &app, &moved, store.path()).unwrap();
        let dst = env.join("node_modules").join("@real3d").join("atoms");
        assert_eq!(
            std::fs::canonicalize(&dst).unwrap(),
            std::fs::canonicalize(&other).unwrap()
        );
        assert!(
            vendor.join("marker.txt").exists(),
            "old target must be untouched"
        );
    }

    #[test]
    fn use_errors_clearly_when_local_target_is_missing() {
        let (_root, _vendor, app, env) = fixture();
        let store = tempfile::TempDir::new().unwrap();
        let node = NodeSection {
            packages: vec![],
            local: vec![local("@real3d/atoms", "../nowhere/atoms")],
            git: vec![],
        };
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
        let node = NodeSection {
            packages: vec![],
            local: vec![local("@real3d/atoms", "../vendor/atoms")],
            git: vec![],
        };
        build_node_modules(&env, &app, &node, store.path()).unwrap();
        link::clean_environments(&env, false).unwrap();
        assert!(!env.join("node_modules").exists());
        assert!(
            vendor.join("marker.txt").exists(),
            "local package source must survive a clean"
        );
        assert!(vendor.join("package.json").exists());
    }

    // ── nested installs ────────────────────────────────────────────────────

    use crate::config::PackageEntry;

    /// Put a fake npm package (conventional `package/` layout) in the store.
    fn store_pkg(store: &Path, name: &str, version: &str) -> PackageEntry {
        let store_path = format!("node/libs/{}-{}", name.replace('/', "+"), version);
        let dir = store.join(&store_path).join("package");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("package.json"),
            format!(r#"{{"name":"{name}","version":"{version}"}}"#),
        )
        .unwrap();
        std::fs::write(
            dir.join("index.js"),
            format!("module.exports='{name}@{version}'"),
        )
        .unwrap();
        PackageEntry {
            name: name.into(),
            version: version.into(),
            hash: None,
            store_path,
            source_url: None,
            path: None,
        }
    }

    fn nested(mut e: PackageEntry, path: &str) -> PackageEntry {
        e.path = Some(path.into());
        e
    }

    fn version_at(env: &Path, rel: &str) -> String {
        let t = std::fs::read_to_string(env.join(rel).join("package.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&t).unwrap();
        v["version"].as_str().unwrap().to_string()
    }

    fn nested_packages(store: &Path) -> Vec<PackageEntry> {
        vec![
            store_pkg(store, "path-scurry", "1.2.0"),
            nested(
                store_pkg(store, "lru-cache", "11.5.2"),
                "node_modules/path-scurry/node_modules/lru-cache",
            ),
            store_pkg(store, "lru-cache", "5.1.1"),
        ]
    }

    fn tree(root: &Path) -> Vec<std::path::PathBuf> {
        walkdir::WalkDir::new(root)
            .sort_by_file_name()
            .into_iter()
            .filter_map(|e| e.ok())
            .map(|e| e.path().strip_prefix(root).unwrap().to_path_buf())
            .collect()
    }

    #[test]
    fn nested_install_keeps_own_version_whatever_the_order() {
        let store = tempfile::TempDir::new().unwrap();
        let base = nested_packages(store.path());
        let perms: [[usize; 3]; 6] = [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ];
        for perm in perms {
            let env = tempfile::TempDir::new().unwrap();
            let node = NodeSection {
                packages: perm.iter().map(|&i| base[i].clone()).collect(),
                local: vec![],
                git: vec![],
            };
            build_node_modules(env.path(), env.path(), &node, store.path()).unwrap();
            assert_eq!(
                version_at(env.path(), "node_modules/lru-cache"),
                "5.1.1",
                "order {perm:?}"
            );
            assert_eq!(
                version_at(
                    env.path(),
                    "node_modules/path-scurry/node_modules/lru-cache"
                ),
                "11.5.2",
                "order {perm:?}"
            );
        }
    }

    #[test]
    fn nested_scoped_install_and_skipped_parent() {
        let store = tempfile::TempDir::new().unwrap();
        let env = tempfile::TempDir::new().unwrap();
        let node = NodeSection {
            packages: vec![
                nested(
                    store_pkg(store.path(), "@scope/inner", "3.0.0"),
                    "node_modules/@scope/outer/node_modules/@scope/inner",
                ),
                store_pkg(store.path(), "@scope/outer", "2.0.0"),
                // parent never installed: the nested entry is skipped, no stray dir.
                nested(
                    store_pkg(store.path(), "orphan", "1.0.0"),
                    "node_modules/missing/node_modules/orphan",
                ),
            ],
            local: vec![],
            git: vec![],
        };
        build_node_modules(env.path(), env.path(), &node, store.path()).unwrap();
        assert_eq!(
            version_at(
                env.path(),
                "node_modules/@scope/outer/node_modules/@scope/inner"
            ),
            "3.0.0"
        );
        assert!(!env.path().join("node_modules/missing").exists());
    }

    #[test]
    fn nested_use_is_idempotent_and_never_writes_the_store() {
        let store = tempfile::TempDir::new().unwrap();
        let env = tempfile::TempDir::new().unwrap();
        let node = NodeSection {
            packages: nested_packages(store.path()),
            local: vec![],
            git: vec![],
        };
        let before = tree(store.path());
        build_node_modules(env.path(), env.path(), &node, store.path()).unwrap();
        let first = tree(env.path());
        build_node_modules(env.path(), env.path(), &node, store.path()).unwrap();
        assert_eq!(
            first,
            tree(env.path()),
            "second `kong use` must leave the same tree"
        );
        assert_eq!(before, tree(store.path()), "store must be untouched");
        assert!(!store
            .path()
            .join("node/libs/path-scurry-1.2.0/package/node_modules")
            .exists());
    }

    #[test]
    fn stale_wrong_toplevel_from_older_kong_is_corrected() {
        let store = tempfile::TempDir::new().unwrap();
        let env = tempfile::TempDir::new().unwrap();
        let pkgs = nested_packages(store.path());
        // Old kong: 11.5.2 hoisted at top level, nothing nested.
        let mut old = NodeSection {
            packages: vec![pkgs[0].clone(), pkgs[1].clone()],
            local: vec![],
            git: vec![],
        };
        old.packages[1].path = None;
        build_node_modules(env.path(), env.path(), &old, store.path()).unwrap();
        assert_eq!(version_at(env.path(), "node_modules/lru-cache"), "11.5.2");

        let node = NodeSection {
            packages: pkgs,
            local: vec![],
            git: vec![],
        };
        build_node_modules(env.path(), env.path(), &node, store.path()).unwrap();
        assert_eq!(version_at(env.path(), "node_modules/lru-cache"), "5.1.1");
        assert_eq!(
            version_at(
                env.path(),
                "node_modules/path-scurry/node_modules/lru-cache"
            ),
            "11.5.2"
        );
        assert!(store
            .path()
            .join("node/libs/lru-cache-11.5.2/package/index.js")
            .is_file());
    }

    // ── git dependencies ───────────────────────────────────────────────────

    const GIT_STORE_PATH: &str = "node/git/r-1234abcd/0123456789ab";

    fn git_entry(lock_dir: Option<&str>) -> GitPackageEntry {
        GitPackageEntry {
            name: "@fx/core".into(),
            version: "1.0.0".into(),
            source: "git+file:///tmp/r#v1.0.0".into(),
            repo: "file:///tmp/r".into(),
            reference: Some("v1.0.0".into()),
            commit: "0123456789abcdef0123456789abcdef01234567".into(),
            path: "packages/core".into(),
            build: None,
            rust_targets: vec![],
            declared_by: "package.json".into(),
            store_path: GIT_STORE_PATH.into(),
            lock_dir: lock_dir.map(Into::into),
            packages: vec![],
            local: vec![],
        }
    }

    /// A hand-made store commit dir: checkout with `src/packages/core` (with a
    /// bin), `.kong-cloned`, and `.kong-installed` when `installed`.
    fn git_store(installed: bool) -> (tempfile::TempDir, std::path::PathBuf) {
        let store = tempfile::TempDir::new().unwrap();
        let commit_dir = store.path().join(GIT_STORE_PATH);
        let pkg = commit_dir.join("src/packages/core");
        std::fs::create_dir_all(pkg.join("bin")).unwrap();
        std::fs::write(
            pkg.join("package.json"),
            r#"{"name":"@fx/core","version":"1.0.0","bin":{"fx-core":"bin/cli.js"}}"#,
        )
        .unwrap();
        std::fs::write(pkg.join("bin/cli.js"), "console.log(1)").unwrap();
        std::fs::write(pkg.join("index.js"), "module.exports=1").unwrap();
        std::fs::write(commit_dir.join("src/README.md"), "repo").unwrap();
        std::fs::write(commit_dir.join(".kong-cloned"), "{}").unwrap();
        if installed {
            std::fs::write(commit_dir.join(".kong-installed"), "{}").unwrap();
        }
        (store, pkg)
    }

    fn git_node(lock_dir: Option<&str>) -> NodeSection {
        NodeSection {
            packages: vec![],
            local: vec![],
            git: vec![git_entry(lock_dir)],
        }
    }

    fn file_count(root: &Path) -> usize {
        walkdir::WalkDir::new(root)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .count()
    }

    #[test]
    fn git_link_points_at_store_checkout_subdir() {
        let (store, pkg) = git_store(true);
        let env = tempfile::TempDir::new().unwrap();
        build_node_modules(env.path(), env.path(), &git_node(Some("")), store.path()).unwrap();
        let dst = env.path().join("node_modules/@fx/core");
        assert!(is_dir_link(&dst));
        assert_eq!(
            std::fs::canonicalize(&dst).unwrap(),
            std::fs::canonicalize(&pkg).unwrap()
        );
        #[cfg(unix)]
        assert!(std::fs::symlink_metadata(env.path().join("node_modules/.bin/fx-core")).is_ok());
        #[cfg(windows)]
        assert!(env.path().join("node_modules/.bin/fx-core.cmd").exists());
    }

    #[test]
    fn git_link_rerun_is_a_noop() {
        let (store, _pkg) = git_store(true);
        let env = tempfile::TempDir::new().unwrap();
        let node = git_node(Some(""));
        build_node_modules(env.path(), env.path(), &node, store.path()).unwrap();
        let first = tree(env.path());
        let store_before = tree(store.path());
        build_node_modules(env.path(), env.path(), &node, store.path()).unwrap();
        assert_eq!(first, tree(env.path()));
        assert_eq!(store_before, tree(store.path()));
    }

    #[test]
    fn git_link_replaces_stale_link_and_real_dir() {
        let (store, pkg) = git_store(true);
        let env = tempfile::TempDir::new().unwrap();
        let dst = env.path().join("node_modules/@fx/core");
        std::fs::create_dir_all(dst.parent().unwrap()).unwrap();

        // Stale link to another directory: replaced, its target untouched.
        let other = env.path().join("elsewhere");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("keep.txt"), "x").unwrap();
        link::link_dir(&other, &dst).unwrap();
        build_node_modules(env.path(), env.path(), &git_node(None), store.path()).unwrap();
        assert_eq!(
            std::fs::canonicalize(&dst).unwrap(),
            std::fs::canonicalize(&pkg).unwrap()
        );
        assert!(other.join("keep.txt").exists());

        // A real env directory (old registry install): replaced, store untouched.
        remove_dir_link(&dst).unwrap();
        std::fs::create_dir_all(&dst).unwrap();
        std::fs::write(dst.join("package.json"), r#"{"version":"0.9.0"}"#).unwrap();
        let store_before = tree(store.path());
        build_node_modules(env.path(), env.path(), &git_node(None), store.path()).unwrap();
        assert!(is_dir_link(&dst));
        assert_eq!(
            std::fs::canonicalize(&dst).unwrap(),
            std::fs::canonicalize(&pkg).unwrap()
        );
        assert_eq!(store_before, tree(store.path()));
    }

    #[test]
    fn git_link_missing_install_marker_errors_with_hint() {
        let (store, _pkg) = git_store(false);
        let env = tempfile::TempDir::new().unwrap();
        let err = build_node_modules(env.path(), env.path(), &git_node(Some("")), store.path())
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("@fx/core"), "{msg}");
        assert!(msg.contains("run `kong rules`"), "{msg}");
        assert!(!env.path().join("node_modules/@fx/core").exists());
    }

    #[test]
    fn git_link_registry_entry_of_same_name_is_skipped() {
        let (store, pkg) = git_store(true);
        let env = tempfile::TempDir::new().unwrap();
        let mut node = git_node(None);
        node.packages = vec![store_pkg(store.path(), "@fx/core", "0.5.0")];
        build_node_modules(env.path(), env.path(), &node, store.path()).unwrap();
        assert_eq!(
            std::fs::canonicalize(env.path().join("node_modules/@fx/core")).unwrap(),
            std::fs::canonicalize(&pkg).unwrap()
        );
    }

    #[test]
    fn clean_environments_never_deletes_through_a_git_link() {
        let (store, _pkg) = git_store(true);
        let env = tempfile::TempDir::new().unwrap();
        let checkout = store.path().join(GIT_STORE_PATH);
        let before = file_count(&checkout);
        build_node_modules(env.path(), env.path(), &git_node(Some("")), store.path()).unwrap();
        link::clean_environments(env.path(), false).unwrap();
        assert!(!env.path().join("node_modules").exists());
        assert_eq!(before, file_count(&checkout));
        assert!(checkout.join("src/packages/core/package.json").is_file());
    }
}
