//! Milestone 3 proof: a git dependency's build step runs once per commit, a rerun is
//! fully cached, a build failure is diagnosable, no credential reaches disk, and
//! solidify / eject handle git dependencies.
//! Run by `scripts/proof-m3.sh`; one fixture repository serves the checks.

use std::path::{Path, PathBuf};

use crate::config::{GitPackageEntry, PackageEntry};
use crate::gitdep::build::{run_build_with, BuildTools};
use crate::node::git::fetch::{self, GitEnv};
use crate::node::git::proof_support::{
    check, fixture_rollback_repo, git, seed_store_package, selected, GitFixture,
};
use crate::node::git::{prepare_all, spec, BuildRecord, BuildRequest, PrepareOptions, PrepareReport};
use crate::node::platform::HostTriple;

const BUILD_SCRIPT: &str = "#!/usr/bin/env bash\nset -e\necho building-fixture\nmkdir -p packages/core/pkg && printf 'module.exports = \"%s\";\\n' \"$KONG_GIT_COMMIT\" > packages/core/pkg/out.js\n";
const FAIL_SCRIPT: &str = "#!/usr/bin/env bash\necho kong-fail-last-line\nexit 3\n";
const SENTINEL: &str = "kongsentinel123";
const BUILD: &str = "scripts/build-package.sh";

fn write(path: &Path, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn build_with_defaults(r: &BuildRequest) -> anyhow::Result<BuildRecord> {
    run_build_with(r, &BuildTools::default())
}

fn options<'a>(
    update: bool,
    git_env: GitEnv,
    builder: &'a dyn Fn(&BuildRequest) -> anyhow::Result<BuildRecord>,
) -> PrepareOptions<'a> {
    PrepareOptions { update, force: false, git_env, builder: Some(builder), host: HostTriple::current() }
}

/// A temp store holding `is-fixture@1.0.0`, and that package's rules entry.
fn seeded_store() -> (tempfile::TempDir, PackageEntry) {
    let store = tempfile::TempDir::new().unwrap();
    let seeded = seed_store_package(
        store.path(),
        "is-fixture",
        "1.0.0",
        &[
            ("package.json", r#"{"name":"is-fixture","version":"1.0.0","main":"index.js"}"#),
            ("index.js", "exports.name = 'is-fixture';\n"),
        ],
    );
    (store, seeded)
}

/// A project whose package.json declares `@fx/core` from `url#v1.0.0` with a build step.
fn project_for(url: &str) -> (tempfile::TempDir, Vec<spec::GitSource>) {
    let dir = tempfile::TempDir::new().unwrap();
    write(
        &dir.path().join("package.json"),
        &serde_json::json!({
            "name": "proof-app",
            "version": "1.0.0",
            "dependencies": { "@fx/core": format!("{url}#v1.0.0") },
            "kong": { "git": { "@fx/core": { "path": "packages/core", "build": BUILD } } }
        })
        .to_string(),
    );
    let sources = crate::node::parser::detect_and_parse(dir.path())
        .unwrap()
        .into_iter()
        .filter_map(|d| d.git)
        .collect();
    (dir, sources)
}

fn is_hex40(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn count_in_tree(root: &Path, needle: &str) -> usize {
    walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| std::fs::read(e.path()).ok())
        .map(|b| String::from_utf8_lossy(&b).matches(needle).count())
        .sum()
}

fn tracked_changes(src: &Path) -> usize {
    if src.is_dir() {
        git(src, &["status", "--porcelain", "--untracked-files=no"]).lines().count()
    } else {
        usize::MAX
    }
}

fn entry<'a>(entries: &'a [GitPackageEntry]) -> &'a GitPackageEntry {
    entries.iter().find(|e| e.name == "@fx/core").expect("@fx/core entry")
}

/// The first `prepare_all`; `report` is false when only a later check needs its entries.
fn build_once(
    store: &Path,
    sources: &[spec::GitSource],
    tagged: &str,
    report_check: bool,
) -> (Vec<GitPackageEntry>, PrepareReport) {
    let builder = build_with_defaults;
    let res = prepare_all(store, sources, &[], &options(false, GitEnv::default(), &builder));
    let (entries, report) = match res {
        Ok(v) => v,
        Err(e) => {
            check("build_once", format!("error: {e:#}"), "prepare_all succeeds", false);
            unreachable!()
        }
    };
    let dir = fetch::commit_dir(store, &sources[0].repo, &entries[0].commit);
    let key = spec::build_key(BUILD);
    let marker = dir.join(format!(".kong-built-{key}")).is_file();
    let log = dir.join(format!("build-{key}.log")).is_file();
    let out = std::fs::read_to_string(dir.join("src/packages/core/pkg/out.js")).unwrap_or_default();
    let out_ok = out.contains(tagged) && is_hex40(tagged);
    let tracked = tracked_changes(&dir.join("src"));
    if !report_check {
        return (entries, report);
    }
    check(
        "build_once",
        format!(
            "marker={} log={} out={} tracked_changes={} clone_seconds={:.3} build_seconds={:.3}",
            marker as u8, log as u8, out_ok as u8, tracked, report.clone_seconds, report.build_seconds
        ),
        "marker, log and pkg/out.js (with the commit) present, tracked changes=0",
        marker && log && out_ok && tracked == 0,
    );
    (entries, report)
}

fn rerun_cached_and_update(
    store: &Path,
    sources: &[spec::GitSource],
    first: &[GitPackageEntry],
    fx: &GitFixture,
) {
    let builder = build_with_defaults;
    let (_, again) = prepare_all(store, sources, first, &options(false, GitEnv::default(), &builder)).unwrap();
    let cached = again.cloned == 0 && again.built == 0 && again.resolved_remote == 0;

    fx.write("packages/core/CHANGED.md", "moved\n");
    let moved = fx.commit_push("move the tag");
    fx.tag_push("v1.0.0", true);
    let (updated, upd) = prepare_all(store, sources, first, &options(true, GitEnv::default(), &builder)).unwrap();
    let new_commit = entry(&updated).commit.clone();
    let new_dir = fetch::commit_dir(store, &sources[0].repo, &new_commit);
    let ok = cached
        && new_commit != entry(first).commit
        && new_commit == moved
        && new_dir.is_dir()
        && upd.built == 1;
    check(
        "rerun_cached_and_update",
        format!(
            "cloned={} built={} resolved_remote={} update_commit={} dir_exists={} update_built={}",
            again.cloned,
            again.built,
            again.resolved_remote,
            spec::commit12(&new_commit),
            new_dir.is_dir(),
            upd.built
        ),
        "rerun cloned=0 built=0 resolved_remote=0; --update-git: new commit, new dir, built=1",
        ok,
    );
}

fn build_failure_error() {
    let fx = GitFixture::new(&[
        ("package.json", r#"{"name":"fx-fail","version":"1.0.0"}"#),
        ("packages/core/package.json", r#"{"name":"@fx/core","version":"1.0.0"}"#),
        (BUILD, FAIL_SCRIPT),
    ]);
    fx.tag_push("v1.0.0", false);
    let commit = git(&fx.work, &["rev-parse", "HEAD"]);
    let store = tempfile::TempDir::new().unwrap();
    let (_project, sources) = project_for(&fx.url);
    let builder = build_with_defaults;
    let err = match prepare_all(store.path(), &sources, &[], &options(false, GitEnv::default(), &builder)) {
        Ok(_) => String::new(),
        Err(e) => format!("{e:#}"),
    };
    let src = &sources[0];
    let log = fetch::commit_dir(store.path(), &src.repo, &commit)
        .join(format!("build-{}.log", spec::build_key(BUILD)));
    let wanted = [
        ("name", src.name.clone()),
        ("spec", src.spec.clone()),
        ("commit", commit),
        ("origin", src.origin.clone()),
        ("log", log.display().to_string()),
        ("last_line", "kong-fail-last-line".to_string()),
    ];
    let missing: Vec<&str> = wanted.iter().filter(|(_, v)| !err.contains(v.as_str())).map(|(k, _)| *k).collect();
    let measured = if missing.is_empty() {
        "all six present".to_string()
    } else {
        format!("missing={} error={err:?}", missing.join(","))
    };
    check(
        "build_failure_error",
        measured,
        "error names dep, spec, 40-hex commit, origin, log path and last log line",
        missing.is_empty() && !err.is_empty(),
    );
}

fn no_secret_written(fx: &GitFixture) {
    let env = crate::gitdep::auth::git_env_from(&|k| (k == "KONG_GIT_TOKEN").then(|| SENTINEL.to_string()));
    let has_secrets = !env.secrets.is_empty();
    let (store, _) = seeded_store();
    let (project, sources) = project_for(&fx.url);
    let builder = build_with_defaults;
    let res = prepare_all(store.path(), &sources, &[], &options(false, env, &builder));
    let (err_count, commit_dirs): (usize, Vec<PathBuf>) = match &res {
        Ok((entries, _)) => (
            0,
            entries.iter().map(|e| fetch::commit_dir(store.path(), &e.repo, &e.commit)).collect(),
        ),
        Err(e) => (format!("{e:#}").matches(SENTINEL).count(), Vec::new()),
    };
    if let Ok((entries, _)) = &res {
        let section = crate::config::NodeSection { packages: Vec::new(), local: Vec::new(), git: entries.clone(), tarballs: Vec::new() };
        crate::config::write_rules(&rules_with(section), &project.path().join("kong.rules")).unwrap();
    }
    let mut count = err_count + count_in_tree(project.path(), SENTINEL);
    for d in &commit_dirs {
        count += count_in_tree(d, SENTINEL);
    }
    check(
        "no_secret_written",
        format!("occurrences={count} secrets_in_env={} prepare_ok={}", has_secrets, res.is_ok()),
        "sentinel occurrences=0 in project, commit dir, error and build log; env.secrets non-empty",
        count == 0 && has_secrets && res.is_ok(),
    );
}

/// Project rules holding only `node`, as `kong rules` would write them.
fn rules_with(node: crate::config::NodeSection) -> crate::config::KongRules {
    crate::config::KongRules {
        version: 1,
        project: "proof-m3".into(),
        generated: "proof".into(),
        runtimes: None,
        python: None,
        node: Some(node),
        rust: None,
        brew: None,
        scripts: Default::default(),
        services: Vec::new(),
    }
}

/// Solidify the project's node_modules from the store, then eject it alone: the
/// commit dir only this project uses must go. Runs last, since eject deletes it.
fn solidify_eject(
    store: &Path,
    project: &Path,
    sources: &[spec::GitSource],
    first: &[GitPackageEntry],
    seeded: &PackageEntry,
) {
    let section = crate::config::NodeSection {
        packages: vec![seeded.clone()],
        local: Vec::new(),
        git: first.to_vec(),
        tarballs: Vec::new(),
    };
    let nm = project.join("node_modules");
    let solid = crate::migrate::solidify_node(&nm, project, &section, store);
    let core = nm.join("@fx/core");
    let real_dir = solid.is_ok() && core.is_dir() && !core.is_symlink();
    let out = core.join("pkg/out.js").is_file();
    let inner_nm = core.join("node_modules/is-fixture/package.json").is_file();

    crate::config::write_rules(&rules_with(section), &project.join("kong.rules")).unwrap();
    let index = crate::migrate::build_dep_index(&[("proj".to_string(), project.to_path_buf())]);
    let rules = crate::config::read_rules(&project.join("kong.rules")).unwrap();
    let paths = crate::migrate::exclusive_store_paths(&rules, &index);
    let removed = crate::migrate::remove_store_entries(store, &paths);
    let commit_dir = fetch::commit_dir(store, &sources[0].repo, &entry(first).commit);
    let dir_removed = removed.is_ok() && !commit_dir.exists();
    let mut measured = format!(
        "real_dir={} out={} inner_nm={} commit_dir_removed={}",
        real_dir as u8, out as u8, inner_nm as u8, dir_removed as u8
    );
    if let Err(e) = &solid {
        measured.push_str(&format!(" solidify_error={e:#}"));
    }
    if let Err(e) = &removed {
        measured.push_str(&format!(" eject_error={e:#}"));
    }
    check(
        "solidify_eject",
        measured,
        "node_modules/@fx/core real dir with pkg/out.js and node_modules/is-fixture; exclusive commit dir removed",
        real_dir && out && inner_nm && dir_removed,
    );
}

#[test]
fn proof_m3() {
    let wants = [
        "build_once",
        "rerun_cached_and_update",
        "build_failure_error",
        "no_secret_written",
        "solidify_eject",
    ]
    .map(selected);
    let [want_build, want_rerun, want_fail, want_secret, want_solid] = wants;

    if want_fail {
        build_failure_error();
    }
    if !(want_build || want_rerun || want_secret || want_solid) {
        return;
    }

    let fx = fixture_rollback_repo();
    fx.write(BUILD, BUILD_SCRIPT);
    fx.commit_push("add the build script");
    fx.tag_push("v1.0.0", true);
    let tagged = git(&fx.work, &["rev-parse", "v1.0.0^{commit}"]);

    if want_secret {
        no_secret_written(&fx);
    }
    if want_build || want_rerun || want_solid {
        let (store, seeded) = seeded_store();
        let (project, sources) = project_for(&fx.url);
        let (entries, _) = build_once(store.path(), &sources, &tagged, want_build);
        if want_rerun {
            rerun_cached_and_update(store.path(), &sources, &entries, &fx);
        }
        if want_solid {
            solidify_eject(store.path(), project.path(), &sources, &entries, &seeded);
        }
    }
}
