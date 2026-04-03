---
name: kong-store
description: "Manage the KONG central store — content-addressable package storage with SHA-256 deduplication. Use when: implementing store layout, download-verify-extract pipeline, computing store paths, checking existing packages, or working with the kong.rules JSON format."
argument-hint: "Which aspect (store-layout, download-pipeline, rules-format, or dedup-logic)"
---

# Central Store Management

## When to Use
- Implementing or modifying the content-addressable store at `~/.kong/`
- Building the download → verify → extract pipeline
- Working with the `kong.rules` JSON format
- Computing store paths for packages
- Checking whether a package is already cached

## Store Layout
```
~/.kong/                              # dirs::home_dir() + ".kong"
├── python/
│   └── libs/
│       └── <name>-<ver>-<pytag>-<platform>/   # unpacked wheel contents
├── node/
│   └── libs/
│       └── <name>-<ver>/                       # unpacked tarball contents
└── rust/
    ├── crates/
    │   └── <name>-<ver>/                       # unpacked .crate source
    └── registry/                               # optional local-registry mirror
```

### Store Path Computation
```rust
fn python_store_path(store_root: &Path, name: &str, version: &str, python_tag: &str, platform_tag: &str) -> PathBuf {
    store_root.join("python").join("libs").join(format!("{name}-{version}-{python_tag}-{platform_tag}"))
}

fn node_store_path(store_root: &Path, name: &str, version: &str) -> PathBuf {
    store_root.join("node").join("libs").join(format!("{name}-{version}"))
}

fn rust_store_path(store_root: &Path, name: &str, version: &str) -> PathBuf {
    store_root.join("rust").join("crates").join(format!("{name}-{version}"))
}
```

### Store Root Detection
1. Check `KONG_STORE` env var
2. Windows: `C:\kong` (short path, avoids long-path issues)
3. Unix: `~/.kong`

## Download → Verify → Extract Pipeline

### Procedure
1. **Check store:** If target store path exists AND contains a `.kong-verified` marker file, skip entirely (idempotent).
2. **Download** to a temp file (`tempfile::NamedTempFile`):
   - Use `reqwest::blocking::get(url)` → stream to file
   - Show progress bar with `indicatif` if terminal is interactive
3. **Verify** SHA-256 hash:
   - Compute `sha2::Sha256` digest of the downloaded file
   - Compare against expected hash from lockfile or registry API
   - On mismatch → delete temp file, return `HashMismatch` error
4. **Extract** to store path:
   - `.whl` → `zip::ZipArchive` → extract to store path
   - `.tgz` / `.tar.gz` → `flate2::read::GzDecoder` + `tar::Archive` → extract
   - `.crate` → same as `.tar.gz`
5. **Write marker:** Create `.kong-verified` file with hash + timestamp inside store path
6. **Cleanup:** Temp file drops automatically via `tempfile`

### Idempotency
- Always check before downloading: `if store_path.join(".kong-verified").exists() { return Ok(()) }`
- Never delete from store unless `--force` flag is passed
- `--force` removes existing store entry and re-downloads

## kong.rules JSON Format
```json
{
  "version": 1,
  "project": "my-app",
  "generated": "2026-04-03T12:00:00Z",
  "python": {
    "version": "3.11",
    "packages": [
      {
        "name": "requests",
        "version": "2.31.0",
        "hash": "sha256:abcdef...",
        "store_path": "python/libs/requests-2.31.0-py3-none-any",
        "source_url": "https://files.pythonhosted.org/..."
      }
    ]
  },
  "node": {
    "packages": [
      {
        "name": "express",
        "version": "4.18.2",
        "hash": "sha256:123456...",
        "store_path": "node/libs/express-4.18.2",
        "source_url": "https://registry.npmjs.org/express/-/express-4.18.2.tgz"
      }
    ]
  },
  "rust": {
    "packages": [
      {
        "name": "serde",
        "version": "1.0.193",
        "hash": "sha256:789abc...",
        "store_path": "rust/crates/serde-1.0.193"
      }
    ]
  }
}
```

## Serde Structs
```rust
#[derive(Debug, Serialize, Deserialize)]
struct KongRules {
    version: u32,
    project: String,
    generated: String,
    #[serde(default)]
    python: Option<PythonSection>,
    #[serde(default)]
    node: Option<NodeSection>,
    #[serde(default)]
    rust: Option<RustSection>,
}
```

## Error Types
```rust
#[derive(Debug, thiserror::Error)]
enum StoreError {
    #[error("hash mismatch for {package}: expected {expected}, got {actual}")]
    HashMismatch { package: String, expected: String, actual: String },
    #[error("unsupported archive format: {0}")]
    UnsupportedFormat(String),
    #[error("store path already exists and --force not specified: {0}")]
    AlreadyExists(PathBuf),
}
```
