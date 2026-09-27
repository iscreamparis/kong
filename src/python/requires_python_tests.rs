use super::*;
use super::super::pep440::SpecifierSet;
use std::io::{Read, Write};
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};

fn file(version: &str, spec: Option<&str>, sdist: bool) -> PypiFileEntry {
    serde_json::from_value(serde_json::json!({
        "filename": if sdist { format!("fixture-{version}.tar.gz") }
            else { format!("fixture-{version}-py3-none-any.whl") },
        "url": "https://example.invalid/archive", "digests": {"sha256": ""},
        "packagetype": if sdist { "sdist" } else { "bdist_wheel" },
        "requires_python": spec
    })).unwrap()
}

#[test]
fn full_python_version_specifiers_and_absent_metadata() {
    for spec in [None, Some(""), Some(" >=3.10.20, <3.11 "), Some("~=3.10.0"),
        Some("==3.10.*"), Some("!=3.10.20"), Some("<=3.10.21")] {
        assert!(supports_python(spec, "3.10.21"), "{spec:?}");
    }
    for spec in [">=3.11", ">=3.10.22", "!=3.10.*", "!=3.10.21", "<3.10.21", "~=3.9.0"] {
        assert!(!supports_python(Some(spec), "3.10.21"), "{spec}");
    }
}

#[test]
fn mixed_release_selects_compatible_file_not_first_file() {
    let files = vec![file("2.0", Some(">=3.11"), false), file("2.0", Some(">=3.10"), false)];
    let chosen = select_best_file(&files, "cp310", "3.10.21").unwrap();
    assert_eq!(chosen.requires_python.as_deref(), Some(">=3.10"));
    assert!(select_best_file(&files[..1], "cp310", "3.10.21").is_none());
}

#[test]
fn sdist_fallback_and_exact_pins_cannot_bypass_requires_python() {
    let releases = vec![
        ("1.0".into(), vec![file("1.0", Some(">=3.10"), true)]),
        ("2.0".into(), vec![file("2.0", Some(">=3.11"), true)]),
    ];
    let resolve = |spec: &str| resolve_from_releases("fixture", &SpecifierSet::parse(spec), "cp310", "3.10.21", &releases);
    assert_eq!(resolve("").unwrap(), "1.0");
    assert_eq!(resolve("==1.0").unwrap(), "1.0");
    let error = resolve("==2.0").unwrap_err().to_string();
    assert!(error.contains("fixture") && error.contains("3.10.21"), "{error}");
    assert!(resolve(">=2").is_err());
    assert!(resolve("==9").is_err());
}

/// Real loopback PyPI JSON endpoints and tiny wheel, with graceful shutdown even
/// if an assertion fails. No external network or environment-variable mutation.
struct Index {
    base: String,
    stopped: Arc<AtomicBool>,
    server: Option<std::thread::JoinHandle<()>>,
}
impl Index {
    fn new(release_only: bool) -> Self {
        Self::with_wheel_metadata(release_only, ">=3.10")
    }
    fn with_wheel_metadata(release_only: bool, requires_python: &str) -> Self {
        use sha2::{Digest, Sha256};
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        zip.start_file("fixture-1.0.dist-info/METADATA", zip::write::SimpleFileOptions::default()).unwrap();
        zip.write_all(format!("Name: fixture\nVersion: 1.0\nRequires-Python: {requires_python}\nRequires-Dist: child>=1\n").as_bytes()).unwrap();
        let wheel = zip.finish().unwrap().into_inner();
        let hash = hex::encode(Sha256::digest(&wheel));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let mut releases: serde_json::Value = serde_json::from_str(include_str!("fixtures/requires_python.json")).unwrap();
        for version in ["1.0", "2.0"] {
            let f = &mut releases[version][0];
            f["url"] = format!("{base}/wheel.whl").into();
            f["digests"]["sha256"] = hash.clone().into();
            if release_only { f["requires_python"] = serde_json::Value::Null; }
        }
        let latest = serde_json::json!({"info":{"requires_python": ">=3.11"}, "releases":releases.clone()}).to_string();
        let old = serde_json::json!({"info":{"requires_python": ">=3.10", "requires_dist":["child>=1"]}, "urls":releases["1.0"]}).to_string();
        let new = serde_json::json!({"info":{"requires_python": ">=3.11", "requires_dist":[]}, "urls":releases["2.0"]}).to_string();
        let stopped = Arc::new(AtomicBool::new(false));
        let stop = stopped.clone();
        let server = std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let (mut stream, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(2));
                        continue;
                    }
                    Err(e) => panic!("{e}"),
                };
                stream.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
                let mut request = [0; 4096];
                let n = stream.read(&mut request).unwrap();
                let request = String::from_utf8_lossy(&request[..n]);
                let body = if request.starts_with("GET /wheel.whl ") { wheel.as_slice() }
                    else if request.contains("/1.0/json ") { old.as_bytes() }
                    else if request.contains("/2.0/json ") { new.as_bytes() }
                    else { latest.as_bytes() };
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
                stream.write_all(body).unwrap();
            }
        });
        Self { base, stopped, server: Some(server) }
    }
    fn resolve(&self, name: &str, spec: &str) -> Result<String> {
        resolve_best_version_from(&self.base, name, &SpecifierSet::parse(spec), "cp310", "3.10.21")
    }
}
impl Drop for Index {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        self.server.take().unwrap().join().unwrap();
    }
}

#[test]
fn fixture_index_direct_and_transitive_cold_and_warm() {
    let index = Index::new(false);
    let store = tempfile::tempdir().unwrap();
    let version = index.resolve("fixture", "").unwrap();
    assert_eq!(version, "1.0");
    let (_, cold_deps) = fetch_and_download_from(&index.base, "fixture", &version, "cp310", "3.10.21", store.path()).unwrap();
    validate_requires_python_in_store(store.path(), "3.10.21").unwrap();
    let warm_deps = crate::config::read_transitive_from_store(store.path());
    for deps in [cold_deps, warm_deps] {
        assert_eq!(deps.len(), 1);
        let child = &deps[0];
        assert_eq!(child.name, "child");
        assert_eq!(index.resolve(&child.name, &child.spec).unwrap(), "1.0");
    }
    assert_eq!(index.resolve("fixture", "").unwrap(), "1.0");
    assert!(index.resolve("fixture", "==2.0").is_err());
    // Download must defend itself too, even when called with a preselected pin.
    assert!(fetch_and_download_from(&index.base, "fixture", "2.0", "cp310", "3.10.21", store.path()).is_err());
}

#[test]
fn incompatible_wheel_metadata_is_not_accepted_into_store() {
    let index = Index::with_wheel_metadata(false, ">=3.11");
    let store = tempfile::tempdir().unwrap();
    assert!(fetch_and_download_from(&index.base, "fixture", "1.0", "cp310", "3.10.21", store.path()).is_err());
    // Same metadata guard is called on the production warm-store path.
    let error = validate_requires_python_in_store(store.path(), "3.10.21").unwrap_err().to_string();
    assert!(error.contains(">=3.11") && error.contains("3.10.21"), "{error}");
}

#[test]
fn selected_release_requires_python_not_latest_info() {
    let index = Index::new(true); // file metadata absent; release info is decisive
    assert_eq!(index.resolve("fixture", "").unwrap(), "1.0");
    assert!(index.resolve("fixture", "==2.0").is_err());
}
