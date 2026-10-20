//! Resolves the own dependencies of a tarball dependency from its package.json.
//!
//! A tarball has no lockfile, so its `dependencies`, `optionalDependencies` and
//! required `peerDependencies` (installed automatically, as npm 7+ does) are
//! resolved against the npm registry into a flat, npm-style tree: one version
//! per name hoisted to the top, a second version nested under the package that
//! needs it. A dependency that is itself an http(s) tarball URL becomes a
//! tarball node of the same tree.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use anyhow::{anyhow, bail, Result};
use tracing::{debug, warn};

use super::semver::{Range, Version};
use crate::config::PackageEntry;
use crate::node::platform::{self, HostTriple};

// ── Registry metadata ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct VersionMeta {
    pub version: String,
    pub deps: BTreeMap<String, String>,
    pub optional: BTreeMap<String, String>,
    pub peers: BTreeMap<String, String>,
    pub peers_optional: BTreeSet<String>,
    pub os: Vec<String>,
    pub cpu: Vec<String>,
    pub libc: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Packument {
    pub tags: BTreeMap<String, String>,
    pub versions: Vec<(Version, VersionMeta)>,
}

fn str_map(v: Option<&serde_json::Value>) -> BTreeMap<String, String> {
    v.and_then(|v| v.as_object())
        .map(|o| {
            o.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

fn str_list(v: Option<&serde_json::Value>) -> Vec<String> {
    match v {
        Some(serde_json::Value::Array(a)) => a.iter().filter_map(|x| x.as_str().map(String::from)).collect(),
        Some(serde_json::Value::String(s)) => vec![s.clone()],
        _ => Vec::new(),
    }
}

/// The fields of a package.json that matter for installing it.
pub fn version_meta(pkg: &serde_json::Value) -> VersionMeta {
    let optional = str_map(pkg.get("optionalDependencies"));
    let mut deps = str_map(pkg.get("dependencies"));
    // npm lists an optional dependency in both maps; the optional one wins.
    deps.retain(|k, _| !optional.contains_key(k));
    let peers_optional = pkg
        .get("peerDependenciesMeta")
        .and_then(|m| m.as_object())
        .map(|m| {
            m.iter()
                .filter(|(_, v)| v.get("optional").and_then(|o| o.as_bool()).unwrap_or(false))
                .map(|(k, _)| k.clone())
                .collect()
        })
        .unwrap_or_default();
    VersionMeta {
        version: pkg.get("version").and_then(|v| v.as_str()).unwrap_or("0.0.0").to_string(),
        deps,
        optional,
        peers: str_map(pkg.get("peerDependencies")),
        peers_optional,
        os: str_list(pkg.get("os")),
        cpu: str_list(pkg.get("cpu")),
        libc: str_list(pkg.get("libc")),
    }
}

/// Parse a registry packument (full or the abbreviated install document).
pub fn parse_packument(doc: &serde_json::Value) -> Packument {
    let tags = str_map(doc.get("dist-tags"));
    let mut versions = Vec::new();
    if let Some(map) = doc.get("versions").and_then(|v| v.as_object()) {
        for (ver, pkg) in map {
            if let Some(parsed) = Version::parse(ver) {
                let mut meta = version_meta(pkg);
                meta.version = ver.clone();
                versions.push((parsed, meta));
            }
        }
    }
    versions.sort_by(|a, b| a.0.cmp(&b.0));
    Packument { tags, versions }
}

/// Pick the version npm would install for `spec`: a dist-tag, else the `latest`
/// tag when it satisfies, else the highest satisfying version.
pub fn pick_version<'a>(pk: &'a Packument, spec: &str) -> Option<&'a VersionMeta> {
    let spec = spec.trim();
    if let Some(v) = pk.tags.get(spec) {
        return pk.versions.iter().find(|(_, m)| &m.version == v).map(|(_, m)| m);
    }
    let range = Range::parse(spec)?;
    if let Some(latest) = pk.tags.get("latest") {
        if let Some((v, m)) = pk.versions.iter().find(|(_, m)| &m.version == latest) {
            if range.satisfies(v) {
                return Some(m);
            }
        }
    }
    pk.versions.iter().rev().find(|(v, _)| range.satisfies(v)).map(|(_, m)| m)
}

// ── The tree ────────────────────────────────────────────────────────────────

/// One declared dependency of a package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Req {
    pub name: String,
    pub spec: String,
    pub optional: bool,
}

/// `dependencies`, `optionalDependencies` and required `peerDependencies` of a package.json.
pub fn requirements(pkg: &serde_json::Value) -> Vec<Req> {
    let m = version_meta(pkg);
    let mut out: BTreeMap<String, Req> = BTreeMap::new();
    for (name, spec) in &m.peers {
        if !m.peers_optional.contains(name) {
            out.insert(name.clone(), Req { name: name.clone(), spec: spec.clone(), optional: false });
        }
    }
    for (name, spec) in &m.deps {
        out.insert(name.clone(), Req { name: name.clone(), spec: spec.clone(), optional: false });
    }
    for (name, spec) in &m.optional {
        out.insert(name.clone(), Req { name: name.clone(), spec: spec.clone(), optional: true });
    }
    out.into_values().collect()
}

/// A tarball dependency reached through the tree.
pub struct TarballNode {
    pub entry: PackageEntry,
    pub manifest: serde_json::Value,
}

/// What the resolver needs from the outside world.
pub trait Env {
    fn packument(&mut self, name: &str) -> Result<Packument>;
    /// Fetch (or find in the store) the tarball behind `spec` (an http(s) URL spec).
    fn tarball(&mut self, name: &str, spec: &str, requested_by: &str) -> Result<TarballNode>;
}

#[derive(Debug, Clone)]
pub enum Source {
    Registry,
    Tarball(PackageEntry),
}

#[derive(Debug, Clone)]
pub struct Resolved {
    pub name: String,
    pub version: String,
    /// `None` = hoisted to `node_modules/<name>`; `Some(key)` = nested lockfile-style key.
    pub path: Option<String>,
    pub source: Source,
}

fn slot_key(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        format!("node_modules/{name}")
    } else {
        format!("{parent}/node_modules/{name}")
    }
}

/// The package directory that encloses `loc` in the tree ("" = the root package).
fn enclosing(loc: &str) -> &str {
    loc.rfind("/node_modules/").map_or("", |i| &loc[..i])
}

struct Placed {
    version: String,
    url: Option<String>,
}

struct Work {
    requester: String,
    requester_name: String,
    req: Req,
}

fn unsupported_spec(spec: &str) -> Option<&'static str> {
    let s = spec.trim();
    if s.starts_with("npm:") {
        Some("an `npm:` alias")
    } else if s.starts_with("file:") || s.starts_with("link:") || s.starts_with('.') || s.starts_with('/') {
        Some("a local path")
    } else if s.starts_with("workspace:") {
        Some("a `workspace:` protocol spec")
    } else if s.starts_with("git") || s.starts_with("github:") || s.contains("://") && !super::fetch::is_tarball_spec(s) {
        Some("a git or other non-registry source")
    } else {
        None
    }
}

/// Resolve `root` (the requirements of the tarball dependency itself) into a flat tree.
pub fn resolve_tree(
    root_name: &str,
    root: Vec<Req>,
    host: &HostTriple,
    env: &mut dyn Env,
) -> Result<Vec<Resolved>> {
    let mut placed: BTreeMap<(String, String), Placed> = BTreeMap::new();
    let mut out: Vec<Resolved> = Vec::new();
    let mut queue: VecDeque<Work> = root
        .into_iter()
        .map(|req| Work { requester: String::new(), requester_name: root_name.to_string(), req })
        .collect();

    while let Some(Work { requester, requester_name, req }) = queue.pop_front() {
        let is_url = super::fetch::is_tarball_spec(&req.spec);
        // Already provided by this package or an ancestor?
        let mut found: Option<&Placed> = None;
        let mut p = requester.as_str();
        loop {
            if let Some(pl) = placed.get(&(p.to_string(), req.name.clone())) {
                found = Some(pl);
                break;
            }
            if p.is_empty() {
                break;
            }
            p = enclosing(p);
        }
        let satisfied = found.is_some_and(|pl| {
            if is_url {
                pl.url.as_deref() == Some(req.spec.split('#').next().unwrap_or(&req.spec))
            } else if pl.url.is_some() {
                // A tarball-provided package satisfies a range only when its version does.
                Range::parse(&req.spec)
                    .zip(Version::parse(&pl.version))
                    .is_some_and(|(r, v)| r.satisfies(&v))
            } else {
                Range::parse(&req.spec)
                    .zip(Version::parse(&pl.version))
                    .map(|(r, v)| r.satisfies(&v))
                    // A dist-tag (`latest`): any placed version stands.
                    .unwrap_or(true)
            }
        });
        if satisfied {
            continue;
        }

        if let Some(what) = unsupported_spec(&req.spec) {
            if req.optional {
                warn!(dep = %req.name, by = %requester_name, "skipping optional dependency declared as {what}");
                continue;
            }
            bail!(
                "cannot install '{}' required by '{requester_name}': its spec '{}' is {what}, \
                 which kong does not resolve inside a tarball dependency",
                req.name,
                req.spec
            );
        }

        // Where it goes: hoisted to the top when that slot is free, else next to its requester.
        let hoist_free = !placed.contains_key(&(String::new(), req.name.clone()));
        let parent = if hoist_free || requester.is_empty() { String::new() } else { requester.clone() };
        if !hoist_free && requester.is_empty() {
            warn!(
                dep = %req.name, by = %requester_name, wanted = %req.spec,
                "conflicting requirement at the top of the tree; keeping the version already placed"
            );
            continue;
        }
        let key = slot_key(&parent, &req.name);
        let path = if parent.is_empty() { None } else { Some(key.clone()) };

        let (version, source, manifest_deps): (String, Source, Vec<Req>) = if is_url {
            let node = env.tarball(&req.name, &req.spec, &requester_name)?;
            let reqs = requirements(&node.manifest);
            let v = node.entry.version.clone();
            let mut entry = node.entry;
            entry.path = path.clone();
            (v, Source::Tarball(entry), reqs)
        } else {
            let pk = env.packument(&req.name).map_err(|e| {
                anyhow!("cannot resolve '{}@{}' required by '{requester_name}': {e:#}", req.name, req.spec)
            })?;
            let Some(meta) = pick_version(&pk, &req.spec) else {
                if req.optional {
                    warn!(dep = %req.name, spec = %req.spec, by = %requester_name, "no version satisfies an optional dependency; skipping it");
                    continue;
                }
                bail!(
                    "cannot resolve '{}@{}' required by '{requester_name}': no published version satisfies it \
                     (latest tag: {})",
                    req.name,
                    req.spec,
                    pk.tags.get("latest").map(String::as_str).unwrap_or("none")
                );
            };
            if !platform::is_compatible(host, &meta.os, &meta.cpu, &meta.libc) {
                if req.optional {
                    debug!(dep = %req.name, ver = %meta.version, "skipping host-incompatible optional dependency");
                    continue;
                }
                bail!(
                    "package '{}@{}' (required by '{requester_name}') is not compatible with this host \
                     (os={:?} cpu={:?} libc={:?}) - it is a required dependency (EBADPLATFORM)",
                    req.name, meta.version, meta.os, meta.cpu, meta.libc
                );
            }
            let mut reqs: BTreeMap<String, Req> = BTreeMap::new();
            for (n, s) in &meta.peers {
                if !meta.peers_optional.contains(n) {
                    reqs.insert(n.clone(), Req { name: n.clone(), spec: s.clone(), optional: false });
                }
            }
            for (n, s) in &meta.deps {
                reqs.insert(n.clone(), Req { name: n.clone(), spec: s.clone(), optional: false });
            }
            for (n, s) in &meta.optional {
                reqs.insert(n.clone(), Req { name: n.clone(), spec: s.clone(), optional: true });
            }
            (meta.version.clone(), Source::Registry, reqs.into_values().collect())
        };

        let url = match &source {
            Source::Tarball(e) => e.source_url.clone(),
            Source::Registry => None,
        };
        placed.insert((parent.clone(), req.name.clone()), Placed { version: version.clone(), url });
        out.push(Resolved { name: req.name.clone(), version: version.clone(), path, source });
        for r in manifest_deps {
            queue.push_back(Work {
                requester: key.clone(),
                requester_name: format!("{}@{}", req.name, version),
                req: r,
            });
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.name.cmp(&b.name)));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn host() -> HostTriple {
        HostTriple { os: "linux".into(), cpu: "x64".into(), libc: Some("glibc".into()) }
    }

    fn pk(tags: &[(&str, &str)], versions: &[serde_json::Value]) -> Packument {
        let mut vs = serde_json::Map::new();
        for v in versions {
            vs.insert(v["version"].as_str().unwrap().to_string(), v.clone());
        }
        parse_packument(&json!({
            "dist-tags": tags.iter().map(|(k, v)| (k.to_string(), json!(v))).collect::<serde_json::Map<_, _>>(),
            "versions": vs,
        }))
    }

    struct Fake {
        pks: BTreeMap<String, Packument>,
        asked: Vec<String>,
    }
    impl Env for Fake {
        fn packument(&mut self, name: &str) -> Result<Packument> {
            self.asked.push(name.to_string());
            self.pks.get(name).cloned().ok_or_else(|| anyhow!("{name} is not in the registry"))
        }
        fn tarball(&mut self, name: &str, spec: &str, _by: &str) -> Result<TarballNode> {
            let manifest = json!({"name": name, "version": "0.5.0", "dependencies": {"leaf": "^1.0.0"}});
            Ok(TarballNode {
                entry: PackageEntry {
                    name: name.into(),
                    version: "0.5.0".into(),
                    hash: Some("sha512-x".into()),
                    store_path: format!("node/tarballs/{name}/aaaaaaaaaaaa"),
                    source_url: Some(spec.split('#').next().unwrap().into()),
                    path: None,
                },
                manifest,
            })
        }
    }

    fn fake() -> Fake {
        let mut pks = BTreeMap::new();
        pks.insert(
            "leaf".to_string(),
            pk(&[("latest", "2.0.0")], &[
                json!({"version": "1.0.0"}),
                json!({"version": "1.4.0"}),
                json!({"version": "2.0.0"}),
            ]),
        );
        pks.insert(
            "mid".to_string(),
            pk(&[("latest", "1.0.0")], &[json!({"version": "1.0.0", "dependencies": {"leaf": "^2.0.0"}})]),
        );
        pks.insert(
            "peer-host".to_string(),
            pk(&[("latest", "3.1.0")], &[json!({"version": "3.1.0"}), json!({"version": "2.9.0"})]),
        );
        pks.insert(
            "native".to_string(),
            pk(&[("latest", "1.0.0")], &[json!({"version": "1.0.0", "os": ["win32"]})]),
        );
        Fake { pks, asked: vec![] }
    }

    fn req(name: &str, spec: &str) -> Req {
        Req { name: name.into(), spec: spec.into(), optional: false }
    }

    #[test]
    fn picks_highest_satisfying_and_prefers_latest_when_it_satisfies() {
        let p = pk(&[("latest", "1.4.0")], &[
            json!({"version": "1.0.0"}),
            json!({"version": "1.4.0"}),
            json!({"version": "1.5.0-beta.1"}),
            json!({"version": "1.6.0"}),
        ]);
        assert_eq!(pick_version(&p, "^1.0.0").unwrap().version, "1.4.0");
        assert_eq!(pick_version(&p, "~1.0.0").unwrap().version, "1.0.0");
        assert_eq!(pick_version(&p, "latest").unwrap().version, "1.4.0");
        assert_eq!(pick_version(&p, ">=1.5.0").unwrap().version, "1.6.0");
        assert!(pick_version(&p, "^9.0.0").is_none());
    }

    #[test]
    fn hoists_one_version_and_nests_a_conflicting_one() {
        let mut env = fake();
        let tree = resolve_tree(
            "pkg",
            vec![req("leaf", "^1.0.0"), req("mid", "^1.0.0")],
            &host(),
            &mut env,
        )
        .unwrap();
        let find = |n: &str, p: Option<&str>| {
            tree.iter().find(|r| r.name == n && r.path.as_deref() == p).map(|r| r.version.clone())
        };
        assert_eq!(find("leaf", None).as_deref(), Some("1.4.0"));
        assert_eq!(find("mid", None).as_deref(), Some("1.0.0"));
        assert_eq!(find("leaf", Some("node_modules/mid/node_modules/leaf")).as_deref(), Some("2.0.0"));
        assert_eq!(tree.len(), 3);
    }

    #[test]
    fn required_peers_are_installed_optional_peers_are_not() {
        let manifest = json!({
            "dependencies": {"leaf": "^1.0.0"},
            "peerDependencies": {"peer-host": "^3.0.0", "native": "*"},
            "peerDependenciesMeta": {"native": {"optional": true}},
        });
        let reqs = requirements(&manifest);
        let names: Vec<&str> = reqs.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["leaf", "peer-host"]);
        let mut env = fake();
        let tree = resolve_tree("pkg", reqs, &host(), &mut env).unwrap();
        assert!(tree.iter().any(|r| r.name == "peer-host" && r.version == "3.1.0"));
    }

    #[test]
    fn optional_dependency_wins_over_dependencies_entry() {
        let reqs = requirements(&json!({
            "dependencies": {"native": "*"}, "optionalDependencies": {"native": "*"}
        }));
        assert_eq!(reqs.len(), 1);
        assert!(reqs[0].optional);
    }

    #[test]
    fn host_incompatible_optional_is_skipped_required_is_an_error() {
        let mut env = fake();
        let optional = vec![Req { name: "native".into(), spec: "*".into(), optional: true }];
        assert!(resolve_tree("pkg", optional, &host(), &mut env).unwrap().is_empty());
        let err = resolve_tree("pkg", vec![req("native", "*")], &host(), &mut env)
            .unwrap_err()
            .to_string();
        assert!(err.contains("EBADPLATFORM") && err.contains("native"), "{err}");
    }

    #[test]
    fn unknown_package_and_unsatisfiable_range_name_the_requester() {
        let mut env = fake();
        let err = resolve_tree("pkg", vec![req("nope", "^1.0.0")], &host(), &mut env)
            .unwrap_err()
            .to_string();
        assert!(err.contains("nope@^1.0.0") && err.contains("required by 'pkg'"), "{err}");
        let err = resolve_tree("pkg", vec![req("leaf", "^9.0.0")], &host(), &mut env)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no published version satisfies"), "{err}");
    }

    #[test]
    fn non_registry_specs_are_refused_with_the_reason() {
        let mut env = fake();
        for spec in ["npm:other@1", "file:../x", "git+https://h/r.git", "workspace:*"] {
            let err = resolve_tree("pkg", vec![req("x", spec)], &host(), &mut env)
                .unwrap_err()
                .to_string();
            assert!(err.contains("required by 'pkg'") && err.contains(spec), "{spec}: {err}");
        }
    }

    #[test]
    fn nested_tarball_dependency_joins_the_tree_with_its_own_deps() {
        let mut env = fake();
        let tree = resolve_tree(
            "pkg",
            vec![req("@x/b", "http://127.0.0.1:1/b.tgz#sha256=00")],
            &host(),
            &mut env,
        )
        .unwrap();
        let b = tree.iter().find(|r| r.name == "@x/b").unwrap();
        assert!(matches!(&b.source, Source::Tarball(e) if e.source_url.as_deref() == Some("http://127.0.0.1:1/b.tgz")));
        // Its registry dependency `leaf@^1` is hoisted next to it.
        assert!(tree.iter().any(|r| r.name == "leaf" && r.version == "1.4.0" && r.path.is_none()));
    }

    #[test]
    fn a_satisfied_requirement_asks_the_registry_once() {
        let mut env = fake();
        resolve_tree("pkg", vec![req("leaf", "^1.0.0"), req("leaf", "^1.2.0")], &host(), &mut env).unwrap();
        assert_eq!(env.asked.iter().filter(|n| *n == "leaf").count(), 1);
    }
}
