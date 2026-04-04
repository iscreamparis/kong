# KONG — Unified Dependency Manager

> One tool. Python, Node.js, and Rust. Zero duplication. Zero wrappers.

[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Built with Rust](https://img.shields.io/badge/built%20with-Rust-orange.svg)](https://www.rust-lang.org/)
[![Platform: Windows](https://img.shields.io/badge/platform-Windows-blue.svg)]()
[![Release](https://img.shields.io/github/v/release/iscreamparis/kong?label=download)](https://github.com/iscreamparis/kong/releases/latest)

---

KONG is a standalone CLI that eliminates duplicated dependencies across all your projects. Every package lives once on disk — shared by hard links and NTFS junctions. Your project sees a normal `.venv`, `node_modules`, and `.cargo` folder. Nothing changes for your tools. Everything changes for your disk.

**No pip. No npm. No cargo install. No external package managers at all.** KONG talks directly to PyPI, the npm registry, and crates.io — in-house, in Rust.

---

## Why KONG?

If you work on 10 Python projects, you currently have 10 copies of `numpy`. With Node.js it's worse — `node_modules` is infamous for a reason. With Rust, the same 478 crates get re-downloaded per project.

KONG fixes this with a **content-addressable global store** at `~/.kong/` (or `C:\kong\` on Windows). Same package + version = one copy on disk, hard-linked into every project that needs it.

pnpm proved this model works for Node.js. KONG extends it to all three ecosystems at once.

---

## Install

Download the latest binary from the [Releases page](https://github.com/iscreamparis/kong/releases/latest) and put it on your PATH.

```powershell
Invoke-WebRequest -Uri "https://github.com/iscreamparis/kong/releases/latest/download/kong-windows-x86_64.exe" -OutFile "C:\kong\kong.exe"
# The installer also adds C:\kong to your system PATH
```

> Linux and macOS builds are on the [roadmap](#roadmap).

---

## Build KONG with KONG

KONG manages its own build dependencies — including the Rust toolchain. No `rustup` required.

```powershell
kong clone https://github.com/iscreamparis/kong
cd kong
kong rules
kong use kong.rules
. .\.rust-toolchain\activate.ps1   # adds cargo + rustc to current console
cargo build --release
Copy-Item target\release\kong.exe C:\kong\kong.exe
```

> The activation script scopes the toolchain to the current console only — no system-wide changes.

---

## Quick Start

```powershell
# 1. Go to your project
cd my-project

# 2. Scan manifests and generate kong.rules (downloads all deps to the global store)
kong rules

# 3. Wire up .venv / node_modules / .cargo in the project directory
kong use kong.rules

# 4. Activate the Rust toolchain (current console only)
. .\.rust-toolchain\activate.ps1

# 5. Work normally — your tools see nothing different
python src/app.py
node src/index.js
cargo build --release

# 6. Run scripts defined in package.json or pyproject.toml
kong run dev
kong run build
kong run test
```

Second project using the same packages? `kong rules` + `kong use` — instant, no downloads, just links.

---

## How It Works

```
kong rules                          kong use kong.rules
─────────────────                   ───────────────────────────────
reads manifests          →          creates junctions / hard links
  requirements.txt                    .venv/          → store/.venv
  package.json                        node_modules/   → store/node_modules
  Cargo.lock                          .cargo/config   → src replacement

queries registries       →          your tools run unchanged
  PyPI JSON API                       python, node, cargo, vite...
  npm registry
  crates.io

downloads + verifies     →          global store (written once)
  SHA-256 checked                     ~/.kong/store/
  hard links shared                   ├── python/libs/numpy-2.2.4/
                                      ├── node/libs/vite-6.3.1/
                                      └── rust/crates/tokio-1.44.2/
```

**The key insight:** KONG creates NTFS junctions from your project directory into its central store. Vite, Python, Node.js, and Cargo all find their packages by walking up the filesystem — exactly as they would with a real local install. No wrappers. No shims. No `PATH` tricks.

---

## Commands

| Command | Description |
|---------|-------------|
| `kong clone <url> [dir]` | Clone a git repository into `[dir]` (defaults to repo name) |
| `kong clone <url> --setup` | Clone and automatically run `kong rules` + `kong use` |
| `kong rules` | Scan manifests, download all deps, write `kong.rules` |
| `kong rules --force` | Re-download everything even if already in store |
| `kong rules --path <dir>` | Run rules on a different project directory |
| `kong use [kong.rules]` | Create `.venv`, `node_modules`, `.cargo` via links |
| `kong use --clean` | Tear down and rebuild the project environment |
| `kong run <script>` | Run a script from `package.json` or `pyproject.toml` |
| `kong run <script> -- <args>` | Pass extra arguments to the script |
| `kong run <script> --path <dir>` | Run a script in a different project directory |
| `kong store path` | Print the global store path |
| `kong doctor` | Check store integrity and environment health |

**Global flag:** `--verbose` / `-v` — enable detailed tracing output on any command.

---

## Supported Manifests

| Ecosystem | Parsed | Lockfile preferred |
|-----------|--------|--------------------|
| Python | `requirements.txt`, `pyproject.toml` | `uv.lock`, `poetry.lock`, `Pipfile.lock` |
| Node.js | `package.json` | `package-lock.json`, `pnpm-lock.yaml` |
| Rust | `Cargo.toml` | `Cargo.lock` ✓ |

---

## The Store

Everything lives in a single content-addressable store:

```
~/.kong/                              (C:\kong\ on Windows)
├── store/
│   ├── python/
│   │   ├── runtime/3.10.20/         ← KONG-managed Python interpreter
│   │   └── libs/
│   │       ├── numpy-2.2.4/
│   │       └── flask-3.1.1/
│   ├── node/
│   │   ├── runtime/24.14.1/         ← KONG-managed Node.js
│   │   └── libs/
│   │       └── vite-6.3.1/
│   └── rust/
│       ├── toolchain/1.94.1/        ← KONG-managed rustc + cargo
│       └── crates/
│           └── tokio-1.44.2/
└── RULEZ/
    └── my-project/                  ← wired environments per project
        ├── .venv/
        └── node_modules/
```

Packages are stored **once** and **hard-linked** into every project that needs them. Cross-drive projects (e.g. source on `Q:`, store on `C:`) use NTFS junctions.

---

## Real Project Example — DummyKong

[DummyKong](https://github.com/iscreamparis/DummyKong) is KONG's reference test project: a Flask backend + Vite/Vue frontend + Rust fractal renderer, all managed by KONG.

```powershell
git clone https://github.com/iscreamparis/DummyKong
cd DummyKong
kong rules          # downloads Flask, Vue, fractal crates — all to global store
kong use kong.rules # wires .venv, node_modules, .cargo into project dir
.\run.ps1           # starts Flask :5000 + Vite :5173 + renders ASCII fractal
```

No pip. No npm. No conda. No rustup. Just KONG.

---

## Roadmap

### v0.2 — Migration
- [ ] **`kong import`** — convert an existing project (with local `.venv`, `node_modules`, `.cargo`) to the KONG way. Moves already-installed packages into the global store instead of re-downloading them, then replaces the local copies with links.
- [ ] **`kong eject`** — convert a KONG-managed project back to standalone. Copies packages from the store into real local directories so the project works without KONG. (We hope nobody uses this, but it should always be an option.)

### v0.3 — Performance
- [ ] **Parallel downloads** — all packages fetched concurrently (currently sequential)
- [ ] **Progress bars** — `indicatif` integration for long downloads
- [ ] **Resume on failure** — partial downloads restart from where they stopped

### v0.4 — Broader compatibility
- [ ] **Python resolver** — resolve `>=` version constraints without a lockfile
- [ ] **`kong shell`** — drop into an activated shell for a project
- [ ] **`kong add <pkg>`** — add a package and update `kong.rules` in one step

### v0.5 — Git integration (lite)
- [x] **`kong clone <url>`** — clone a repo, then `kong rules` + `kong use` separately (or `--setup` for all-in-one)
- [ ] **`kong login`** — authenticate with GitHub/GitLab for private repos
- [ ] Bundles a minimal `git` client (clone, fetch, pull) via the `gitoxide` / `gix` Rust crate — no system git required

### v1.0 — Production
- [ ] **Linux / macOS support** — symlinks instead of junctions, platform-specific wheel/binary selection, CI pipeline for cross-platform builds
- [ ] private registry support
- [ ] Windows installer with proper PATH management (NSIS → WiX)
- [ ] `kong doctor` full report with auto-fix suggestions

---

## Design Principles

- **No external package managers.** KONG never calls `pip`, `npm`, `yarn`, `pnpm`, or `cargo install` as subprocesses. All registry communication is in-house via `reqwest`.
- **Idempotent.** Every command is safe to re-run. Already in store? Skip the download. Link already exists? Skip the link.
- **Transparent.** Your project directory looks exactly like a normal project to every tool. KONG is invisible at runtime.
- **Windows-only (for now).** NTFS junctions + hard links. Long paths. `\\?\` prefixes where needed. Linux/macOS support is on the roadmap.

---

## Contributing

KONG is in active development. The codebase is 100% Rust. See `agents.md` for the full architecture and module breakdown.

```
src/
├── cli.rs          # clap CLI definitions
├── config.rs       # kong.rules schema + manifest parsers
├── download.rs     # HTTP download + SHA-256 verification
├── extract.rs      # zip / tar.gz / .crate extraction
├── link.rs         # hard links, junctions, project-dir wiring
├── runner.rs       # kong run <script>
├── store.rs        # store layout + doctor
├── python/         # PyPI client, venv builder, runtime
├── node/           # npm client, node_modules builder, runtime
└── rust_eco/       # crates.io client, source replacement, toolchain
```

---

## License

MIT © iscreamparis
