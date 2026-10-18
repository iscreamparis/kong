//! Milestone 2 proof: a `kong.git` dependency is pinned to a commit, cloned once
//! into the store, linked in place, and finds its own dependencies. Run by
//! `scripts/proof-m2.sh`; one fixture repository and one store serve every check.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::proof_support::{check, fixture_rollback_repo, git, seed_store_package, selected};
use super::{prepare_all, spec, PrepareOptions};
use crate::config::{self, GitPackageEntry, KongRules, NodeSection};
use crate::node::platform::HostTriple;

const EXPECTED_HELLO: &str = "hello from @fx/core with @fx/util and is-fixture";

fn write(path: &Path, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn options() -> PrepareOptions<'static> {
    PrepareOptions {
        update: false,
        force: false,
        git_env: Default::default(),
        builder: None,
        host: HostTriple::current(),
    }
}

fn rules_with(node: NodeSection) -> KongRules {
    KongRules {
        version: 1,
        project: "proof-m2".into(),
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

/// A symlink, or on Windows any reparse point (a junction made by `link::link_dir`).
fn is_dir_link(p: &Path) -> bool {
    let Ok(m) = std::fs::symlink_metadata(p) else { return false };
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if m.file_attributes() & 0x400 != 0 {
            return true;
        }
    }
    m.file_type().is_symlink()
}

/// First ancestor of `from` (itself included) that holds `node_modules/<rel>`.
fn find_up(from: &Path, rel: &str) -> Option<PathBuf> {
    from.ancestors()
        .map(|d| d.join("node_modules").join(rel))
        .find(|p| p.exists())
}

/// A project without a `kong` key: one `file:` dep and one registry dep. Its
/// NodeSection JSON must be the literal kong wrote before git dependencies existed.
fn backcompat(store: &Path) {
    let root = tempfile::TempDir::new().unwrap();
    let app = root.path().join("app");
    write(
        &root.path().join("localpkg/package.json"),
        r#"{"name":"localpkg","version":"2.0.0"}"#,
    );
    write(
        &app.join("package.json"),
        r#"{"name":"app","version":"1.0.0","dependencies":{"localpkg":"file:../localpkg","is-fixture":"1.0.0"}}"#,
    );
    write(
        &app.join("package-lock.json"),
        r#"{"name":"app","version":"1.0.0","lockfileVersion":3,"requires":true,"packages":{
  "":{"name":"app","version":"1.0.0","dependencies":{"localpkg":"file:../localpkg","is-fixture":"1.0.0"}},
  "../localpkg":{"name":"localpkg","version":"2.0.0"},
  "node_modules/localpkg":{"resolved":"../localpkg","link":true},
  "node_modules/is-fixture":{"version":"1.0.0","resolved":"https://registry.npmjs.org/is-fixture/-/is-fixture-1.0.0.tgz"}
}}"#,
    );
    let deps = crate::node::parser::detect_and_parse(&app).unwrap();
    let mut packages = Vec::new();
    let mut local = Vec::new();
    for dep in &deps {
        if let Some(src) = &dep.local {
            local.push(config::local_entry_for(&app, &dep.name, src).unwrap());
        } else {
            packages.push(config::node_package_entry(dep, store, false).unwrap());
        }
    }
    let json = serde_json::to_string(&NodeSection { packages, local, git: Vec::new() }).unwrap();
    // Pre-change shape: no `git` key, no hash/source_url on a stored package.
    let expected = r#"{"packages":[{"name":"is-fixture","version":"1.0.0","store_path":"node/libs/is-fixture-1.0.0"}],"local":[{"name":"localpkg","version":"2.0.0","path":"../localpkg"}]}"#;
    let same = json == expected;
    let measured = if same { "identical".to_string() } else { json };
    check("backcompat", measured, "NodeSection JSON == pre-change literal", same);
}

#[test]
#[ignore = "M2 proof: run by scripts/proof-m2.sh until the integration task enables it"]
fn proof_m2() {
    let store_dir = tempfile::TempDir::new().unwrap();
    let store = store_dir.path();
    seed_store_package(
        store,
        "is-fixture",
        "1.0.0",
        &[
            ("package.json", r#"{"name":"is-fixture","version":"1.0.0","main":"index.js"}"#),
            ("index.js", "exports.name = 'is-fixture';\n"),
        ],
    );

    // backcompat does not depend on prepare_all, so it runs before the git flow.
    if selected("backcompat") {
        backcompat(store);
    }
    let want_commit = selected("commit_pinned");
    let want_link = selected("link_in_place");
    let want_deps = selected("own_deps");
    let want_runs = selected("runs");
    if !(want_commit || want_link || want_deps || want_runs) {
        return;
    }

    let fx = fixture_rollback_repo();
    let tagged = git(&fx.work, &["rev-parse", "v1.0.0^{commit}"]);
    let project_dir = tempfile::TempDir::new().unwrap();
    let project = project_dir.path();
    write(
        &project.join("package.json"),
        &serde_json::json!({
            "name": "proof-app",
            "version": "1.0.0",
            "dependencies": { "@fx/core": format!("{}#v1.0.0", fx.url) },
            "kong": { "git": { "@fx/core": { "path": "packages/core" } } }
        })
        .to_string(),
    );

    let sources: Vec<spec::GitSource> = crate::node::parser::detect_and_parse(project)
        .unwrap()
        .into_iter()
        .filter_map(|d| d.git)
        .collect();
    let first = prepare_all(store, &sources, &[], &options());
    let entries = match first {
        Ok((entries, _)) => entries,
        Err(e) => {
            let label = if want_commit { "commit_pinned" } else { "prepare_all" };
            check(label, format!("error: {e:#}"), "prepare_all succeeds", false);
            unreachable!()
        }
    };
    let section = || NodeSection { packages: Vec::new(), local: Vec::new(), git: entries.clone() };
    let rules_path = project.join("kong.rules");
    config::write_rules(&rules_with(section()), &rules_path).unwrap();
    crate::node::modules::build_node_modules(project, project, &section(), store).unwrap();
    let core: Option<&GitPackageEntry> = entries.iter().find(|e| e.name == "@fx/core");

    if want_commit {
        let recorded = core.map(|e| e.commit.clone()).unwrap_or_else(|| "(none)".into());
        fx.write("packages/core/CHANGED.md", "moved\n");
        let moved = fx.commit_push("move the tag");
        fx.tag_push("v1.0.0", true);
        let previous = config::read_rules(&rules_path).unwrap().node.unwrap().git;
        let (again, report) = prepare_all(store, &sources, &previous, &options()).unwrap();
        let rerun = again
            .iter()
            .find(|e| e.name == "@fx/core")
            .map(|e| e.commit.clone())
            .unwrap_or_else(|| "(none)".into());
        let ok = recorded == tagged
            && rerun == recorded
            && rerun != moved
            && report.resolved_remote == 0;
        check(
            "commit_pinned",
            format!(
                "commit={} rerun_commit={} resolved_remote={}",
                spec::commit12(&recorded),
                spec::commit12(&rerun),
                report.resolved_remote
            ),
            "commit==rev-parse v1.0.0 after the tag moved, resolved_remote=0",
            ok,
        );
    }

    let link = project.join("node_modules/@fx/core");
    let real = std::fs::canonicalize(&link).ok();

    if want_link {
        let is_link = is_dir_link(&link);
        let expected = store
            .join("node/git")
            .join(spec::repo_key(&sources[0].repo))
            .join(spec::commit12(&tagged))
            .join("src/packages/core");
        let expected = std::fs::canonicalize(&expected).ok();
        let checkout = store
            .join("node/git")
            .join(spec::repo_key(&sources[0].repo))
            .join(spec::commit12(&tagged))
            .join("src");
        let tracked = if checkout.is_dir() {
            git(&checkout, &["status", "--porcelain", "--untracked-files=no"])
                .lines()
                .count()
        } else {
            usize::MAX
        };
        let ok = is_link && real.is_some() && real == expected && tracked == 0;
        check(
            "link_in_place",
            format!(
                "link={} target={} tracked_changes={}",
                is_link,
                real.as_deref().map_or("(none)".into(), |p| p.display().to_string()),
                tracked
            ),
            "canonical link == <store>/node/git/<key>/<c12>/src/packages/core, tracked changes=0",
            ok,
        );
    }

    if want_deps {
        let fixture = real.as_deref().and_then(|r| find_up(r, "is-fixture/package.json"));
        let util = real.as_deref().and_then(|r| find_up(r, "@fx/util"));
        #[allow(unused_mut)]
        let mut nlink_ok = true;
        #[allow(unused_mut)]
        let mut nlink = "n/a".to_string();
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let n = fixture
                .as_deref()
                .and_then(|p| std::fs::metadata(p.with_file_name("index.js")).ok())
                .map_or(0, |m| m.nlink());
            nlink = n.to_string();
            nlink_ok = n >= 2;
        }
        let ok = fixture.is_some() && util.is_some() && nlink_ok;
        check(
            "own_deps",
            format!(
                "is-fixture={} @fx/util={} nlink={}",
                fixture.is_some(),
                util.is_some(),
                nlink
            ),
            "both found walking up from the real path, nlink>=2 (unix)",
            ok,
        );
    }

    if want_runs {
        let has_node = Command::new("node")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if has_node {
            let out = Command::new("node")
                .args(["-e", "console.log(require('@fx/core').hello())"])
                .current_dir(project)
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let ok = out.status.success() && stdout == EXPECTED_HELLO;
            let measured = if out.status.success() {
                stdout
            } else {
                format!("exit={} {}", out.status, String::from_utf8_lossy(&out.stderr).trim())
            };
            check("runs", measured, EXPECTED_HELLO, ok);
        } else {
            check("runs", "node not on PATH", "node output or node not on PATH (own_deps stands in)", true);
        }
    }
}
