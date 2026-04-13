# KONG — Unified Dependency Manager

> One tool. Python, Node.js, and Rust. Zero duplication. Zero wrappers.

[![License: MIT OR Apache-2.0](https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-blue.svg)](https://github.com/iscreamparis/kong/blob/main/LICENSE-MIT)
[![Built with Rust](https://img.shields.io/badge/built%20with-Rust-orange.svg)](https://www.rust-lang.org/)
[![Platform: Windows + macOS](https://img.shields.io/badge/platform-Windows%20%7C%20macOS-blue.svg)]()
[![Release](https://img.shields.io/github/v/release/iscreamparis/kong?include_prereleases&label=download)](https://github.com/iscreamparis/kong/releases)

---

KONG is a standalone CLI that eliminates duplicated dependencies across all your projects. Every package lives once on disk — shared by hard links and symlinks. Your project sees a normal `.venv`, `node_modules`, and `.cargo` folder. Nothing changes for your tools. Everything changes for your disk.

**No pip. No npm. No cargo install. No brew. No external package managers at all.** KONG talks directly to PyPI, the npm registry, crates.io, and GitHub Container Registry (for Homebrew bottles) — in-house, in Rust.

---

## Why KONG?

If you work on 10 Python projects, you currently have 10 copies of `numpy`. With Node.js it's worse — `node_modules` is infamous for a reason. With Rust, the same 478 crates get re-downloaded per project.

KONG fixes this with a **content-addressable global store** at `~/.kong/` (or `C:\kong\` on Windows). Same package + version = one copy on disk, hard-linked into every project that needs it.

pnpm proved this model works for Node.js. KONG extends it to all three ecosystems at once.

---

## Install

### Windows

Download the installer from the [Releases page](https://github.com/iscreamparis/kong/releases) and run it. It drops `kong.exe` into `C:\kong\`, creates the store directory, and adds it to your system PATH.

### macOS

```bash
# Build from source (requires Rust toolchain)
git clone https://github.com/iscreamparis/kong
cd kong
cargo build --release
cp target/release/kong ~/.local/bin/
```

The store lives at `~/Library/Application Support/kong/`. On macOS, KONG uses symlinks instead of NTFS junctions.

---

## Build KONG with KONG

KONG manages its own build dependencies — including the Rust toolchain. No `rustup` required.

```bash
kong clone https://github.com/iscreamparis/kong
cd kong
kong rules
kong use kong.rules
cargo build --release
cp target/release/kong ~/.local/bin/kong
```

---

## Quick Start

```bash
# 1. Go to your project
cd my-project

# 2. Scan manifests and generate kong.rules (downloads all deps to the global store)
kong rules

# 3. Wire up .venv / node_modules / .cargo in the project directory
kong use kong.rules

# 4. Work normally — your tools see nothing different
python src/app.py
node src/index.js
cargo build --release

# 5. Run scripts defined in package.json or pyproject.toml
kong run dev
kong run build
kong run test
```

Or do it all in one shot:

```bash
# Clone, install everything, and run smoke tests
kong super https://github.com/owner/repo -r build -r test
```

Second project using the same packages? `kong rules` + `kong use` — instant, no downloads, just links.

---

## How It Works

```
kong rules                          kong use kong.rules
─────────────────                   ───────────────────────────────
reads manifests          →          creates symlinks / hard links
  requirements.txt                    .venv/          → store/.venv
  package.json                        node_modules/   → store/node_modules
  Cargo.lock                          .cargo/config   → src replacement
  Brewfile                            brew bottles    → store/brew/

queries registries       →          your tools run unchanged
  PyPI JSON API                       python, node, cargo, vite,
  npm registry                        jq, psql, redis-server...
  crates.io
  GHCR (Homebrew bottles)

downloads + verifies     →          global store (written once)
  SHA-256 checked                     ~/Library/Application Support/kong/store/
  hard links shared                   ├── python/libs/numpy-2.2.4/
                                      ├── node/libs/vite-6.3.1/
                                      ├── rust/crates/tokio-1.44.2/
                                      └── brew/jq-1.7.1/
```

**The key insight:** KONG creates symlinks (macOS/Linux) or NTFS junctions (Windows) from your project directory into its central store. Vite, Python, Node.js, and Cargo all find their packages by walking up the filesystem — exactly as they would with a real local install. Homebrew bottles are downloaded directly from GHCR and injected into `PATH` at runtime. No wrappers. No shims.

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
| `kong super <url> [dir]` | Clone + rules + use + run — full end-to-end setup & smoke test |
| `kong super <url> -r build -r test` | Run only specific scripts after setup |
| `kong store path` | Print the global store path |
| `kong doctor` | Check store integrity and environment health |

**Global flag:** `--verbose` / `-v` — enable detailed tracing output on any command.

---

## Supported Manifests

| Ecosystem | Parsed | Lockfile preferred |
|-----------|--------|--------------------|
| Python | `requirements.txt`, `pyproject.toml` | `uv.lock`, `poetry.lock`, `Pipfile.lock` |
| Node.js | `package.json` | `package-lock.json`, `pnpm-lock.yaml` |
| Rust | `Cargo.toml` | `Cargo.lock` ✓ || Homebrew | `Brewfile` | — (fetches latest bottles from GHCR) |
---

## The Store

Everything lives in a single content-addressable store:

```
~/Library/Application Support/kong/   (macOS)
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
│   ├── rust/
│   │   ├── toolchain/1.94.1/        ← KONG-managed rustc + cargo
│   │   └── crates/
│   │       └── tokio-1.44.2/
│   └── brew/
│       ├── jq-1.7.1/               ← Homebrew bottles from GHCR
│       ├── postgresql@17-17.5/
│       └── redis-7.4.3/
└── RULEZ/
    └── my-project/                  ← wired environments per project
        ├── .venv/
        └── node_modules/
```

Packages are stored **once** and **hard-linked** into every project that needs them.

---

## Real Project Example — DummyKong

[DummyKong](https://github.com/iscreamparis/DummyKong) is KONG's reference test project: a Flask backend + Vite/Vue frontend + Rust fractal renderer, all managed by KONG.

```bash
# One command does everything: clone → rules → use → run
kong super https://github.com/iscreamparis/DummyKong -r build -r fractal
```

Or step by step:

```bash
git clone https://github.com/iscreamparis/DummyKong
cd DummyKong
kong rules              # downloads Flask, Vue, Rust crates, brew bottles — all to global store
kong use kong.rules     # wires .venv, node_modules, brew bins into project dir
kong run backend        # starts Flask on :5000 (uses Postgres + Redis via KONG-managed bottles)
kong run dev            # starts Vite on :5173
kong run fractal        # renders ASCII Mandelbrot (auto-builds Rust binary)
kong run health         # checks Postgres/Redis connectivity via jq
```

DummyKong uses a `Brewfile` for system dependencies (`postgresql@17`, `redis`, `jq`) — KONG downloads the bottles directly from GHCR, no `brew` CLI needed.

No pip. No npm. No brew. No conda. No rustup. Just KONG.

---

## Roadmap

### v0.2 — Cross-platform + critical fixes ✅
- [x] **macOS / Apple Silicon support** — symlinks instead of NTFS junctions, platform-aware store path, arm64 wheel selection
- [x] **Node.js bin scripts** — link CLI tools into `node_modules/.bin/` so `vite`, `tsc`, `eslint` etc. work via `kong run`
- [x] **Architecture-aware wheel selection** — correctly pick arm64 wheels on Apple Silicon (any compatible macOS version)
- [ ] **Proper wheel selection** — full PEP 427 filename parsing; check `requires_python`
- [ ] **Transitive dependency cycle detection** — prevent `kong rules` from hanging on circular deps
- [ ] **Download retry** — retry failed downloads before giving up

### v0.3 — Homebrew / system dependencies ✅
- [x] **`Brewfile` support** — `kong rules` detects `Brewfile`, resolves formulas + transitive deps via Homebrew API
- [x] **Direct GHCR bottle downloads** — downloads pre-built bottles from GitHub Container Registry, no `brew` CLI needed
- [x] **Mach-O fixup** — rewrites `@@HOMEBREW_PREFIX@@` placeholders in binaries + codesigns for macOS Sequoia
- [x] **BFS transitive deps** — walks the full dependency tree so `psql`, `redis-server`, `jq` all get their shared libs
- [x] **Runtime PATH/lib injection** — `kong run` injects brew `bin/` and `lib/` into the script environment automatically
- [ ] **`ca-certificates` bottle** — ARM64 Sequoia bottle tag not yet available upstream

### v0.4 — End-to-end workflow ✅
- [x] **`kong super <url>`** — one command to clone, generate rules, set up environments, and run scripts
- [x] **Lazy cargo build** — `kong run` auto-builds Rust binaries when the target is missing
- [ ] **`kong super` parallel script execution** — run independent scripts concurrently

### v0.5 — Service management ← **current**
- [ ] **`kong service start <name>`** — start a service (postgres, redis) as a background daemon with pid tracking
- [ ] **`kong service stop <name>`** — stop a running service gracefully
- [ ] **`kong service status`** — show running services, ports, pids, uptime
- [ ] **`kong service logs <name>`** — tail stdout/stderr of a running service
- [ ] **`kong.rules` services section** — declare services alongside scripts; `kong use` knows what can be started
- [ ] **Auto-start on `kong run`** — scripts can declare service dependencies; KONG starts them before running the script

### v0.6 — GUI (Slint)
- [ ] **`kong gui`** — native desktop window using [Slint](https://slint.dev/) (Cupertino style on macOS, Fluent on Windows)
- [ ] **Projects tab** — list all KONG-managed projects, run/stop scripts, view status
- [ ] **Services tab** — start/stop/restart services, see ports, health, live logs
- [ ] **Store tab** — browse global store by ecosystem, see disk usage, clean unused packages
- [ ] **Packages tab** — per-project dependency view with ecosystem filters (Python / Node / Rust / Brew)
- [ ] **Doctor tab** — system health checks, one-click auto-fix

### v0.7 — Migration
- [ ] **`kong import`** — convert an existing project (with local `.venv`, `node_modules`, `.cargo`) to the KONG way. Moves already-installed packages into the global store instead of re-downloading them, then replaces the local copies with links.
- [ ] **`kong eject`** — convert a KONG-managed project back to standalone. Copies packages from the store into real local directories so the project works without KONG.

### v0.8 — Performance
- [ ] **Parallel downloads** — all packages fetched concurrently (currently sequential)
- [ ] **Progress bars** — `indicatif` integration for long downloads
- [ ] **Resume on failure** — partial downloads restart from where they stopped

### v0.9 — Broader compatibility
- [ ] **Python resolver** — resolve `>=` version constraints without a lockfile
- [ ] **`kong shell`** — drop into an activated shell for a project
- [ ] **`kong add <pkg>`** — add a package and update `kong.rules` in one step
- [ ] **`kong store move <path>`** — move the global store to another disk
- [ ] **`kong store add <path>`** — add a secondary store on another disk

### v0.10 — Git integration (lite)
- [x] **`kong clone <url>`** — clone a repo, then `kong rules` + `kong use` separately (or `--setup` for all-in-one)
- [ ] **`kong login`** — authenticate with GitHub/GitLab for private repos
- [ ] Bundles a minimal `git` client (clone, fetch, pull) via the `gitoxide` / `gix` Rust crate — no system git required

### v1.0 — Production
- [x] **macOS support** — symlinks, platform-specific selection, GHCR bottles
- [ ] **Linux support** — CI pipeline for cross-platform builds
- [ ] Private registry support (PyPI, npm, crates.io)
- [ ] Windows installer with proper PATH management (NSIS → WiX)
- [ ] `kong doctor` full report with auto-fix suggestions

---

## Design Principles

- **No external package managers.** KONG never calls `pip`, `npm`, `yarn`, `pnpm`, `cargo install`, or `brew` as subprocesses. All registry communication is in-house via `reqwest`.
- **Idempotent.** Every command is safe to re-run. Already in store? Skip the download. Link already exists? Skip the link.
- **Transparent.** Your project directory looks exactly like a normal project to every tool. KONG is invisible at runtime.
- **Cross-platform.** Windows (NTFS junctions + hard links) and macOS (symlinks + hard links). Linux support coming.

---

## Known Limitations

KONG is early-stage software. Here's what doesn't work yet — no surprises.

### Python
- **No sdist compilation.** KONG downloads pre-built wheels only. If a package has no wheel for your platform (rare for popular packages, common for niche ones), it will download the source tarball but won't compile C extensions. Packages like `numpy`, `flask`, `requests` ship wheels and work fine.
- **Loose wheel selection.** Platform tag matching uses substring search — it may pick a wheel for the wrong Python minor version on edge cases.
- **`requires_python` not checked.** A package requiring Python 3.11+ will be selected even if KONG manages Python 3.10.
- **Version ranges not resolved.** `>=1.0` or `~=2.3` in `requirements.txt` are skipped — only exact pins (`==`) and lockfile versions are handled. Use a lockfile for reliable results.

### Node.js
- ~~**Bin scripts not linked.**~~ Fixed — `node_modules/.bin/` is now populated with symlinks (macOS/Linux) or `.cmd` wrappers (Windows) for all packages declaring `"bin"` in their `package.json`.
- **No peer/optional dependency handling.** Peer deps are not resolved or validated.
- **pnpm-lock.yaml not fully supported.** Declared in docs but the parser is incomplete — `package-lock.json` is the reliable path.

### Rust
- **Cargo features and patches ignored.** Source replacement works for vanilla `Cargo.lock` deps, but `[features]` selections and `[patch]` overrides in `Cargo.toml` are not reflected.

### Homebrew
- **macOS only.** Brew bottle support currently targets macOS (arm64_sequoia, arm64_sonoma). Linux Homebrew (linuxbrew) is not yet supported.
- **`ca-certificates` missing.** The `ca-certificates` formula has no ARM64 Sequoia bottle tag upstream — skipped during dependency resolution.
- **No cask support.** Only formulas (command-line tools) are supported — GUI apps via `brew cask` are not handled.
- **No version pinning.** KONG always fetches the latest stable bottle. Version-locked Brewfiles are not respected.

### General
- **Sequential downloads.** All packages are fetched one at a time. Large projects (478 crates for gflow) take minutes.
- **No retry on network failure.** A single timeout or connection drop fails the entire `kong rules` run.
- **No proxy support.** Corporate networks behind HTTP proxies can't use KONG yet.

---

## Contributing


```
src/
├── cli.rs          # clap CLI definitions
├── config.rs       # kong.rules schema + manifest parsers
├── download.rs     # HTTP download + SHA-256 verification
├── extract.rs      # zip / tar.gz / .crate extraction
├── link.rs         # hard links, symlinks, project-dir wiring
├── runner.rs       # kong run <script> — PATH/lib injection
├── store.rs        # store layout + doctor
├── brew/           # Homebrew API client, GHCR bottle downloader, Mach-O fixup
├── python/         # PyPI client, venv builder, runtime
├── node/           # npm client, node_modules builder, runtime
└── rust_eco/       # crates.io client, source replacement, toolchain
```

---

## License

KONG is dual licensed under either of

- MIT License ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)
- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)

at your option.

---

Copyright (c) 2026 iscreamparis