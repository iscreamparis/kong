# Git dependencies

Kong can install a Node dependency straight from a git repository, including a package that lives in a subdirectory of a monorepo and needs a build step (for example a Rust/wasm package). You declare it with a standard npm git spec in `package.json` plus a small `kong.git` block in the same file. `kong rules` pins the repository to an exact commit, clones it once into the Kong store, installs the dependency's own dependencies from its `package-lock.json`, runs the optional build once, and records everything in `kong.rules`. `kong use` then links the result at `node_modules/<name>`. This page describes the contract as users see it.

## Declaration

A git dependency is a normal npm git spec as the dependency value, plus a top-level `kong.git` block in the SAME `package.json`:

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

A git spec that no `kong.git` entry names keeps exactly the handling it had before (backward compatible).

**Accepted specs:** `github:ORG/REPO[#ref]`, `git+https://...`, `git+http://...`, `git+ssh://[git@]host/...`, `git+file:///abs/path`, `git://...`.

**Rejected specs** (each error names the dependency and the manifest):
- userinfo in an http(s) URL (credentials belong in the environment or the credential helper, see Credentials);
- `#semver:...`;
- `::` in the fragment;
- an abbreviated commit of 7 to 39 hex digits ("use the full 40-hex commit or a tag/branch").

**Canonical repo URL.** Drop the `git+` prefix. `github:ORG/REPO` becomes `https://github.com/ORG/REPO.git`. A trailing `/` is removed. For every scheme except `file`, a `.git` suffix is added when missing.

**Reference kinds.** The `#` fragment gives the reference:
- absent or empty: `Default` (the remote HEAD);
- exactly 40 lowercase hex: `Commit`;
- anything else: `Name` (a tag or a branch).

**`kong.git.<name>` keys:**
- `path`: optional string, default `""` (the repository root). Normalised to `/` separators with a leading `./` and trailing `/` removed. Absolute paths, `..` and `:` are rejected.
- `build`: optional non-empty string (a script file in the repository, or a shell command).
- `rustTargets`: optional array of target triples.
- Any other key is an error.

A `kong.git` entry whose name is not in `dependencies`, `devDependencies` or `optionalDependencies`, or whose value there is not a git spec, is an error naming both.

**Where declarations are read.** The project's root `package.json`, plus the `package.json` of each workspace member. Members come from the root `workspaces` field (an array, or an object's `packages` array). An entry is a literal directory, or `dir/*`, meaning every direct subdirectory that holds a `package.json`. Other glob forms are ignored with a warning. The same name declared twice is an error naming both manifests.

**Hoisting.** Every git dependency is linked at the project's top-level `node_modules/<name>`, hoisted the way npm hoists workspace dependencies, whichever member declared it.

**Origin.** Errors quote where the dependency was declared in this format, with `<manifest>` relative to the project (for example `apps/web/package.json`):

```
<manifest> <section> "<name>": "<spec>" (kong.git "<name>")
```

`declared_by` in `kong.rules` is `<manifest>` alone.

**Lockfile handling.** For each git dependency name, the parser drops every lockfile-derived entry with that name (any path, registry or local), and every entry whose path starts with `node_modules/<name>/node_modules/`. A stale `file:` link or npm git entry for it in the project's own lockfile is therefore ignored.

**Error format.** Every git-dependency failure uses the same shape: it names the dependency, the declared source, the resolved commit (or `(unresolved)`) and the declaring manifest entry.

```
git dependency '<name>' <what failed>:
  source      : <spec as declared>
  commit      : <40 hex | (unresolved)>
  declared by : <origin | kong.rules node.git (declared in <declared_by>)>
  [log         : <abs path>]
  [<other labelled facts: ref, path, checkout>]
<one hint line>
```

A build failure appends `last lines of the log:` followed by the last 20 lines of the build log (redacted). The hint for anything missing in the store at `kong use` time is "run `kong rules` to fetch and build it".

## kong.rules

Git dependencies are recorded under `node.git`, sorted by `name`. The key is omitted when there are none, so a project without a `kong.git` block produces the same `kong.rules` as before (apart from the `generated` timestamp).

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
  "packages": [ PackageEntry... ],
  "local": [ LocalPackageEntry... ]
} ] }
```

- Omitted when empty or none: `reference`, `path`, `build`, `rust_targets`, `lock_dir`, `packages`, `local`.
- `version` is read from `<checkout>/<path>/package.json`, default `0.0.0`. If that file's `name` differs from the dependency name, Kong warns; the package is still linked under the dependency name, as npm does.
- `packages` are the dependency's own lockfile registry packages. They use the same entry shape and `node/libs/...` store paths as the project's own packages, so they are shared with them.
- `local` holds the dependency's own workspace links. Their `path` is relative to `<checkout>/<lock_dir>`.
- Paths in `kong.rules` use `/` separators and are relative (to the store root or the checkout).

## Store layout

```
<store>/node/git/<repo_key>/<commit12>/        commit dir (one per repository and commit)
  src/                                       the checkout (keeps .git; tracked files never modified)
  src/<lock_dir>/node_modules/               the dependency's own tree (hard links)
  .kong-cloned                               JSON {"repo","commit","seconds","cloned_at"}, written before the atomic rename
  .kong-installed                            JSON {"lock_dir","packages":n,"local":n,"seconds"}
  .kong-built-<build_key>                    JSON {"build","commit","seconds","rust","finished"}
  build-<build_key>.log                      combined stdout+stderr of the build
  .kong-build-<build_key>.lock               present only while a build runs
<store>/node/git/<repo_key>/.tmp-<commit12>-<pid>-<nanos>/   clone in progress (renamed into place)
<store>/node/git/.cargo-home/                CARGO_HOME shared by every build step
```

- `repo_key` is the last path segment of the repo URL without `.git`, sanitised (keeps `[A-Za-z0-9._-]`, everything else becomes `_`), then `-`, then the first 8 hex of `sha256(repo)`.
- `commit12` is the first 12 hex of the commit. `.kong-cloned` stores the full commit and Kong checks it on reuse; a mismatch is an error.
- `build_key` is the first 12 hex of `sha256(build string)`.
- The markers make each step idempotent: a clone, an install and a build each happen once per commit.
- **Lock dir.** The first directory holding a `package-lock.json`, walking from `<checkout>/<path>` up to `<checkout>` (both included). If none is found and that `package.json` declares any `dependencies`, `devDependencies` or `optionalDependencies`, it is an error: kong installs a git dependency's own dependencies from its `package-lock.json` only. With no dependencies declared there is no lock dir and nothing to install.
- **Why the tree lives in the checkout.** Node and Vite resolve symlinks to their real path. Resolving from the real `<checkout>/<path>` walks up into `<checkout>/<lock_dir>/node_modules`, so the dependency finds its own packages there. Putting the tree in the project would not work.

## Credentials

- Git's own credential helper and ssh agent always work, because Kong runs the `git` CLI. Every git call sets `GIT_TERMINAL_PROMPT=0` and `GCM_INTERACTIVE=never`, so a missing credential fails instead of hanging on a prompt.
- A token can come from the environment. Precedence: `KONG_GIT_TOKEN` (applies to every `https://` host), otherwise `GITHUB_TOKEN`, then `GH_TOKEN` (these two apply to `https://github.com` only).
- The token reaches git as a one-off config entry in the git child's environment (`GIT_CONFIG_COUNT`, `GIT_CONFIG_KEY_0`, `GIT_CONFIG_VALUE_0`). The key is `http.<scheme>://<host>/.extraheader`, and the value is an HTTP Authorization header (basic auth, using the conventional GitHub access-token user name and the token, base64-encoded) with placeholders only here: `<header name>: basic <base64(<user>:<token>)>`.
- The token is never on argv, in a URL, in `kong.rules`, in a store path, in a marker or in a log line. Any git stderr that Kong prints has the token and the header value replaced with `***`.

## Build step

- **When.** Runs when `build` is set and `.kong-built-<build_key>` is missing, after the install. The working directory is `<checkout>` (the repository root).
- **argv.** If `<checkout>/<build>` is a regular file: `[bash, <build>]`. Otherwise `[bash, -c, <build>]`.
- **Which bash.** Unix: `bash` from PATH, else `sh`. Windows: the bash that ships with Git for Windows, never the one on PATH (it can be WSL's `System32\bash.exe`). Kong takes the parent of `git --exec-path`, walks up at most 3 directories to the first one containing `bin/bash.exe`; else tries `C:\Program Files\Git\bin\bash.exe`, then `C:\Program Files (x86)\Git\bin\bash.exe`; else it is an error. The command is the same on every OS.
- **Environment.** The inherited environment plus:
  - PATH prepended in this order: `<rust toolchain>/bin`, `<store>/node/git/.cargo-home/bin`, the Node runtime's executable directory, `<checkout>/<lock_dir>/node_modules/.bin` (joined with the platform separator).
  - `CARGO_HOME=<store>/node/git/.cargo-home` and `RUSTC=<toolchain>/bin/rustc` (`.exe` on Windows).
  - `KONG_GIT_DEP=<name>`, `KONG_GIT_COMMIT=<commit>`, `KONG_GIT_CHECKOUT=<checkout>`.
- **Rust targets.** The union of `rustTargets` and `[toolchain].targets` in `<checkout>/rust-toolchain.toml` (when present). For each, its `rust-std` component is merged into the Kong-managed toolchain once.
- **Lock.** A build runs under a lock file, then the marker is re-checked. A lock older than 2 hours is removed as stale; otherwise Kong polls every second for up to 30 minutes for the marker.
- **Tracked files.** After a successful build, if `git status --porcelain --untracked-files=no` is not empty, Kong warns and lists the modified tracked files. A build should only produce untracked outputs.
- **Failure.** The error follows the format in Declaration, names the log file (`build-<build_key>.log`) and shows its last 20 lines.

## Updating

`kong rules --update-git` moves every git dependency to the remote's current commit for its reference.

- **Commit reuse rule.** If the previous `kong.rules` holds an entry with the same `name`, `repo`, `reference` and `path`, and `--update-git` is not given, its `commit` is reused with no remote call. Re-running `kong rules` therefore gives the same commit even after the remote tag or branch moved.
- Otherwise: a `Commit` reference uses that commit with no remote call; a `Name` or `Default` reference runs `git ls-remote`.
- **ls-remote match priority** for `Name(r)`: `refs/tags/r^{}`, then `refs/tags/r`, then `refs/heads/r`. `Default` matches the `HEAD` line. No match is an error.

## Limits

- No nested git dependencies inside a dependency's own lockfile.
- No abbreviated commits (use the full 40 hex).
- No npm `::path:` syntax; use `kong.git.<name>.path` instead.
- `--force` never re-clones or rebuilds. To redo a clone or a build, delete the commit dir `<store>/node/git/<repo_key>/<commit12>/`.
- The Vite dev server must be allowed to serve files from the store: its `server.fs.allow` must include the store paths, because the linked package resolves to its real path inside the store.
