# KONG — Unified Dependency Manager (Python + Node.js + Rust)
**Build this entire CLI tool in Rust — 100% in-house, no external package managers**

## 1. Project Overview
You are building **KONG**, a lightweight, cross-platform (Windows-first) CLI tool that eliminates duplicated dependencies across many projects.

**Core philosophy:**
- One global content-addressable store (`~/.kong` or `C:\kong`).
- Per-project `.rules` file that maps exact dependency versions to the central store.
- `kong use ./project.rules` creates a **completely transparent virtual environment** inside the project folder (`.venv`, `node_modules`, `.cargo/config.toml` etc.).
- The launched app sees **nothing different** — zero wrappers, zero env vars, zero performance penalty.
- Massive disk-space and download-time savings.

**CRITICAL REQUIREMENT — NO EXTERNAL PACKAGE MANAGERS**
- **Never** call `pip`, `uv`, `npm`, `yarn`, `pnpm`, `cargo` (except for optional metadata if truly needed) as subprocesses.
- **All downloads and metadata fetching must be done in-house** using `reqwest` directly against the official registries:
  - PyPI: `https://pypi.org/pypi/<package>/json` (and simple API if needed)
  - npm registry: `https://registry.npmjs.org/<package>` and `https://registry.npmjs.org/<package>/<version>`
  - crates.io: direct JSON API or tarball URLs from Cargo.lock
- KONG must parse manifests, resolve exact versions from lockfiles, download, verify, and extract everything itself.

Supported languages (all three first-class):
- Python (pip-style)
- Node.js (npm-style)
- Rust (Cargo)

## 2. Goals
- Zero duplication: same package+version exists only once on disk (hard links + junctions).
- `kong rules` + `kong use` workflow must be dead simple.
- 100% standalone binary — user should not need pip/npm/uv installed to use KONG.
- Fast, safe, and production-ready.

## 3. Non-Goals (v1)
- No full dependency solver from scratch (for v1 we rely on existing lockfiles or exact versions).
- No private registries or authentication in v1.
- No Docker / Nix replacement.

## 4. Central Store Layout (global)

~/.kong/
├── python/
│   └── libs/
│       └── <package>-<version>-py<major.minor>-<platform>/   ← unpacked wheel contents
├── node/
│   └── libs/
│       └── <package>-<version>/                              ← unpacked .tgz contents
└── rust/
├── crates/
│   └── <crate>-<version>/                               ← unpacked crate source
└── registry/                                            ← optional local-registry mirror


Use SHA256 of the downloaded archive + version as the deduplication key.

## 5. .rules File Format (JSON)
Located at `./kong.rules` (or user-specified path).

Contains sections for Python, Node, and Rust with exact store paths, versions, and hashes.

## 6. CLI Commands (use `clap`)

```bash
kong rules                  # parse manifests → create/update kong.rules
kong rules --force          # force re-download even if present
kong use ./kong.rules       # create virtual envs inside project
kong use --clean
kong store path
kong doctor

7. Detailed Implementation (All In-House)
Python

Parse requirements.txt and pyproject.toml (simple line-by-line + toml).
Prefer lockfiles (uv.lock, poetry.lock, Pipfile.lock) if present.
For each package+exact version:
Query PyPI JSON API → find best wheel for current Python version + platform (win_amd64, manylinux, etc.).
Download .whl (or fallback to .tar.gz).
Extract wheel contents to central store.

kong use:
Create minimal .venv folder structure (no subprocess venv — manual creation of folders + pyvenv.cfg).
In .venv/Lib/site-packages/ create hard links (files) + NTFS junctions (dirs) pointing to the central store.


Node.js

Parse package.json and package-lock.json (or pnpm-lock.yaml if present).
For each package+version:
Query npm registry JSON → get dist.tarball URL.
Download .tgz → extract to central store.

kong use:
Build ./node_modules structure exactly like pnpm does (hard links + junctions in .pnpm folder + top-level symlinks/junctions so Node resolver is happy).


Rust

Parse Cargo.toml + Cargo.lock using cargo-lock crate.
Download crates directly from crates.io tarball URLs (or use the registry index JSON).
kong use:
Generate .cargo/config.toml with source replacement pointing to the local registry mirror (directory-based).


8. Technical Stack (Rust only)
Required crates:

clap (derive)
serde + serde_json + toml
cargo-lock
reqwest + tokio (or blocking with reqwest::blocking)
sha2 + hex
zip + tar + flate2 + bzip2 (for extraction)
junction (for Windows NTFS junctions)
dirs / directories
walkdir, tempfile, fs_extra
anyhow + thiserror
tracing + tracing-subscriber
indicatif (optional nice progress bars)

No std::process::Command for pip/npm/uv/cargo download steps.
9. Platform Notes

Windows primary: junctions + hard links + \\?\ long paths.
macOS/Linux: symlinks + hard links.
Detect current Python version, platform tags, Node version, Rust target automatically.

10. Development Plan

Project skeleton + CLI + config
Central store + content-addressable logic
Manifest parsers (requirements.txt, package.json, Cargo.lock)
Direct registry clients (PyPI + npm + crates.io)
Download + verify + extract pipeline
Python virtual environment builder
Node.js node_modules builder (pnpm-style)
Rust source replacement
Polish + doctor command + README

11. Extra Requirements

Excellent error messages.
Verbose mode.
Idempotent operations.
Safe: never delete from central store unless explicitly told.
At the end, generate a full README.md with usage examples.

Now start building.
First output the complete Cargo.toml with all dependencies and features, then proceed step by step, showing each major module as you complete it.
Let’s build KONG — fully standalone and in-house! 🚀