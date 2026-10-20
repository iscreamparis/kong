//! Tarball dependencies against a local http server (no real network).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use serde_json::json;
use tempfile::TempDir;

use super::*;
use crate::node::git::proof_support::seed_store_package;

// ── A tiny http server ──────────────────────────────────────────────────────

#[derive(Clone)]
pub(crate) enum Route {
    Body(Vec<u8>),
    Redirect(String),
    Status(u16),
}

#[derive(Clone, Debug)]
pub(crate) struct Hit {
    pub path: String,
    pub auth: Option<String>,
}

pub(crate) struct Server {
    base: String,
    routes: Arc<Mutex<HashMap<String, Route>>>,
    hits: Arc<Mutex<Vec<Hit>>>,
}

impl Server {
    pub(crate) fn start() -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let routes: Arc<Mutex<HashMap<String, Route>>> = Arc::default();
        let hits: Arc<Mutex<Vec<Hit>>> = Arc::default();
        let (r, h) = (routes.clone(), hits.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut buf = [0u8; 8192];
                let n = stream.read(&mut buf).unwrap_or(0);
                let text = String::from_utf8_lossy(&buf[..n]).to_string();
                let path = text.split_whitespace().nth(1).unwrap_or("/").to_string();
                let auth = text
                    .lines()
                    .find_map(|l| {
                        l.strip_prefix("authorization: ")
                            .or_else(|| l.strip_prefix("Authorization: "))
                    })
                    .map(|v| v.trim().to_string());
                h.lock().unwrap().push(Hit { path: path.clone(), auth });
                let route = r.lock().unwrap().get(&path).cloned().unwrap_or(Route::Status(404));
                let (head, body): (String, Vec<u8>) = match route {
                    Route::Body(b) => ("200 OK".into(), b),
                    Route::Redirect(to) => (format!("302 Found\r\nLocation: {to}"), Vec::new()),
                    Route::Status(c) => (format!("{c} Nope"), Vec::new()),
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {head}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(&body);
            }
        });
        Server { base, routes, hits }
    }

    pub(crate) fn put(&self, path: &str, route: Route) {
        self.routes.lock().unwrap().insert(path.to_string(), route);
    }

    pub(crate) fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    pub(crate) fn hits_for(&self, path: &str) -> Vec<Hit> {
        self.hits.lock().unwrap().iter().filter(|h| h.path == path).cloned().collect()
    }

    pub(crate) fn total_hits(&self) -> usize {
        self.hits.lock().unwrap().len()
    }
}

/// An `npm pack`-like tarball: every file under `top/`.
pub(crate) fn pack(top: &str, files: &[(&str, &str)]) -> Vec<u8> {
    let mut raw = Vec::new();
    {
        let gz = flate2::write::GzEncoder::new(&mut raw, flate2::Compression::fast());
        let mut b = tar::Builder::new(gz);
        for (path, content) in files {
            let mut h = tar::Header::new_gnu();
            h.set_size(content.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            b.append_data(&mut h, format!("{top}/{path}"), content.as_bytes()).unwrap();
        }
        b.into_inner().unwrap().finish().unwrap();
    }
    raw
}

pub(crate) fn pkg_json(name: &str, version: &str, deps: serde_json::Value) -> String {
    json!({"name": name, "version": version, "main": "index.js", "dependencies": deps}).to_string()
}

pub(crate) fn simple_tarball(name: &str, version: &str, deps: serde_json::Value) -> Vec<u8> {
    pack(
        "package",
        &[
            ("package.json", &pkg_json(name, version, deps)),
            ("index.js", &format!("exports.name = '{name}@{version}';\n")),
        ],
    )
}

fn sri512(bytes: &[u8]) -> String {
    fetch::Digests::of(bytes).of_algo(fetch::Algo::Sha512).sri()
}

fn source(name: &str, spec: &str) -> TarballSource {
    let (url, declared) = fetch::parse_tarball_spec(spec).unwrap();
    TarballSource {
        name: name.into(),
        spec: spec.into(),
        url,
        declared,
        origin: format!("package.json dependencies \"{name}\": \"{spec}\""),
        declared_by: "package.json".into(),
    }
}

struct World {
    store: TempDir,
    server: Server,
    lookups: Arc<Mutex<Vec<String>>>,
}

impl World {
    fn new() -> World {
        let store = TempDir::new().unwrap();
        seed_store_package(
            store.path(),
            "leaf",
            "1.4.0",
            &[
                ("package.json", r#"{"name":"leaf","version":"1.4.0","main":"index.js"}"#),
                ("index.js", "exports.leaf = 'leaf-1.4.0';\n"),
            ],
        );
        World { store, server: Server::start(), lookups: Arc::default() }
    }

    fn registry(&self) -> impl Fn(&str) -> Result<resolve::Packument> {
        let lookups = self.lookups.clone();
        move |name: &str| {
            lookups.lock().unwrap().push(name.to_string());
            match name {
                "leaf" => Ok(resolve::parse_packument(&json!({
                    "dist-tags": {"latest": "1.4.0"},
                    "versions": {"1.0.0": {}, "1.4.0": {}},
                }))),
                other => anyhow::bail!("{other} is not in the test registry"),
            }
        }
    }

    fn run(
        &self,
        sources: &[TarballSource],
        previous: &[TarballPackageEntry],
        token: Option<&str>,
    ) -> Result<(Vec<TarballPackageEntry>, PrepareReport)> {
        let registry = self.registry();
        let token = token.map(String::from);
        let token_for = move |_: &str| token.clone();
        prepare_all(
            self.store.path(),
            sources,
            previous,
            &PrepareOptions {
                force: false,
                host: HostTriple::current(),
                token_for: &token_for,
                registry: &registry,
                allow_plain_http_token: true,
            },
        )
    }

    fn lookups(&self) -> usize {
        self.lookups.lock().unwrap().len()
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[test]
fn downloads_once_records_integrity_and_resolves_own_dependencies() {
    let w = World::new();
    let bytes = simple_tarball("@etcito/haa", "0.1.0", json!({"leaf": "^1.0.0"}));
    w.server.put("/rel/haa-0.1.0.tgz", Route::Body(bytes.clone()));
    let src = source("@etcito/haa", &w.server.url("/rel/haa-0.1.0.tgz"));

    let (entries, report) = w.run(&[src], &[], None).unwrap();
    let e = &entries[0];
    assert_eq!(e.name, "@etcito/haa");
    assert_eq!(e.version, "0.1.0");
    assert_eq!(e.url, w.server.url("/rel/haa-0.1.0.tgz"));
    assert_eq!(e.integrity, sri512(&bytes));
    assert!(e.store_path.starts_with("node/tarballs/@etcito+haa/"), "{}", e.store_path);
    assert_eq!((report.downloaded, report.registry_lookups), (1, 1));
    assert_eq!(e.packages.len(), 1);
    assert_eq!((e.packages[0].name.as_str(), e.packages[0].version.as_str()), ("leaf", "1.4.0"));
    check_prepared(w.store.path(), e).unwrap();

    // The package and its own dependency are where Node will look for them.
    let target = tarball_link_target(w.store.path(), e);
    assert!(target.join("index.js").is_file());
    assert!(target.join("node_modules").join("leaf").join("index.js").is_file());
    assert_eq!(w.server.hits_for("/rel/haa-0.1.0.tgz").len(), 1);
}

#[test]
fn an_unchanged_lock_does_no_network_at_all() {
    let w = World::new();
    let bytes = simple_tarball("haa", "0.1.0", json!({"leaf": "^1.0.0"}));
    w.server.put("/haa.tgz", Route::Body(bytes));
    let src = source("haa", &w.server.url("/haa.tgz"));
    let (first, _) = w.run(&[src.clone()], &[], None).unwrap();
    let (http, lookups) = (w.server.total_hits(), w.lookups());

    let (again, report) = w.run(&[src], &first, None).unwrap();
    assert_eq!(again, first);
    assert_eq!((report.downloaded, report.registry_lookups), (0, 0));
    assert_eq!(w.server.total_hits(), http, "no http request on a re-run");
    assert_eq!(w.lookups(), lookups, "no registry lookup on a re-run");
}

#[test]
fn a_declared_integrity_is_verified_and_a_mismatch_is_a_hard_error() {
    let w = World::new();
    let bytes = simple_tarball("haa", "0.1.0", json!({}));
    w.server.put("/haa.tgz", Route::Body(bytes.clone()));
    let d = fetch::Digests::of(&bytes);

    // sha256 in hex and sha512 in base64 are both accepted.
    for fragment in [
        format!("sha256={}", d.of_algo(fetch::Algo::Sha256).hex()),
        sri512(&bytes),
    ] {
        let w2 = World::new();
        w2.server.put("/haa.tgz", Route::Body(bytes.clone()));
        let src = source("haa", &format!("{}#{fragment}", w2.server.url("/haa.tgz")));
        let (entries, _) = w2.run(&[src], &[], None).unwrap();
        assert_eq!(entries[0].integrity, sri512(&bytes));
    }

    let wrong = format!("sha256={}", "00".repeat(32));
    let src = source("haa", &format!("{}#{wrong}", w.server.url("/haa.tgz")));
    let err = w.run(&[src], &[], None).unwrap_err().to_string();
    for expected in ["integrity mismatch", "declared", "tarball dependency 'haa'"] {
        assert!(err.contains(expected), "missing {expected:?} in {err}");
    }
    // Nothing was extracted for a refused file.
    assert!(!w.store.path().join("node/tarballs").exists());
}

#[test]
fn trust_on_first_use_then_the_lock_is_enforced() {
    let w = World::new();
    w.server.put("/haa.tgz", Route::Body(simple_tarball("haa", "0.1.0", json!({}))));
    let src = source("haa", &w.server.url("/haa.tgz"));
    let (first, _) = w.run(&[src.clone()], &[], None).unwrap();

    // The asset is replaced on the server and the store is empty (a new machine).
    w.server.put("/haa.tgz", Route::Body(simple_tarball("haa", "0.1.1", json!({}))));
    std::fs::remove_dir_all(w.store.path().join("node/tarballs")).unwrap();
    let err = w.run(&[src], &first, None).unwrap_err().to_string();
    assert!(err.contains("integrity mismatch") && err.contains("recorded"), "{err}");
    assert!(err.contains(&first[0].integrity), "{err}");
}

#[test]
fn a_declared_hash_that_disagrees_with_the_stored_file_is_refused_offline() {
    let w = World::new();
    w.server.put("/haa.tgz", Route::Body(simple_tarball("haa", "0.1.0", json!({}))));
    let (first, _) = w.run(&[source("haa", &w.server.url("/haa.tgz"))], &[], None).unwrap();
    let hits = w.server.total_hits();
    let wrong = format!("sha256={}", "11".repeat(32));
    let src = source("haa", &format!("{}#{wrong}", w.server.url("/haa.tgz")));
    let err = w.run(&[src], &first, None).unwrap_err().to_string();
    assert!(err.contains("integrity mismatch"), "{err}");
    assert_eq!(w.server.total_hits(), hits, "detected from the store, no download");
}

#[test]
fn the_token_goes_to_the_original_origin_only_and_is_never_recorded() {
    let w = World::new();
    let other = Server::start();
    let bytes = simple_tarball("haa", "0.1.0", json!({}));
    other.put("/signed/asset?sig=SECRETSIG", Route::Body(bytes));
    w.server.put("/rel/haa.tgz", Route::Redirect(other.url("/signed/asset?sig=SECRETSIG")));
    let src = source("haa", &w.server.url("/rel/haa.tgz"));

    let token = "tok-SENTINEL-123";
    let (entries, _) = w.run(&[src], &[], Some(token)).unwrap();
    assert_eq!(
        w.server.hits_for("/rel/haa.tgz")[0].auth.as_deref(),
        Some(format!("Bearer {token}").as_str())
    );
    assert_eq!(
        other.hits_for("/signed/asset?sig=SECRETSIG")[0].auth,
        None,
        "no header after the redirect"
    );

    // The token appears in no entry and in no file of the store.
    let rules = serde_json::to_string(&entries).unwrap();
    assert!(!rules.contains(token) && !rules.contains("SECRETSIG"));
    for f in walkdir::WalkDir::new(w.store.path())
        .into_iter()
        .flatten()
        .filter(|e| e.file_type().is_file())
    {
        let text = std::fs::read(f.path()).unwrap();
        assert!(
            !String::from_utf8_lossy(&text).contains(token),
            "token found in {}",
            f.path().display()
        );
    }
}

#[test]
fn a_missing_credential_fails_fast_with_a_clear_message_and_no_secret() {
    let w = World::new();
    w.server.put("/private.tgz", Route::Status(404));
    let src = source("haa", &w.server.url("/private.tgz"));
    let err = w.run(&[src.clone()], &[], None).unwrap_err().to_string();
    for expected in [
        "HTTP 404",
        "no credential was sent",
        "KONG_GIT_TOKEN",
        "GITHUB_TOKEN",
        "tarball dependency 'haa'",
    ] {
        assert!(err.contains(expected), "missing {expected:?} in {err}");
    }
    let err = w.run(&[src], &[], Some("tok-SENTINEL-9")).unwrap_err().to_string();
    assert!(err.contains("refused"), "{err}");
    assert!(!err.contains("tok-SENTINEL-9"), "{err}");
}

#[test]
fn plain_http_never_receives_a_token_in_production_mode() {
    let w = World::new();
    w.server.put("/a.tgz", Route::Body(simple_tarball("haa", "0.1.0", json!({}))));
    fetch::get_with(&w.server.url("/a.tgz"), Some("tok-SENTINEL-7"), false).unwrap();
    assert_eq!(w.server.hits_for("/a.tgz")[0].auth, None);
}

#[test]
fn a_package_folder_other_than_package_works_and_scoped_names_are_kept() {
    let w = World::new();
    let bytes = pack(
        "etcito-haa-0.1.0",
        &[
            ("package.json", &pkg_json("@etcito/haa", "0.1.0", json!({}))),
            ("index.js", "exports.ok = true;\n"),
        ],
    );
    w.server.put("/haa.tgz", Route::Body(bytes));
    let (entries, _) = w.run(&[source("@etcito/haa", &w.server.url("/haa.tgz"))], &[], None).unwrap();
    let target = tarball_link_target(w.store.path(), &entries[0]);
    assert!(target.join("package.json").is_file() && target.join("index.js").is_file());
}

#[test]
fn a_tarball_that_is_not_an_npm_package_is_refused_and_leaves_no_store_dir() {
    let w = World::new();
    w.server.put("/bad.tgz", Route::Body(pack("package", &[("readme.txt", "hi")])));
    let err = w.run(&[source("bad", &w.server.url("/bad.tgz"))], &[], None).unwrap_err().to_string();
    assert!(err.contains("no package.json"), "{err}");
    let leftovers = std::fs::read_dir(w.store.path().join("node/tarballs/bad"))
        .map(|d| d.count())
        .unwrap_or(0);
    assert_eq!(leftovers, 0);
}

#[test]
fn a_tarball_can_depend_on_another_tarball_by_url() {
    let w = World::new();
    let b = simple_tarball("@etcito/b", "0.2.0", json!({"leaf": "^1.0.0"}));
    w.server.put("/b.tgz", Route::Body(b.clone()));
    let a = simple_tarball("@etcito/a", "0.1.0", json!({"@etcito/b": w.server.url("/b.tgz")}));
    w.server.put("/a.tgz", Route::Body(a));
    let src = source("@etcito/a", &w.server.url("/a.tgz"));

    let (entries, report) = w.run(&[src.clone()], &[], None).unwrap();
    assert_eq!(report.downloaded, 2);
    let a = &entries[0];
    let nested = a.packages.iter().find(|p| p.name == "@etcito/b").unwrap();
    assert_eq!(nested.hash.as_deref(), Some(sri512(&b).as_str()));
    assert_eq!(nested.source_url.as_deref(), Some(w.server.url("/b.tgz").as_str()));
    let target = tarball_link_target(w.store.path(), a);
    let nm = target.join("node_modules");
    assert!(nm.join("@etcito").join("b").join("index.js").is_file());
    assert!(nm.join("leaf").join("index.js").is_file(), "B's own registry dependency is hoisted");

    // Locked: no network; and the nested file is re-verified if the store lost it.
    let hits = w.server.total_hits();
    let (again, report) = w.run(&[src.clone()], &entries, None).unwrap();
    assert_eq!((again == entries, report.downloaded, w.server.total_hits()), (true, 0, hits));
    std::fs::remove_dir_all(w.store.path().join(&nested.store_path)).unwrap();
    let (again, report) = w.run(&[src], &entries, None).unwrap();
    assert_eq!((again == entries, report.downloaded), (true, 1));
    assert!(w.store.path().join(&nested.store_path).join(".kong-fetched").is_file());
}

#[test]
fn the_same_file_with_another_resolved_tree_gets_its_own_tree_directory() {
    let w = World::new();
    w.server.put("/haa.tgz", Route::Body(simple_tarball("haa", "0.1.0", json!({"leaf": "^1.0.0"}))));
    let src = source("haa", &w.server.url("/haa.tgz"));
    let (first, _) = w.run(&[src.clone()], &[], None).unwrap();
    // Another project resolved `leaf` to 1.0.0 at another time.
    seed_store_package(
        w.store.path(),
        "leaf",
        "1.0.0",
        &[
            ("package.json", r#"{"name":"leaf","version":"1.0.0"}"#),
            ("index.js", "exports.leaf='1.0.0';\n"),
        ],
    );
    let mut other = first[0].clone();
    other.packages[0].version = "1.0.0".into();
    other.packages[0].store_path = "node/libs/leaf-1.0.0".into();
    let (second, _) = w.run(&[src], &[other], None).unwrap();
    assert_eq!(second[0].packages[0].version, "1.0.0");
    assert_ne!(second[0].store_path, first[0].store_path);
    let read = |e: &TarballPackageEntry| {
        std::fs::read_to_string(tarball_link_target(w.store.path(), e).join("node_modules/leaf/index.js"))
            .unwrap()
    };
    assert!(read(&first[0]).contains("1.4.0") && read(&second[0]).contains("1.0.0"));
}

#[test]
fn check_prepared_asks_for_kong_rules_when_the_tree_is_gone() {
    let w = World::new();
    w.server.put("/haa.tgz", Route::Body(simple_tarball("haa", "0.1.0", json!({}))));
    let (entries, _) = w.run(&[source("haa", &w.server.url("/haa.tgz"))], &[], None).unwrap();
    std::fs::remove_dir_all(w.store.path().join("node/tarballs")).unwrap();
    let err = check_prepared(w.store.path(), &entries[0]).unwrap_err().to_string();
    for expected in [
        "tarball dependency 'haa'",
        entries[0].integrity.as_str(),
        "kong.rules node.tarballs",
        "run `kong rules`",
    ] {
        assert!(err.contains(expected), "missing {expected:?} in {err}");
    }
}

#[test]
fn redact_masks_every_secret() {
    assert_eq!(redact("a tok b tok", &["tok".into(), "".into()]), "a *** b ***");
}
