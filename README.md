# KONG — Unified Dependency Manager

> One tool. Python, Node.js, and Rust. Zero duplication. Zero wrappers.

[![License: MIT OR Apache-2.0](https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-blue.svg)](https://github.com/iscreamparis/kong/blob/main/LICENSE-MIT)
[![Built with Rust](https://img.shields.io/badge/built%20with-Rust-orange.svg)](https://www.rust-lang.org/)
[![Platform: Windows + Linux](https://img.shields.io/badge/prebuilt-Windows%20%7C%20Linux-blue.svg)]()
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

Download **[Kong-windows-x64-setup.exe](https://github.com/iscreamparis/kong/releases/latest/download/Kong-windows-x64-setup.exe)** from the [Releases page](https://github.com/iscreamparis/kong/releases/latest) and run it. It drops `kong.exe` into `C:\kong\`, creates the store directory, and adds it to your system PATH.

For CI or a manual install, **[Kong-windows-x64.zip](https://github.com/iscreamparis/kong/releases/latest/download/Kong-windows-x64.zip)** contains just `kong.exe`. Versioned names (`Kong-<version>-windows-x64-setup.exe`, `Kong-<version>-windows-x64.zip`) and a `.sha256` next to each asset are on every release.

### macOS

macOS builds are **not published** (no Mac to build and sign them on). Building from source still works:

```bash
git clone https://github.com/iscreamparis/kong
cd kong
cargo build --release
cp target/release/kong ~/.local/bin/
```

The store lives at `~/Library/Application Support/kong/`. On macOS, KONG uses symlinks instead of NTFS junctions.

### Linux

Download **[Kong-linux-x64.tar.gz](https://github.com/iscreamparis/kong/releases/latest/download/Kong-linux-x64.tar.gz)** from the [Releases page](https://github.com/iscreamparis/kong/releases/latest) (a `Kong-linux-x64.tar.gz.sha256` sits next to it), extract it, and put `kong` on your PATH:

```bash
curl -fsSL -o Kong-linux-x64.tar.gz \
  https://github.com/iscreamparis/kong/releases/latest/download/Kong-linux-x64.tar.gz
tar -xzf Kong-linux-x64.tar.gz
chmod +x kong
mkdir -p ~/.local/bin && mv kong ~/.local/bin/   # ensure ~/.local/bin is on $PATH
kong --version
```

The Linux release is a **statically linked musl binary** (`file kong` shows
`statically linked`) — no glibc-version dependency, so it runs on assorted /
older Ubuntu VMs with nothing else installed. It's a **CLI-only** build: the
optional Slint GUI is disabled, so you don't need `fontconfig` / X11 / OpenGL
system libraries either.

<details>
<summary>Build from source</summary>

```bash
git clone https://github.com/iscreamparis/kong
cd kong
cargo build --release --no-default-features   # CLI only, no GUI
cp target/release/kong ~/.local/bin/
```

For a portable static build, target musl (matches the released binary):

```bash
rustup target add x86_64-unknown-linux-musl
sudo apt-get install -y musl-tools
cargo build --release --no-default-features --target x86_64-unknown-linux-musl
cp target/x86_64-unknown-linux-musl/release/kong ~/.local/bin/
```

</details>

The store lives at `~/.local/share/kong/` (XDG data dir; respects
`$XDG_DATA_HOME`). Linux uses symlinks + hard links — same as macOS, no
NTFS junctions. KONG downloads its own Python interpreter
(python-build-standalone), Node.js, and Rust toolchain, so you don't need
`python3` / `node` / `rustup` on the host — just a C toolchain (`gcc`) for any
crate that builds native code. System packages (`Brewfile`) are **not** managed
on Linux — install those with `apt` instead (see Homebrew section).

> Building **with** the GUI on Linux (`cargo build --release`, default features)
> requires the Slint system dependencies: `libfontconfig1-dev`,
> `libxcb1-dev`, `libxkbcommon-dev`, and OpenGL/Wayland dev headers. The
> CLI-only build above avoids all of them.

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
kong setup https://github.com/owner/repo -r build -r test
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

**The key insight:** KONG creates symlinks (macOS/Linux) or NTFS junctions (Windows) from your project directory into its central store. Vite, Python, Node.js, and Cargo all find their packages by walking up the filesystem — exactly as they would with a real local install. Homebrew bottles are downloaded directly from GHCR and injected into `PATH` at runtime (macOS only — on Linux, system deps come from `apt`). No wrappers. No shims.

---

## Commands

| Command | Description |
|---------|-------------|
| `kong clone <url> [dir]` | Clone a git repository into `[dir]` (defaults to repo name) |
| `kong clone <url> --setup` | Clone and automatically run `kong rules` + `kong use` |
| `kong rules` | Scan manifests, download all deps, write `kong.rules` |
| `kong rules --force` | Re-download everything even if already in store |
| `kong rules --path <dir>` | Run rules on a different project directory |
| `kong rules --python <ver>` | Pick the Python runtime (version prefix or PEP 440 specifier); default: `.python-version`, then `requires-python`, then latest |
| `kong rules --update-git` | Re-resolve git dependencies (`kong.git`) to the remote's current commit; default keeps the commits recorded in `kong.rules` |
| `kong use [kong.rules]` | Create `.venv`, `node_modules`, `.cargo` via links; re-points a stale or dangling link, never touches a real directory |
| `kong use --clean` | Tear down and rebuild the project environment |
| `kong run <script>` | Run a script from `package.json` or `pyproject.toml` |
| `kong run <script> -- <args>` | Pass extra arguments to the script |
| `kong run <script> --path <dir>` | Run a script in a different project directory |
| `kong setup <url> [dir]` | Clone + rules + use in one command |
| `kong setup <url> -r build -r test` | Also run specific scripts after setup |
| `kong service start [name]` | Start a service (or all) as a background daemon |
| `kong service stop [name]` | Stop a running service (or all) gracefully |
| `kong service status` | Show running services, ports, PIDs |
| `kong service logs <name>` | Tail stdout/stderr of a service |
| `kong delete [--path <dir>]` | Delete a project's KONG environment (RULEZ + junctions) |
| `kong import [--path <dir>]` | Import existing local envs into the global store (no re-download) |
| `kong solidify [--path <dir>]` | Copy packages from store into real local directories (standalone) |
| `kong eject [--path <dir>]` | Remove all KONG artifacts and store-only deps from a project |
| `kong gui [--path <dir>]` | Open the native GUI (Slint, Cupertino style on macOS) |
| `kong store path` | Print the global store path |
| `kong doctor` | Check store integrity and environment health |

**Global flag:** `--verbose` / `-v` — enable detailed tracing output on any command.

---

## Supported Manifests

| Ecosystem | Parsed | Lockfile preferred |
|-----------|--------|--------------------|
| Python | `requirements.txt`, `pyproject.toml` | `uv.lock`, `poetry.lock`, `Pipfile.lock` |
| Node.js | `package.json` | `package-lock.json`, `pnpm-lock.yaml` |
| Rust | `Cargo.toml` | `Cargo.lock` |
| Homebrew | `Brewfile` | — (fetches latest bottles from GHCR) |

### Local Node packages (`file:` / `link:` / workspaces)

A dependency declared as `"@scope/pkg": "file:../some/dir"` (or `link:`) is a **local package**:
KONG never asks the npm registry for it and never copies it into the store. `kong use` makes
`node_modules/<name>` a **junction (Windows) / symlink (Unix) to the directory itself** — exactly
what npm does with its default `install-links=false` — so edits to the local package are live.

- `kong rules` records it in `kong.rules` under `node.local`, with the path **as the project
  declares it** (relative to the project for `file:../x`; absolute only if `package.json` says so):
  ```json
  "node": { "packages": [ … ], "local": [ { "name": "@real3d/atoms", "version": "0.1.0", "path": "../vendor/atoms" } ] }
  ```
  `node.local` is omitted when empty, so projects without local deps get a byte-identical `kong.rules`.
- From `package-lock.json`: the `"node_modules/<name>": { "link": true, "resolved": "<dir>" }` entry
  becomes the local dep. The target's own entry (`"<dir>": { "name", "version", … }`) and anything
  else keyed outside `node_modules/` (stale `"extraneous": true` targets left by an old link, the
  target's own `node_modules`) are **not** installed — npm does not install them into the project
  either. `"resolved": "file:<dir>"` entries (`npm install --install-links`) are local too.
- **The local package's own dependencies are not installed by KONG** — npm does not install them
  either for a linked directory (verified with npm 10.9: the lockfile only records the target's
  metadata; at run time Node resolves the target's imports from the *target's own*
  `node_modules`). Build the local package in its own directory, or ship it pre-built (`dist/`)
  and let the consumer's bundler resolve shared peers (`resolve.dedupe`).
- Errors name the dependency, the declared path, the resolved path and the manifest entry that
  declared it, e.g.
  `local Node dependency '@real3d/atoms' points at a directory that does not exist: declared path : ../vendor/atoms …
  declared by : package-lock.json entry "node_modules/@real3d/atoms" (link → "../vendor/atoms")`.
- **An absolute path in `package.json` wins over the lockfile.** npm records a local dependency
  relative to the lockfile even when `package.json` declares `file:/abs/path`; used from another
  depth (a git worktree, a copy) the relative path points nowhere. When `package.json` gives an
  absolute `file:`/`link:` path, `kong rules` uses it; a relative one keeps the lockfile's path.
- `kong solidify` keeps local packages linked (a copy would freeze live source).

### Tarball dependencies (install a built release)

The better path for a module that publishes built releases: a dependency whose spec is an
**http(s) tarball URL** (typically a GitHub Release asset made by `npm pack`) is downloaded **once**
into the store, integrity-checked, extracted and linked at `node_modules/<name>`. **No clone, no build,
no project-specific code or branch**: the game imports each module already built. No `kong.git` block
is needed: the URL in the dependency value is enough.

```json
{
  "dependencies": {
    "@etcito/haa": "https://github.com/Etcito/haa/releases/download/v0.1.0/etcito-haa-0.1.0.tgz",
    "@etcito/raa": "https://github.com/Etcito/raa/releases/download/v0.3.0/etcito-raa-0.3.0.tgz#sha256=<64 hex>"
  }
}
```

- **Where declared:** `dependencies`, `devDependencies` or `optionalDependencies` of the root
  `package.json` and of each workspace member (like `kong.git`); linked at the project's top-level
  `node_modules/<name>`. Scoped names work, and so does a tarball whose package folder is `package/`
  or any other single top-level folder. The same name with two different URLs is an error.
- **Integrity.** `kong.rules` records the URL, the version (from the tarball's `package.json`) and a
  `sha512-<base64>` of the file under `node.tarballs`. A `#sha256=<hex>`, `#sha512=<hex>`,
  `#sha256-<base64>` or `#sha512-<base64>` fragment in the spec is verified, and a mismatch is a hard
  error (the message shows expected and actual). Without a declared one the first run records it
  (trust on first use) and every later run enforces it, from the store or after a new download.
- **Own dependencies.** There is no lockfile in a tarball, so its `dependencies`,
  `optionalDependencies` and required `peerDependencies` (installed automatically, as npm 7+ does;
  `peerDependenciesMeta` optional ones are not) are resolved from its `package.json` against the npm
  registry (highest satisfying version, `latest` preferred when it satisfies; a host-incompatible
  optional one is skipped) into a flat npm-style tree: one version hoisted, a conflicting one nested.
  A dependency that is itself a tarball URL is fetched the same way and joins the tree. The resolved
  packages are recorded in `kong.rules`; they come from the shared `node/libs` store.
- **Lock semantics.** A re-run with an unchanged `kong.rules` does **no network** (no download, no
  registry lookup). Change the URL (a new release) to get a new entry; to re-pin the same URL, delete
  its entry from `kong.rules`. Nothing needs `--update-git`.
- **Store layout:**
  ```
  <store>/node/tarballs/<name>/<sha512 12 hex>/      the extracted file (package/…) + .kong-fetched marker
  <store>/node/tarballs/<name>/<sha512 12 hex>/tree-<deps hash>/package/   hard-linked copy + node_modules
  ```
  `node_modules/<name>` links to `tree-*/package` (a junction or symlink: Node resolves the real path,
  where the package's own `node_modules` lives). Two projects that resolved different dependency
  versions get two `tree-*` directories and never disturb each other.
- **Private GitHub release assets.** The same credentials as `kong.git`: `KONG_GIT_TOKEN` for any
  https host, `GITHUB_TOKEN` then `GH_TOKEN` for `github.com` only. The token is sent as
  `Authorization: Bearer <token>` on the first request, to the URL's own host only; redirects (GitHub
  sends the asset to a signed URL on another host) are followed by KONG without the header, and an
  https download is never downgraded to http. The token is never sent over plain `http://`, and is
  never in a command line, a URL, `kong.rules`, a path, a marker or a log line; errors are redacted
  and signed redirect URLs are not printed. There are no prompts: without a credential a private
  asset fails at once (`HTTP 404 ... no credential was sent: set KONG_GIT_TOKEN, or GITHUB_TOKEN /
  GH_TOKEN for github.com`). A user:password@ in the URL is refused.
- **`kong solidify`** copies a tarball dependency (with its own `node_modules`) as a real directory;
  **`kong eject`** removes a tarball directory only this project uses and keeps one another project is
  locked to.
- **Limits:** a tarball's own dependencies must be registry ranges, dist-tags or tarball URLs (`npm:`
  aliases, `file:`, `workspace:` and git specs inside a tarball's `package.json` are refused with a
  message naming the package); the resolver is a small flat one, not a full SAT solver; a required
  peer is installed inside the tarball's own tree, so a peer that must be the *same instance* as the
  project's (a UI framework, say) is a duplicate: keep such a peer out of the tarball's
  `dependencies` and let the consumer bundle it; no `--update` flag yet. Use **git dependencies** (below)
  as the fallback for a dependency that only exists as source.

### Git dependencies (`kong.git`)

> Prefer a [tarball dependency](#tarball-dependencies-install-a-built-release) when the module publishes
> a built release: it needs no clone and no build. Git dependencies remain the fallback for a
> dependency that is only available as source.

A Node dependency declared as a standard npm git spec **and** named in a top-level `kong.git` block of
the same `package.json` is cloned once into the store, has its own dependencies installed from its
lockfile, optionally gets a build step, and is linked at `node_modules/<name>`.

```json
{
  "dependencies": {
    "@gamesaloon/rollback": "git+https://github.com/Etcito/gamesaloon-rollback.git#v0.4.0",
    "@etcito/raa": "github:Etcito/raa#3f2c…(40 hex)"
  },
  "kong": { "git": {
    "@gamesaloon/rollback": { "path": "packages/core" },
    "@etcito/raa": { "path": "packages/raa", "build": "scripts/build-package.sh", "rustTargets": ["wasm32-unknown-unknown"] }
  } }
}
```

- **Where declarations are read:** the root `package.json` and each workspace member's `package.json`
  (`workspaces` entries: a literal directory or `dir/*`). The same name declared twice is an error.
  Every git dependency is linked at the project's top-level `node_modules/<name>`.
- **`kong.git.<name>` keys:** `path` (subdirectory of the repository, default the root; no absolute
  path, `..` or `:`), `build` (a script path in the repository, or a shell command), `rustTargets`
  (target triples). Any other key is an error, as is an entry whose dependency is missing or not a git spec.
- **Accepted specs:** `github:ORG/REPO[#ref]`, `git+https://…`, `git+http://…`, `git+ssh://[git@]host/…`,
  `git+file:///abs/path`, `git://…`. The `#ref` is a tag, a branch or a full 40-hex commit; without it the
  remote `HEAD` is used. **Rejected:** userinfo in an http(s) URL, `#semver:…`, `::` in the fragment, an
  abbreviated 7–39-hex commit.
- **`kong.rules`** records each one under `node.git` (omitted when empty):
  ```json
  "node": { "packages": [ … ], "git": [ { "name": "@etcito/raa", "version": "0.3.0",
    "source": "github:Etcito/raa#v0.3.0", "repo": "https://github.com/Etcito/raa.git",
    "reference": "v0.3.0", "commit": "<40 hex>", "path": "packages/raa",
    "build": "scripts/build-package.sh", "rust_targets": ["wasm32-unknown-unknown"],
    "declared_by": "apps/web/package.json", "store_path": "node/git/raa-1a2b3c4d/3f2c0a9d81b7",
    "lock_dir": "packages/raa", "packages": [ … ] } ] }
  ```
- **Store layout** (one commit dir per repository and commit, shared by every project):
  ```
  <store>/node/git/<repo_key>/<commit12>/
    src/                      the checkout (tracked files never modified)
    src/<lock_dir>/node_modules/   the dependency's own tree (hard links into node/libs)
    .kong-cloned  .kong-installed  .kong-built-<build_key>   markers (JSON, with timings)
    build-<build_key>.log     combined stdout+stderr of the build
  <store>/node/git/.cargo-home/    CARGO_HOME shared by every build
  ```
  `repo_key` is the repository name plus 8 hex of the hash of its URL. The tree lives under the checkout
  because Node and Vite resolve symlinks to their real path and walk up from there.
- **Credentials.** KONG runs the `git` CLI, so your git credential helper and ssh agent work as-is.
  Prompts are disabled (`GIT_TERMINAL_PROMPT=0`, `GCM_INTERACTIVE=never`): a missing credential fails
  instead of hanging. `KONG_GIT_TOKEN` applies to every `https://` host; `GITHUB_TOKEN`, then `GH_TOKEN`,
  apply to `github.com` only. The token is passed to the git child only through `GIT_CONFIG_*`
  environment variables; it is never on a command line, in a URL, in `kong.rules`, in a path, in a marker
  or in a log line (git stderr printed by KONG is redacted).
- **Build step** (when `build` is set). Runs once per commit and build string, after the install, with
  the repository root as working directory: `bash <build>` if it is a file in the repository, else
  `bash -c "<build>"`. On Windows it uses the bash of Git for Windows (never a `bash.exe` from `PATH`,
  which may be WSL's). `PATH` gets, in front: the Kong-managed Rust toolchain `bin`, the shared
  `CARGO_HOME/bin`, the Kong Node runtime, and `<lock_dir>/node_modules/.bin`. `CARGO_HOME` and `RUSTC`
  are set, as are `KONG_GIT_DEP`, `KONG_GIT_COMMIT` and `KONG_GIT_CHECKOUT`. Rust targets (`rustTargets`
  plus `[toolchain].targets` of `rust-toolchain.toml`) are added to the managed toolchain first. Output
  goes to `build-<build_key>.log`; a lock file serialises concurrent builds. A failure names the
  dependency, spec, commit, origin and log, and shows the last 20 log lines.
- **Pinning and updating.** `kong rules` keeps the commit recorded in `kong.rules` (same name, repo,
  reference and path): no `ls-remote`, no clone, no build on a re-run, even if the tag or branch moved.
  `kong rules --update-git` re-resolves tags, branches and `HEAD` to the remote's current commit.
  A full 40-hex commit in the spec never touches the remote for resolution.
- **`kong solidify`** replaces the link with a real directory: the dependency's `path` from the checkout
  (without `.git` and `node_modules`, with build outputs), its own registry packages in
  `node_modules/`, and its own workspace links. **`kong eject`** removes a commit dir that only this
  project uses, and keeps one shared with other projects.
- **Limits:** the dependency's own dependencies come from its `package-lock.json` only; no `#semver:`;
  full 40-hex commits only (no abbreviations); the repository is never modified.

### Rust: warm registry and build cache

For a Rust project with a committed `Cargo.lock`, `kong use` makes Cargo build offline with no `cargo fetch`:

```bash
kong rules && kong use
eval "$(kong env)"                          # PowerShell: kong env --shell ps1 | Invoke-Expression
cargo build --offline                       # and cargo test --offline
```

- **Variables.** `kong env` prints the exports; the generated `cargo` wrapper in `.rust-toolchain/bin` sets the same ones.
  `CARGO_HOME` is per environment; its `config.toml` replaces crates-io with the vendored directory and sets
  `net.offline = true`. `CARGO_TARGET_DIR` is the shared build cache, `CARGO_NET_OFFLINE=true`,
  `RUSTC_WORKSPACE_WRAPPER` is the per-env wrapper, and `PATH` gets the toolchain `bin` first. Nothing is
  written into the repository: no `.cargo/config.toml` and no `target/` in the project.
- **Layout.**
  - `<cache>/cargo-vendor/<key>/`: directory registry, hard links from `store/rust/crates`, checksums taken from `Cargo.lock`.
  - `<cache>/cargo-target/<key>/`: shared target dir, keyed by workspace members + `Cargo.lock` hash + toolchain + host.
    Feature sets coexist through Cargo's own metadata hash.
  - `<RULEZ>/<project>/cargo/{home,bin,incremental,env.sh,env.ps1}`: the per-env part.
- **Sharing.** Dependency artifacts are reused by every worktree with the same lockfile. The project's own
  crates and the incremental dirs stay per worktree (the wrapper path is per env).
- **Sandbox / read-only store.** The store may be read-only. Only two places must be writable: the RULEZ env
  dir and the cache root. The cache root is `KONG_CACHE`, else `<install root>/cache`, else
  `<tmp>/kong-cache`, chosen automatically with a warning. Without network, `kong rules` reuses the newest
  Rust toolchain already in the store.
- **Limits.** crates.io dependencies only (git/path dependencies in `Cargo.lock` are not vendored).
  The wrapper's `--config` flag overrides any `~/.cargo/config.toml` or parent-directory config, so a
  sandbox config that replaces crates-io with another registry does not break offline builds.

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

## Homebrew / System Dependencies

KONG replaces the `brew` CLI entirely. Drop a `Brewfile` into your project and KONG handles the rest.

### How it works

1. **`kong rules`** detects your `Brewfile`, queries the [Homebrew Formulae API](https://formulae.brew.sh/api/) for each formula, and walks the full transitive dependency tree (BFS).
2. For each formula, KONG downloads the pre-built **bottle** directly from GitHub Container Registry (GHCR) — no `brew` CLI needed.
3. Bottles are extracted into the global store (`store/brew/<name>-<version>/`), Mach-O binaries are patched to fix `@@HOMEBREW_PREFIX@@` placeholders, and re-codesigned for macOS Sequoia.
4. **`kong run`** automatically injects brew `bin/` directories into `PATH` and `lib/` directories into `DYLD_LIBRARY_PATH` / `PKG_CONFIG_PATH` so tools like `psql`, `redis-server`, and `jq` just work.

### Example Brewfile

```ruby
brew "jq"
brew "postgresql@17"
brew "redis"
```

### Usage

```bash
kong rules              # detects Brewfile, downloads jq + postgres + redis + all transitive deps
kong use kong.rules     # links environments (brew bottles are available via kong run)
kong run health         # runs: curl ... | jq . — jq is found via KONG's PATH injection

# Services (postgres, redis) are auto-inferred from brew packages
kong service start      # starts postgres + redis as background daemons
kong service status     # shows PIDs, ports, status
kong service logs redis # tail redis logs
kong service stop       # stops all services
```

### What KONG manages from Homebrew

| What | Example | How |
|------|---------|-----|
| CLI tools | `jq`, `curl`, `git` | Bottle downloaded, `bin/` injected into PATH |
| Databases | `postgresql@17`, `redis` | Bottle + runtime data dirs in RULEZ, managed as services |
| Shared libraries | `openssl`, `libpq`, `ca-certificates` | Transitive deps resolved automatically, `lib/` injected |
| Build tools | `cmake`, `pkg-config` | Available during `kong run` via PATH injection |

> **No Homebrew installation required.** KONG talks to GHCR directly to download bottles. It supports `arm64_sequoia`, `arm64_sonoma`, and platform-independent (`all`) bottle tags. Intel Macs and Linux Homebrew are planned.

---

## Migration Commands

KONG provides three commands to move projects in and out of KONG management:

### `kong import` — adopt an existing project

```bash
cd my-project          # has a real .venv/ and node_modules/ already
kong import            # moves packages to the global store, replaces with symlinks
```

What happens:
1. **Adopts** the already-installed environment: finds the project's virtualenv
   (any dir with a `pyvenv.cfg` — `.venv`, `venv`, `env`, …) and `node_modules`,
   and copies each installed distribution into the store **byte-for-byte**,
   including native extensions (`.so` / `.pyd` / `.node`). `kong.rules` is built
   from the installed set — packages are **never re-resolved or re-downloaded**.
   (Only an ecosystem with *no* installed env falls back to the resolver so a
   genuinely-missing dep can still be fetched.)
2. Removes the local directories
3. Recreates them as symlinks into the store (like `kong use`)

Your project keeps working exactly as before — but now shares packages with all your other projects.

### `kong solidify` — make a project standalone

```bash
kong solidify          # copies packages from store into real local .venv / node_modules
```

After solidify, the project works without KONG. Useful for deploying to a machine without KONG, or for handing off to someone who doesn't use it.

### `kong eject` — fully remove KONG from a project

```bash
kong eject             # removes everything KONG-related
```

What gets removed:
- `.venv` and `node_modules` symlinks
- RULEZ directory for this project
- `kong.rules` file
- Store entries used **only** by this project (shared packages are kept)
- Project entry from KONG's registry

After eject, the project has no trace of KONG. Run `pip install` / `npm install` normally to restore dependencies.

---

## Real Project Example — DummyKong

[DummyKong](https://github.com/iscreamparis/DummyKong) is KONG's reference test project: a Flask backend + Vite/Vue frontend + Rust fractal renderer, all managed by KONG.

```bash
# Clone, install everything, and run smoke tests
kong setup https://github.com/iscreamparis/DummyKong -r fractal -r build
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
- [x] **Proper wheel selection** — full PEP 427 filename parsing; pick the wheel matching the target interpreter's CPython/ABI tag
- [x] **Check `requires_python`** — honored for releases and files, direct and transitive (0.8.7)
- [ ] **Transitive dependency cycle detection** — prevent `kong rules` from hanging on circular deps
- [ ] **Download retry** — retry failed downloads before giving up

### v0.3 — Homebrew / system dependencies ✅
- [x] **`Brewfile` support** — `kong rules` detects `Brewfile`, resolves formulas + transitive deps via Homebrew API
- [x] **Direct GHCR bottle downloads** — downloads pre-built bottles from GitHub Container Registry, no `brew` CLI needed
- [x] **Mach-O fixup** — rewrites `@@HOMEBREW_PREFIX@@` placeholders in binaries + codesigns for macOS Sequoia
- [x] **BFS transitive deps** — walks the full dependency tree so `psql`, `redis-server`, `jq` all get their shared libs
- [x] **Runtime PATH/lib injection** — `kong run` injects brew `bin/` and `lib/` into the script environment automatically

### v0.4 — End-to-end workflow ✅
- [x] **`kong setup <url>`** — one command to clone, generate rules, set up environments, and run scripts
- [x] **Lazy cargo build** — `kong run` auto-builds Rust binaries when the target is missing
- [ ] **`kong setup` parallel script execution** — run independent scripts concurrently

### v0.5 — Service management ✅
- [x] **`kong service start <name>`** — start a service (postgres, redis) as a background daemon with pid tracking
- [x] **`kong service stop <name>`** — stop a running service gracefully (SIGTERM on unix)
- [x] **`kong service status`** — show running services, ports, pids
- [x] **`kong service logs <name>`** — tail stdout/stderr of a running service
- [x] **`kong.rules` services section** — declare services alongside scripts; auto-inferred from brew packages
- [ ] **Auto-start on `kong run`** — scripts can declare service dependencies; KONG starts them before running the script

### v0.6 — GUI (Slint) ✅
- [x] **`kong gui`** — native desktop window using [Slint](https://slint.dev/) with Cupertino style (auto dark/light following system)
- [x] **Projects tab** — list all KONG-managed projects, open terminal, delete environment
- [x] **Services tab** — start/stop services, see ports/PIDs/status, view live logs
- [x] **Store tab** — browse global store by ecosystem with disk usage per package
- [x] **Packages tab** — per-project dependency view showing which deps are shared across projects
- [x] **Doctor tab** — system health checks with diagnostic output
- [x] **`kong delete`** — CLI command to remove a project's KONG environment (RULEZ + junctions)

### v0.7 — Migration ✅
- [x] **`kong import`** — convert an existing project (with local `.venv`, `node_modules`) into KONG. Moves installed packages into the global store instead of re-downloading, then replaces local copies with symlinks.
- [x] **`kong solidify`** — convert a KONG-managed project back to standalone. Copies packages from the store into real local directories so the project works without KONG.
- [x] **`kong eject`** — full removal: deletes symlinks, RULEZ environment, `kong.rules`, and any store entries used exclusively by this project. The project is left clean with no trace of KONG.
- [x] **`kong super` → `kong setup`** — renamed for clarity.
- [x] **Project registry** — KONG tracks managed projects in `projects.json` instead of guessing paths. GUI and eject use the registry for reliable project discovery.
- [x] **Single-instance GUI** — `flock()` prevents multiple GUI windows from opening simultaneously.
- [x] **`ca-certificates` bottle fix** — platform-independent (`all`) bottles now resolved correctly.

### v0.8 — Linux GA + migration hardening ← **current (0.8.7)**
- [x] **Linux and Windows releases** — static musl Linux tarball, Windows installer and `kong.exe` zip, all built + attached by GitHub Actions on every `vX.Y.Z` tag (no macOS build is published)
- [x] **Wheel selection by target CPython/ABI tag** — picks the exact `cpXY` / `abi3` / `none` wheel for the managed interpreter and rejects a wrong CPython minor (e.g. won't pick a `cp313t` wheel for a `cp310` runtime)
- [x] **`kong import` copy-adopts installed packages** — copies the existing `.venv` / `node_modules` into the store byte-for-byte (native extensions included), with no re-download or re-resolve
- [x] **Robust import copy** — handles venv internals such as directory symlinks (`lib64 -> lib`) and dangling links
- [x] **Directory-mode normalisation on extraction** (0.8.5) — `tar.gz` extraction now creates directories with a traversable mode (`0o755`) instead of preserving the tarball's recorded mode. Some npm tarballs ship directory entries as `0o666` (no execute/traverse bit — e.g. `pngjs` 5.0.0), which made the store dir non-traversable and caused a later `hard_link` into it to fail with `Permission denied (EACCES)`. npm / pnpm / yarn normalise dir modes for the same reason; kong now does too. File modes (executable bits) are preserved.

- [x] **Python markers, extras, `Requires-Python`** (0.8.7) — PEP 508 markers evaluated against the managed interpreter, requested extras installed, incompatible releases skipped; runtime picked from `--python` / `.python-version` / `requires-python`; `py3-none-<platform>` wheels accepted
- [x] **Nested npm installs** (0.8.7) — a second version npm keeps under another package is placed at its lockfile path; the top-level version is always the lockfile's
- [x] **Stale links and local deps** (0.8.7) — `kong use` re-points dangling `node_modules` / `.venv` links instead of failing with `os error 17`; an absolute `file:`/`link:` path in `package.json` wins over the lockfile's relative one; glibc beats an extra musl loader

### v0.9 — Performance + broader compatibility
- [ ] **Parallel downloads** — all packages fetched concurrently (currently sequential)
- [ ] **Progress bars** — `indicatif` integration for long downloads
- [ ] **Resume on failure** — partial downloads restart from where they stopped
- [ ] **Python resolver** — resolve `>=` version constraints without a lockfile
- [ ] **`kong shell`** — drop into an activated shell for a project
- [ ] **`kong add <pkg>`** — add a package and update `kong.rules` in one step
- [ ] **`kong store move <path>`** — move the global store to another disk
- [ ] **`kong store add <path>`** — add a secondary store on another disk

### v0.10 — Unified system package registry
- [ ] **Cross-platform package resolution** — single `[tools]` / `[services]` section in `kong.rules` resolves to the best source per OS: Homebrew bottles (macOS), Scoop portable apps (Windows), winget (Windows fallback), Chocolatey (Windows last resort)
- [ ] **Scoop bucket parser** — read Scoop JSON manifests (url, hash, bin) for portable Windows packages
- [ ] **winget manifest parser** — read winget YAML manifests from `microsoft/winget-pkgs` for Windows installers
- [ ] **Aggregation server** — lightweight API that indexes Homebrew, Scoop, winget, and Chocolatey into a unified `GET /resolve?name=postgresql&os=windows` endpoint with download URL, SHA-256, and bin path
- [ ] **Offline fallback** — Kong can resolve locally from cached registry snapshots when the server is unreachable
- [ ] **Source priority** — Scoop (portable) > winget (official) > Chocolatey (community); Homebrew for macOS/Linux

### v0.11 — Git integration (lite)
- [x] **`kong clone <url>`** — clone a repo, then `kong rules` + `kong use` separately (or `--setup` for all-in-one)
- [ ] **`kong login`** — authenticate with GitHub/GitLab for private repos
- [ ] Bundles a minimal `git` client (clone, fetch, pull) via the `gitoxide` / `gix` Rust crate — no system git required

### v1.0 — Production
- [x] **macOS support** — symlinks, platform-specific selection, GHCR bottles
- [x] **Linux support** — CLI-only build (Slint GUI gated behind an optional `gui` Cargo feature so no fontconfig/X11/OpenGL needed), XDG store path (`~/.local/share/kong`), symlinks + hard links, manylinux wheel selection, linux Node/Rust/Python runtime downloads, exec-bit `.bin/` shims; `Brewfile` warns-and-skips (apt manages system deps). Smoke-tested end-to-end on Ubuntu/WSL against real Python + Node manifests. **Prebuilt releases** — `.github/workflows/release.yml` builds on every `vX.Y.Z` tag a static musl Linux binary (`Kong-<version>-linux-x64.tar.gz`), the Windows installer (`Kong-<version>-windows-x64-setup.exe`) and a `kong.exe` zip (`Kong-<version>-windows-x64.zip`); macOS is not published.
- [ ] Private registry support (PyPI, npm, crates.io)
- [ ] Windows installer with proper PATH management (NSIS → WiX)
- [ ] `kong doctor` full report with auto-fix suggestions

---

## Design Principles

- **No external package managers.** KONG never calls `pip`, `npm`, `yarn`, `pnpm`, `cargo install`, or `brew` as subprocesses. All registry communication is in-house via `reqwest`.
- **Idempotent.** Every command is safe to re-run. Already in store? Skip the download. Link already correct? Keep it; stale or dangling? Re-point it.
- **Transparent.** Your project directory looks exactly like a normal project to every tool. KONG is invisible at runtime.
- **Cross-platform.** Windows (NTFS junctions + hard links), macOS, and Linux (symlinks + hard links). The native GUI is desktop-only; Linux ships as a CLI-only build. Prebuilt releases: Linux and Windows.

---

## Known Limitations

KONG is early-stage software. Here's what doesn't work yet — no surprises.

### Python
- **Target markers and extras:** requirements.txt / PEP 621 dependencies and transitive `Requires-Dist` markers are evaluated against the managed interpreter, not a Python on PATH. Boolean expressions, version comparisons and requested extras are honored, including extras discovered after the base package was expanded. Invalid markers fail with the requirement name. Downloads use the selected release's metadata. This does not add SAT/backtracking resolution or platform selection for universal lockfile graphs.
- **No sdist compilation.** KONG downloads pre-built wheels only. If a package has no wheel for your platform (rare for popular packages, common for niche ones), it will download the source tarball but won't compile C extensions. Packages like `numpy`, `flask`, `requests` ship wheels and work fine. Release selection prefers the newest version that publishes a compatible wheel; `py3-none-<platform>` wheels are accepted for a matching host.
- **Python runtime selection.** The interpreter comes from `--python`, else `.python-version`, else `requires-python` in `pyproject.toml` (newest available runtime that satisfies it), else the latest.
- **Requires-Python:** PyPI JSON release/file constraints are checked against the full managed interpreter version, including exact pins, transitives, sdist fallback and warm-store reuse. Incompatible releases are skipped; impossible bounds fail instead of falling back to global latest. Kong currently uses the PyPI JSON API, not the PEP 503 HTML simple index.
- **Version ranges without a lockfile.** Exact pins (`==`) and lockfile versions are the reliable path; ranges are resolved to the newest compatible release (wheel-aware, `Requires-Python`-aware) but there is no SAT/backtracking resolver, so conflicting ranges are not reconciled. Use a lockfile for reproducible results.

### Node.js
- ~~**Bin scripts not linked.**~~ Fixed — `node_modules/.bin/` is now populated with symlinks (macOS/Linux) or `.cmd` wrappers (Windows) for all packages declaring `"bin"` in their `package.json`.
- **No peer/optional dependency handling.** Peer deps are not resolved or validated.
- **pnpm-lock.yaml not fully supported.** Declared in docs but the parser is incomplete — `package-lock.json` is the reliable path. (Local `link:`/`file:` entries of a pnpm lockfile are therefore not handled either.)
- **Nested registry installs (`package-lock.json` v1/v2/v3).** A second version npm keeps under another package (`node_modules/path-scurry/node_modules/lru-cache`) is recorded in `kong.rules` with its lockfile `path` and placed at that path by `kong use`; the hoisted `node_modules/<name>` is always the lockfile's top-level version, and a wrong top-level left by an older KONG is relinked. Not covered: a nested install without a lockfile (`package.json` only), pnpm virtual-store keys and workspace-member nested installs (still flat, by name).
- **Stale links.** A `node_modules` / `.venv` link left by another machine (committed to the repo, dangling here) is re-pointed by `kong use`; a real directory at that path is never replaced.
- **Local packages: directories only, top level only.** `file:`/`link:` directories are linked (see *Local Node packages*). A local **tarball** (`file:../x-1.0.0.tgz`) fails with a clear error; a local link nested under another package (`node_modules/a/node_modules/b` → dir) is skipped with a warning (local links are only placed at the top level). The local package's own dependencies are never installed by KONG — same as npm.
- **Git dependencies (`kong.git`).** Only a spec named in a `kong.git` block is handled by KONG (other git specs keep their old handling). The dependency's own dependencies come from **its `package-lock.json` only** (no lockfile and declared dependencies is an error). `#semver:` ranges are rejected, abbreviated commits (7–39 hex) are rejected (use the full 40-hex commit, a tag or a branch), and userinfo in an `http(s)` URL is rejected. The cloned repository is never modified (a build that changes tracked files only produces a warning). See *Git dependencies*.

### Rust
- **Cargo features and patches ignored.** Source replacement works for vanilla `Cargo.lock` deps, but `[features]` selections and `[patch]` overrides in `Cargo.toml` are not reflected.

### Homebrew
- **macOS only.** Brew bottle support currently targets macOS (arm64_sequoia, arm64_sonoma). On **Linux**, a `Brewfile` is detected but **warns-and-skips** — KONG does not manage system packages on Linux yet (install them with `apt`). Linux Homebrew (linuxbrew) is not yet supported.
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