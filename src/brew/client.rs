//! Homebrew bottle client — download pre-built macOS packages directly from the
//! Homebrew API and GitHub Container Registry, without requiring `brew` CLI.
//!
//! Flow:
//!   1. `resolve_formula(name)` → fetch JSON from `https://formulae.brew.sh/api/formula/{name}.json`
//!   2. Pick the best bottle for the current platform via `bottle_platform_tag()`
//!   3. `download_bottle(...)` → obtain anonymous GHCR bearer token, download blob, verify SHA-256
//!   4. Extract tar.gz into the kong store (strip 2 path components: `<name>/<version>/`)

use std::path::Path;

use anyhow::{bail, Context, Result};
use tracing::{debug, info, warn};

use crate::download::FileInfo;

// ── Types ────────────────────────────────────────────────────────────────────

/// Metadata for a resolved Homebrew formula.
#[derive(Debug, Clone)]
pub struct FormulaInfo {
    pub name: String,
    pub version: String,
    pub keg_only: bool,
    pub dependencies: Vec<String>,
    pub bottle_url: String,
    pub bottle_sha256: String,
}

// ── Public API ───────────────────────────────────────────────────────────────

/// Fetch formula metadata from the Homebrew JSON API and pick the best bottle
/// for the current platform.
pub fn resolve_formula(name: &str) -> Result<FormulaInfo> {
    let url = format!("https://formulae.brew.sh/api/formula/{name}.json");
    debug!(formula = %name, url = %url, "Fetching formula metadata");

    let client = http_client()?;
    let resp = client
        .get(&url)
        .send()
        .with_context(|| format!("failed to fetch formula metadata for '{name}'"))?;

    if !resp.status().is_success() {
        bail!(
            "Homebrew API returned HTTP {} for formula '{name}'",
            resp.status()
        );
    }

    let json: serde_json::Value = resp
        .json()
        .with_context(|| format!("failed to parse formula JSON for '{name}'"))?;

    let version = json["versions"]["stable"]
        .as_str()
        .context("missing versions.stable")?
        .to_string();

    let keg_only = json["keg_only"].as_bool().unwrap_or(false);

    // Runtime dependencies
    let dependencies: Vec<String> = json["dependencies"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    // Pick the best bottle for this platform
    let tag = bottle_platform_tag()?;
    let files = &json["bottle"]["stable"]["files"];

    let (bottle_url, bottle_sha256) = pick_bottle(files, &tag)
        .with_context(|| format!("no suitable bottle found for '{name}' (looking for tag '{tag}')"))?;

    debug!(
        formula = %name, version = %version, tag = %tag,
        deps = ?dependencies, keg_only, "Formula resolved"
    );

    Ok(FormulaInfo {
        name: name.to_string(),
        version,
        keg_only,
        dependencies,
        bottle_url,
        bottle_sha256,
    })
}

/// Download a bottle to the kong store.
/// Returns the hash of the downloaded file.
///
/// Streams the response to a temp file while incrementally computing SHA-256,
/// avoiding loading the entire bottle into memory.
pub fn download_bottle(formula: &FormulaInfo, dest: &Path) -> Result<FileInfo> {
    use sha2::{Digest, Sha256};
    use std::io::{Read, Write};

    info!(
        formula = %formula.name, version = %formula.version,
        dest = %dest.display(), "Downloading Homebrew bottle"
    );

    // GHCR requires an anonymous bearer token per-repository
    let token = ghcr_token(&formula.name)?;

    let client = http_client()?;
    let mut resp = client
        .get(&formula.bottle_url)
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .with_context(|| format!("failed to download bottle for '{}'", formula.name))?;

    if !resp.status().is_success() {
        bail!(
            "GHCR returned HTTP {} for bottle '{}'",
            resp.status(),
            formula.name
        );
    }

    // Stream to temp file while incrementally computing SHA-256
    let tmp = tempfile::TempDir::new()?;
    let archive_path = tmp.path().join(format!("{}.tar.gz", formula.name));
    let mut file = std::fs::File::create(&archive_path)
        .with_context(|| format!("failed to create temp file: {}", archive_path.display()))?;
    let mut hasher = Sha256::new();
    let mut total_bytes: u64 = 0;
    let mut buf = vec![0u8; 256 * 1024]; // 256 KiB buffer

    loop {
        let n = resp.read(&mut buf)
            .with_context(|| format!("failed to read bottle bytes for '{}'", formula.name))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n])?;
        total_bytes += n as u64;
    }
    drop(file);

    let hash = hex::encode(hasher.finalize());

    if hash != formula.bottle_sha256 {
        bail!(
            "bottle hash mismatch for '{}': expected {}, got {}",
            formula.name,
            formula.bottle_sha256,
            hash
        );
    }
    debug!(formula = %formula.name, "Bottle hash verified");

    info!(
        formula = %formula.name, size = total_bytes,
        "Bottle downloaded, extracting"
    );

    // Homebrew bottles have structure: <name>/<version>/bin/... — strip 2 components
    crate::extract::extract_targz_strip(&archive_path, dest, 2)?;

    // NOTE: Mach-O placeholder fixup (@@HOMEBREW_CELLAR@@, @@HOMEBREW_PREFIX@@)
    // is deferred until all bottles are downloaded, so cross-dep references resolve.
    // Call `fixup_macho_placeholders()` after all bottles are in the store.

    // Set executable bit on bin/* files
    #[cfg(unix)]
    {
        let bin_dir = dest.join("bin");
        if bin_dir.exists() {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(entries) = std::fs::read_dir(&bin_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_file() {
                        let _ = std::fs::set_permissions(
                            &path,
                            std::fs::Permissions::from_mode(0o755),
                        );
                    }
                }
            }
        }
    }

    info!(
        formula = %formula.name, dest = %dest.display(),
        "Bottle extracted"
    );

    Ok(FileInfo {
        hash,
        url: formula.bottle_url.clone(),
    })
}



// ── Platform detection ───────────────────────────────────────────────────────

/// Map the current OS/arch/version to a Homebrew bottle platform tag.
///
/// Bottle tags look like: `arm64_sequoia`, `arm64_sonoma`, `sonoma`, `x86_64_linux`.
/// On macOS, we detect the version name via `sw_vers -productVersion`.
fn bottle_platform_tag() -> Result<String> {
    #[cfg(target_os = "macos")]
    {
        let arch_prefix = if cfg!(target_arch = "aarch64") {
            "arm64_"
        } else {
            "" // intel macs don't have prefix (e.g. "sonoma" not "x86_64_sonoma")
        };

        let version_name = macos_version_name()?;
        Ok(format!("{arch_prefix}{version_name}"))
    }

    #[cfg(target_os = "linux")]
    {
        if cfg!(target_arch = "aarch64") {
            Ok("arm64_linux".to_string())
        } else {
            Ok("x86_64_linux".to_string())
        }
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        bail!("Homebrew bottles are not available for this platform");
    }
}

/// Detect the macOS marketing version name from the major version number.
#[cfg(target_os = "macos")]
fn macos_version_name() -> Result<String> {
    let output = std::process::Command::new("sw_vers")
        .arg("-productVersion")
        .output()
        .context("failed to run sw_vers")?;

    let version_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let major: u32 = version_str
        .split('.')
        .next()
        .and_then(|s| s.parse().ok())
        .context("failed to parse macOS major version")?;

    let name = match major {
        26 => "tahoe",
        15 => "sequoia",
        14 => "sonoma",
        13 => "ventura",
        12 => "monterey",
        11 => "big_sur",
        _ => {
            warn!(major, "Unknown macOS version, trying sequoia");
            "sequoia"
        }
    };

    debug!(version = %version_str, name, "Detected macOS version");
    Ok(name.to_string())
}

/// Pick the best bottle from the `files` JSON object for the given platform tag.
/// Falls back to older macOS versions if the exact tag isn't available.
fn pick_bottle(
    files: &serde_json::Value,
    preferred_tag: &str,
) -> Option<(String, String)> {
    // Try exact match first
    if let Some(entry) = files.get(preferred_tag) {
        return extract_bottle_entry(entry);
    }

    // Fallback chain for ARM Macs: try progressively older versions
    let arm64_fallbacks = [
        "arm64_tahoe",
        "arm64_sequoia",
        "arm64_sonoma",
        "arm64_ventura",
        "arm64_monterey",
        "arm64_big_sur",
    ];
    // Fallback chain for Intel Macs
    let intel_fallbacks = [
        "tahoe",
        "sequoia",
        "sonoma",
        "ventura",
        "monterey",
        "big_sur",
    ];

    let fallbacks = if preferred_tag.starts_with("arm64_") {
        &arm64_fallbacks[..]
    } else if preferred_tag.contains("linux") {
        // No fallback for Linux
        return None;
    } else {
        &intel_fallbacks[..]
    };

    // Find where we are in the chain, then try everything from there downward
    let start = fallbacks
        .iter()
        .position(|t| *t == preferred_tag)
        .unwrap_or(0);

    for tag in &fallbacks[start..] {
        if let Some(entry) = files.get(*tag) {
            debug!(preferred = preferred_tag, used = *tag, "Falling back to older bottle");
            return extract_bottle_entry(entry);
        }
    }

    None
}

fn extract_bottle_entry(entry: &serde_json::Value) -> Option<(String, String)> {
    let url = entry.get("url")?.as_str()?.to_string();
    let sha256 = entry.get("sha256")?.as_str()?.to_string();
    Some((url, sha256))
}

// ── GHCR authentication ──────────────────────────────────────────────────────

/// Obtain an anonymous bearer token from GHCR for pulling a Homebrew bottle.
///
/// This is the standard OCI token dance:
///   GET https://ghcr.io/token?scope=repository:homebrew/core/<name>:pull
///   → {"token": "..."}
///
/// No credentials needed — Homebrew packages are public.
fn ghcr_token(formula_name: &str) -> Result<String> {
    // GHCR repository names replace '@' with '/' in versioned formulas:
    //   postgresql@17 → homebrew/core/postgresql/17
    //   openssl@3     → homebrew/core/openssl/3
    let repo_name = formula_name.replace('@', "/");
    let scope = format!("repository:homebrew/core/{repo_name}:pull");

    debug!(formula = %formula_name, scope = %scope, "Fetching GHCR token");

    let client = http_client()?;
    let resp = client
        .get("https://ghcr.io/token")
        .query(&[("scope", &scope), ("service", &"ghcr.io".to_string())])
        .send()
        .with_context(|| format!("failed to fetch GHCR token for '{formula_name}'"))?;

    if !resp.status().is_success() {
        bail!("GHCR token request failed with HTTP {}", resp.status());
    }

    let json: serde_json::Value = resp
        .json()
        .context("failed to parse GHCR token response")?;

    let token = json["token"]
        .as_str()
        .context("no 'token' field in GHCR response")?
        .to_string();

    debug!(formula = %formula_name, "GHCR token obtained");
    Ok(token)
}

// ── Mach-O fixup ─────────────────────────────────────────────────────────────

/// Homebrew bottles contain placeholder strings in Mach-O load commands:
///   - `@@HOMEBREW_CELLAR@@/<name>/<version>/...` → `<store_root>/<name>-<version>/...`
///   - `@@HOMEBREW_PREFIX@@/opt/<name>/...`       → `<store_root>/<name>-<version>/...`
///
/// We use `install_name_tool -change` to rewrite them to point into the kong store.
/// Must be called AFTER all bottles are downloaded so cross-dep references resolve.
#[cfg(target_os = "macos")]
pub fn fixup_macho_placeholders(bottle_dir: &Path, store_root: &Path) -> Result<()> {
    let macho_files = find_macho_files(bottle_dir);
    if macho_files.is_empty() {
        return Ok(());
    }

    debug!(
        dir = %bottle_dir.display(),
        count = macho_files.len(),
        "Fixing Mach-O placeholders"
    );

    for file in &macho_files {
        let load_commands = read_otool_deps(file)?;
        let mut changes: Vec<(String, String)> = Vec::new();

        for old_path in &load_commands {
            if let Some(new_path) = resolve_placeholder(old_path, store_root) {
                changes.push((old_path.clone(), new_path));
            }
        }

        if !changes.is_empty() {
            // Also fix the library ID for .dylib files
            let is_dylib = file.extension().map_or(false, |e| e == "dylib");

            let mut cmd = std::process::Command::new("install_name_tool");
            for (old, new) in &changes {
                cmd.arg("-change").arg(old).arg(new);
            }

            // Fix the library's own ID if it's a .dylib
            if is_dylib {
                if let Some(lib_id) = read_otool_id(file)? {
                    if let Some(new_id) = resolve_placeholder(&lib_id, store_root) {
                        cmd.arg("-id").arg(&new_id);
                    }
                }
            }

            cmd.arg(file);

            let output = cmd.output()
                .with_context(|| format!("failed to run install_name_tool on {}", file.display()))?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                warn!(file = %file.display(), stderr = %stderr, "install_name_tool warning");
            }
        }

        // Re-sign ALL Mach-O files with ad-hoc signature — required on macOS
        // Sequoia where com.apple.provenance blocks execution of unsigned binaries.
        let _ = std::process::Command::new("codesign")
            .args(["-f", "-s", "-"])
            .arg(file)
            .output();
    }

    debug!(dir = %bottle_dir.display(), "Mach-O placeholders fixed");
    Ok(())
}

/// Find Mach-O files (binaries + dylibs) in a bottle directory.
#[cfg(target_os = "macos")]
fn find_macho_files(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();

    for subdir in &["bin", "sbin", "lib", "libexec"] {
        let d = dir.join(subdir);
        if d.exists() {
            collect_macho_recursive(&d, &mut files);
        }
    }

    files
}

#[cfg(target_os = "macos")]
fn collect_macho_recursive(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_macho_recursive(&path, out);
        } else if path.is_file() {
            let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            // Include binaries (no extension or ELF) and dylibs
            if name.contains(".dylib") || !name.contains('.') {
                out.push(path);
            }
        }
    }
}

/// Read the shared library dependencies from `otool -L`.
#[cfg(target_os = "macos")]
fn read_otool_deps(file: &Path) -> Result<Vec<String>> {
    let output = std::process::Command::new("otool")
        .arg("-L")
        .arg(file)
        .output()
        .with_context(|| format!("failed to run otool -L on {}", file.display()))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let deps: Vec<String> = stdout
        .lines()
        .skip(1) // first line is the file name
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.starts_with("@@HOMEBREW") {
                Some(
                    trimmed
                        .split(" (compatibility")
                        .next()
                        .unwrap_or(trimmed)
                        .trim()
                        .to_string(),
                )
            } else {
                None
            }
        })
        .collect();

    Ok(deps)
}

/// Read the library ID (install name) from `otool -D`.
#[cfg(target_os = "macos")]
fn read_otool_id(file: &Path) -> Result<Option<String>> {
    let output = std::process::Command::new("otool")
        .arg("-D")
        .arg(file)
        .output()
        .with_context(|| format!("failed to run otool -D on {}", file.display()))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let id = stdout
        .lines()
        .skip(1)
        .next()
        .map(|l| l.trim().to_string())
        .filter(|s| s.starts_with("@@HOMEBREW"));

    Ok(id)
}

/// Replace `@@HOMEBREW_CELLAR@@/<name>/<version>/...` or
/// `@@HOMEBREW_PREFIX@@/opt/<name>/...` with the kong store path.
#[cfg(target_os = "macos")]
fn resolve_placeholder(path: &str, store_root: &Path) -> Option<String> {
    if path.starts_with("@@HOMEBREW_CELLAR@@/") {
        // Path format: @@HOMEBREW_CELLAR@@/<name>/<version>/lib/...
        let rest = &path["@@HOMEBREW_CELLAR@@/".len()..];
        let mut parts = rest.splitn(3, '/');
        let name = parts.next()?;
        let version = parts.next()?;
        let remainder = parts.next().unwrap_or("");
        let store_path = store_root.join(format!("{name}-{version}"));
        if remainder.is_empty() {
            Some(store_path.to_string_lossy().into_owned())
        } else {
            Some(store_path.join(remainder).to_string_lossy().into_owned())
        }
    } else if path.starts_with("@@HOMEBREW_PREFIX@@/opt/") {
        // Path format: @@HOMEBREW_PREFIX@@/opt/<name>/lib/...
        let rest = &path["@@HOMEBREW_PREFIX@@/opt/".len()..];
        let mut parts = rest.splitn(2, '/');
        let name = parts.next()?;
        let remainder = parts.next().unwrap_or("");
        // Find the versioned directory in the store
        let version = find_store_version(store_root, name)?;
        let store_path = store_root.join(format!("{name}-{version}"));
        if remainder.is_empty() {
            Some(store_path.to_string_lossy().into_owned())
        } else {
            Some(store_path.join(remainder).to_string_lossy().into_owned())
        }
    } else {
        None
    }
}

/// Find the installed version of a formula by scanning the store directory.
#[cfg(target_os = "macos")]
fn find_store_version(store_root: &Path, name: &str) -> Option<String> {
    let prefix = format!("{name}-");
    if let Ok(entries) = std::fs::read_dir(store_root) {
        for entry in entries.flatten() {
            let dir_name = entry.file_name().to_string_lossy().to_string();
            if dir_name.starts_with(&prefix) && entry.path().is_dir() {
                return Some(dir_name[prefix.len()..].to_string());
            }
        }
    }
    None
}

// ── Shared bottle-ensure logic ───────────────────────────────────────────────

/// Ensure all bottles from a `BrewSection` are present in the kong store.
///
/// On unsupported platforms (not macOS/Linux) this logs a warning and skips.
/// On macOS, runs `fixup_macho_placeholders()` after downloading so freshly
/// downloaded bottles are immediately usable.
pub fn ensure_bottles_in_store(
    brew: &crate::config::BrewSection,
    store: &std::path::Path,
) -> Result<()> {
    if !cfg!(target_os = "macos") && !cfg!(target_os = "linux") {
        warn!(
            count = brew.packages.len(),
            os = std::env::consts::OS,
            "Skipping Homebrew bottles: only supported on macOS/Linux"
        );
        return Ok(());
    }

    info!(count = brew.packages.len(), "Ensuring Homebrew bottles in store");
    let mut downloaded = Vec::new();

    for entry in &brew.packages {
        let bottle_dir = store.join(&entry.store_path);
        if !bottle_dir.exists() {
            info!(pkg = %entry.name, "Downloading missing bottle");
            let formula = resolve_formula(&entry.name)?;
            download_bottle(&formula, &bottle_dir)?;
            downloaded.push(entry.name.clone());
        } else {
            debug!(pkg = %entry.name, "Bottle already in store");
        }
    }

    if downloaded.is_empty() {
        info!("All Homebrew bottles already in store");
    } else {
        info!(packages = ?downloaded, "Newly downloaded Homebrew bottles");
    }

    // On macOS, fix Mach-O placeholders for all bottles so cross-dep
    // references resolve correctly.
    #[cfg(target_os = "macos")]
    {
        for entry in &brew.packages {
            let bottle_dir = store.join(&entry.store_path);
            if bottle_dir.exists() {
                fixup_macho_placeholders(&bottle_dir, store)?;
            }
        }
    }

    Ok(())
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn http_client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .user_agent("kong-dependency-manager/0.1")
        .timeout(std::time::Duration::from_secs(600))
        .connect_timeout(std::time::Duration::from_secs(30))
        .build()
        .context("failed to build HTTP client")
}
