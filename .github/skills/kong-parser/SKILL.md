---
name: kong-parser
description: "Parse dependency manifests and lockfiles for Python, Node.js, and Rust. Use when: implementing parsers for requirements.txt, pyproject.toml, uv.lock, poetry.lock, Pipfile.lock, package.json, package-lock.json, pnpm-lock.yaml, Cargo.toml, Cargo.lock. Extracts exact package names and versions."
argument-hint: "Which manifest to parse (e.g. 'requirements.txt', 'package-lock.json', 'Cargo.lock', or 'all')"
---

# Manifest & Lockfile Parsing

## When to Use
- Implementing or modifying a manifest/lockfile parser
- Adding support for a new lockfile format
- Debugging version extraction from dependency files
- Implementing `kong rules` command (manifest → .rules conversion)

## Supported Formats

### Python (priority order — use first found)
| File | Format | Notes |
|------|--------|-------|
| `uv.lock` | TOML-based | Preferred — has exact versions + hashes |
| `poetry.lock` | TOML | `[[package]]` sections with `name`, `version` |
| `Pipfile.lock` | JSON | `default` + `develop` keys → package versions |
| `requirements.txt` | Line-based | `package==version` lines, ignore comments/flags |
| `pyproject.toml` | TOML | `[project.dependencies]` or `[tool.poetry.dependencies]` — may have ranges, not exact |

### Node.js (priority order)
| File | Format | Notes |
|------|--------|-------|
| `package-lock.json` | JSON | `packages` (v3) or `dependencies` (v1/v2) — has exact versions + integrity hashes |
| `pnpm-lock.yaml` | YAML | `packages` map with versions, skip if too complex for v1 |
| `package.json` | JSON | `dependencies` + `devDependencies` — may have ranges |

### Rust
| File | Format | Notes |
|------|--------|-------|
| `Cargo.lock` | TOML | Use `cargo-lock` crate — gives `Package { name, version, checksum }` |
| `Cargo.toml` | TOML | For metadata only (project name, edition) |

## Procedure

### 1. Detect manifests
Scan the project directory for all known filenames. Return a detection result:
```rust
struct DetectedManifests {
    python: Vec<PythonManifest>,  // ordered by priority
    node: Vec<NodeManifest>,
    rust: Option<CargoManifest>,
}
```

### 2. Parse each format
For each parser, extract a flat list of `(package_name, exact_version)` pairs.

**requirements.txt parsing:**
- Split on newlines
- Skip empty lines, `#` comments, `-r` includes, `--` flags
- Parse `package==version` (exact) or `package>=version` (use specified version as minimum, warn)
- Normalize package names: lowercase, replace `-` and `.` with `_`

**pyproject.toml parsing:**
- Use `toml` crate → extract `[project.dependencies]` array
- Each entry is a PEP 508 string: `"requests>=2.28"` → parse name + version specifier
- For `[tool.poetry.dependencies]`: keys are package names, values are version strings or tables

**package-lock.json parsing (v3):**
- Parse JSON → navigate to `packages` object
- Each key is a path like `node_modules/express`
- Extract `version` and `resolved` (tarball URL) and `integrity` (SRI hash)
- Skip the root entry (empty string key)

**Cargo.lock parsing:**
- Use `cargo_lock::Lockfile::load(path)?`
- Iterate `.packages` → extract `.name`, `.version`, `.checksum`

### 3. Output unified dependency list
```rust
struct Dependency {
    name: String,
    version: String,
    ecosystem: Ecosystem,  // Python | Node | Rust
    hash: Option<String>,  // from lockfile if available
    source_url: Option<String>,  // from lockfile if available
}
```

### 4. Generate .rules entries
Transform the dependency list into the `.rules` JSON format with store paths computed from the central store layout.

## Package Name Normalization
- **Python:** lowercase, replace `-._` with `_` (PEP 503)
- **Node:** keep as-is (scoped: `@scope/name`)
- **Rust:** keep as-is (crate names use `-`)

## Testing
- Create test fixtures for each format in `tests/fixtures/`
- Unit test each parser with known input → expected output
- Test edge cases: empty files, comments, ranges vs exact, scoped npm packages
