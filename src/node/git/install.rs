//! Installs a git dependency's own dependencies from its package-lock.json into the
//! checkout, sharing registry packages with every project through `node/libs`.

use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result};
use tracing::{debug, info};

use super::spec::{commit12, GitSource};
use crate::config::{LocalPackageEntry, NodeSection, PackageEntry};
use crate::node::platform::HostTriple;

const INSTALL_MARKER: &str = ".kong-installed";
const DEP_SECTIONS: [&str; 3] = ["dependencies", "devDependencies", "optionalDependencies"];

#[derive(Debug, Clone)]
pub struct InstalledDeps {
    /// Relative to the checkout, `""` = root; `None` = no deps declared and no lockfile.
    pub lock_dir: Option<String>,
    pub packages: Vec<PackageEntry>,
    pub local: Vec<LocalPackageEntry>,
    /// False when `.kong-installed` already existed and `force` was not given.
    pub installed_now: bool,
}

/// First directory holding a `package-lock.json`, walking from `<checkout>/<subdir>`
/// up to `<checkout>` (both included). npm workspaces keep one lockfile at the root,
/// so a package inside a monorepo usually resolves to `""`.
pub fn find_lock_dir(checkout: &Path, subdir: &str) -> Option<String> {
    let mut parts: Vec<&str> = subdir.split('/').filter(|s| !s.is_empty()).collect();
    loop {
        let rel = parts.join("/");
        if checkout.join(&rel).join("package-lock.json").is_file() {
            return Some(rel);
        }
        parts.pop()?;
    }
}

/// Install the dependency's own lockfile tree into `<checkout>/<lock_dir>/node_modules`.
///
/// Node resolves a linked package from its real path, so the tree must live in the
/// checkout and not in the project. `build_node_modules` runs every time (it is
/// idempotent and repairs a partially removed tree); the marker only records that
/// the first install completed.
pub fn install_own_dependencies(
    checkout: &Path,
    commit_dir: &Path,
    src: &GitSource,
    commit: &str,
    store_root: &Path,
    force: bool,
    host: &HostTriple,
) -> Result<InstalledDeps> {
    let started = Instant::now();
    let Some(lock_dir) = find_lock_dir(checkout, &src.path) else {
        let pkg_json = checkout.join(&src.path).join("package.json");
        if let Some(dep) = first_declared_dependency(&pkg_json) {
            return Err(install_error(
                src,
                commit,
                "has no package-lock.json",
                &[
                    ("path", display_path(&src.path)),
                    ("checkout", checkout.display().to_string()),
                    ("dependency", dep),
                ],
                "kong installs a git dependency's own dependencies from its package-lock.json only",
            ));
        }
        debug!(pkg = %src.name, "No lockfile and no dependencies declared, nothing to install");
        return Ok(InstalledDeps {
            lock_dir: None,
            packages: Vec::new(),
            local: Vec::new(),
            installed_now: false,
        });
    };

    let lock_abs = checkout.join(&lock_dir);
    let lock_file = lock_abs.join("package-lock.json");
    let deps = crate::node::parser::parse_package_lock(&lock_file)
        .and_then(|d| crate::node::parser::filter_for_host(d, host))
        .map_err(|e| {
            install_error(
                src,
                commit,
                "has an unreadable package-lock.json",
                &[
                    ("lockfile", lock_file.display().to_string()),
                    ("reason", format!("{e:#}")),
                ],
                "Fix the dependency's package-lock.json at that commit, or pin another ref.",
            )
        })?;

    let mut packages = Vec::new();
    let mut local = Vec::new();
    for dep in &deps {
        let resolved = dep._resolved.as_deref().unwrap_or("");
        if ["git+", "git:", "github:"]
            .iter()
            .any(|p| resolved.starts_with(p))
        {
            return Err(install_error(
                src,
                commit,
                &format!(
                    "cannot be installed: nested git dependency '{}' in {}'s lockfile is not supported",
                    dep.name, src.name
                ),
                &[("lockfile", lock_file.display().to_string()), ("resolved", resolved.to_string())],
                "Publish the nested package to a registry or vendor it into the repository.",
            ));
        }
        match &dep.local {
            Some(ls) => local.push(crate::config::local_entry_for(&lock_abs, &dep.name, ls)?),
            None => packages.push(crate::config::node_package_entry(dep, store_root, force)?),
        }
    }
    packages.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.path.cmp(&b.path)));
    local.sort_by(|a, b| a.name.cmp(&b.name));

    let node = NodeSection {
        packages,
        local,
        git: vec![], tarballs: vec![],
    };
    crate::node::modules::build_node_modules(&lock_abs, &lock_abs, &node, store_root)
        .with_context(|| {
            format!(
                "installing the own dependencies of git dependency '{}'",
                src.name
            )
        })?;

    let marker = commit_dir.join(INSTALL_MARKER);
    let installed_now = force || !marker.exists();
    if installed_now {
        let seconds = started.elapsed().as_secs_f64();
        let body = serde_json::json!({
            "lock_dir": lock_dir,
            "packages": node.packages.len(),
            "local": node.local.len(),
            "seconds": seconds,
        });
        std::fs::write(&marker, serde_json::to_string_pretty(&body)?)
            .with_context(|| format!("failed to write {}", marker.display()))?;
        info!(
            pkg = %src.name, commit = %commit12(commit), packages = node.packages.len(),
            local = node.local.len(),
            "Installed {} {} in {:.1} s", src.name, commit12(commit), seconds
        );
    } else {
        debug!(pkg = %src.name, commit = %commit12(commit), "Own dependencies already installed");
    }

    Ok(InstalledDeps {
        lock_dir: Some(lock_dir),
        packages: node.packages,
        local: node.local,
        installed_now,
    })
}

/// `name` of the first dependency the package.json declares, if any.
fn first_declared_dependency(pkg_json: &Path) -> Option<String> {
    let txt = std::fs::read_to_string(pkg_json).ok()?;
    let v: serde_json::Value = serde_json::from_str(&txt).ok()?;
    DEP_SECTIONS.iter().find_map(|section| {
        v.get(*section)
            .and_then(|d| d.as_object())
            .and_then(|d| d.keys().next().cloned())
    })
}

fn display_path(p: &str) -> String {
    if p.is_empty() {
        "(repository root)".into()
    } else {
        p.to_string()
    }
}

/// Shared §6 diagnostic, scoped to the install step.
fn install_error(
    src: &GitSource,
    commit: &str,
    what: &str,
    facts: &[(&str, String)],
    hint: &str,
) -> anyhow::Error {
    let mut text = format!(
        "git dependency '{}' {what}:\n  source      : {}\n  commit      : {commit}\n  declared by : {}\n",
        src.name, src.spec, src.origin
    );
    for (label, value) in facts {
        let value = value.replace('\n', &format!("\n{:16}", ""));
        text.push_str(&format!("  {label:<11} : {value}\n"));
    }
    text.push_str(hint);
    anyhow::anyhow!(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::git::proof_support::{fixture_rollback_repo, seed_store_package};
    use crate::node::git::spec::GitRef;
    use tempfile::TempDir;

    const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

    fn source(path: &str) -> GitSource {
        GitSource {
            name: "@fx/core".into(),
            spec: "git+file:///fixture/remote.git#v1.0.0".into(),
            repo: "file:///fixture/remote.git".into(),
            reference: GitRef::Name("v1.0.0".into()),
            path: path.into(),
            build: None,
            rust_targets: vec![],
            origin: r#"package.json dependencies "@fx/core": "git+file:///fixture/remote.git#v1.0.0" (kong.git "@fx/core")"#.into(),
            declared_by: "package.json".into(),
        }
    }

    fn seed(store: &Path) {
        seed_store_package(
            store,
            "is-fixture",
            "1.0.0",
            &[
                ("package.json", r#"{"name":"is-fixture","version":"1.0.0"}"#),
                ("index.js", "exports.name = 'is-fixture';\n"),
            ],
        );
    }

    fn write(dir: &Path, rel: &str, content: &str) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    fn install(
        checkout: &Path,
        commit_dir: &Path,
        path: &str,
        store: &Path,
    ) -> Result<InstalledDeps> {
        install_own_dependencies(
            checkout,
            commit_dir,
            &source(path),
            COMMIT,
            store,
            false,
            &HostTriple::current(),
        )
    }

    #[test]
    fn root_lockfile_installs_registry_and_workspace_deps_once() {
        let fx = fixture_rollback_repo();
        let store = TempDir::new().unwrap();
        let commit_dir = TempDir::new().unwrap();
        seed(store.path());

        assert_eq!(
            find_lock_dir(&fx.work, "packages/core"),
            Some(String::new())
        );
        let first = install(&fx.work, commit_dir.path(), "packages/core", store.path()).unwrap();
        assert_eq!(first.lock_dir.as_deref(), Some(""));
        assert!(first.installed_now);
        let nm = fx.work.join("node_modules");
        assert!(nm.join("is-fixture/package.json").is_file());
        assert!(
            nm.join("@fx/util/package.json").is_file(),
            "@fx/util link missing"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let meta = std::fs::metadata(nm.join("is-fixture/index.js")).unwrap();
            assert!(
                meta.nlink() >= 2,
                "is-fixture/index.js is not a hard link (nlink={})",
                meta.nlink()
            );
        }

        let marker: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(commit_dir.path().join(INSTALL_MARKER)).unwrap(),
        )
        .unwrap();
        assert_eq!(marker["lock_dir"], "");
        assert_eq!(marker["packages"], 1);
        assert_eq!(marker["local"], 2);
        assert!(marker["seconds"].is_number());

        let second = install(&fx.work, commit_dir.path(), "packages/core", store.path()).unwrap();
        assert!(!second.installed_now);
        assert_eq!(second.packages, first.packages);
        assert_eq!(second.local, first.local);
    }

    #[test]
    fn outputs_are_sorted_by_name() {
        let checkout = TempDir::new().unwrap();
        let store = TempDir::new().unwrap();
        let commit_dir = TempDir::new().unwrap();
        for n in ["zeta", "alpha", "mid"] {
            seed_store_package(store.path(), n, "1.0.0", &[("package.json", "{}")]);
        }
        for n in ["z-local", "a-local"] {
            write(
                checkout.path(),
                &format!("libs/{n}/package.json"),
                r#"{"version":"1.0.0"}"#,
            );
        }
        let lock = serde_json::json!({
            "lockfileVersion": 3,
            "packages": {
                "": {"name": "root"},
                "node_modules/zeta": {"version": "1.0.0", "resolved": "https://registry.npmjs.org/zeta/-/zeta-1.0.0.tgz"},
                "node_modules/z-local": {"resolved": "libs/z-local", "link": true},
                "node_modules/alpha": {"version": "1.0.0", "resolved": "https://registry.npmjs.org/alpha/-/alpha-1.0.0.tgz"},
                "node_modules/a-local": {"resolved": "libs/a-local", "link": true},
                "node_modules/mid": {"version": "1.0.0", "resolved": "https://registry.npmjs.org/mid/-/mid-1.0.0.tgz"}
            }
        });
        write(checkout.path(), "package-lock.json", &lock.to_string());
        let got = install(checkout.path(), commit_dir.path(), "", store.path()).unwrap();
        let names: Vec<&str> = got.packages.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["alpha", "mid", "zeta"]);
        let locals: Vec<&str> = got.local.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(locals, ["a-local", "z-local"]);
    }

    #[test]
    fn missing_lockfile_with_dependencies_is_an_error() {
        let checkout = TempDir::new().unwrap();
        let store = TempDir::new().unwrap();
        let commit_dir = TempDir::new().unwrap();
        write(
            checkout.path(),
            "packages/core/package.json",
            r#"{"name":"@fx/core","devDependencies":{"left-pad":"1.0.0"}}"#,
        );
        let err = install(
            checkout.path(),
            commit_dir.path(),
            "packages/core",
            store.path(),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("git dependency '@fx/core'"), "{err}");
        assert!(err.contains("left-pad"), "{err}");
        assert!(
            err.contains("git+file:///fixture/remote.git#v1.0.0"),
            "{err}"
        );
        assert!(err.contains(COMMIT), "{err}");
        assert!(err.contains("from its package-lock.json only"), "{err}");
        assert!(!commit_dir.path().join(INSTALL_MARKER).exists());
    }

    #[test]
    fn no_dependencies_and_no_lockfile_installs_nothing() {
        let checkout = TempDir::new().unwrap();
        let store = TempDir::new().unwrap();
        let commit_dir = TempDir::new().unwrap();
        write(
            checkout.path(),
            "package.json",
            r#"{"name":"@fx/core","version":"1.0.0"}"#,
        );
        let got = install(checkout.path(), commit_dir.path(), "", store.path()).unwrap();
        assert_eq!(got.lock_dir, None);
        assert!(got.packages.is_empty() && got.local.is_empty());
        assert!(!checkout.path().join("node_modules").exists());
        assert!(!commit_dir.path().join(INSTALL_MARKER).exists());
    }

    #[test]
    fn nested_git_dependency_is_rejected() {
        let checkout = TempDir::new().unwrap();
        let store = TempDir::new().unwrap();
        let commit_dir = TempDir::new().unwrap();
        let lock = serde_json::json!({
            "lockfileVersion": 3,
            "packages": {
                "": {"name": "root"},
                "node_modules/inner": {
                    "version": "2.0.0",
                    "resolved": "git+https://github.com/org/inner.git#0123456789abcdef0123456789abcdef01234567"
                }
            }
        });
        write(checkout.path(), "package-lock.json", &lock.to_string());
        let err = install(checkout.path(), commit_dir.path(), "", store.path())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("nested git dependency 'inner' in @fx/core's lockfile is not supported"),
            "{err}"
        );
        assert!(err.contains("declared by :"), "{err}");
    }

    #[test]
    fn lock_dir_prefers_the_nearest_lockfile() {
        let checkout = TempDir::new().unwrap();
        write(checkout.path(), "package-lock.json", "{}");
        write(checkout.path(), "packages/core/package-lock.json", "{}");
        assert_eq!(
            find_lock_dir(checkout.path(), "packages/core"),
            Some("packages/core".into())
        );
        assert_eq!(
            find_lock_dir(checkout.path(), "packages/util"),
            Some(String::new())
        );
        let empty = TempDir::new().unwrap();
        assert_eq!(find_lock_dir(empty.path(), ""), None);
    }
}
