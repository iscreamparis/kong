use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use tracing::{debug, info};

use crate::download::{self, FileInfo};

/// Metadata from PyPI JSON API.
#[derive(Debug, Deserialize)]
struct PypiPackageInfo {
    info: PypiInfo,
    releases: std::collections::HashMap<String, Vec<PypiFileEntry>>,
}

#[derive(Debug, Deserialize)]
struct PypiInfo {
    requires_dist: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct PypiFileEntry {
    filename: String,
    url: String,
    digests: PypiDigests,
    #[serde(default)]
    packagetype: String,
    #[serde(default)]
    requires_python: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PypiDigests {
    sha256: String,
}

/// A transitive dependency discovered from a wheel's `Requires-Dist`.
///
/// `version` is a concrete pin ONLY when the parent declared an exact `==X.Y.Z`
/// (with no wildcard); otherwise it is empty and `spec` carries the original
/// PEP 440 specifier (`>=2.10,<3`, `~=1.4`, …) so the resolver can pick the
/// highest version that actually satisfies the bound — not the global latest.
#[derive(Debug, Clone)]
pub struct TransitiveDep {
    pub name: String,
    pub version: String,
    /// The raw specifier string as declared by the parent (may be empty).
    pub spec: String,
}

/// Fetch metadata from PyPI, download the best wheel, and extract to store.
/// Returns the file info plus any transitive dependencies found in wheel METADATA.
///
/// `target_py_tag` is the interpreter's wheel tag (e.g. "cp310" for Python 3.10),
/// driven entirely by the runtime version — no package names are special-cased.
/// It is used to reject native wheels built for a different CPython
/// version/ABI (e.g. a cp313t wheel must never be chosen for a cp310 runtime),
/// so the wheel actually written matches the `cpXY` tag in the store dir name.
pub fn fetch_and_download(name: &str, version: &str, target_py_tag: &str, store_path: &Path) -> Result<(FileInfo, Vec<TransitiveDep>)> {
    let url = format!("https://pypi.org/pypi/{name}/json");
    debug!(url = %url, "Fetching PyPI metadata");

    let response = reqwest::blocking::get(&url)
        .with_context(|| format!("failed to fetch PyPI metadata for {name}"))?;

    if response.status() == reqwest::StatusCode::NOT_FOUND {
        bail!("package '{name}' not found on PyPI");
    }

    let info: PypiPackageInfo = response
        .json()
        .with_context(|| format!("failed to parse PyPI response for {name}"))?;

    let files = info
        .releases
        .get(version)
        .with_context(|| format!("version '{version}' not found for '{name}' on PyPI"))?;

    // Select best file: prefer a wheel COMPATIBLE with the target interpreter
    // (exact cpXY > abi3 > pure-python), then sdist. A wheel for a different
    // CPython version/ABI is rejected outright.
    let file = select_best_file(files, target_py_tag)
        .with_context(|| format!("no suitable file for {name}=={version} (target {target_py_tag})"))?;

    info!(filename = %file.filename, "Selected: {}", file.filename);

    // Download to a temp directory, then extract
    let tmp = tempfile::TempDir::new()?;
    let result = download::download_and_verify(
        &file.url,
        tmp.path(),
        Some(&file.digests.sha256),
    )?;

    // Extract to store
    crate::extract::extract(&result.path, store_path)?;
    crate::store::write_verified_marker(store_path, &result.hash)?;

    // Collect transitive deps from PyPI info.requires_dist (more reliable than
    // reading the extracted METADATA file — same data, no filesystem walk).
    let transitive = parse_requires_dist(info.info.requires_dist.as_deref().unwrap_or(&[]));

    Ok((FileInfo {
        hash: result.hash,
        url: file.url.clone(),
    }, transitive))
}

/// Resolve the latest version of a package from PyPI (used as the last-resort
/// fallback when a package has no applicable specifier).
pub fn resolve_latest_version(name: &str) -> Result<String> {
    #[derive(Deserialize)]
    struct Info { version: String }
    #[derive(Deserialize)]
    struct Response { info: Info }

    let url = format!("https://pypi.org/pypi/{name}/json");
    let resp: Response = reqwest::blocking::get(&url)
        .with_context(|| format!("failed to fetch PyPI metadata for {name}"))?
        .json()
        .with_context(|| format!("failed to parse PyPI response for {name}"))?;
    Ok(resp.info.version)
}

/// List every release version string for a package from PyPI (the keys of the
/// `releases` map), filtering out yanked-only releases (those whose every file
/// is yanked have an empty file list and are skipped).
pub fn list_versions(name: &str) -> Result<Vec<String>> {
    #[derive(Deserialize)]
    struct Response {
        releases: std::collections::HashMap<String, Vec<serde_json::Value>>,
    }

    let url = format!("https://pypi.org/pypi/{name}/json");
    let resp: Response = reqwest::blocking::get(&url)
        .with_context(|| format!("failed to fetch PyPI metadata for {name}"))?
        .json()
        .with_context(|| format!("failed to parse PyPI response for {name}"))?;

    // A release with no files has been fully removed/never-published — skip it.
    let versions: Vec<String> = resp
        .releases
        .into_iter()
        .filter(|(_, files)| !files.is_empty())
        .map(|(v, _)| v)
        .collect();
    Ok(versions)
}

/// Resolve the highest PyPI version of `name` that satisfies `spec`.
///
/// This is the core from-scratch resolver: instead of taking the global latest,
/// we list every released version and pick the highest one matching the PEP 440
/// specifier set. An empty specifier means "no constraint" → the latest stable.
/// If nothing satisfies (a genuine conflict / impossible bound) we warn and fall
/// back to the global latest so provisioning degrades rather than aborting.
pub fn resolve_best_version(name: &str, spec: &crate::python::pep440::SpecifierSet) -> Result<String> {
    // An exact `==` pin needs no version listing — honor it directly.
    if let Some(pin) = spec.exact_pin() {
        debug!(pkg = %name, ver = %pin, "Exact pin — using as-is");
        return Ok(pin);
    }

    let versions = list_versions(name)?;
    if let Some(best) = crate::python::pep440::select_best(&versions, spec) {
        debug!(pkg = %name, ver = %best, "Selected highest version satisfying specifier");
        return Ok(best.to_string());
    }

    // Nothing satisfied the constraint. This is either an unsatisfiable bound or
    // a version PyPI lists in a form we couldn't parse. Be loud, then degrade.
    tracing::warn!(
        pkg = %name,
        "No released version satisfies the constraint; falling back to latest \
         (provisioning may need a manual pin)"
    );
    resolve_latest_version(name)
}

/// Parse `Requires-Dist` lines — public alias so config.rs can call it for
/// already-cached packages.
pub fn parse_requires_dist_pub(entries: &[String]) -> Vec<TransitiveDep> {
    parse_requires_dist(entries)
}

/// Parse `Requires-Dist` lines from PyPI `info.requires_dist`.
/// Skips extras (conditional deps like `extra == "async"`) and environment
/// markers that would exclude this platform. Returns the dependency name, an
/// exact pin if the parent declared one (`==X.Y.Z`), and the RAW PEP 440
/// specifier string so the resolver can pick the highest satisfying version.
///
/// A bracketed extras request on the dependency itself (`requests[security]`)
/// has the `[...]` stripped — we resolve the base package and let its own
/// `Requires-Dist` surface any extra-gated deps (which we skip, matching pip's
/// default no-extras behaviour for transitive resolution here).
fn parse_requires_dist(entries: &[String]) -> Vec<TransitiveDep> {
    let mut deps = Vec::new();
    for entry in entries {
        // Skip anything with "extra ==" — those are optional deps
        if entry.contains("extra ==") || entry.contains("extra==") {
            continue;
        }
        // Strip environment markers (semicolon and after)
        let body = if let Some(idx) = entry.find(';') {
            entry[..idx].trim()
        } else {
            entry.trim()
        };
        // Parse "Name>=version,<other" — split the name (incl. optional [extras])
        // from the specifier. The name runs until the first specifier operator
        // or whitespace; brackets are part of the name token.
        let name_end = body
            .find(|c: char| {
                !c.is_alphanumeric() && c != '-' && c != '_' && c != '.' && c != '[' && c != ']'
            })
            .unwrap_or(body.len());
        // Drop any [extras] suffix from the resolved package name.
        let raw_name = body[..name_end].trim();
        let dep_name = match raw_name.find('[') {
            Some(b) => raw_name[..b].trim().to_string(),
            None => raw_name.to_string(),
        };
        if dep_name.is_empty() {
            continue;
        }
        let spec_str = body[name_end..].trim().to_string();
        // Honor an exact pin directly; otherwise carry the raw specifier.
        let version = extract_exact_pin(&spec_str).unwrap_or_default();
        deps.push(TransitiveDep {
            name: dep_name,
            version,
            spec: spec_str,
        });
    }
    deps
}

/// Extract a concrete version from a specifier ONLY when it is a single exact,
/// non-wildcard `==X.Y.Z` pin. For >= / ~= / ==X.* / ranges → None, so the
/// caller resolves the highest satisfying version via the specifier set.
fn extract_exact_pin(spec: &str) -> Option<String> {
    let set = crate::python::pep440::SpecifierSet::parse(spec);
    set.exact_pin()
}

/// Select the best file for the target interpreter.
///
/// The previous implementation matched wheels by PLATFORM tag only and never
/// looked at the Python-version / ABI tag, so for a multi-wheel release it could
/// pick a wheel built for a different CPython (e.g. a `cp313t` bcrypt wheel for a
/// `cp310` runtime). That .so cannot load on 3.10 → `ModuleNotFoundError`.
///
/// We now SCORE every wheel against the target tag and pick the highest-scoring
/// COMPATIBLE one, rejecting any wheel whose Python/ABI tag is for a different
/// interpreter. The compatibility check is fully version-driven from
/// `target_py_tag` (e.g. "cp310") — no package names are special-cased.
///
/// Preference order (most specific → least): exact `cpXY` (+abi) > `abi3`
/// (forward-compatible stable ABI, `cpXY`..target) > pure-python (`py3`/`none`).
/// Ties break deterministically on filename so the same wheel is always chosen.
fn select_best_file<'a>(files: &'a [PypiFileEntry], target_py_tag: &str) -> Option<&'a PypiFileEntry> {
    select_best_file_for(files, target_py_tag, &current_platform_tag(), &current_arch_suffix())
}

/// Core selection — platform tags are passed in so it is host-independent and
/// unit-testable (the host this runs on need not be the target platform).
fn select_best_file_for<'a>(
    files: &'a [PypiFileEntry],
    target_py_tag: &str,
    platform_tag: &str,
    arch_suffix: &str,
) -> Option<&'a PypiFileEntry> {
    let target = TargetTag::parse(target_py_tag);

    let mut best: Option<(i32, &PypiFileEntry)> = None;
    for f in files.iter().filter(|f| f.packagetype == "bdist_wheel") {
        if let Some(score) = score_wheel(&f.filename, &target, platform_tag, arch_suffix) {
            let better = match best {
                None => true,
                // Higher score wins; tie-break on filename for determinism.
                Some((bs, bf)) => score > bs || (score == bs && f.filename < bf.filename),
            };
            if better {
                best = Some((score, f));
            }
        }
    }
    if let Some((_, f)) = best {
        return Some(f);
    }

    // No compatible wheel — fall back to a source dist (built locally if able).
    files.iter().find(|f| f.packagetype == "sdist")
}

/// The target interpreter's wheel tag, parsed once. e.g. "cp310" → cp/3/10.
struct TargetTag {
    /// Full python tag of the interpreter, e.g. "cp310".
    py_tag: String,
    /// Interpreter implementation prefix, e.g. "cp" (CPython), "pp" (PyPy).
    impl_prefix: String,
    /// ABI minor version (the XY in cpXY) for abi3 forward-compat checks.
    abi_minor: Option<u32>,
}

impl TargetTag {
    fn parse(tag: &str) -> Self {
        // Split a leading alpha implementation prefix (cp / pp / py) from digits.
        let split = tag.find(|c: char| c.is_ascii_digit()).unwrap_or(tag.len());
        let impl_prefix = tag[..split].to_string();
        let digits = &tag[split..];
        // "310" → minor 10 ; "39" → minor 9.  Major is the first digit.
        let abi_minor = if digits.len() >= 2 {
            digits[1..].parse::<u32>().ok()
        } else {
            None
        };
        TargetTag { py_tag: tag.to_string(), impl_prefix, abi_minor }
    }
}

/// Score a wheel filename for the target, or None if it is INCOMPATIBLE.
///
/// Wheel filename grammar (PEP 427):
///   `{distribution}-{version}(-{build})?-{python}-{abi}-{platform}.whl`
/// where each of python/abi/platform may be a dot-separated set of tags (a
/// "compressed tag set"), any of which makes the wheel installable.
fn score_wheel(filename: &str, target: &TargetTag, platform_tag: &str, arch_suffix: &str) -> Option<i32> {
    let stem = filename.strip_suffix(".whl")?;
    // The python/abi/platform tags are the LAST three '-' separated fields.
    let parts: Vec<&str> = stem.split('-').collect();
    if parts.len() < 3 {
        return None;
    }
    let plat_field = parts[parts.len() - 1];
    let abi_field = parts[parts.len() - 2];
    let py_field = parts[parts.len() - 3];

    // ── Platform compatibility ──────────────────────────────────────────────
    let plat_tags: Vec<&str> = plat_field.split('.').collect();
    let platform_ok = plat_tags.iter().any(|t| {
        *t == "any"
            || (!platform_tag.is_empty() && *t == platform_tag)
            || (!arch_suffix.is_empty() && t.ends_with(arch_suffix.trim_end_matches(".whl")))
    });
    let is_pure = plat_tags.iter().any(|t| *t == "any");
    if !platform_ok {
        return None;
    }

    // ── Python / ABI compatibility ──────────────────────────────────────────
    // Evaluate every (python, abi) combination in the compressed sets and keep
    // the best score. A wheel is compatible if ANY combination matches.
    let py_tags: Vec<&str> = py_field.split('.').collect();
    let abi_tags: Vec<&str> = abi_field.split('.').collect();

    let mut best: Option<i32> = None;
    for py in &py_tags {
        for abi in &abi_tags {
            if let Some(s) = score_py_abi(py, abi, target, is_pure) {
                best = Some(best.map_or(s, |b| b.max(s)));
            }
        }
    }
    best
}

/// Score a single (python_tag, abi_tag) combination against the target, or None
/// if incompatible. Higher = more specific/preferred.
fn score_py_abi(py: &str, abi: &str, target: &TargetTag, is_pure: bool) -> Option<i32> {
    // 1. Exact CPython match: python tag == target ("cp310"), native ABI.
    //    abi may be "cp310" or "cp310<flags>" (e.g. cp310-cp310m on old wheels).
    if py == target.py_tag {
        // Reject a stable-ABI-only mismatch is impossible here (py is exact).
        // Native ABI tags start with the implementation prefix (e.g. "cp310").
        if abi.starts_with(&target.py_tag) {
            return Some(100); // exact version + exact ABI — most specific
        }
        if abi == "abi3" {
            return Some(95); // exact version, stable ABI
        }
        if abi == "none" {
            return Some(90); // exact version, no ABI (rare)
        }
        // py says target but abi is for a DIFFERENT interpreter — reject.
        return None;
    }

    // 2. Stable ABI (abi3) forward-compat: a wheel tagged e.g. cp37-abi3 works on
    //    any CPython >= 3.7. Match only same implementation and minor <= target.
    if abi == "abi3" && py.starts_with(&target.impl_prefix) {
        let py_split = py.find(|c: char| c.is_ascii_digit()).unwrap_or(py.len());
        let digits = &py[py_split..];
        if digits.len() >= 2 {
            if let (Ok(wheel_minor), Some(target_minor)) =
                (digits[1..].parse::<u32>(), target.abi_minor)
            {
                if wheel_minor <= target_minor {
                    // Closer to the target minor is slightly preferred.
                    return Some(70 + wheel_minor.min(target_minor) as i32);
                }
            }
        }
        return None; // abi3 wheel built for a NEWER interpreter — incompatible
    }

    // 3. Pure-Python wheels: py3 / py2.py3 (handled per-tag here) with none ABI.
    if abi == "none" && is_pure {
        match py {
            "py3" | "cp3" => return Some(50),
            "py2.py3" => return Some(45),
            // "py30".."py3X" style major-only sometimes appears; accept py3 family.
            p if p.starts_with("py3") => return Some(48),
            _ => {}
        }
    }

    // Anything else (different CPython minor like cp39/cp313/cp313t, PyPy pp*,
    // or a native ABI for another interpreter) is INCOMPATIBLE with the target.
    None
}

fn current_platform_tag() -> String {
    crate::config::platform_tag()
}

/// Returns the architecture-specific suffix to match in wheel filenames.
/// e.g. "arm64.whl" on Apple Silicon, "x86_64.whl" on Intel Mac, "" otherwise.
fn current_arch_suffix() -> String {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "arm64.whl".to_string(),
        ("macos", "x86_64")  => "x86_64.whl".to_string(),
        ("linux", "x86_64")  => "x86_64.whl".to_string(),
        ("linux", "aarch64") => "aarch64.whl".to_string(),
        ("windows", "x86_64") => "amd64.whl".to_string(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Linux x86_64 manylinux target — matches the live bug environment.
    const PLAT: &str = "manylinux2014_x86_64";
    const ARCH: &str = "x86_64.whl";

    fn entry(filename: &str) -> PypiFileEntry {
        PypiFileEntry {
            filename: filename.to_string(),
            url: format!("https://example/{filename}"),
            digests: PypiDigests { sha256: "0".repeat(64) },
            packagetype: "bdist_wheel".to_string(),
            requires_python: None,
        }
    }

    fn sdist(filename: &str) -> PypiFileEntry {
        let mut e = entry(filename);
        e.packagetype = "sdist".to_string();
        e
    }

    fn target(tag: &str) -> TargetTag {
        TargetTag::parse(tag)
    }

    // ── The bug: cp313t must NEVER be chosen for a cp310 runtime ─────────────
    #[test]
    fn rejects_cp313t_wheel_for_cp310_target() {
        let t = target("cp310");
        // The exact wheel from the live failure.
        let bad = "bcrypt-4.3.0-cp313-cp313t-manylinux_2_34_x86_64.whl";
        assert_eq!(
            score_wheel(bad, &t, PLAT, ARCH),
            None,
            "a cp313t wheel must be rejected for a cp310 target"
        );
        // Also reject a plain cp313 and cp39 (different minor either direction).
        assert_eq!(
            score_wheel("bcrypt-4.3.0-cp313-cp313-manylinux2014_x86_64.whl", &t, PLAT, ARCH),
            None
        );
        assert_eq!(
            score_wheel("bcrypt-4.3.0-cp39-cp39-manylinux2014_x86_64.whl", &t, PLAT, ARCH),
            None
        );
    }

    #[test]
    fn accepts_exact_cp310_wheel() {
        let t = target("cp310");
        let good = "bcrypt-4.3.0-cp310-cp310-manylinux2014_x86_64.whl";
        let s = score_wheel(good, &t, PLAT, ARCH).expect("cp310 wheel must match cp310");
        assert_eq!(s, 100, "exact version + native ABI is the most specific");
    }

    #[test]
    fn picks_cp310_over_other_wheels_in_a_multi_wheel_release() {
        // Mirrors bcrypt 4.3.0: many native wheels, only one for cp310.
        let files = vec![
            entry("bcrypt-4.3.0-cp39-cp39-manylinux2014_x86_64.whl"),
            entry("bcrypt-4.3.0-cp313-cp313t-manylinux_2_34_x86_64.whl"),
            entry("bcrypt-4.3.0-cp310-cp310-manylinux2014_x86_64.whl"),
            entry("bcrypt-4.3.0-cp312-cp312-manylinux2014_x86_64.whl"),
            sdist("bcrypt-4.3.0.tar.gz"),
        ];
        let chosen = select_best_file_for(&files, "cp310", PLAT, ARCH).expect("a compatible wheel exists");
        assert_eq!(chosen.filename, "bcrypt-4.3.0-cp310-cp310-manylinux2014_x86_64.whl");
    }

    // ── abi3 / stable-ABI forward compatibility ─────────────────────────────
    #[test]
    fn accepts_abi3_built_for_older_minor() {
        let t = target("cp310");
        // cp37-abi3 is forward-compatible with any CPython >= 3.7.
        let s = score_wheel(
            "cryptography-42.0.0-cp37-abi3-manylinux2014_x86_64.whl",
            &t, PLAT, ARCH,
        );
        assert!(s.is_some(), "cp37-abi3 must be usable on cp310");
    }

    #[test]
    fn rejects_abi3_built_for_newer_minor() {
        let t = target("cp310");
        // cp311-abi3 requires CPython >= 3.11 — not usable on 3.10.
        assert_eq!(
            score_wheel("foo-1.0-cp311-abi3-manylinux2014_x86_64.whl", &t, PLAT, ARCH),
            None
        );
    }

    #[test]
    fn prefers_exact_cp_over_abi3() {
        let files = vec![
            entry("foo-1.0-cp37-abi3-manylinux2014_x86_64.whl"),
            entry("foo-1.0-cp310-cp310-manylinux2014_x86_64.whl"),
        ];
        let chosen = select_best_file_for(&files, "cp310", PLAT, ARCH).unwrap();
        assert_eq!(chosen.filename, "foo-1.0-cp310-cp310-manylinux2014_x86_64.whl");
    }

    // ── pure-python wheels ──────────────────────────────────────────────────
    #[test]
    fn accepts_pure_python_wheel() {
        let t = target("cp310");
        let s = score_wheel("requests-2.31.0-py3-none-any.whl", &t, PLAT, ARCH);
        assert!(s.is_some(), "py3-none-any is always compatible");
        // But a native cp310 wheel should outrank it when both exist.
        let files = vec![
            entry("requests-2.31.0-py3-none-any.whl"),
            entry("requests-2.31.0-cp310-cp310-manylinux2014_x86_64.whl"),
        ];
        let chosen = select_best_file_for(&files, "cp310", PLAT, ARCH).unwrap();
        assert_eq!(chosen.filename, "requests-2.31.0-cp310-cp310-manylinux2014_x86_64.whl");
    }

    #[test]
    fn pure_python_chosen_when_no_native_match() {
        let files = vec![
            entry("foo-1.0-cp39-cp39-manylinux2014_x86_64.whl"),
            entry("foo-1.0-py3-none-any.whl"),
        ];
        let chosen = select_best_file_for(&files, "cp310", PLAT, ARCH).unwrap();
        assert_eq!(chosen.filename, "foo-1.0-py3-none-any.whl");
    }

    // ── platform rejection ──────────────────────────────────────────────────
    #[test]
    fn rejects_wrong_platform() {
        let t = target("cp310");
        // Right python, wrong platform (windows) on a linux target.
        assert_eq!(
            score_wheel("foo-1.0-cp310-cp310-win_amd64.whl", &t, PLAT, ARCH),
            None
        );
    }

    // ── compressed tag sets ─────────────────────────────────────────────────
    #[test]
    fn handles_compressed_tag_set() {
        let t = target("cp310");
        // A single wheel covering several pythons via a dot-set; cp310 is in it.
        let s = score_wheel(
            "foo-1.0-cp38.cp39.cp310-abi3-manylinux2014_x86_64.whl",
            &t, PLAT, ARCH,
        );
        assert!(s.is_some(), "compressed set containing cp310 must match");
    }

    // ── falls back to sdist when nothing compatible ─────────────────────────
    #[test]
    fn falls_back_to_sdist_when_no_compatible_wheel() {
        let files = vec![
            entry("foo-1.0-cp313-cp313t-manylinux2014_x86_64.whl"),
            sdist("foo-1.0.tar.gz"),
        ];
        let chosen = select_best_file_for(&files, "cp310", PLAT, ARCH).expect("sdist fallback");
        assert_eq!(chosen.filename, "foo-1.0.tar.gz");
    }
}
