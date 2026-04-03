---
name: kong-venv
description: "Build virtual environments using hard links and junctions — Python .venv, Node.js node_modules (pnpm-style), Rust source replacement. Use when: implementing 'kong use', creating .venv without subprocess, building pnpm-style node_modules, generating .cargo/config.toml, or debugging link/junction issues on Windows or Unix."
argument-hint: "Which ecosystem (python, node, rust, or all)"
---

# Virtual Environment Builder

## When to Use
- Implementing `kong use` command for any ecosystem
- Creating Python `.venv` without calling `python -m venv`
- Building pnpm-style `node_modules` with hard links and junctions
- Generating Rust `.cargo/config.toml` source replacements
- Debugging junction/symlink/hard-link issues

## Platform-Aware Linking

### Windows (default)
```rust
use junction;  // NTFS junctions for directories
use std::fs::hard_link;  // hard links for files

fn link_dir(src: &Path, dst: &Path) -> Result<()> {
    junction::create(src, dst)?;
    Ok(())
}

fn link_file(src: &Path, dst: &Path) -> Result<()> {
    std::fs::hard_link(src, dst)?;
    Ok(())
}
```

### Unix
```rust
#[cfg(unix)]
fn link_dir(src: &Path, dst: &Path) -> Result<()> {
    std::os::unix::fs::symlink(src, dst)?;
    Ok(())
}
```

### Long paths on Windows
- Prefix paths with `\\?\` for paths > 260 chars
- Use `dunce::canonicalize()` or manual prefixing

## Python .venv Builder

### Procedure
1. **Create directory structure** (no subprocess):
   ```
   .venv/
   ├── pyvenv.cfg
   ├── Scripts/          # Windows
   │   └── python.exe    # hard link to system Python
   ├── bin/              # Unix
   │   └── python        # symlink to system Python
   └── Lib/
       └── site-packages/
   ```

2. **Write `pyvenv.cfg`:**
   ```ini
   home = C:\Python311
   include-system-site-packages = false
   version = 3.11.0
   ```
   - `home` = directory containing the Python executable
   - Detect Python path: search PATH for `python3` / `python`, resolve to real path

3. **Link packages into site-packages:**
   For each package in kong.rules → python.packages:
   - Source: `{store_root}/{store_path}/`
   - Walk the store path:
     - **Files** → hard link into `.venv/Lib/site-packages/`
     - **Directories** → NTFS junction (Windows) or symlink (Unix)
   - Preserve the top-level package structure (e.g., `requests/`, `urllib3/`)
   - Also link `.dist-info/` directories for metadata

4. **Idempotency:** If `.venv/` exists re-run should reconcile (add missing links, skip existing).

### `--clean` flag
Delete `.venv/` entirely, then rebuild from kong.rules.

## Node.js node_modules Builder (pnpm-style)

### Layout
pnpm uses a content-addressable `.pnpm` directory with top-level symlinks:
```
node_modules/
├── .pnpm/
│   ├── express@4.18.2/
│   │   └── node_modules/
│   │       ├── express/        ← hard links to store
│   │       ├── accepts/        ← junction to .pnpm/accepts@1.3.8/...
│   │       └── ...peer deps
│   └── accepts@1.3.8/
│       └── node_modules/
│           └── accepts/        ← hard links to store
├── express/                    ← junction → .pnpm/express@4.18.2/.../express
└── ...other top-level deps
```

### Procedure
1. Create `node_modules/.pnpm/` directory.
2. For each package in kong.rules → node.packages:
   - Create `.pnpm/{name}@{version}/node_modules/{name}/`
   - Hard link all files from `{store_root}/{store_path}/` into it
   - Junction all subdirectories
3. For top-level dependencies (from `package.json` `dependencies` + `devDependencies`):
   - Create junction: `node_modules/{name}` → `.pnpm/{name}@{version}/node_modules/{name}`
4. For transitive dependencies: resolve from `package-lock.json` `packages` tree, create junctions within each `.pnpm` entry's `node_modules/`.

### Scoped packages
- `@scope/name` → create `node_modules/.pnpm/@scope+name@version/` (replace `/` with `+`)
- Top-level: `node_modules/@scope/name` → junction

## Rust Source Replacement

### Procedure
1. Create `.cargo/` directory in project root if not exists.
2. Generate `.cargo/config.toml`:
   ```toml
   [source.crates-io]
   replace-with = "kong-local"

   [source.kong-local]
   directory = "C:\\kong\\rust\\registry"
   ```
3. For the local directory registry, ensure each crate has the expected structure:
   ```
   {store_root}/rust/registry/{crate}-{version}/
   ├── Cargo.toml
   ├── src/
   └── .cargo-checksum.json    # {"files": {}, "package": "<sha256>"}
   ```
4. The `.cargo-checksum.json` is required by Cargo's directory source — generate it from the known hash.

### `--clean` flag
Remove `.cargo/config.toml` source replacement entries (keep other config if present).

## Testing
- Use `tempfile::TempDir` for all tests
- Verify hard links: `std::fs::metadata(a).ino() == std::fs::metadata(b).ino()` (Unix) or check `nNumberOfLinks` on Windows
- Verify junctions: `junction::exists(path)` on Windows
- Test idempotency: run builder twice, assert no errors and same result
