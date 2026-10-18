//! Node git dependencies declared in `package.json`, cloned once per commit into
//! `<store>/node/git`, then linked into projects by `kong use`.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::Result;
use tracing::debug;

use crate::config::GitPackageEntry;
use crate::node::platform::HostTriple;

pub mod fetch;
pub mod install;
pub mod spec;

#[cfg(test)]
pub(crate) mod proof_support;

/// Keep all build inputs explicit so builders need no process-global setup.
pub struct BuildRequest {
    pub name: String,
    pub source: String,
    pub commit: String,
    pub origin: String,
    pub build: String,
    pub checkout: PathBuf,
    pub commit_dir: PathBuf,
    pub package_dir: PathBuf,
    pub lock_dir: Option<PathBuf>,
    pub rust_targets: Vec<String>,
    pub store_root: PathBuf,
    pub marker: PathBuf,
    pub log: PathBuf,
}

pub struct BuildRecord {
    pub seconds: f64,
    pub log: PathBuf,
}

pub type BuildFn = fn(&BuildRequest) -> Result<BuildRecord>;

#[derive(Clone, Copy)]
pub struct GitHooks {
    pub git_env: fn() -> fetch::GitEnv,
    pub build: Option<BuildFn>,
}

static HOOKS: OnceLock<GitHooks> = OnceLock::new();

/// The first installation wins so later callers cannot replace CLI policy.
pub fn install_hooks(h: GitHooks) {
    if HOOKS.set(h).is_err() {
        debug!("Git hooks already installed; ignoring second installation");
    }
}

/// Default to Git's inherited authentication and no build integration.
pub fn hooks() -> GitHooks {
    HOOKS.get().copied().unwrap_or(GitHooks {
        git_env: fetch::GitEnv::default,
        build: None,
    })
}

pub struct PrepareOptions<'a> {
    pub update: bool,
    pub force: bool,
    pub git_env: fetch::GitEnv,
    pub builder: Option<&'a dyn Fn(&BuildRequest) -> Result<BuildRecord>>,
    pub host: HostTriple,
}

#[derive(Debug, Default, Clone)]
pub struct PrepareReport {
    pub resolved_remote: u32,
    pub cloned: u32,
    pub installed: u32,
    pub built: u32,
    pub clone_seconds: f64,
    pub build_seconds: f64,
}

pub fn prepare_all(
    _store_root: &Path,
    _sources: &[spec::GitSource],
    _previous: &[GitPackageEntry],
    _opts: &PrepareOptions<'_>,
) -> Result<(Vec<GitPackageEntry>, PrepareReport)> {
    // The proof is written against this signature first; the next task implements it.
    anyhow::bail!("prepare_all is implemented by the next node-git-prepare task")
}

/// Link directly to the package in the checkout so Node finds its own dependencies.
pub fn git_link_target(store_root: &Path, e: &GitPackageEntry) -> PathBuf {
    let mut target = store_root.join(&e.store_path).join("src");
    for part in e.path.split('/').filter(|part| !part.is_empty()) {
        target.push(part);
    }
    target
}

/// Refuse incomplete store entries before `kong use` links a package into a project.
pub fn check_prepared(store_root: &Path, e: &GitPackageEntry) -> Result<()> {
    let commit_dir = store_root.join(&e.store_path);
    let checkout = commit_dir.join("src");
    let mut markers = vec![".kong-cloned".to_owned()];
    if e.lock_dir.is_some() {
        markers.push(".kong-installed".to_owned());
    }
    if let Some(build) = &e.build {
        markers.push(format!(".kong-built-{}", spec::build_key(build)));
    }
    for marker in markers {
        if !commit_dir.join(&marker).is_file() {
            anyhow::bail!(
                "git dependency '{}' is missing preparation marker '{}':\n  source      : {}\n  commit      : {}\n  declared by : kong.rules node.git (declared in {})\n  checkout    : {}\nrun `kong rules` to fetch and build it",
                e.name,
                marker,
                e.source,
                e.commit,
                e.declared_by,
                checkout.display(),
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn entry() -> GitPackageEntry {
        GitPackageEntry {
            name: "@fixture/core".into(),
            version: "1.0.0".into(),
            source: "github:fixture/core#v1.0.0".into(),
            repo: "https://github.com/fixture/core.git".into(),
            reference: Some("v1.0.0".into()),
            commit: "0123456789abcdef0123456789abcdef01234567".into(),
            path: "packages/core".into(),
            build: Some("scripts/build-package.sh".into()),
            rust_targets: vec![],
            declared_by: "apps/web/package.json".into(),
            store_path: "node/git/core-fixture/0123456789ab".into(),
            lock_dir: Some("".into()),
            packages: vec![],
            local: vec![],
        }
    }

    fn write_markers(store: &Path, e: &GitPackageEntry, markers: &[&str]) {
        let commit_dir = store.join(&e.store_path);
        fs::create_dir_all(&commit_dir).unwrap();
        for marker in markers {
            fs::write(commit_dir.join(marker), "{}").unwrap();
        }
    }

    #[test]
    fn git_link_target_package_subdir() {
        let store = TempDir::new().unwrap();
        let e = entry();
        assert_eq!(
            git_link_target(store.path(), &e),
            store
                .path()
                .join(&e.store_path)
                .join("src")
                .join("packages")
                .join("core")
        );
    }

    #[test]
    fn git_link_target_checkout_root() {
        let store = TempDir::new().unwrap();
        let mut e = entry();
        e.path.clear();
        assert_eq!(
            git_link_target(store.path(), &e),
            store.path().join(&e.store_path).join("src")
        );
    }

    #[test]
    fn check_prepared_all_markers_present() {
        let store = TempDir::new().unwrap();
        let e = entry();
        let build_marker = format!(".kong-built-{}", spec::build_key(e.build.as_ref().unwrap()));
        write_markers(
            store.path(),
            &e,
            &[".kong-cloned", ".kong-installed", &build_marker],
        );
        check_prepared(store.path(), &e).unwrap();
    }

    #[test]
    fn check_prepared_missing_install_has_actionable_error() {
        let store = TempDir::new().unwrap();
        let e = entry();
        write_markers(store.path(), &e, &[".kong-cloned"]);
        let error = check_prepared(store.path(), &e).unwrap_err().to_string();
        for expected in [
            &e.name,
            &e.source,
            &e.commit,
            ".kong-installed",
            "declared by : kong.rules node.git (declared in apps/web/package.json)",
            "checkout    :",
            &store
                .path()
                .join(&e.store_path)
                .join("src")
                .display()
                .to_string(),
            "run `kong rules` to fetch and build it",
        ] {
            assert!(error.contains(expected), "missing {expected:?} in {error}");
        }
    }

    #[test]
    fn check_prepared_requires_matching_build_marker() {
        let store = TempDir::new().unwrap();
        let e = entry();
        let other_marker = format!(".kong-built-{}", spec::build_key("another build"));
        write_markers(
            store.path(),
            &e,
            &[".kong-cloned", ".kong-installed", &other_marker],
        );
        let error = check_prepared(store.path(), &e).unwrap_err().to_string();
        let expected = format!(".kong-built-{}", spec::build_key(e.build.as_ref().unwrap()));
        assert!(error.contains(&expected), "{error}");
    }

    #[test]
    fn check_prepared_requires_clone_first() {
        let store = TempDir::new().unwrap();
        let e = entry();
        write_markers(store.path(), &e, &[".kong-installed"]);
        let error = check_prepared(store.path(), &e).unwrap_err().to_string();
        assert!(error.contains(".kong-cloned"), "{error}");
    }

    #[test]
    fn check_prepared_without_install_or_build() {
        let store = TempDir::new().unwrap();
        let mut e = entry();
        e.lock_dir = None;
        e.build = None;
        write_markers(store.path(), &e, &[".kong-cloned"]);
        check_prepared(store.path(), &e).unwrap();
    }

    #[test]
    fn hooks_default_has_no_builder() {
        let h = hooks();
        assert!(h.build.is_none());
        let env = (h.git_env)();
        assert!(env.vars.is_empty());
        assert!(env.secrets.is_empty());
    }
}
