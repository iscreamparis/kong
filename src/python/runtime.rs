/// Download a standalone Python runtime from python-build-standalone.
/// Stores it at `<store>/python/runtime/<version>/` and returns the path to python.exe.
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use tracing::{debug, info};

const PBS_RELEASES_API: &str =
    "https://api.github.com/repos/indygreg/python-build-standalone/releases";

/// A resolved Python runtime in the store.
pub struct PythonRuntime {
    pub version: String,
    pub store_path: String,     // relative, e.g. "python/runtime/3.12.9"
    pub _python_exe: PathBuf,   // absolute path to python.exe / python3
}

/// Ensure the requested Python major.minor is present in the store.
/// `requested` can be "3.12", "3.11", etc. Pass "" / "latest" to pick the newest.
pub fn ensure_runtime(store_root: &Path, requested: &str) -> Result<PythonRuntime> {
    ensure_runtime_for(store_root, &RuntimeRequest::from_cli(requested))
}

/// Like `ensure_runtime`, for an already-classified request (see `runtime_request`).
pub fn ensure_runtime_for(store_root: &Path, request: &RuntimeRequest) -> Result<PythonRuntime> {
    let (asset_url, version) = resolve_asset(request)?;

    let store_path = format!("python/runtime/{version}");
    let runtime_dir = store_root.join(&store_path);

    if runtime_dir.exists() && python_exe_in(&runtime_dir).is_some() {
        debug!(version = %version, "Python runtime already in store");
    } else {
        info!(version = %version, "Downloading Python runtime");
        download_and_extract(&asset_url, &runtime_dir)?;
    }

    let exe = python_exe_in(&runtime_dir)
        .with_context(|| format!("python executable not found after extraction in {}", runtime_dir.display()))?;

    info!(version = %version, exe = %exe.display(), "Python runtime ready");
    Ok(PythonRuntime { version, store_path, _python_exe: exe })
}

// ── Runtime version selection ───────────────────────────────────────────────

/// Which Python runtime a project asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeRequest {
    /// No preference: historical behaviour, the first matching asset of the
    /// most recent python-build-standalone release.
    Latest,
    /// A version prefix on component boundaries: "3.11" or "3.11.9".
    Version(String),
    /// A PEP 440 specifier set (`requires-python`): newest runtime satisfying it.
    Spec(String),
}

impl RuntimeRequest {
    /// Classify a user-supplied value (`--python`, or the legacy string API).
    /// "" / "latest" → Latest; a leading operator → Spec; else → Version.
    pub fn from_cli(value: &str) -> Self {
        let v = value.trim();
        if v.is_empty() || v.eq_ignore_ascii_case("latest") {
            return RuntimeRequest::Latest;
        }
        if v.starts_with(['<', '>', '=', '!', '~']) {
            return RuntimeRequest::Spec(v.to_string());
        }
        let v = v.strip_prefix("cpython-").unwrap_or(v);
        RuntimeRequest::Version(v.trim_start_matches("python").trim().to_string())
    }
}

/// Decide the runtime for a project. Precedence: explicit `--python` flag, then
/// `.python-version`, then `[project] requires-python` in pyproject.toml, else
/// Latest (unchanged default). Unreadable/unusable files are skipped.
pub fn runtime_request(project_dir: &Path, cli: Option<&str>) -> RuntimeRequest {
    if let Some(v) = cli.filter(|v| !v.trim().is_empty()) {
        return RuntimeRequest::from_cli(v);
    }
    if let Ok(text) = std::fs::read_to_string(project_dir.join(".python-version")) {
        if let Some(v) = parse_python_version_file(&text) {
            return RuntimeRequest::Version(v);
        }
    }
    if let Ok(text) = std::fs::read_to_string(project_dir.join("pyproject.toml")) {
        if let Some(spec) = pyproject_requires_python(&text) {
            return RuntimeRequest::Spec(spec);
        }
    }
    RuntimeRequest::Latest
}

/// First usable CPython version in a `.python-version` file (pyenv / uv format):
/// skips blank and `#` lines; accepts "3.11", "3.11.9", "cpython-3.11", "python3.11".
/// Non-CPython entries (e.g. "pypy3.10", "system") yield None.
pub fn parse_python_version_file(text: &str) -> Option<String> {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty() && !l.starts_with('#'))?;
    let v = line.strip_prefix("cpython-").unwrap_or(line);
    let v = v.strip_prefix("python").unwrap_or(v).trim();
    // uv may write e.g. "3.11.9-macos-aarch64-none"; keep the version part.
    let v = v.split(['-', '/']).next().unwrap_or("");
    let ok = !v.is_empty()
        && v.chars().next().is_some_and(|c| c.is_ascii_digit())
        && v.chars().all(|c| c.is_ascii_digit() || c == '.');
    ok.then(|| v.trim_end_matches('.').to_string())
}

/// `[project] requires-python` from pyproject.toml, if present and non-empty.
pub fn pyproject_requires_python(text: &str) -> Option<String> {
    let doc: toml::Value = toml::from_str(text).ok()?;
    let spec = doc.get("project")?.get("requires-python")?.as_str()?.trim();
    (!spec.is_empty()).then(|| spec.to_string())
}

/// Pick the index of the runtime to use among `candidates` (versions in the
/// order the release feed lists them).
pub fn select_runtime_version(candidates: &[String], request: &RuntimeRequest) -> Option<usize> {
    match request {
        RuntimeRequest::Latest => (!candidates.is_empty()).then_some(0),
        RuntimeRequest::Version(want) => candidates
            .iter()
            .position(|v| v == want || v.starts_with(&format!("{want}."))),
        RuntimeRequest::Spec(spec) => {
            let set = super::pep440::SpecifierSet::parse(spec);
            let best = super::pep440::select_best(candidates, &set)?;
            candidates.iter().position(|v| v == best)
        }
    }
}

// ── Internal helpers ────────────────────────────────────────────────────────

/// Fetch the GitHub releases list and find the best asset for this platform.
fn resolve_asset(request: &RuntimeRequest) -> Result<(String, String)> {
    let platform_suffix = platform_asset_suffix();

    let client = reqwest::blocking::Client::builder()
        .user_agent("kong-dependency-manager/0.1")
        .build()?;

    // Fetch the first page of releases (most recent first)
    let releases: Vec<GhRelease> = client
        .get(PBS_RELEASES_API)
        .query(&[("per_page", "10")])
        .send()
        .context("failed to reach GitHub API for python-build-standalone")?
        .json()
        .context("failed to parse GitHub releases JSON")?;

    // Every installable asset for this platform, in feed order. The first entry
    // per version wins (newest release first), as before.
    let mut versions: Vec<String> = Vec::new();
    let mut urls: Vec<(String, String)> = Vec::new();
    for release in &releases {
        for asset in &release.assets {
            if !asset.name.ends_with(".tar.gz") {
                continue;
            }
            if !asset.name.contains("install_only") {
                continue;
            }
            if !asset.name.contains(&platform_suffix) {
                continue;
            }
            // Extract version from filename: cpython-3.12.9+20250101-x86_64-...
            let version = extract_version(&asset.name)?;
            if versions.contains(&version) {
                continue;
            }
            versions.push(version.clone());
            urls.push((asset.name.clone(), asset.browser_download_url.clone()));
        }
    }

    if let Some(i) = select_runtime_version(&versions, request) {
        debug!(asset = %urls[i].0, version = %versions[i], "Selected Python asset");
        return Ok((urls[i].1.clone(), versions[i].clone()));
    }

    bail!(
        "No suitable Python runtime found for platform '{platform_suffix}' (requested: {request:?}). \
         Check https://github.com/indygreg/python-build-standalone/releases"
    )
}

/// Download the tar.gz and extract into `dest`.
fn download_and_extract(url: &str, dest: &Path) -> Result<()> {
    info!(url = %url, "Downloading Python runtime archive");
    let tmp = tempfile::TempDir::new()?;
    let result = crate::download::download_and_verify(url, tmp.path(), None)?;
    info!(size = result.path.metadata().map(|m| m.len()).unwrap_or(0), "Downloaded Python runtime");

    std::fs::create_dir_all(dest)?;

    // python-build-standalone tarballs contain a top-level `python/` directory
    let file = std::fs::File::open(&result.path)?;
    let decompressed = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(decompressed);

    // Extract, stripping the first component ("python/") so files land directly in dest
    for entry in archive.entries()? {
        let mut entry = entry?;
        let raw_path = entry.path()?.to_path_buf();
        let stripped = raw_path
            .components()
            .skip(1)
            .collect::<std::path::PathBuf>();
        if stripped.as_os_str().is_empty() {
            continue;
        }
        let out = dest.join(&stripped);
        if entry.header().entry_type().is_dir() {
            std::fs::create_dir_all(&out)?;
        } else {
            if let Some(p) = out.parent() {
                std::fs::create_dir_all(p)?;
            }
            entry.unpack(&out)?;
        }
    }

    info!(dest = %dest.display(), "Python runtime extracted");
    Ok(())
}

/// Find python.exe / python3 inside the extracted runtime directory.
pub fn python_exe_in(runtime_dir: &Path) -> Option<PathBuf> {
    #[cfg(windows)]
    let candidates = ["python.exe", "Scripts/python.exe"];
    #[cfg(not(windows))]
    let candidates = ["bin/python3", "bin/python"];

    for rel in &candidates {
        let p = runtime_dir.join(rel);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

fn extract_version(filename: &str) -> Result<String> {
    // e.g. "cpython-3.12.9+20250101-x86_64-pc-windows-msvc-install_only.tar.gz"
    let rest = filename
        .strip_prefix("cpython-")
        .context("unexpected asset filename format")?;
    let version = rest
        .split('+')
        .next()
        .context("could not parse version from filename")?
        .to_string();
    Ok(version)
}

fn platform_asset_suffix() -> String {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => "x86_64-pc-windows-msvc-install_only".to_string(),
        ("linux", "x86_64")   => "x86_64-unknown-linux-gnu-install_only".to_string(),
        ("macos", "x86_64")   => "x86_64-apple-darwin-install_only".to_string(),
        ("macos", "aarch64")  => "aarch64-apple-darwin-install_only".to_string(),
        (os, arch) => format!("{arch}-unknown-{os}-install_only"),
    }
}

// ── GitHub API types ────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct GhRelease {
    assets: Vec<GhAsset>,
}

#[derive(Debug, Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed() -> Vec<String> {
        // Feed order as python-build-standalone lists it (oldest minor first).
        ["3.10.19", "3.11.14", "3.12.12", "3.13.9", "3.14.0"].iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn cli_values_are_classified() {
        assert_eq!(RuntimeRequest::from_cli(""), RuntimeRequest::Latest);
        assert_eq!(RuntimeRequest::from_cli("latest"), RuntimeRequest::Latest);
        assert_eq!(RuntimeRequest::from_cli("3.11"), RuntimeRequest::Version("3.11".into()));
        assert_eq!(RuntimeRequest::from_cli("python3.12"), RuntimeRequest::Version("3.12".into()));
        assert_eq!(RuntimeRequest::from_cli(">=3.11,<3.13"), RuntimeRequest::Spec(">=3.11,<3.13".into()));
    }

    #[test]
    fn python_version_file_parsing() {
        assert_eq!(parse_python_version_file("3.11\n"), Some("3.11".into()));
        assert_eq!(parse_python_version_file("# pinned\n\n  3.12.4  \n3.10\n"), Some("3.12.4".into()));
        assert_eq!(parse_python_version_file("cpython-3.11.9-linux-x86_64-gnu\n"), Some("3.11.9".into()));
        assert_eq!(parse_python_version_file("python3.13"), Some("3.13".into()));
        assert_eq!(parse_python_version_file("pypy3.10"), None);
        assert_eq!(parse_python_version_file("system"), None);
        assert_eq!(parse_python_version_file(""), None);
    }

    #[test]
    fn pyproject_requires_python_parsing() {
        let t = "[project]\nname = \"x\"\nrequires-python = \">=3.11\"\n";
        assert_eq!(pyproject_requires_python(t), Some(">=3.11".into()));
        assert_eq!(pyproject_requires_python("[project]\nname = \"x\"\n"), None);
        assert_eq!(pyproject_requires_python("[tool.poetry]\nname = \"x\"\n"), None);
        assert_eq!(pyproject_requires_python("not toml ["), None);
    }

    #[test]
    fn selection_honours_each_request_kind() {
        let f = feed();
        // Latest keeps today's behaviour: first asset in feed order.
        assert_eq!(select_runtime_version(&f, &RuntimeRequest::Latest), Some(0));
        assert_eq!(select_runtime_version(&f, &RuntimeRequest::Version("3.11".into())), Some(1));
        assert_eq!(select_runtime_version(&f, &RuntimeRequest::Version("3.12.12".into())), Some(2));
        // Component boundary: "3.1" must not match 3.10.x.
        assert_eq!(select_runtime_version(&f, &RuntimeRequest::Version("3.1".into())), None);
        assert_eq!(select_runtime_version(&f, &RuntimeRequest::Version("3.9".into())), None);
        // Spec: newest satisfying.
        assert_eq!(select_runtime_version(&f, &RuntimeRequest::Spec(">=3.11".into())), Some(4));
        assert_eq!(select_runtime_version(&f, &RuntimeRequest::Spec(">=3.11,<3.13".into())), Some(2));
        assert_eq!(select_runtime_version(&f, &RuntimeRequest::Spec(">=4".into())), None);
        assert_eq!(select_runtime_version(&[], &RuntimeRequest::Latest), None);
    }

    #[test]
    fn project_request_precedence() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        assert_eq!(runtime_request(p, None), RuntimeRequest::Latest);
        std::fs::write(p.join("pyproject.toml"), "[project]\nname = \"x\"\nrequires-python = \">=3.11\"\n").unwrap();
        assert_eq!(runtime_request(p, None), RuntimeRequest::Spec(">=3.11".into()));
        std::fs::write(p.join(".python-version"), "3.12\n").unwrap();
        assert_eq!(runtime_request(p, None), RuntimeRequest::Version("3.12".into()));
        assert_eq!(runtime_request(p, Some("3.13")), RuntimeRequest::Version("3.13".into()));
        assert_eq!(runtime_request(p, Some("")), RuntimeRequest::Version("3.12".into()));
        // An unusable .python-version falls through to pyproject.
        std::fs::write(p.join(".python-version"), "pypy3.10\n").unwrap();
        assert_eq!(runtime_request(p, None), RuntimeRequest::Spec(">=3.11".into()));
    }
}
