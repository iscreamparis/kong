//! Node git dependencies declared in `package.json`, cloned once per commit into
//! `<store>/node/git`, then linked into projects by `kong use`.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::Result;
use tracing::{debug, info, warn};

use crate::config::GitPackageEntry;
use crate::node::platform::HostTriple;

pub mod fetch;
pub mod install;
pub mod spec;

#[cfg(test)]
mod proof_m2;
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

/// Resolve, clone, install and build every git dependency, in name order.
///
/// `previous` is the `node.git` list of the last `kong.rules`: a matching entry keeps
/// its commit (lockfile semantics) unless `opts.update`, so a moved tag or branch
/// changes nothing until the user asks for it. Every store step is idempotent, so a
/// second run reports zero clones, installs, builds and remote resolutions.
pub fn prepare_all(
    store_root: &Path,
    sources: &[spec::GitSource],
    previous: &[GitPackageEntry],
    opts: &PrepareOptions<'_>,
) -> Result<(Vec<GitPackageEntry>, PrepareReport)> {
    let mut ordered: Vec<&spec::GitSource> = sources.iter().collect();
    ordered.sort_by(|a, b| a.name.cmp(&b.name));
    let mut report = PrepareReport::default();
    let mut entries = Vec::with_capacity(ordered.len());
    for src in ordered {
        entries.push(prepare_one(store_root, src, previous, opts, &mut report)?);
    }
    Ok((entries, report))
}

fn prepare_one(
    store_root: &Path,
    src: &spec::GitSource,
    previous: &[GitPackageEntry],
    opts: &PrepareOptions<'_>,
    report: &mut PrepareReport,
) -> Result<GitPackageEntry> {
    let env = &opts.git_env;
    let reference = src.reference.as_option();
    let prev = previous.iter().find(|p| {
        p.name == src.name && p.repo == src.repo && p.reference == reference && p.path == src.path
    });
    let (commit, resolution) = fetch::resolve_commit(src, prev, opts.update, env)
        .map_err(|e| in_context(src, None, "could not resolve its commit", e, env))?;
    if resolution == fetch::Resolution::Remote {
        report.resolved_remote += 1;
    }

    let checkout = fetch::ensure_checkout(store_root, src, &commit, env)
        .map_err(|e| in_context(src, Some(&commit), "could not be cloned", e, env))?;
    if checkout.cloned {
        report.cloned += 1;
        report.clone_seconds += checkout.seconds;
    }

    let package_dir = join_rel(&checkout.checkout, &src.path);
    let manifest = package_dir.join("package.json");
    if !manifest.is_file() {
        return Err(prepare_error(
            src,
            Some(&commit),
            "has no package.json at its path",
            &[
                ("path", display_path(&src.path)),
                ("checkout", checkout.checkout.display().to_string()),
            ],
            &format!(
                "Set `path` in kong.git \"{}\" to the repository directory that holds the package's package.json.",
                src.name
            ),
            env,
        ));
    }
    let version = read_version(src, &commit, &manifest, env)?;

    let installed = install::install_own_dependencies(
        &checkout.checkout,
        &checkout.commit_dir,
        src,
        &commit,
        store_root,
        opts.force,
        &opts.host,
    )
    .map_err(|e| {
        in_context(
            src,
            Some(&commit),
            "could not install its own dependencies",
            e,
            env,
        )
    })?;
    if installed.installed_now {
        report.installed += 1;
    }

    let mut built = false;
    if let Some(build) = &src.build {
        let key = spec::build_key(build);
        let marker = checkout.commit_dir.join(format!(".kong-built-{key}"));
        if !marker.is_file() {
            let Some(builder) = opts.builder else {
                return Err(prepare_error(
                    src,
                    Some(&commit),
                    "declares a build step but this kong runs no build steps",
                    &[
                        ("build", build.clone()),
                        ("checkout", checkout.checkout.display().to_string()),
                    ],
                    &format!(
                        "Build steps arrive with a later kong: upgrade kong, or remove `build` from kong.git \"{}\".",
                        src.name
                    ),
                    env,
                ));
            };
            let request = BuildRequest {
                name: src.name.clone(),
                source: src.spec.clone(),
                commit: commit.clone(),
                origin: src.origin.clone(),
                build: build.clone(),
                checkout: checkout.checkout.clone(),
                commit_dir: checkout.commit_dir.clone(),
                package_dir: package_dir.clone(),
                lock_dir: installed
                    .lock_dir
                    .as_deref()
                    .map(|d| join_rel(&checkout.checkout, d)),
                rust_targets: src.rust_targets.clone(),
                store_root: store_root.to_path_buf(),
                marker,
                log: checkout.commit_dir.join(format!("build-{key}.log")),
            };
            let record = builder(&request)
                .map_err(|e| in_context(src, Some(&commit), "build step failed", e, env))?;
            report.built += 1;
            report.build_seconds += record.seconds;
            built = true;
        }
    }

    info!(
        pkg = %src.name,
        commit = %spec::commit12(&commit),
        version = %version,
        cloned = checkout.cloned,
        installed = installed.installed_now,
        built,
        "Prepared git dependency {} {} ({})",
        src.name,
        spec::commit12(&commit),
        version
    );
    Ok(GitPackageEntry {
        name: src.name.clone(),
        version,
        source: src.spec.clone(),
        repo: src.repo.clone(),
        reference,
        store_path: fetch::store_path_for(&src.repo, &commit),
        commit,
        path: src.path.clone(),
        build: src.build.clone(),
        rust_targets: src.rust_targets.clone(),
        declared_by: src.declared_by.clone(),
        lock_dir: installed.lock_dir,
        packages: installed.packages,
        local: installed.local,
    })
}

/// The package is linked under the dependency name whatever its own `name` says
/// (npm does the same), so a mismatch is only worth a warning.
fn read_version(
    src: &spec::GitSource,
    commit: &str,
    manifest: &Path,
    env: &fetch::GitEnv,
) -> Result<String> {
    let parsed = std::fs::read_to_string(manifest)
        .map_err(anyhow::Error::from)
        .and_then(|text| Ok(serde_json::from_str::<serde_json::Value>(&text)?));
    let pkg = parsed.map_err(|e| {
        prepare_error(
            src,
            Some(commit),
            "has an unreadable package.json",
            &[
                ("path", display_path(&src.path)),
                ("manifest", manifest.display().to_string()),
                ("reason", e.to_string()),
            ],
            "Fix the package.json at that commit, or pin another ref.",
            env,
        )
    })?;
    if let Some(own) = pkg.get("name").and_then(|n| n.as_str()) {
        if own != src.name {
            warn!(
                pkg = %src.name,
                package_name = %own,
                "Git dependency {} is named '{}' in its package.json; linking it as {}",
                src.name,
                own,
                src.name
            );
        }
    }
    Ok(pkg
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("0.0.0")
        .to_owned())
}

/// `base` joined with a `/`-separated relative path (`""` = `base` itself).
fn join_rel(base: &Path, rel: &str) -> PathBuf {
    let mut path = base.to_path_buf();
    for part in rel.split('/').filter(|part| !part.is_empty()) {
        path.push(part);
    }
    path
}

fn display_path(p: &str) -> String {
    if p.is_empty() {
        "(repository root)".into()
    } else {
        p.to_owned()
    }
}

/// Shared §6 diagnostic for the steps `prepare_all` checks itself.
fn prepare_error(
    src: &spec::GitSource,
    commit: Option<&str>,
    what: &str,
    facts: &[(&str, String)],
    hint: &str,
    env: &fetch::GitEnv,
) -> anyhow::Error {
    let mut text = format!(
        "git dependency '{}' {what}:\n  source      : {}\n  commit      : {}\n  declared by : {}\n",
        src.name,
        src.spec,
        commit.unwrap_or("(unresolved)"),
        src.origin
    );
    for (label, value) in facts {
        let value = value.replace('\n', &format!("\n{:16}", ""));
        text.push_str(&format!("  {label:<11} : {value}\n"));
    }
    text.push_str(hint);
    anyhow::anyhow!(fetch::redact(&text, env))
}

/// Fetch, install and the builder already report shared §6 errors; anything else
/// (a store write, a registry download) is wrapped so it still names the dependency.
fn in_context(
    src: &spec::GitSource,
    commit: Option<&str>,
    what: &str,
    error: anyhow::Error,
    env: &fetch::GitEnv,
) -> anyhow::Error {
    if error
        .to_string()
        .starts_with(&format!("git dependency '{}' ", src.name))
    {
        return error;
    }
    prepare_error(
        src,
        commit,
        what,
        &[("reason", format!("{error:#}"))],
        "Fix the problem above, then run `kong rules` again.",
        env,
    )
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

    fn prepare_options(
        builder: Option<&dyn Fn(&BuildRequest) -> Result<BuildRecord>>,
    ) -> PrepareOptions<'_> {
        PrepareOptions {
            update: false,
            force: false,
            git_env: fetch::GitEnv::default(),
            builder,
            host: HostTriple::current(),
        }
    }

    fn fixture_source(fx: &proof_support::GitFixture, name: &str, path: &str) -> spec::GitSource {
        let spec_value = format!("{}#v1.0.0", fx.url);
        let (repo, reference) = spec::parse_git_spec(&spec_value).unwrap();
        spec::GitSource {
            name: name.into(),
            origin: format!(
                r#"package.json dependencies "{name}": "{spec_value}" (kong.git "{name}")"#
            ),
            spec: spec_value,
            repo,
            reference,
            path: path.into(),
            build: None,
            rust_targets: vec![],
            declared_by: "package.json".into(),
        }
    }

    fn seeded_store() -> TempDir {
        let store = TempDir::new().unwrap();
        proof_support::seed_store_package(
            store.path(),
            "is-fixture",
            "1.0.0",
            &[
                ("package.json", r#"{"name":"is-fixture","version":"1.0.0"}"#),
                ("index.js", "exports.name = 'is-fixture';\n"),
            ],
        );
        store
    }

    #[test]
    fn prepare_all_build_without_builder_is_actionable() {
        let fx = proof_support::fixture_rollback_repo();
        let store = seeded_store();
        let commit = proof_support::git(&fx.work, &["rev-parse", "v1.0.0^{commit}"]);
        let mut src = fixture_source(&fx, "@fx/core", "packages/core");
        src.build = Some("scripts/build-package.sh".into());
        let error = prepare_all(store.path(), &[src.clone()], &[], &prepare_options(None))
            .unwrap_err()
            .to_string();
        for expected in [
            "git dependency '@fx/core' declares a build step but this kong runs no build steps",
            &format!("commit      : {commit}"),
            &format!("declared by : {}", src.origin),
            "scripts/build-package.sh",
            "later kong",
        ] {
            assert!(error.contains(expected), "missing {expected:?} in {error}");
        }
    }

    #[test]
    fn prepare_all_missing_package_json_names_the_path() {
        let fx = proof_support::fixture_rollback_repo();
        let store = seeded_store();
        let src = fixture_source(&fx, "@fx/core", "packages/nothere");
        let error = prepare_all(store.path(), &[src.clone()], &[], &prepare_options(None))
            .unwrap_err()
            .to_string();
        for expected in [
            "git dependency '@fx/core' has no package.json at its path",
            "path        : packages/nothere",
            &src.spec,
            &src.origin,
        ] {
            assert!(error.contains(expected), "missing {expected:?} in {error}");
        }
    }

    #[test]
    fn prepare_all_sorts_by_name_and_reuses_the_store() {
        let fx = proof_support::fixture_rollback_repo();
        let store = seeded_store();
        let commit = proof_support::git(&fx.work, &["rev-parse", "v1.0.0^{commit}"]);
        let sources = [
            fixture_source(&fx, "@fx/util", "packages/util"),
            fixture_source(&fx, "@fx/core", "packages/core"),
        ];
        let (entries, report) =
            prepare_all(store.path(), &sources, &[], &prepare_options(None)).unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["@fx/core", "@fx/util"]);
        let core = &entries[0];
        assert_eq!(core.commit, commit);
        assert_eq!(core.version, "1.0.0");
        assert_eq!(core.reference.as_deref(), Some("v1.0.0"));
        assert_eq!(core.store_path, fetch::store_path_for(&core.repo, &commit));
        assert_eq!(core.lock_dir.as_deref(), Some(""));
        assert_eq!(core.packages.len(), 1);
        assert!(git_link_target(store.path(), core)
            .join("package.json")
            .is_file());
        check_prepared(store.path(), core).unwrap();
        // Both packages share one commit dir: one clone and one install.
        assert_eq!(
            (report.resolved_remote, report.cloned, report.installed),
            (2, 1, 1)
        );

        let (again, report) =
            prepare_all(store.path(), &sources, &entries, &prepare_options(None)).unwrap();
        assert_eq!(again, entries);
        assert_eq!(
            (report.resolved_remote, report.cloned, report.installed),
            (0, 0, 0)
        );
    }

    #[test]
    fn prepare_all_builds_once_with_store_paths() {
        let fx = proof_support::fixture_rollback_repo();
        let store = seeded_store();
        let mut src = fixture_source(&fx, "@fx/core", "packages/core");
        src.build = Some("echo build".into());
        let calls = std::cell::Cell::new(0);
        let builder = |req: &BuildRequest| -> Result<BuildRecord> {
            calls.set(calls.get() + 1);
            let key = spec::build_key("echo build");
            assert_eq!(
                req.marker,
                req.commit_dir.join(format!(".kong-built-{key}"))
            );
            assert_eq!(req.log, req.commit_dir.join(format!("build-{key}.log")));
            assert_eq!(req.lock_dir.as_deref(), Some(req.checkout.as_path()));
            assert_eq!(req.package_dir, req.checkout.join("packages").join("core"));
            fs::write(&req.marker, "{}").unwrap();
            Ok(BuildRecord {
                seconds: 0.5,
                log: req.log.clone(),
            })
        };
        let (entries, report) = prepare_all(
            store.path(),
            &[src.clone()],
            &[],
            &prepare_options(Some(&builder)),
        )
        .unwrap();
        assert_eq!((report.built, report.build_seconds), (1, 0.5));
        let (_, report) = prepare_all(
            store.path(),
            &[src],
            &entries,
            &prepare_options(Some(&builder)),
        )
        .unwrap();
        assert_eq!(report.built, 0);
        assert_eq!(calls.get(), 1);
        check_prepared(store.path(), &entries[0]).unwrap();
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
