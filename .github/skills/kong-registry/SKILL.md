---
name: kong-registry
description: "Implement in-house registry API clients for PyPI, npm, and crates.io using reqwest. Use when: building download clients, fetching package metadata, selecting wheels/tarballs, querying registry APIs. Covers: PyPI JSON API, npm registry API, crates.io tarball URLs. NEVER calls pip, npm, or cargo as subprocesses."
argument-hint: "Which registry to implement (pypi, npm, crates-io, or all)"
---

# Registry Client Implementation

## When to Use
- Building or modifying a registry API client (PyPI, npm, crates.io)
- Fetching package metadata or download URLs
- Selecting the best wheel for a Python platform/version combo
- Debugging download failures or API response parsing

## Critical Rule
**NEVER** use `std::process::Command` to call `pip`, `npm`, `yarn`, `pnpm`, `uv`, or `cargo`. All HTTP calls go through `reqwest::blocking::Client` directly.

## Registry APIs

### PyPI
- **Metadata endpoint:** `GET https://pypi.org/pypi/{package}/json`
- Response contains `releases` map: version → list of file objects
- Each file object has: `filename`, `url`, `digests.sha256`, `requires_python`, `packagetype`
- **Wheel selection priority:**
  1. Match `python_version` tag (e.g., `cp311`, `cp312`, `py3`)
  2. Match `platform` tag (e.g., `win_amd64`, `manylinux2014_x86_64`, `macosx_*`)
  3. Prefer `bdist_wheel` over `sdist`
  4. Fall back to `.tar.gz` source dist if no wheel matches

```rust
// Pseudocode structure
struct PypiClient { client: reqwest::blocking::Client }

impl PypiClient {
    fn fetch_metadata(&self, package: &str) -> Result<PypiPackageInfo>;
    fn select_best_wheel(&self, info: &PypiPackageInfo, python_ver: &str, platform: &str) -> Result<PypiFileInfo>;
    fn download_wheel(&self, file_info: &PypiFileInfo, dest: &Path) -> Result<PathBuf>;
}
```

### npm
- **Package metadata:** `GET https://registry.npmjs.org/{package}`
- **Specific version:** `GET https://registry.npmjs.org/{package}/{version}`
- Version response has `dist.tarball` (URL) and `dist.shasum` (SHA-1) and `dist.integrity` (SHA-512 SRI)
- Tarballs are `.tgz` files containing a `package/` directory

```rust
struct NpmClient { client: reqwest::blocking::Client }

impl NpmClient {
    fn fetch_version(&self, package: &str, version: &str) -> Result<NpmVersionInfo>;
    fn download_tarball(&self, info: &NpmVersionInfo, dest: &Path) -> Result<PathBuf>;
}
```

### crates.io
- **Tarball URL:** `https://static.crates.io/crates/{crate}/{crate}-{version}.crate`
- `.crate` files are `.tar.gz` archives containing `{crate}-{version}/` with source code
- Checksum from `Cargo.lock` entry (`checksum` field)
- Use `cargo-lock` crate to parse the lock file

```rust
struct CratesClient { client: reqwest::blocking::Client }

impl CratesClient {
    fn download_crate(&self, name: &str, version: &str, dest: &Path) -> Result<PathBuf>;
}
```

## Procedure
1. Create the client struct with `reqwest::blocking::Client` (reuse across requests).
2. Define serde structs for the JSON API response (only fields we need — use `#[serde(default)]` liberally).
3. Implement metadata fetching with proper error messages (package not found, version not found, network error).
4. Implement download with:
   - Progress bar via `indicatif` (optional)
   - SHA-256 verification after download
   - Write to temp file first, then rename (atomic)
5. Add `#[cfg(test)] mod tests` with recorded JSON fixtures (don't hit real APIs in unit tests).

## Error Handling
Use `thiserror` for typed errors per client:
```rust
#[derive(Debug, thiserror::Error)]
enum RegistryError {
    #[error("package '{0}' not found on {1}")]
    PackageNotFound(String, &'static str),
    #[error("version '{1}' not found for '{0}'")]
    VersionNotFound(String, String),
    #[error("no compatible wheel for {0} on {1}/{2}")]
    NoCompatibleWheel(String, String, String),
    #[error("hash mismatch for {0}: expected {1}, got {2}")]
    HashMismatch(String, String, String),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
}
```

## Platform Detection
- Python version: look for `python3` or `python` on PATH, parse `--version` output
- Platform tags: detect via `std::env::consts::{OS, ARCH}` → map to wheel platform tags
- Node version: look for `node` on PATH for compatibility, but not required
