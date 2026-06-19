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

/// The OS family of a wheel platform tag — what kind of binary it carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OsFamily {
    Linux,
    MacOs,
    Windows,
    /// `any` — pure-python, no platform binary, installable everywhere.
    Pure,
    /// An OS family we don't recognise (e.g. a future platform). Never matched.
    Unknown,
}

/// The target platform we are resolving FOR, parsed once from the kong runtime's
/// platform tag (e.g. "manylinux2014_x86_64", "win_amd64", "macosx_11_0_arm64").
///
/// This is what makes platform selection GENERAL and target-driven: a Linux
/// target accepts manylinux/musllinux x86_64 + `any`; a macOS target accepts
/// macosx + `any`; a Windows target accepts win_* + `any` — each REJECTING the
/// others (the live bug: a `macosx_10_9_x86_64` wheel passed a naive
/// `ends_with("x86_64")` check on a Linux target). Nothing here is hardcoded to
/// "linux" — flip the runtime's OS and the accepted set flips with it.
struct TargetPlatform {
    family: OsFamily,
    /// Normalised CPU architecture, e.g. "x86_64", "aarch64". Empty if unknown.
    arch: String,
}

impl TargetPlatform {
    /// Derive the target (os family, arch) from the runtime's platform tag.
    /// `arch_suffix` (e.g. "x86_64.whl") is a fallback source of the arch when
    /// the platform tag is empty/unparseable.
    fn derive(platform_tag: &str, arch_suffix: &str) -> Self {
        let family = Self::family_of(platform_tag);
        let mut arch = Self::arch_of(platform_tag);
        if arch.is_empty() {
            // Fall back to the explicit arch suffix ("x86_64.whl" → "x86_64",
            // "arm64.whl" → "aarch64", "amd64.whl" → "x86_64").
            arch = Self::normalize_arch(arch_suffix.trim_end_matches(".whl"));
        }
        TargetPlatform { family, arch }
    }

    /// Classify a platform tag's OS family by its prefix (PEP 425/600 forms).
    fn family_of(tag: &str) -> OsFamily {
        if tag == "any" {
            OsFamily::Pure
        } else if tag.starts_with("manylinux")
            || tag.starts_with("musllinux")
            || tag.starts_with("linux")
        {
            OsFamily::Linux
        } else if tag.starts_with("macosx") {
            OsFamily::MacOs
        } else if tag.starts_with("win") {
            OsFamily::Windows
        } else {
            OsFamily::Unknown
        }
    }

    /// Extract and normalise the CPU arch from a platform tag.
    /// Handles: manylinux1/2010/2014_<arch>, manylinux_<glibc>_<arch> (PEP 600),
    /// musllinux_<ver>_<arch>, linux_<arch>, macosx_<ver>_<arch>, win_<arch>/win32.
    fn arch_of(tag: &str) -> String {
        if tag == "any" || tag.is_empty() {
            return String::new();
        }
        // Windows special cases first (no trailing arch token to split off).
        if tag == "win32" {
            return "x86".to_string();
        }
        if let Some(rest) = tag.strip_prefix("win_") {
            return Self::normalize_arch(rest); // win_amd64, win_arm64
        }
        // For the rest, the architecture is the trailing token(s). The arch
        // names we care about ("x86_64", "i686", "aarch64", "arm64", "ppc64le",
        // "s390x", "universal2", "intel"…) are matched by suffix so the variable
        // glibc/musl/macos version in the middle is irrelevant.
        const KNOWN: &[&str] = &[
            "x86_64", "i686", "aarch64", "arm64", "armv7l", "ppc64le", "ppc64",
            "s390x", "universal2", "universal", "intel", "amd64", "x86",
        ];
        for k in KNOWN {
            if tag.ends_with(k) {
                return Self::normalize_arch(k);
            }
        }
        String::new()
    }

    /// Collapse arch aliases to one canonical name per CPU.
    fn normalize_arch(a: &str) -> String {
        match a {
            "amd64" | "x86_64" => "x86_64".to_string(),
            "arm64" | "aarch64" => "aarch64".to_string(),
            "x86" | "i686" | "win32" => "x86".to_string(),
            other => other.to_string(),
        }
    }

    /// Does the target accept a single wheel platform tag?
    /// True iff the tag is `any`, OR its OS family == target family AND its arch
    /// is compatible with the target arch.
    fn accepts(&self, tag: &str) -> bool {
        let fam = Self::family_of(tag);
        if fam == OsFamily::Pure {
            return true; // `any` installs everywhere
        }
        if fam == OsFamily::Unknown || fam != self.family {
            return false; // wrong OS family (the darwin-on-linux bug)
        }
        let wheel_arch = Self::arch_of(tag);
        Self::arch_compatible(&self.arch, &wheel_arch)
    }

    /// Arch compatibility within the same OS family.
    /// Exact match always wins. macOS fat tags ("universal2"/"universal"/"intel")
    /// carry multiple slices and run on the relevant single-arch targets.
    fn arch_compatible(target_arch: &str, wheel_arch: &str) -> bool {
        if target_arch.is_empty() || wheel_arch.is_empty() {
            // Without a known arch we cannot prove compatibility — be strict.
            return false;
        }
        if target_arch == wheel_arch {
            return true;
        }
        match wheel_arch {
            // universal2 = x86_64 + arm64 ; intel = i386 + x86_64.
            "universal2" => target_arch == "x86_64" || target_arch == "aarch64",
            "universal" | "intel" => target_arch == "x86_64" || target_arch == "x86",
            _ => false,
        }
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
    // Derive the TARGET's (os_family, arch) once from the runtime platform tag,
    // so the check is fully target-driven (the host running the resolver need
    // not be the target OS). `arch_suffix` is kept for back-compat fallback when
    // the runtime gave us no platform tag (it still encodes the target arch).
    let target_plat = TargetPlatform::derive(platform_tag, arch_suffix);

    let plat_tags: Vec<&str> = plat_field.split('.').collect();
    // A wheel is platform-OK if ANY of its compressed platform tags is
    // compatible with the target's OS family AND architecture.
    let platform_ok = plat_tags.iter().any(|t| target_plat.accepts(t));
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

    // ── THE PLATFORM BUG: darwin/win/other-arch wheels on a Linux target ─────
    // The live failure: a `macosx_*_x86_64` wheel slipped past the old
    // `ends_with("x86_64")` check on a Linux x86_64 target → darwin .so in the
    // venv → ModuleNotFoundError. These lock the OS-family filter.

    #[test]
    fn rejects_darwin_wheel_on_linux_target() {
        let t = target("cp310");
        // arm64 darwin — arch differs too, but the OS family alone must reject it.
        assert_eq!(
            score_wheel("psycopg2-2.9.9-cp310-cp310-macosx_11_0_arm64.whl", &t, PLAT, ARCH),
            None,
            "darwin arm64 wheel must be rejected on a linux target"
        );
        // x86_64 DARWIN — the exact trap: same arch suffix, WRONG OS. This is the
        // case the old ends_with('x86_64') check wrongly accepted.
        assert_eq!(
            score_wheel("psycopg2-2.9.9-cp310-cp310-macosx_10_9_x86_64.whl", &t, PLAT, ARCH),
            None,
            "darwin x86_64 wheel must be rejected on a linux target (the live bug)"
        );
        // universal2 darwin (fat) — still darwin, still rejected on linux.
        assert_eq!(
            score_wheel("pglast-6.0-cp310-cp310-macosx_11_0_universal2.whl", &t, PLAT, ARCH),
            None
        );
    }

    #[test]
    fn rejects_windows_wheel_on_linux_target() {
        let t = target("cp310");
        assert_eq!(
            score_wheel("foo-1.0-cp310-cp310-win_amd64.whl", &t, PLAT, ARCH),
            None
        );
        assert_eq!(score_wheel("foo-1.0-cp310-cp310-win32.whl", &t, PLAT, ARCH), None);
    }

    #[test]
    fn rejects_other_linux_arch_on_x86_64_target() {
        let t = target("cp310");
        // aarch64 / i686 linux wheels are wrong-arch for an x86_64 linux target.
        assert_eq!(
            score_wheel("foo-1.0-cp310-cp310-manylinux2014_aarch64.whl", &t, PLAT, ARCH),
            None
        );
        assert_eq!(
            score_wheel("foo-1.0-cp310-cp310-musllinux_1_2_aarch64.whl", &t, PLAT, ARCH),
            None
        );
        assert_eq!(
            score_wheel("foo-1.0-cp310-cp310-manylinux1_i686.whl", &t, PLAT, ARCH),
            None
        );
    }

    #[test]
    fn accepts_all_linux_x86_64_variants() {
        let t = target("cp310");
        for plat in [
            "manylinux1_x86_64",
            "manylinux2010_x86_64",
            "manylinux2014_x86_64",
            "manylinux_2_17_x86_64",  // PEP 600 glibc form
            "manylinux_2_34_x86_64",
            "musllinux_1_1_x86_64",
            "musllinux_1_2_x86_64",
            "linux_x86_64",
        ] {
            let f = format!("foo-1.0-cp310-cp310-{plat}.whl");
            assert!(
                score_wheel(&f, &t, PLAT, ARCH).is_some(),
                "{plat} must be accepted on a linux x86_64 target"
            );
        }
    }

    #[test]
    fn selects_manylinux_rejecting_darwin_and_win_in_a_release() {
        // The acceptance scenario from the task: only the manylinux wheel is valid.
        let files = vec![
            entry("pkg-1.0-cp310-cp310-macosx_11_0_arm64.whl"),
            entry("pkg-1.0-cp310-cp310-macosx_10_9_x86_64.whl"),
            entry("pkg-1.0-cp310-cp310-win_amd64.whl"),
            entry("pkg-1.0-cp310-cp310-manylinux2014_x86_64.whl"),
        ];
        let chosen = select_best_file_for(&files, "cp310", PLAT, ARCH)
            .expect("the manylinux wheel must be selected");
        assert_eq!(chosen.filename, "pkg-1.0-cp310-cp310-manylinux2014_x86_64.whl");
    }

    // ── macOS target mirror (target-driven, not hardcoded to linux) ──────────
    // Setting the runtime platform tag to a darwin one flips the accepted set.
    const MAC_PLAT: &str = "macosx_11_0_arm64";
    const MAC_ARCH: &str = "arm64.whl";

    #[test]
    fn macos_target_accepts_darwin_rejects_manylinux_and_win() {
        let t = target("cp310");
        // arm64 darwin accepted.
        assert!(
            score_wheel("pkg-1.0-cp310-cp310-macosx_11_0_arm64.whl", &t, MAC_PLAT, MAC_ARCH).is_some()
        );
        // universal2 carries arm64 — accepted on an arm64 mac.
        assert!(
            score_wheel("pkg-1.0-cp310-cp310-macosx_11_0_universal2.whl", &t, MAC_PLAT, MAC_ARCH).is_some()
        );
        // manylinux + win REJECTED on a mac target.
        assert_eq!(
            score_wheel("pkg-1.0-cp310-cp310-manylinux2014_x86_64.whl", &t, MAC_PLAT, MAC_ARCH),
            None
        );
        assert_eq!(
            score_wheel("pkg-1.0-cp310-cp310-win_amd64.whl", &t, MAC_PLAT, MAC_ARCH),
            None
        );
        // pure-python still accepted.
        assert!(
            score_wheel("pkg-1.0-py3-none-any.whl", &t, MAC_PLAT, MAC_ARCH).is_some()
        );
        // x86_64-only darwin wheel rejected on an arm64 mac (arch mismatch).
        assert_eq!(
            score_wheel("pkg-1.0-cp310-cp310-macosx_10_9_x86_64.whl", &t, MAC_PLAT, MAC_ARCH),
            None
        );
    }

    #[test]
    fn macos_target_picks_darwin_over_linux_in_a_release() {
        let files = vec![
            entry("pkg-1.0-cp310-cp310-manylinux2014_x86_64.whl"),
            entry("pkg-1.0-cp310-cp310-win_amd64.whl"),
            entry("pkg-1.0-cp310-cp310-macosx_11_0_arm64.whl"),
        ];
        let chosen = select_best_file_for(&files, "cp310", MAC_PLAT, MAC_ARCH)
            .expect("darwin arm64 wheel must be selected on a mac target");
        assert_eq!(chosen.filename, "pkg-1.0-cp310-cp310-macosx_11_0_arm64.whl");
    }

    // ── Windows target mirror ────────────────────────────────────────────────
    #[test]
    fn windows_target_accepts_win_rejects_others() {
        let t = target("cp310");
        const WIN_PLAT: &str = "win_amd64";
        const WIN_ARCH: &str = "amd64.whl";
        assert!(
            score_wheel("pkg-1.0-cp310-cp310-win_amd64.whl", &t, WIN_PLAT, WIN_ARCH).is_some()
        );
        assert_eq!(
            score_wheel("pkg-1.0-cp310-cp310-manylinux2014_x86_64.whl", &t, WIN_PLAT, WIN_ARCH),
            None
        );
        assert_eq!(
            score_wheel("pkg-1.0-cp310-cp310-macosx_11_0_arm64.whl", &t, WIN_PLAT, WIN_ARCH),
            None
        );
        assert!(score_wheel("pkg-1.0-py3-none-any.whl", &t, WIN_PLAT, WIN_ARCH).is_some());
    }

    // ── unit tests on the platform parser itself ─────────────────────────────
    #[test]
    fn platform_parser_classifies_families_and_arch() {
        assert_eq!(TargetPlatform::family_of("manylinux2014_x86_64"), OsFamily::Linux);
        assert_eq!(TargetPlatform::family_of("musllinux_1_2_x86_64"), OsFamily::Linux);
        assert_eq!(TargetPlatform::family_of("linux_x86_64"), OsFamily::Linux);
        assert_eq!(TargetPlatform::family_of("macosx_11_0_arm64"), OsFamily::MacOs);
        assert_eq!(TargetPlatform::family_of("win_amd64"), OsFamily::Windows);
        assert_eq!(TargetPlatform::family_of("win32"), OsFamily::Windows);
        assert_eq!(TargetPlatform::family_of("any"), OsFamily::Pure);

        assert_eq!(TargetPlatform::arch_of("manylinux_2_17_x86_64"), "x86_64");
        assert_eq!(TargetPlatform::arch_of("manylinux2014_aarch64"), "aarch64");
        assert_eq!(TargetPlatform::arch_of("macosx_11_0_arm64"), "aarch64");
        assert_eq!(TargetPlatform::arch_of("win_amd64"), "x86_64");
        assert_eq!(TargetPlatform::arch_of("win32"), "x86");
        assert_eq!(TargetPlatform::arch_of("any"), "");
    }

    #[test]
    fn derive_falls_back_to_arch_suffix_when_no_platform_tag() {
        // If the runtime gave no platform tag, the arch_suffix still pins arch,
        // but with no family we cannot accept any native wheel — only `any`.
        let t = target("cp310");
        let s = score_wheel("pkg-1.0-py3-none-any.whl", &t, "", "x86_64.whl");
        assert!(s.is_some(), "pure-python still installs with no platform tag");
        assert_eq!(
            score_wheel("pkg-1.0-cp310-cp310-manylinux2014_x86_64.whl", &t, "", "x86_64.whl"),
            None,
            "without a known target OS family, native wheels are not provably compatible"
        );
    }
}
