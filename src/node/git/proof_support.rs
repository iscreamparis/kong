//! Test-only helpers shared by the git-dependency proofs: proof-line printing,
//! a fixture git CLI wrapper, local bare remotes reached through `git+file://`,
//! and pre-seeded store packages so no test needs the network.

use crate::config::PackageEntry;
use std::path::{Path, PathBuf};
use std::process::Command;

/// True when `KONG_PROOF_ONLY` is unset or contained in `label`. Never sets the variable.
pub fn selected(label: &str) -> bool {
    match std::env::var("KONG_PROOF_ONLY") {
        Ok(p) if !p.is_empty() && !label.contains(&p) => {
            println!("{label}: skipped (--only)");
            false
        }
        _ => true,
    }
}

/// Print the conventions line for one check and panic when it failed.
pub fn check(label: &str, measured: impl std::fmt::Display, threshold: &str, ok: bool) {
    println!(
        "{label}: measured={measured} threshold={threshold} -> {}",
        if ok { "PASS" } else { "FAIL" }
    );
    assert!(ok, "{label} failed: measured={measured} threshold={threshold}");
}

/// Run fixture git in `dir`; returns trimmed stdout, panics with stderr on failure.
pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args([
            "-c", "user.name=kong-test",
            "-c", "user.email=test@kong.invalid",
            "-c", "init.defaultBranch=main",
            "-c", "core.autocrlf=false",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .expect("failed to run git");
    assert!(
        out.status.success(),
        "git {:?} failed in {}: {}",
        args,
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A work repository plus a local bare remote (`url` is a `git+file://` spec).
pub struct GitFixture {
    pub root: tempfile::TempDir,
    pub work: PathBuf,
    pub bare: PathBuf,
    pub url: String,
}

impl GitFixture {
    pub fn new(files: &[(&str, &str)]) -> GitFixture {
        let root = tempfile::TempDir::new().unwrap();
        let work = root.path().join("work");
        let bare = root.path().join("remote.git");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::create_dir_all(&bare).unwrap();
        git(&bare, &["init", "--bare"]);
        git(&work, &["init"]);
        let b = bare.to_string_lossy().replace('\\', "/");
        let url = if b.starts_with('/') {
            format!("git+file://{b}")
        } else {
            format!("git+file:///{b}")
        };
        let fx = GitFixture { root, work, bare, url };
        for (rel, content) in files {
            fx.write(rel, content);
        }
        git(&fx.work, &["add", "-A"]);
        git(&fx.work, &["commit", "-m", "initial"]);
        git(&fx.work, &["branch", "-M", "main"]);
        let bare_s = fx.bare.to_string_lossy().to_string();
        git(&fx.work, &["remote", "add", "origin", &bare_s]);
        git(&fx.work, &["push", "origin", "main"]);
        fx
    }

    pub fn write(&self, rel: &str, content: &str) {
        let p = self.work.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    pub fn commit_push(&self, msg: &str) -> String {
        git(&self.work, &["add", "-A"]);
        git(&self.work, &["commit", "-m", msg]);
        git(&self.work, &["push", "origin", "main"]);
        git(&self.work, &["rev-parse", "HEAD"])
    }

    pub fn tag_push(&self, tag: &str, force: bool) {
        let mut a = vec!["tag", "-a", "-m", tag];
        if force {
            a.push("-f");
        }
        a.push(tag);
        git(&self.work, &a);
        let mut p = vec!["push"];
        if force {
            p.push("--force");
        }
        p.extend(["origin", tag]);
        git(&self.work, &p);
    }
}

/// Write a registry package into `<store>/node/libs/<name with / as +>-<version>/package/`
/// and return the entry `config::node_package_entry` gives for an already-stored package.
pub fn seed_store_package(
    store: &Path,
    name: &str,
    version: &str,
    files: &[(&str, &str)],
) -> PackageEntry {
    let store_path = format!("node/libs/{}-{}", name.replace('/', "+"), version);
    let pkg = store.join(&store_path).join("package");
    std::fs::create_dir_all(&pkg).unwrap();
    for (rel, content) in files {
        let p = pkg.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }
    PackageEntry {
        name: name.to_string(),
        version: version.to_string(),
        hash: None,
        store_path,
        source_url: None,
        path: None,
    }
}

/// Fixture with workspaces `packages/core` (`@fx/core`, depends on `@fx/util` and
/// `is-fixture@1.0.0`) and `packages/util`, a root package-lock.json, tagged `v1.0.0`.
///
/// The proof seeds `is-fixture@1.0.0` with `index.js` = `exports.name = 'is-fixture';`,
/// so the expected `hello()` output is `hello from @fx/core with @fx/util and is-fixture`.
pub fn fixture_rollback_repo() -> GitFixture {
    let root_pkg = r#"{"name":"fx-root","version":"1.0.0","private":true,"workspaces":["packages/*"]}"#;
    let core_pkg = r#"{"name":"@fx/core","version":"1.0.0","main":"index.js","dependencies":{"@fx/util":"1.0.0","is-fixture":"1.0.0"}}"#;
    let core_js = "const u = require('@fx/util'); const f = require('is-fixture'); exports.hello = () => 'hello from @fx/core with ' + u.name + ' and ' + f.name;\n";
    let util_pkg = r#"{"name":"@fx/util","version":"1.0.0","main":"index.js"}"#;
    let util_js = "exports.name = '@fx/util';\n";
    let lock = serde_json::json!({
        "name": "fx-root",
        "version": "1.0.0",
        "lockfileVersion": 3,
        "requires": true,
        "packages": {
            "": {"name": "fx-root", "version": "1.0.0", "workspaces": ["packages/*"]},
            "node_modules/@fx/core": {"resolved": "packages/core", "link": true},
            "node_modules/@fx/util": {"resolved": "packages/util", "link": true},
            "node_modules/is-fixture": {
                "version": "1.0.0",
                "resolved": "https://registry.npmjs.org/is-fixture/-/is-fixture-1.0.0.tgz"
            },
            "packages/core": {
                "name": "@fx/core",
                "version": "1.0.0",
                "dependencies": {"@fx/util": "1.0.0", "is-fixture": "1.0.0"}
            },
            "packages/util": {"name": "@fx/util", "version": "1.0.0"}
        }
    });
    let lock_s = serde_json::to_string_pretty(&lock).unwrap();
    let fx = GitFixture::new(&[
        ("package.json", root_pkg),
        ("packages/core/package.json", core_pkg),
        ("packages/core/index.js", core_js),
        ("packages/util/package.json", util_pkg),
        ("packages/util/index.js", util_js),
        ("package-lock.json", &lock_s),
    ]);
    fx.tag_push("v1.0.0", false);
    fx
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tag_commit(fx: &GitFixture) -> String {
        let bare = fx.bare.to_string_lossy().to_string();
        git(&fx.work, &["ls-remote", &bare, "refs/tags/v1.0.0^{}"])
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_string()
    }

    #[test]
    fn fixture_tag_moves_with_force() {
        let fx = fixture_rollback_repo();
        let first = tag_commit(&fx);
        assert_eq!(first, git(&fx.work, &["rev-parse", "HEAD"]));
        fx.write("packages/util/index.js", "exports.name = '@fx/util2';\n");
        let c2 = fx.commit_push("second");
        assert_eq!(tag_commit(&fx), first);
        fx.tag_push("v1.0.0", true);
        assert_eq!(tag_commit(&fx), c2);
        assert_ne!(first, c2);
        assert!(fx.url.starts_with("git+file:///"));
    }

    #[test]
    fn fixture_lockfile_parses() {
        let fx = fixture_rollback_repo();
        let deps = crate::node::parser::parse_package_lock(&fx.work.join("package-lock.json")).unwrap();
        let find = |n: &str| deps.iter().find(|d| d.name == n).unwrap_or_else(|| panic!("{n} missing"));
        assert!(find("is-fixture").local.is_none());
        assert!(find("@fx/util").local.is_some());
        assert!(find("@fx/core").local.is_some());
    }

    #[test]
    fn seed_creates_package_json() {
        let store = tempfile::TempDir::new().unwrap();
        let e = seed_store_package(store.path(), "@a/b", "1.0.0", &[("package.json", "{}")]);
        assert_eq!(e.store_path, "node/libs/@a+b-1.0.0");
        assert!(store.path().join("node/libs/@a+b-1.0.0/package/package.json").is_file());
    }
}
