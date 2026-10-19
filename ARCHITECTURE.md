I've checked the contract against the final code. Five things differ from the contract text, and the document below follows the code:

- **M5 was missing from the contract.** It added `rust_eco/cargo_env.rs`, `rust_eco/target_cache.rs`, changes to `rust_eco/source.rs`, `rust_eco/parser.rs` and `link.rs`, a `kong env` command, `proof_m5.rs` and `scripts/proof-m5.sh`.
- **`Cargo.lock` is now committed**, and the proof scripts run `eval "$(kong env)"`. The old "warm the cargo registry with `cargo fetch`" rule is no longer used.
- **`.nmc/commands.json` uses `--out docs/measurements/royal-fiesta-git-deps`.**
- **There is no `transcript-vm.txt` file.** `vm-run.txt` holds the instructions for the supervisor's VM run, which has not been delivered.
- **`README.md` line 352 is out of date.** It still describes `<RULEZ>/<project>/cargo/{home,bin,incremental,env.sh,env.ps1}`, but the code uses `.rust-toolchain/{cargo-home,bin,incremental,activate.sh,activate.ps1}`. I haven't changed it.

# kong — Architecture

kong is a dependency manager for Python, Node and Rust, in a single Rust binary crate. This document covers three things:

- **Node git dependencies** (`kong.git`): they are pinned to a commit, cloned once into the store, installed and built once per commit, and linked into `node_modules`.
- **The Royal Fiesta migration**, the first real project to use them.
- **The Rust Cargo environment**: an offline vendored registry and a build cache shared by worktrees.

## Conventions

### Language, layout, build
- Rust 2021, in the existing single binary crate `kong` (`src/main.rs`, no lib target). The plan adds no new crates. The code uses crates that were already dependencies (sha2, hex, serde_json, toml, tempfile, walkdir, junction). base64 is a 15-line local helper.
- Node git-dependency code lives in `src/node/git/` (`pub mod git;` in `src/node/mod.rs`). The build step and the credentials live in the top-level module `src/gitdep/` (`mod gitdep;` in `src/main.rs`). `node::git` reaches `gitdep` only through hooks (`node::git::install_hooks`). The Cargo environment lives in `src/rust_eco/`.
- If a file appears in no module, it is existing code outside this plan. A task changes such a file only to fix a small defect that the milestone lead authorizes, and the commit message names it.
- `Cargo.lock` is committed.
- The check command is `cargo check`, which builds with the default features (GUI included). Tests and proofs run headless with `cargo test --offline --no-default-features`.
- On WSL, follow CLAUDE.md: use `bash -c` (never `bash -lc`) and a clean Linux PATH that starts with `/root/.cargo/bin`. `#[cfg(unix)]` assertions only run on Linux.

### Style
- **Match the surrounding code.**
  - Errors: `anyhow::Result`, `bail!` and `with_context`.
  - Logging: `tracing::{info, debug, warn}` with structured fields (`pkg = %name`).
  - `///` doc comments explain why.
  - Error messages are multi-line with aligned labels (§ 6).
- **Paths.** Paths recorded in `kong.rules` use `/` separators and are relative, to the store root or to the checkout. On Windows, directory links are junctions (`link::link_dir`) and file links are hard links (`link::link_file`).
- **Determinism and backward compatibility.**
  - Lists written to `kong.rules` are sorted by `name`.
  - A project with no `kong.git` block and no `Cargo.lock` produces the same `kong.rules` as before, byte for byte, apart from the `generated` timestamp.
  - New fields always use `#[serde(default, skip_serializing_if = ...)]`. `deny_unknown_fields` is never added.
- **Timing.** Timing is reported honestly. Clone, install and build durations are measured with `std::time::Instant`, logged at info level as `"<what> <name> <commit12> in <s:.1> s"` and stored in the markers. There is no timing threshold anywhere.
- **Secrets.**
  - A secret never appears in `kong.rules`, in store paths, in markers, in build logs that kong writes, or in log lines.
  - A token only ever travels in a child process's environment variables (§ 5).
  - Any git stderr that kong prints goes through `fetch::redact`.
- **Never push.** Work ends as local commits; Claude does the pushing.

### Tests
- Unit tests stay inline in the file they test (`#[cfg(test)] mod tests`). Fixture files are created at runtime with `tempfile::TempDir`.
- **No network in any test.** That means no npm registry, nodejs.org, static.rust-lang.org or GitHub.
  - Git remotes are local bare repositories reached through `git+file://` URLs.
  - Any registry package a fixture needs is pre-seeded into a temporary store (`proof_support::seed_store_package`), so `config::node_package_entry` finds it and downloads nothing.
  - Cargo children run with a cleared environment, and their proxies point at a closed loopback port.
- **No shared process state.** Tests always pass `store_root` (and the cache root) explicitly and never set process environment variables, because tests run in parallel. Hooks and credentials are passed as values (`PrepareOptions`, `auth::git_env_from`).
- Tests need the `git` CLI, and the build tests also need `bash`. Fixture git commands run with `-c user.name=kong-test -c user.email=test@kong.invalid -c init.defaultBranch=main -c core.autocrlf=false`.

### Proofs
- **Invocation.** A milestone's proof is `bash scripts/proof-mN.sh [--only <pattern>] [--full]`, run from the repository root. Each script belongs to its own milestone's module; there is no shared dispatcher.
- **`--only` and `--full`.**
  - `--only P` runs only the checks whose label contains P. In cargo-test proofs, the script exports `KONG_PROOF_ONLY=P`, and `proof_support::selected(label)` skips the other checks.
  - `--quick` is the default.
  - `--full` also runs the milestone's whole set of unit-test modules.
- **Output.** Every check prints one line, `<label>: measured=<value> threshold=<rule> -> PASS|FAIL`. The script exits non-zero on the first failure in the selected set.
- **Cargo-test proofs.**
  - The script runs `cargo test --offline --no-default-features --quiet <proof_mN> -- --nocapture --test-threads=${NMC_CPUS:-1}`, so it never uses more threads than `NMC_CPUS` (1 when unset).
  - Before that, it loads Kong's Cargo environment when one exists: `if command -v kong … && [ -f kong.rules ] && kong env …; then eval "$(kong env)"; fi`. The registry and the build cache come from Kong, never from `cargo fetch`.
  - Long runs, such as a cold one-core test build, run in the background with a log file and a done-marker file that holds the exit code, and are polled through that marker (never with pgrep).
- **Budget.**
  - Once the test binary is compiled, a milestone proof runs in at most 3 minutes. The proof's `timeout_s` leaves headroom for a cold compile.
  - Fixture repositories are created once per test, not once per check.
  - A proof has at most 5 checks.
  - Clone and build seconds are printed as values only.
- **Closeout.** A milestone closeout runs only its own checks, its own proof and the project smoke test. Earlier proofs are never re-run.
- **Long verifications.** A verification that can take more than 5 minutes (the M4 real build) is a programmatic card fed by `.nmc/commands.json`. A model task never waits on it.

### Quality bar
Deliverable: an extension of a CLI dependency manager (a new kind of Node dependency source and a warm Cargo environment), plus one real consumer migrated to the new source.

1. **Backward compatible.** A project with no `kong.git` block gets the same `kong.rules` as before, byte for byte except `generated`, and the same `kong use` result. A git spec that no `kong.git` entry names keeps its old handling. A Rust project with no `Cargo.lock` gets the old `kong.rules` and no `.rust-toolchain/`. Checked by: M2 `backcompat`, M5 `backcompat` (in `proof_m5`), and the smoke test.
2. **Lockfile semantics.** Re-running `kong rules` gives the same commit, even after the remote tag or branch has moved, and makes no `ls-remote` call. `--update-git` moves to the remote's current commit. Checked by: M2 `commit_pinned`, M3 `rerun_cached_and_update`.
3. **Clone once, install once, build once per commit.** A second run reports 0 clones, 0 builds and 0 remote resolutions. Checked by: M3 `rerun_cached_and_update`.
4. **The repository is never modified.** After clone, install and build, `git status --porcelain --untracked-files=no` in the store checkout is empty; only untracked outputs exist. Checked by: M2 `link_in_place`, M3 `build_once`.
5. **The dependency's own dependencies resolve.** Resolving from the real path of `node_modules/<name>` finds every top-level package in the dependency's lockfile, and registry files are hard links into `node/libs`. Checked by: M2 `own_deps` and `runs`.
6. **Errors can be diagnosed.** Every git-dependency error names the dependency, the declared source, the resolved commit (or `(unresolved)`) and the manifest entry that declares it. A build failure also names the log file and shows its last 20 lines. Checked by: M3 `build_failure_error`. Review: the owner reads a printed error and answers yes or no to "could I fix this without reading the code?"
7. **No secret is written.** A sentinel token passed through the credential path appears in no file under the project or the store commit dir, and in no kong log line. Checked by: M3 `no_secret_written`.
8. **solidify and eject keep working.** A solidified project has a real directory at `node_modules/<name>` holding the build outputs and the dependency's own `node_modules`. Eject removes a commit dir that only that project uses. Checked by: M3 `solidify_eject`.
9. **Real case.** In an empty directory, a fresh clone of Royal Fiesta runs `kong rules && kong use && npm run web:build`. Each command exits 0, `apps/web/dist/index.html` exists, and the evidence records clone and build seconds. Checked by: the M4 proof on the committed evidence. Review: "is this a genuine fresh-clone build with honest timings?"
10. **Windows parity.** The same code paths are implemented under `cfg(windows)`: junction links, Git for Windows bash, the `;` PATH join, `core.autocrlf=false`, and `cargo.cmd` with `activate.ps1`. Review: the owner runs `cargo test --no-default-features proof_m3` on Windows once.
11. **Warm Cargo.**
    - A fresh clone at HEAD runs `kong rules && kong use` against a read-only store and with a hostile `~/.cargo/config.toml`.
    - With `.rust-toolchain/activate.sh` sourced, plain `cargo check` and `cargo test node::` then pass offline with nothing fetched.
    - A second worktree recompiles 0 dependency crates.

    Checked by: the M5 proof.
12. **Documented.**
    - `README.md` has a "Git dependencies" section and a "Rust: warm registry and build cache" section.
    - `CHANGELOG.md` `[Unreleased]` describes both.

    Review item.

## Shared contract

### 1. Declaration (in a package.json)
**Example.** The dependency value is a standard npm git spec. A top-level `kong.git` block in the same package.json makes it a kong git dependency:
```json
{
  "dependencies": {
    "@gamesaloon/rollback": "git+https://github.com/Etcito/gamesaloon-rollback.git#v0.4.0",
    "@etcito/raa": "github:Etcito/raa#3f2c...40hex"
  },
  "kong": { "git": {
    "@gamesaloon/rollback": { "path": "packages/core" },
    "@etcito/raa": { "path": "packages/raa", "build": "scripts/build-package.sh", "rustTargets": ["wasm32-unknown-unknown"] }
  } }
}
```
**Accepted specs and the canonical `repo` URL.**
- Accepted specs: `github:ORG/REPO[#ref]`, `git+https://…`, `git+http://…`, `git+ssh://[git@]host/…`, `git+file:///abs/path` and `git://…`.
- The canonical `repo` URL is built as follows:
  - Drop the `git+` prefix.
  - `github:ORG/REPO` becomes `https://github.com/ORG/REPO.git`.
  - Remove a trailing `/`.
  - For every scheme except `file`, add a `.git` suffix when it is missing.

**Rejected specs.** Each gets a clear error that names the dependency and the manifest:
- Userinfo in an http(s) URL. Credentials belong in the environment or the credential helper.
- `#semver:…`.
- `::` in the fragment.
- An abbreviated 7–39-hex commit. The error says "use the full 40-hex commit or a tag/branch".

**Reference.** The `#` fragment gives the reference:
- Absent or empty: `Default` (the remote HEAD).
- Exactly 40 lowercase hex characters: `Commit`.
- Anything else: `Name` (a tag or a branch).

**`kong.git.<name>` keys.**
- `path`: optional string, default `""` (the repository root). It is normalised to `/` separators, with a leading `./` and a trailing `/` removed. Absolute paths, `..` and `:` are rejected.
- `build`: optional non-empty string.
- `rustTargets`: optional array of target triples.
- Any other key is an error.

**Validation.**
- A `kong.git` entry is an error that names both sides if its name is not in `dependencies`, `devDependencies` or `optionalDependencies`, or if its value there is not a git spec.
- A git spec that no `kong.git` entry names keeps exactly the old handling.

**Where declarations are read.**
- From the project's root `package.json`, plus the `package.json` of each workspace member.
- Members come from the root `workspaces` field: an array, or an object's `packages` array. Each entry is either a literal directory or `dir/*`, meaning every direct subdirectory that holds a package.json. Other glob forms are ignored with a warning.
- Declaring the same name twice is an error that names both manifests.
- Every git dependency is linked at the project's top-level `node_modules/<name>`, hoisted the way npm hoists workspace dependencies.

**`origin` and `declared_by`.** `origin` is quoted in errors, in the form `<manifest> <section> "<name>": "<spec>" (kong.git "<name>")`, where `<manifest>` is relative to the project (for example `apps/web/package.json`). `declared_by` is the `<manifest>` part alone.

**Interplay with the project's lockfile.** For each git dependency name, the parser removes:
- every lockfile-derived `NodeDep` with that `name` (any `path`, registry or local);
- every dep whose `path` starts with `node_modules/<name>/node_modules/`.

As a result, a stale `file:` link or npm git entry in the project's own lockfile is ignored.

### 2. kong.rules shape (`node.git`, omitted when empty)
```json
"node": { "packages": [...], "local": [...], "git": [ {
  "name": "@etcito/raa",
  "version": "0.3.0",
  "source": "github:Etcito/raa#v0.3.0",
  "repo": "https://github.com/Etcito/raa.git",
  "reference": "v0.3.0",
  "commit": "<40 hex>",
  "path": "packages/raa",
  "build": "scripts/build-package.sh",
  "rust_targets": ["wasm32-unknown-unknown"],
  "declared_by": "apps/web/package.json",
  "store_path": "node/git/raa-1a2b3c4d/3f2c0a9d81b7",
  "lock_dir": "packages/raa",
  "packages": [ PackageEntry… ],
  "local": [ LocalPackageEntry… ]
} ] }
```
**Fields.**
- These fields are omitted when empty or unset: `reference`, `path`, `build`, `rust_targets`, `lock_dir`, `packages`, `local`.
- `version` is read from `<checkout>/<path>/package.json` and defaults to `0.0.0`. If that file's `name` differs from the dependency name, kong warns; the package is still linked under the dependency name, as npm does.
- `packages` are the registry packages from the dependency's own lockfile. They use the same `PackageEntry` shape and `node/libs/...` store paths as the project's own packages, so the two share them.
- `local` holds the dependency's own workspace links. Their `path` is relative to `<checkout>/<lock_dir>`.

**Commit reuse rule.**
- If the previous `kong.rules` has an entry with the same `name`, `repo`, `reference` and `path`, and `--update-git` is not given, its `commit` is reused with no remote call.
- Otherwise, `Commit(c)` gives `c` with no remote call, and `Name` or `Default` runs `git ls-remote`.

**`ls-remote` matching.** For `Name(r)`, the priority is `refs/tags/r^{}`, then `refs/tags/r`, then `refs/heads/r`. `Default` matches the `HEAD` line. No match is an error.

### 3. Store layout
```
<store>/node/git/<repo_key>/<commit12>/        commit dir (one per repository and commit)
  src/                                       the checkout (keeps .git; tracked files never modified)
  src/<lock_dir>/node_modules/               the dependency's own tree, built by build_node_modules (hard links)
  .kong-cloned                               JSON {"repo","commit","seconds","cloned_at"}, written before the atomic rename
  .kong-installed                            JSON {"lock_dir","packages":n,"local":n,"seconds"}
  .kong-built-<build_key>                    JSON {"build","commit","seconds","rust":"<toolchain version>|null","finished"}
  build-<build_key>.log                      combined stdout+stderr of the build
  .kong-build-<build_key>.lock               present only while a build runs
<store>/node/git/<repo_key>/.tmp-<commit12>-<pid>-<nanos>/   clone in progress (renamed into place)
<store>/node/git/.cargo-home/                CARGO_HOME shared by every build step
```
**Keys.**
- `repo_key` is `sanitize(last path segment of repo, without .git)`, then `-`, then the first 8 hex characters of `sha256(repo)`. `sanitize` keeps `[A-Za-z0-9._-]` and maps everything else to `_`.
- `commit12` is the first 12 hex characters of the commit. `.kong-cloned` stores the full commit, and kong checks it on reuse; a mismatch is an error.
- `build_key` is the first 12 hex characters of `sha256(build string)`.

**Lock dir.**
- The lock dir is the first directory that holds a `package-lock.json`, walking up from `<checkout>/<path>` to `<checkout>` (both included).
- If none is found and that package.json declares any dependencies, it is an error: "kong installs a git dependency's own dependencies from its package-lock.json only".
- If no dependencies are declared, there is no lock dir and nothing to install.

**Why the tree lives in the store.** Node and Vite resolve symlinks to their real path. Resolving from the real `<checkout>/<path>` walks up into `<checkout>/<lock_dir>/node_modules`, so the dependency's tree has to live there and not in the project.

### 4. Build step
**When it runs.** When `build` is set and `.kong-built-<build_key>` is missing, after the install. The working directory is `<checkout>`.

**argv.**
- If `<checkout>/<build>` is a regular file, the argv is `[bash, <build>]`; otherwise it is `[bash, -c, <build>]`.
- On Unix, `bash` comes from PATH, falling back to `sh`.
- On Windows, kong uses the Git for Windows bash and never the one on PATH, which can be WSL's `System32\bash.exe`. To find it, kong:
  1. takes the parent of `git --exec-path` and walks up at most 3 directories to the first one that contains `bin/bash.exe`;
  2. otherwise tries `C:\Program Files\Git\bin\bash.exe`, then `C:\Program Files (x86)\Git\bin\bash.exe`;
  3. otherwise fails with an error.

**Environment.** The build inherits kong's environment, plus:
- PATH, with these prepended in this order: `<rust toolchain>/bin`, `<store>/node/git/.cargo-home/bin`, the Node runtime's executable dir, and `<checkout>/<lock_dir>/node_modules/.bin`. They are joined with `std::env::join_paths`.
- `CARGO_HOME=<store>/node/git/.cargo-home` and `RUSTC=<toolchain>/bin/rustc[.exe]`.
- `KONG_GIT_DEP=<name>`, `KONG_GIT_COMMIT=<commit>` and `KONG_GIT_CHECKOUT=<checkout>`.

**Rust targets.** The targets are the union of `rust_targets` and `[toolchain].targets` in `<checkout>/rust-toolchain.toml` (when that file exists). For each target, `rust-std-<target>` is merged into the Kong-managed toolchain once.

**Locking.**
- A build runs under a lock file, and the marker is re-checked once the lock is held.
- A lock older than 2 hours is removed as stale. Otherwise kong polls every second, for up to 30 minutes, for the marker to appear.

**After a successful build.** If `git status --porcelain --untracked-files=no` is not empty, kong warns and lists the modified tracked files.

### 5. Credentials
**Git's own mechanisms.** Git's credential helper and the ssh agent always work, because kong runs the git CLI. Every git call sets `GIT_TERMINAL_PROMPT=0` and `GCM_INTERACTIVE=never`, so a missing credential fails instead of hanging.

**Token from the environment.**
- If `KONG_GIT_TOKEN` is set, it applies to every https URL, through the config key `http.extraheader`.
- Otherwise `GITHUB_TOKEN`, then `GH_TOKEN`, applies to `https://github.com` only, through the key `http.https://github.com/.extraheader`.
- An empty token is ignored.

**How the token reaches git.**
- It is passed to the git child process only, as `GIT_CONFIG_COUNT=1`, `GIT_CONFIG_KEY_0=<key>` and `GIT_CONFIG_VALUE_0=AUTHORIZATION: basic <base64("x-access-token:"+token)>`.
- It never appears on argv, in a URL, in `kong.rules` or in a path.
- `GitEnv.secrets` holds both the token and the header value, so that `redact` replaces them with `***`.

### 6. Error messages (every git-dependency failure)
```
git dependency '<name>' <what failed>:
  source      : <spec as declared>
  commit      : <40 hex | (unresolved)>
  declared by : <origin | kong.rules node.git (declared in <declared_by>)>
  [log         : <abs path>]
  [<other labelled facts: ref, path, checkout>]
<one hint line>
```
Build failures append `last lines of the log:` followed by the last 20 lines of the log, redacted. If something is missing from the store at `kong use` time, the hint is "run `kong rules` to fetch and build it".

### 7. Cargo environment (Rust projects with a Cargo.lock)
**What `kong rules` records.** `rust.packages[].hash` is the `Cargo.lock` checksum. `rust.workspace` lists the lock packages that have no `source`. `rust.lock_hash` is the sha256 of `Cargo.lock` with CRLF normalised to LF. All three are omitted when empty, so a project without a `Cargo.lock` is unchanged.

**Cache root.** It is the first writable choice among `KONG_CACHE`, `<install root>/cache` and `<tmp>/kong-cache`. Kong warns for each one it skips. The store may be read-only; only the cache root and the RULEZ env have to be writable.

**Vendored registry** (`<cache>/cargo-vendor/<vendor_key>/`).
- `vendor_key` is the first 16 hex characters of the sha256 over the sorted lines `<name> <version> <hash>`.
- It holds one `<name>-<version>/` per crate, hard-linked from the store crates, plus a freshly written `.cargo-checksum.json` whose `package` is the `Cargo.lock` checksum (or `null`).
- It is filled in a temp dir and then renamed into place, so an existing directory is complete and is reused as is.
- A crate missing from the store is a § 6-style error with the hint "run `kong rules`".

**Shared target dir** (`<cache>/cargo-target/<target_key>/`).
- `target_key` is the first 16 hex characters of the sha256 over the workspace members, the lock hash, the toolchain version and the host triple. Features are not part of the key.
- A `kong.rules` written before these fields existed falls back to the project slug and `vendor_key`.

**Toolchain.**
- Kong uses the toolchain recorded in `kong.rules` when it is complete in the store.
- Otherwise it calls `runtime::ensure_runtime`. When the Rust channel is unreachable, that falls back to `newest_store_toolchain`.

**Per environment** (`<RULEZ env>/.rust-toolchain/`).
- `bin/`: the toolchain links, plus `cargo`, a wrapper that execs the store cargo with `--config <cargo-home>/config.toml` before the user's arguments. On Windows the wrapper is `cargo.cmd`, and a `cargo.exe` there is removed.
- `cargo-home/config.toml`: replaces crates-io with `kong-vendor` (the vendored registry directory), and sets `[net] offline = true` and `[build] target-dir`.
- `incremental/`: incremental artifacts, kept per worktree.
- `rustc-workspace-wrapper` (Unix only): rewrites `incremental=` to the per-worktree dir.
- `activate.sh` and `activate.ps1`.

**Variables.** The variables are, in order: `CARGO_HOME=<.rust-toolchain>/cargo-home`, `CARGO_NET_OFFLINE=true`, `CARGO_TARGET_DIR=<shared target dir>`, and on Unix `RUSTC_WORKSPACE_WRAPPER`. `bin/` is prepended to PATH.

**Why this layout.** The `--config` flag outranks every config file, so a read-only `~/.cargo/config.toml` that replaces crates-io (loaded as CARGO_HOME config or as a parent-directory config) cannot win. No `.cargo/config.toml` is ever written.

**How it reaches the project.**
- `link::create_project_junctions` mirrors the env's `.rust-toolchain/` into the project as a real directory. Its subdirectories are junctions or symlinks, and its top-level files are hard links that are relinked on every run.
- The mirror is a real directory because the git ignore pattern `/.rust-toolchain/` only matches directories.
- Cleaning removes the links, never the env.

**`kong env [--path P] [--shell sh|ps1]`.** Prints the same variables as `export K='V'` lines (or PowerShell assignments) on stdout, while logs go to stderr. A project without a Rust section gets empty output.

## Module ownership

| Module | Milestone | Files |
|---|---|---|
| git-deps-design | 1 | `docs/git-dependencies.md`, `docs/measurements/2026-10-18-royal-fiesta-fresh-clone.txt`, `scripts/proof-m1.sh` |
| node-git-spec | 2 | `src/node/git/spec.rs` |
| node-git-fetch | 2 | `src/node/git/fetch.rs` |
| node-git-install | 2 | `src/node/git/install.rs` |
| node-git-prepare | 2 | `src/node/git/mod.rs` |
| node-manifest | 2 | `src/node/parser.rs`, `src/node/mod.rs` |
| rules-config | 2 | `src/config.rs` |
| node-modules-link | 2 | `src/node/modules.rs` |
| proof-m2 | 2 | `src/node/git/proof_support.rs`, `src/node/git/proof_m2.rs`, `scripts/proof-m2.sh` |
| gitdep-build | 3 | `src/gitdep/mod.rs`, `src/gitdep/build.rs`, `src/rust_eco/runtime.rs` |
| gitdep-auth | 3 | `src/gitdep/auth.rs` |
| cli-wiring | 3 | `src/cli.rs`, `src/main.rs` |
| migrate-git | 3 | `src/migrate.rs` |
| proof-m3 | 3 | `src/gitdep/proof_m3.rs`, `scripts/proof-m3.sh` |
| docs-release | 3 | `README.md`, `CHANGELOG.md` |
| realcase-royal-fiesta | 4 | `scripts/realcase/royal-fiesta.sh`, `.nmc/commands.json`, `docs/measurements/royal-fiesta-git-deps/{transcript-wsl.txt,vm-run.txt,dist-listing.txt,kong.rules.json,timings.txt}`, `scripts/proof-m4.sh` |
| royal-fiesta-git-deps | 4 | Etcito/royal-fiesta: `package.json`, `apps/web/package.json`, `package-lock.json`, `apps/web/vite.config.ts`, `apps/web/tsconfig.json`, `scripts/fresh-clone-web-build.sh`, `.nmc/commands.json` |
| rust-cargo-env | 5 | `src/rust_eco/mod.rs`, `src/rust_eco/cargo_env.rs`, `src/rust_eco/target_cache.rs`, `src/rust_eco/source.rs`, `src/rust_eco/parser.rs`, `src/link.rs` |
| proof-m5 | 5 | `src/rust_eco/proof_m5.rs`, `scripts/proof-m5.sh` |

## Module: git-deps-design
- Files: `docs/git-dependencies.md`, `docs/measurements/2026-10-18-royal-fiesta-fresh-clone.txt`, `scripts/proof-m1.sh`
- Consumes: none

Repository: kong.

**`docs/git-dependencies.md`** is the design doc. It has exactly these H2 sections, in this order:
- `## Measurements`:
  - the reproduced failure;
  - the Royal Fiesta layout: which package.json declares the three `file:` deps, the root `workspaces`, the lockfile, `tools/bricks.ts`, and `apps/web/aliases.json` and its readers;
  - brick readiness for `Etcito/raa` and `Etcito/gamesaloon-rollback`: whether each exists on GitHub, its root/package `package-lock.json`, the package names under `packages/*`, the build script and what it needs, and `rust-toolchain.toml`.
- `## Declaration`, `## kong.rules`, `## Store layout`, `## Credentials`, `## Build step` and `## Updating`: these restate § 1–§ 6 for users.
- `## Limits`:
  - no nested git deps inside a dependency's lockfile;
  - no abbreviated commits;
  - no npm `::path:` syntax;
  - `--force` never re-clones or rebuilds (delete the commit dir instead);
  - the Vite dev server needs `fs.allow` for store paths.
- `## Open questions`: measured facts that contradict this contract.

**The transcript** follows this format: each command is a `$ <command>` line, then its output, then `exit=<code>`. It covers, in order:
- a `git clone` of Royal Fiesta into an empty temporary parent;
- `$ ls -A <parent>` and a `siblings=<count>` line;
- the clone's `git rev-parse HEAD`;
- `kong --version`;
- `kong rules`, which exits non-zero, with its error text verbatim.

It contains no credentials.

**`scripts/proof-m1.sh [--only P]`** uses only bash and grep. It has four checks:
- `doc_sections`: 9 of the 9 required headings are present.
- `failure_reproduced`: the transcript contains `points at a directory that does not exist` and `@gamesaloon/rollback`, and a `$ kong rules` block ends with a non-zero `exit=`.
- `fresh_clone`: `siblings=0`.
- `no_secret`: neither file matches `ghp_|github_pat_|gho_|x-access-token|AUTHORIZATION:`.

## Module: node-git-spec
- Files: `src/node/git/spec.rs`
- Consumes: none

Repository: kong. This module is pure parsing.
```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitRef { Default, Name(String), Commit(String) }
impl GitRef { pub fn as_option(&self) -> Option<String>; pub fn from_option(s: Option<&str>) -> GitRef }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitSource {
    pub name: String, pub spec: String, pub repo: String, pub reference: GitRef,
    pub path: String, pub build: Option<String>, pub rust_targets: Vec<String>,
    pub origin: String, pub declared_by: String,
}
pub fn is_git_spec(spec: &str) -> bool;
pub fn parse_git_spec(spec: &str) -> anyhow::Result<(String, GitRef)>;   // all § 1 rejections
pub struct KongGitEntry { pub path: String, pub build: Option<String>, pub rust_targets: Vec<String> }
pub fn parse_kong_git_entry(name: &str, value: &serde_json::Value, manifest: &str) -> anyhow::Result<KongGitEntry>;
pub fn normalize_subdir(p: &str) -> anyhow::Result<String>;
pub fn repo_key(repo: &str) -> String;
pub fn build_key(build: &str) -> String;
pub fn commit12(commit: &str) -> &str;
pub fn is_full_commit(s: &str) -> bool;
```
Tests cover:
- every accepted spec form (`github:` and `git+https` give the same `repo` and `repo_key`);
- every rejection;
- path normalisation;
- unknown entry keys.

## Module: node-git-fetch
- Files: `src/node/git/fetch.rs`
- Consumes: node-git-spec

Repository: kong. This module runs the git CLI, resolves commits and clones into the store.
```rust
#[derive(Debug, Clone, Default)]
pub struct GitEnv { pub vars: Vec<(String, String)>, pub secrets: Vec<String> }
pub fn git_program() -> PathBuf;   // PATH git, then C:\Program Files\Git\cmd\git.exe, (x86) variant
pub fn run_git(args: &[&str], cwd: Option<&Path>, env: &GitEnv) -> anyhow::Result<String>;
    // sets GIT_TERMINAL_PROMPT=0, GCM_INTERACTIVE=never, env.vars; failure "git <redacted args> failed (exit N): <redacted stderr>"
pub fn redact(text: &str, env: &GitEnv) -> String;  // secrets -> "***"; "scheme://user[:pw]@" -> "scheme://***@"
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution { Reused, Pinned, Remote }
pub fn resolve_commit(src: &GitSource, previous: Option<&GitPackageEntry>, update: bool, env: &GitEnv)
    -> anyhow::Result<(String, Resolution)>;   // § 2
pub fn commit_dir(store_root: &Path, repo: &str, commit: &str) -> PathBuf;
pub fn store_path_for(repo: &str, commit: &str) -> String;
pub struct CheckoutOutcome { pub commit_dir: PathBuf, pub checkout: PathBuf, pub cloned: bool, pub seconds: f64 }
pub fn ensure_checkout(store_root: &Path, src: &GitSource, commit: &str, env: &GitEnv) -> anyhow::Result<CheckoutOutcome>;
pub fn read_marker(path: &Path) -> Option<serde_json::Value>;
```
`ensure_checkout` works as follows:
1. **Existing clone.** If `.kong-cloned` exists, kong compares its commit with the requested one and reuses the clone if they match. A mismatch is an error.
2. **New clone.** Otherwise kong clones into a fresh tmp dir, always with `-c core.autocrlf=false -c advice.detachedHead=false`:
   - For `Default` and `Name`: `git clone --quiet --no-checkout --depth 1 [--branch <ref>] -- <repo> <tmp>/src`. If `rev-parse HEAD` equals the commit, kong runs `checkout --quiet --detach <commit>`. Otherwise it discards the tmp dir and makes a full clone, as for `Commit`.
   - For `Commit`: a full `git clone --quiet --no-checkout`, then `checkout --quiet --detach <commit>`. If that fails, the error is "commit not found in repository".
3. **Submodules.** If the checkout has a `.gitmodules`, kong runs `git submodule update --init --recursive --depth 1`.
4. **Commit to the store.** Kong writes `.kong-cloned` and renames the tmp dir to the commit dir. If the rename fails and the commit dir is already complete (another run won), it removes the tmp dir and reuses the commit dir.
5. **Log.** Kong logs `Cloned <name> <commit12> in X.X s`.

## Module: node-git-install
- Files: `src/node/git/install.rs`
- Consumes: node-git-spec, rules-config, node-modules-link, node-manifest

Repository: kong. This module installs a checkout's own dependencies.
```rust
pub struct InstalledDeps { pub lock_dir: Option<String>, pub packages: Vec<PackageEntry>, pub local: Vec<LocalPackageEntry>, pub installed_now: bool }
pub fn find_lock_dir(checkout: &Path, subdir: &str) -> Option<String>;
pub fn install_own_dependencies(checkout: &Path, commit_dir: &Path, src: &GitSource, commit: &str,
    store_root: &Path, force: bool, host: &HostTriple) -> anyhow::Result<InstalledDeps>;
```
- The lock dir follows § 3.
- The lockfile is read with `parser::parse_package_lock` and `parser::filter_for_host`.
- A dep whose `_resolved` starts with `git+`, `git:` or `github:` is an error: "nested git dependency '<x>' in <name>'s lockfile is not supported".
- Registry deps go through `config::node_package_entry(dep, store_root, force)`, which downloads only what is missing. Local deps go through `config::local_entry_for(<checkout>/<lock_dir>, name, src)`.
- `node::modules::build_node_modules(lock_abs, lock_abs, &NodeSection{packages, local, git: vec![]}, store_root)` runs every time; it is idempotent.
- `.kong-installed` is written on the first install. The output lists are sorted by name.

## Module: node-git-prepare
- Files: `src/node/git/mod.rs`
- Consumes: node-git-spec, node-git-fetch, node-git-install, rules-config

Repository: kong. `pub mod spec; pub mod fetch; pub mod install;` and `#[cfg(test)] pub(crate) mod proof_support; #[cfg(test)] mod proof_m2;`.
```rust
pub struct BuildRequest {
    pub name: String, pub source: String, pub commit: String, pub origin: String, pub build: String,
    pub checkout: PathBuf, pub commit_dir: PathBuf, pub package_dir: PathBuf,
    pub lock_dir: Option<PathBuf>, pub rust_targets: Vec<String>, pub store_root: PathBuf,
    pub marker: PathBuf, pub log: PathBuf,
}
pub struct BuildRecord { pub seconds: f64, pub log: PathBuf }
pub type BuildFn = fn(&BuildRequest) -> anyhow::Result<BuildRecord>;
#[derive(Clone, Copy)]
pub struct GitHooks { pub git_env: fn() -> fetch::GitEnv, pub build: Option<BuildFn> }
pub fn install_hooks(h: GitHooks);   // OnceLock; a second call is ignored
pub fn hooks() -> GitHooks;          // else { git_env: GitEnv::default, build: None }
pub struct PrepareOptions<'a> { pub update: bool, pub force: bool, pub git_env: fetch::GitEnv,
    pub builder: Option<&'a dyn Fn(&BuildRequest) -> anyhow::Result<BuildRecord>>, pub host: HostTriple }
#[derive(Debug, Default, Clone)]
pub struct PrepareReport { pub resolved_remote: u32, pub cloned: u32, pub installed: u32, pub built: u32,
    pub clone_seconds: f64, pub build_seconds: f64 }
pub fn prepare_all(store_root: &Path, sources: &[GitSource], previous: &[GitPackageEntry], opts: &PrepareOptions)
    -> anyhow::Result<(Vec<GitPackageEntry>, PrepareReport)>;
pub fn git_link_target(store_root: &Path, e: &GitPackageEntry) -> PathBuf;   // <store>/<store_path>/src/<path>
pub fn check_prepared(store_root: &Path, e: &GitPackageEntry) -> anyhow::Result<()>;
```
`prepare_all` handles the sources in name order. For each source it:
1. calls `resolve_commit`, counting `Remote` resolutions;
2. calls `ensure_checkout`;
3. requires `<checkout>/<path>/package.json`;
4. reads the version;
5. calls `install_own_dependencies`;
6. calls `opts.builder` if `build` is set and the marker is missing. With no builder, the error is "declares a build step but this kong runs no build steps";
7. builds the `GitPackageEntry`.

`check_prepared` is used by `kong use`. It requires `.kong-cloned`, plus `.kong-installed` when `lock_dir` is set and `.kong-built-<key>` when `build` is set. If any is missing, it returns a § 6 error with the `kong rules` hint.

## Module: node-manifest
- Files: `src/node/parser.rs`, `src/node/mod.rs`
- Consumes: node-git-spec

Repository: kong.
- `src/node/mod.rs` declares `pub mod git;`.
- `NodeDep` has the field `pub git: Option<GitSource>`, which is `Some` only for a kong git dependency.
- `pub fn git_declarations(project_dir: &Path) -> anyhow::Result<Vec<GitSource>>` reads the root and workspace-member manifests (§ 1) and returns the sources sorted by name. If no manifest has a `kong` key, it returns an empty vec without reading any member.
- Both branches of `parse_all` (lockfile and package.json) end with `apply_git_declarations`. This removes the lockfile-derived deps (§ 1) and pushes one `NodeDep{ name, version: "0.0.0", git: Some(src), … }` per source. With no declarations, `deps` is left untouched.
- Tests cover:
  - declarations in the root;
  - declarations in an `apps/*` member;
  - the same name declared in two manifests;
  - removal of a stale `file:` link;
  - an undeclared git spec staying unchanged.

## Module: rules-config
- Files: `src/config.rs`
- Consumes: node-git-prepare, node-manifest

Repository: kong.
```rust
pub struct GitPackageEntry { /* § 2 fields; optional ones with serde(default, skip_serializing_if) */ }
// NodeSection: #[serde(default, skip_serializing_if = "Vec::is_empty")] pub git: Vec<GitPackageEntry>
// RustSection: packages (PackageEntry.hash = Cargo.lock checksum), workspace: Vec<String>, lock_hash: Option<String>
#[derive(Debug, Clone, Default)]
pub struct RulesOptions { pub force: bool, pub name: Option<String>, pub python: Option<String>, pub update_git: bool }
pub fn generate_rules_with_options(project_dir: &Path, opts: &RulesOptions) -> anyhow::Result<KongRules>;
pub(crate) fn node_package_entry(dep: &NodeDep, store_root: &Path, force: bool) -> anyhow::Result<PackageEntry>;
```
- `generate_rules_with_python` delegates with `update_git: false`. `generate_rules` is unchanged.
- In the Node branch:
  - Deps with `git` set are collected.
  - The previous entries come from `read_rules(project_dir/kong.rules)` → `node.git`; a read error counts as no previous entries.
  - `node::git::prepare_all` runs with `PrepareOptions{ update: opts.update_git, force, git_env: (hooks().git_env)(), builder: hooks().build, host: HostTriple::current() }`.
  - The `PrepareReport` is logged at info level.
- The Rust branch records the checksum, workspace and lock hash (§ 7).
- Tests cover:
  - a NodeSection without git serializes to `{"packages":[]}`;
  - an old `kong.rules` without `git` still reads;
  - a full `GitPackageEntry` round-trips.

## Module: node-modules-link
- Files: `src/node/modules.rs`
- Consumes: node-git-prepare, rules-config

Repository: kong.
- `build_node_modules(env_dir, project_dir, node, store_root)` keeps its signature. After `link_local_packages`, it calls `pub fn link_git_packages(nm: &Path, git: &[GitPackageEntry], store_root: &Path) -> anyhow::Result<()>`.
- For each entry, `link_git_packages` calls `check_prepared`, then links `nm/<name>` to `git_link_target(...)`:
  - it keeps a correct link;
  - it replaces a stale link;
  - it removes a real directory in the env;
  - it never writes to the target.
- The `.bin` shims also cover `node.git` names.
- Tests cover:
  - the link points at the subdirectory in the store checkout;
  - a re-run is idempotent;
  - a missing marker gives the § 6 error;
  - `link::clean_environments` never deletes through a git link.

## Module: proof-m2
- Files: `src/node/git/proof_support.rs`, `src/node/git/proof_m2.rs`, `scripts/proof-m2.sh`
- Consumes: node-git-prepare, node-manifest, rules-config, node-modules-link

Repository: kong. `proof_support` is `#[cfg(test)] pub(crate)` and is reused by M3 and M5:
```rust
pub fn selected(label: &str) -> bool;
pub fn check(label: &str, measured: impl Display, threshold: &str, ok: bool);   // prints the conventions line; panics on FAIL
pub fn git(dir: &Path, args: &[&str]) -> String;
pub struct GitFixture { pub root: TempDir, pub work: PathBuf, pub bare: PathBuf, pub url: String }
impl GitFixture { pub fn new(files: &[(&str, &str)]) -> Self; pub fn write(&self, rel: &str, content: &str);
    pub fn commit_push(&self, msg: &str) -> String; pub fn tag_push(&self, tag: &str, force: bool); }
pub fn seed_store_package(store: &Path, name: &str, version: &str, files: &[(&str, &str)]) -> PackageEntry;
pub fn fixture_rollback_repo() -> GitFixture;   // root package-lock.json; packages/core (@fx/core → @fx/util, is-fixture@1.0.0), packages/util; tag v1.0.0
```
**Setup.** `proof_m2` runs with no hooks and no network. It pre-seeds `is-fixture@1.0.0` and declares `@fx/core` as `<url>#v1.0.0` with `kong.git {path: packages/core}`. It then runs parse → `prepare_all` → `write_rules` → `build_node_modules`.

**Checks.**
- `commit_pinned`: the recorded commit equals `rev-parse v1.0.0`. After the tag is force-moved, a re-run keeps the commit and reports `resolved_remote=0`.
- `link_in_place`: `node_modules/@fx/core` is a directory link whose canonical path is `<store>/node/git/<key>/<c12>/src/packages/core`, and the tracked files are clean.
- `own_deps`: walking up from the canonical target finds `node_modules/is-fixture/package.json` and `node_modules/@fx/util`. On Unix, `is-fixture/index.js` has nlink ≥ 2.
- `runs`: when `node` is on PATH, `node -e "console.log(require('@fx/core').hello())"` prints the expected string. Otherwise the measured value is `node not on PATH`.
- `backcompat`: a project with a `file:` dep and a registry dep and no `kong` block produces the pre-change NodeSection JSON.

**Script.** `scripts/proof-m2.sh [--only P] [--quick|--full]` follows the § Proofs rules and passes `--include-ignored`. `--full` adds `node::`.

## Module: gitdep-build
- Files: `src/gitdep/mod.rs`, `src/gitdep/build.rs`, `src/rust_eco/runtime.rs`
- Consumes: node-git-prepare, node-git-spec

Repository: kong. `src/gitdep/mod.rs` declares `pub mod build; pub mod auth; #[cfg(test)] mod proof_m3;`.
```rust
pub struct BuildTools { pub path_prepend: Vec<PathBuf>, pub env: Vec<(String, String)>, pub rust_version: Option<String> }
pub fn locate_bash() -> anyhow::Result<PathBuf>;                 // § 4
pub fn build_argv(checkout: &Path, build: &str) -> Vec<String>;
pub fn rust_targets_for(req: &BuildRequest) -> Vec<String>;     // sorted, deduplicated union
pub fn build_tools_for(req: &BuildRequest) -> anyhow::Result<BuildTools>;   // rust ensure_runtime + ensure_target, node runtime "lts", § 4 env
pub fn run_build_with(req: &BuildRequest, tools: &BuildTools) -> anyhow::Result<BuildRecord>;
    // lock → re-check marker → bash in req.checkout, stdout+stderr to req.log → non-zero = § 6 with log + last 20 lines
    // → marker JSON → warn on modified tracked files → info "Built <name> <c12> in X.X s"
pub fn run_build(req: &BuildRequest) -> anyhow::Result<BuildRecord>;   // the BuildFn installed by main
```
`src/rust_eco/runtime.rs` exports:
- `pub fn ensure_target(store_root, runtime, target) -> anyhow::Result<()>`. It does nothing if `<toolchain>/lib/rustlib/<target>/lib` exists. Otherwise it reads `channel-rust-<version>.toml`, takes `pkg.rust-std.target.<target>.url`, and merges it with `download_component` (strip 2).
- `pub fn newest_store_toolchain(store_root) -> Option<RustRuntime>`. It scans `<store>/rust/toolchain/*/`, keeps the X.Y.Z directories that contain both cargo and rustc, returns the numerically highest one, and never uses the network.

When the channel manifest cannot be fetched or parsed, `ensure_runtime(store_root)` falls back to `newest_store_toolchain`:
- If one is found, it warns "Rust channel unreachable, using newest toolchain in store".
- If none is found, it returns the original error with the context "Rust channel unreachable and no toolchain in the store".

Tests cover the choice of argv, the error text of a failing script, the lock/marker re-check, the union of targets, and how the newest store toolchain is selected.

## Module: gitdep-auth
- Files: `src/gitdep/auth.rs`
- Consumes: node-git-fetch

Repository: kong.
```rust
pub fn git_env_from(get: &dyn Fn(&str) -> Option<String>) -> GitEnv;   // § 5
pub fn machine_git_env() -> GitEnv;                                    // git_env_from(&|k| std::env::var(k).ok())
pub fn basic_auth_header(token: &str) -> String;                       // "AUTHORIZATION: basic " + base64("x-access-token:"+token)
fn base64(bytes: &[u8]) -> String;                                     // standard alphabet, '=' padding
```
- With `KONG_GIT_TOKEN`, the key is `http.extraheader`. Otherwise, with `GITHUB_TOKEN` or `GH_TOKEN`, the key is `http.https://github.com/.extraheader`.
- `secrets` holds the token and the full header.
- Tests cover the RFC 4648 base64 vectors, the precedence order, and that an empty token is ignored.

## Module: cli-wiring
- Files: `src/cli.rs`, `src/main.rs`
- Consumes: rules-config, node-git-prepare, gitdep-build, gitdep-auth, rust-cargo-env

Repository: kong.
- **`RulesCmd`** has `#[arg(long)] pub update_git: bool`, documented as "Re-resolve git dependencies to the remote's current commit (default: keep the commits recorded in kong.rules)".
- **`EnvCmd`** is `kong env [-p PATH] [--shell sh|ps1]`; the default shell is `sh`.
- **`main.rs`**:
  - declares `mod gitdep;`;
  - calls `node::git::install_hooks(GitHooks{ git_env: gitdep::auth::machine_git_env, build: Some(gitdep::build::run_build) })` first thing in `main()`;
  - routes tracing to stderr for `Commands::Env`.
- **Commands.**
  - `Commands::Rules` calls `config::generate_rules_with_options` with `RulesOptions{ force, name, python, update_git }`. `Clone` and `Setup` get the hooks through `generate_rules*`.
  - `Commands::Use` does not change for git dependencies; `build_node_modules` links them. For a Rust section it calls `rust_eco::source::configure_source_replacement` and logs `Cargo env ready: eval "$(kong env)"`.
  - `Commands::Env` reads `kong.rules` and, when there is a Rust section, prints `cargo_env::shell_exports` or `activate_ps1` of `source::plan_cargo_env(rulez_dir, rust, store_root, rules, target_cache::cache_root())`. An unknown shell is an error.

## Module: migrate-git
- Files: `src/migrate.rs`
- Consumes: rules-config, node-git-prepare, node-modules-link

Repository: kong.

**`solidify_node`.** After the local links, it does this for each `node.git` entry:
1. `remove_link_or_dir(nm/<name>)`.
2. Copy `<store>/<store_path>/src/<path>` to `nm/<name>` with `copy_dir_excluding(src, dst, &[".git", "node_modules"])`, which skips any entry that has a path component with one of those names.
3. Copy each top-level `packages` entry from `package_content_root(...)` to `nm/<name>/node_modules/<p.name>`, and each nested entry to `nm/<name>/<p.path>`.
4. Copy each `local` entry other than the dependency itself from `<checkout>/<lock_dir>/<l.path>` (excluding `node_modules`) to `nm/<name>/node_modules/<l.name>`.

The result is real files only.

**`eject_project` and `build_dep_index`.**
- Git entries are indexed under the key `("node-git", repo, commit)`, and their `packages` under `("node", name, version)`.
- A git entry that only this project uses has its `<store>/<store_path>` removed. Inner packages that only this project uses are removed like other node packages.

**Tests.** Solidify produces a real directory with the build output and an inner `node_modules`, and leaves the store untouched. Eject removes a commit dir that only this project uses and keeps a shared one.

## Module: proof-m3
- Files: `src/gitdep/proof_m3.rs`, `scripts/proof-m3.sh`
- Consumes: proof-m2, gitdep-build, gitdep-auth, migrate-git, node-git-prepare

Repository: kong.

**Fixture.** The M2 rollback repo, plus a `scripts/build-package.sh` that writes `packages/core/pkg/out.js` containing `$KONG_GIT_COMMIT`. `@fx/core` is declared with `build`, and the builder is `|r| run_build_with(r, &BuildTools::default())`.

**Checks.**
- `build_once`: the marker, the log and `pkg/out.js` exist in the commit dir, and the tracked files are clean.
- `rerun_cached_and_update`: a second `prepare_all` reports `cloned=0 built=0 resolved_remote=0`. After the tag is moved, `update=true` gives the new commit, a new commit dir and `built=1`.
- `build_failure_error`: a script that exits 3 gives an error that contains the name, the spec, the 40-hex commit, the origin, the log path and the log's last line.
- `no_secret_written`: `git_env_from` is called with `KONG_GIT_TOKEN=kongsentinel123`. The sentinel appears in no file under the project or the commit dir, and in none of the captured error and log text.
- `solidify_eject`: runs solidify and eject through the migrate internals with the explicit store root.

**Script.** `scripts/proof-m3.sh [--only P] [--full]` follows the § Proofs rules. `--full` adds `node::` and `gitdep::`.

## Module: docs-release
- Files: `README.md`, `CHANGELOG.md`
- Consumes: cli-wiring, migrate-git, rust-cargo-env

Repository: kong.

**README.**
- Under `## Supported Manifests`:
  - `### Git dependencies (`kong.git`)` covers the declaration, accepted specs, `node.git`, the store layout, credentials, the build step, `kong rules --update-git`, solidify/eject and the limits.
  - `### Rust: warm registry and build cache` covers `kong env`, the variables, the layout (§ 7), sharing, read-only stores and the limits.
- `## Commands` mentions `--update-git` and `kong env`.
- `## Known Limitations` lists the limits.

**CHANGELOG.** `[Unreleased]` has `### Added` entries for git dependencies and for the offline Cargo registry with its shared build cache. It also notes that `Cargo.lock` is now committed and that `kong.rules` Rust entries record the checksum, the workspace members and the lock hash.

## Module: realcase-royal-fiesta
- Files: `scripts/realcase/royal-fiesta.sh`, `.nmc/commands.json`, `docs/measurements/royal-fiesta-git-deps/{transcript-wsl.txt,vm-run.txt,dist-listing.txt,kong.rules.json,timings.txt}`, `scripts/proof-m4.sh`
- Consumes: cli-wiring, docs-release

Repository: kong.

**`scripts/realcase/royal-fiesta.sh [--game <url|path>] [--branch <b>] [--kong <binary>] [--host <ssh target>] [--out <dir>]`.**

Environment setup:
- Right after `set -u`, the script rebuilds a clean PATH: `${CARGO_HOME:-$HOME/.cargo}/bin`, `/root/.cargo/bin`, `/usr/local/bin`, `/usr/bin`, `/bin`, `/usr/sbin`, `/sbin`, then the inherited entries that contain no space and are not under `/mnt/`. It never appends the raw `$PATH`.
- If cargo is missing, it exits 2 with `royal-fiesta: cargo not found on PATH (<PATH>)`.

Local mode:
1. If `--kong` is not given, it builds `cargo build --release --no-default-features`.
2. It clones the game into a `mktemp -d` parent, records `$ ls -A <parent>` and `siblings=N`, and prints the game commit.
3. It uses a fresh `KONG_STORE` under the temp dir.
4. The transcript starts with `cargo --version`, `rustup target list --installed`, and `node --version` and `npm --version` when they are present.
5. It runs `kong rules`, `kong use` and `npm run web:build`. npm comes from PATH, else from the Node runtime that Kong manages.
6. It runs `kong rules` and `kong use` once more, to record the cached run.
7. It writes:
   - `transcript-wsl.txt`: `$ cmd`, output and `exit=N`, redacted;
   - `dist-listing.txt`;
   - `kong.rules.json`;
   - `timings.txt`: clone, build and web-build seconds, plus the wall-clock seconds of each command.

Local mode never runs ssh, scp or curl.

`--host` mode:
- Only the supervisor runs it, outside the sandbox.
- It builds the musl binary (`--target x86_64-unknown-linux-musl`), runs the same steps over ssh, and writes only `transcript-vm.txt`, or the reason the run was not performed.
- The usage header documents the command: `bash scripts/realcase/royal-fiesta.sh --host "${RF_VM_HOST:-etcito-vm}" --branch nmc/68-kong-dependencies-from-github-b2--m4-royal-fiesta-builds-from --out /root/nmc/evidence/68-m4/royal-fiesta-vm`.
- `docs/measurements/royal-fiesta-git-deps/vm-run.txt` records these instructions and states that the VM run had not been delivered when the evidence was committed.

**`.nmc/commands.json`.** It follows the programmatic-card contract of `server/commands_file.py` and must validate with zero errors:
- Each `command` is one shell-free argv string.
- It has no `bash -c`/`sh -c`, no env exports, no `;`, `&&`, `|` or `$`, and no network programs.
- `timeout_s` is within 1..3600.

It has exactly one entry: `{"name":"realcase-wsl","command":"bash scripts/realcase/royal-fiesta.sh --game /root/nmc/repos/royal-fiesta --branch nmc/68-kong-dependencies-from-github-b2--m4-royal-fiesta-builds-from --out docs/measurements/royal-fiesta-git-deps","timeout_s":3600}`. The WSL run needs network access to github.com, the crates.io index, static.crates.io and registry.npmjs.org. A blocked host is reported as a failure that names it.

**`scripts/proof-m4.sh [--only P]`** runs offline, on the committed evidence. Checks:
- `commands_ok`: `kong rules`, `kong use` and `npm run web:build` each end with `exit=0` in `transcript-wsl.txt`.
- `empty_dir`: `siblings=0`.
- `pinned`: `kong.rules.json` has at least 3 `"commit": "<40hex>"` lines, names `@etcito/raa` with a `build`, and names the two rollback packages.
- `dist`: the listing has `index.html` and at least one `.js` file, with total bytes > 0.
- `no_secret`: the same check as M1.

## Module: royal-fiesta-git-deps
- Files (Etcito/royal-fiesta): `package.json`, `apps/web/package.json`, `package-lock.json`, `apps/web/vite.config.ts`, `apps/web/tsconfig.json`, `scripts/fresh-clone-web-build.sh`, `.nmc/commands.json`
- Consumes: none

Repository: Etcito/royal-fiesta, branch `nmc/68-kong-dependencies-from-github-b2--m4-royal-fiesta-builds-from`. Claude pushes it before the supervisor's VM run.

**Dependency declarations.**
- In the package.json that declares them, the three `file:../../../gamesaloon-rollback/...` deps are now pinned git specs on `Etcito/gamesaloon-rollback` (`packages/core`, `packages/rapier`).
- `@etcito/raa` comes from `Etcito/raa`, with `path: packages/raa`, `build: scripts/build-package.sh` and the measured `rustTargets`.
- All are pinned to tags or 40-hex commits in that package.json's `kong.git`.

**Removed and cleaned.**
- `tools/bricks.ts`, its npm script hook and `apps/web/aliases.json` are deleted. The Vite and TS configs import `@etcito/raa` normally, with `resolve.dedupe` where a package shared with the bricks needs it.
- `package-lock.json` no longer has the stale entries for the old links and their link targets.

**Fresh-clone build.** `scripts/fresh-clone-web-build.sh` (bash, `set -o pipefail`, run from the repository root) does the following:
- runs `mktemp -d` and records that the directory is empty;
- runs `git clone --no-local` and prints the commit;
- runs `kong rules`, `kong use` and `npm run web:build`, printing each one's exit code and seconds and stopping at the first failure;
- lists `apps/web/dist`.

`.nmc/commands.json` has the single entry `{"name":"fresh-clone-web-build","command":"bash scripts/fresh-clone-web-build.sh","timeout_s":3000}`.

## Module: rust-cargo-env
- Files: `src/rust_eco/mod.rs`, `src/rust_eco/cargo_env.rs`, `src/rust_eco/target_cache.rs`, `src/rust_eco/source.rs`, `src/rust_eco/parser.rs`, `src/link.rs`
- Consumes: rules-config, gitdep-build

Repository: kong. This module implements § 7. `src/rust_eco/mod.rs` declares `pub mod cargo_env; pub mod client; pub mod parser; pub mod runtime; pub mod source; pub mod target_cache;` and `#[cfg(test)] mod proof_m5;`.
```rust
// parser.rs
pub struct CargoDep { /* … */ pub checksum: Option<String> }
pub fn workspace_members(lock: &Path) -> anyhow::Result<Vec<String>>;
pub fn lock_hash(lock: &Path) -> anyhow::Result<String>;            // CRLF → LF, sha256

// target_cache.rs
pub fn cache_root() -> anyhow::Result<PathBuf>;                     // pick_writable(&default_candidates())
pub fn default_candidates() -> Vec<PathBuf>;                        // KONG_CACHE, <install root>/cache, <tmp>/kong-cache
pub fn pick_writable(candidates: &[PathBuf]) -> anyhow::Result<PathBuf>;
pub fn target_key(workspace_members: &[String], lock_hash: &str, toolchain: &str, host: &str) -> String;  // 16 hex
pub fn target_dir(cache_root: &Path, key: &str) -> anyhow::Result<PathBuf>;
pub fn vendor_key(packages: &[(String, String, String)]) -> String; // order-independent, 16 hex
pub fn vendor_dir(cache_root: &Path, vendor_key: &str) -> PathBuf;

// cargo_env.rs
pub struct CargoEnv { pub project_root: PathBuf, pub store_cargo: PathBuf, pub vendor_dir: PathBuf, pub target_dir: PathBuf }
impl CargoEnv { toolchain_dir, cargo_home, config_path, bin_dir, incremental_dir, workspace_wrapper,
    pub fn vars(&self) -> Vec<(&'static str, String)> }
pub fn config_toml(env) -> String; pub fn cargo_wrapper_sh(env) -> String; pub fn cargo_wrapper_cmd(env) -> String;
pub fn workspace_wrapper_sh(env) -> String; pub fn activate_sh(env) -> String; pub fn activate_ps1(env) -> String;
pub fn shell_exports(env) -> String;
pub fn write_all(env: &CargoEnv) -> anyhow::Result<usize>;   // idempotent; unlinks before writing (bin/cargo may be a store hard link)

// source.rs
pub fn configure_source_replacement(env_dir, rust: &RustSection, store_root, rules: &KongRules) -> anyhow::Result<()>;
pub fn configure_with(env_dir, rust, store_root, rules, cache_root: &Path) -> anyhow::Result<CargoEnv>;
pub fn plan_cargo_env(env_dir, rust, store_root, rules, cache_root) -> anyhow::Result<CargoEnv>;   // writes nothing
pub fn build_vendor_dir(store_root: &Path, packages: &[PackageEntry], dir: &Path) -> anyhow::Result<()>;
```
- `configure_with` runs, in order: `plan_cargo_env` → `build_vendor_dir` → create the target dir → link the toolchain into `.rust-toolchain/` → `cargo_env::write_all`.
- `link::create_project_junctions` mirrors `.rust-toolchain/` into the project (§ 7), and `link::clean_environments` removes those links.
- Tests cover:
  - the config's TOML keys;
  - the wrapper passes `--config` before the user's arguments;
  - shell quoting;
  - idempotent writes that never write through the store hard link;
  - the wrappers and `activate.sh` run (Unix);
  - target and vendor key stability;
  - the writable picker;
  - checksums, workspace members and lock hash from the parser.

## Module: proof-m5
- Files: `src/rust_eco/proof_m5.rs`, `scripts/proof-m5.sh`
- Consumes: rust-cargo-env, proof-m2, cli-wiring

Repository: kong.

**`proof_m5`** (cargo test, uses `proof_support`). The fixture is created once, and every child cargo runs with a cleared environment. Checks:
- `backcompat`: a project without `Cargo.lock` gets the pre-M5 `kong.rules` bytes and no `.rust-toolchain/` or `.cargo/`.
- `offline_build`: a crate whose single dependency is in the store builds through `.rust-toolchain/bin/cargo build --offline`, with the network blocked and nothing fetched.
- `shared_target`: a second worktree at the same lock recompiles 0 dependency crates.
- `hostile_home`: a `$HOME/.cargo/config.toml` that replaces crates-io breaks the bare store cargo, while Kong's wrapper still runs `cargo test --offline`.

**`scripts/proof-m5.sh [--only P] [--full] [--keep]`**, run from the repository root.

Setup:
- It builds kong with `cargo build --offline --no-default-features`, or uses `KONG_BIN`.
- It seeds a store toolchain (links to the `rustc --print sysroot`), plus the crates found in the local cargo download cache.
- It plants a hostile, read-only `$HOME/.cargo/config.toml` that replaces crates-io with a directory registry whose checksums are wrong.
- It clones HEAD under that HOME and runs `kong rules`. The real proxy is used only if a crate is missing from the cache.
- It makes the store read-only, then runs `kong use`.
- Every kong and cargo child runs under `env -i`, with proxies pointing at a closed loopback port.

Checks:
- `registry_offline`: `cargo metadata --locked --offline` exits 0, every lock crate is vendored, nothing is fetched into `cargo-home`, and `Cargo.lock` is untouched.
- `cargo_check`: with `activate.sh` sourced, plain `cargo check --offline --no-default-features` exits 0 with no fetch.
- `cargo_test_node`: with `activate.sh` sourced, `cargo test --offline --no-default-features node::` passes with no fetch.
- `second_worktree_reuse`: a `git worktree` at the same commit runs `kong rules && kong use`, then `eval "$(kong env)"` and the same test. It passes with 0 dependency crates recompiled and the same `CARGO_TARGET_DIR` as the first worktree.
- `sandbox_config_override`: the bare store cargo fails on the HOME replacement, while kong's `cargo check` passes. The wrapper carries `--config <cargo-home>/config.toml`, and `activate.sh` and the wrapper both export the env's `CARGO_HOME`.

## Milestone proofs
- M1 Failure reproduced on a fresh Royal Fiesta clone, and the git-dependency design written: `bash scripts/proof-m1.sh`
- M2 Git dependencies resolved, cloned once into the store, linked, and their own dependencies installed: `bash scripts/proof-m2.sh`
- M3 Build step cached by commit, private-repository credential, --update-git, and solidify/eject for git dependencies: `bash scripts/proof-m3.sh`
- M4 Royal Fiesta builds from GitHub alone: kong rules && kong use && npm run web:build in an empty directory: `bash scripts/proof-m4.sh`
- M5 Kong keeps the Cargo registry and the build cache warm for every project: `bash scripts/proof-m5.sh`
