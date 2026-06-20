# Releasing KONG

KONG ships one GitHub Release per version, tagged `vX.Y.Z`, carrying three
platform assets:

| Platform | Asset                              | Built by                                   |
|----------|------------------------------------|--------------------------------------------|
| Windows  | `Kong-<version>-windows-x64-setup.exe` | NSIS installer, built locally (`build.rs` → `build/kong-installer.nsi`) |
| macOS    | `Kong-<version>-macos-<arch>.dmg`  | `./buildOSX.sh` (local, on a Mac)          |
| Linux    | `Kong-<version>-linux-x64.tar.gz`  | **CI** — `.github/workflows/release-linux.yml` (static musl) |

The version is the single source of truth in `Cargo.toml` (`version = "X.Y.Z"`);
all asset names and the Linux workflow derive `<version>` from it.

## Version-less alias assets (required)

Each release MUST upload, **alongside** the versioned assets above, version-less
**alias copies** named exactly:

| Platform | Versioned asset                        | Version-less alias               |
|----------|----------------------------------------|----------------------------------|
| Windows  | `Kong-<version>-windows-x64-setup.exe` | `Kong-windows-x64-setup.exe`     |
| macOS    | `Kong-<version>-macos-arm64.dmg`       | `Kong-macos-arm64.dmg`           |
| Linux    | `Kong-<version>-linux-x64.tar.gz`      | `Kong-linux-x64.tar.gz` (+ `.sha256`) |

The README links to `releases/latest/download/<version-less-name>` — never-stale
permalinks that always resolve to the newest release. If a release omits an
alias, those README download links 404. Upload the alias as a renamed copy of
the same artifact (identical bytes); do not regenerate it.

## Cutting a release

1. **Bump the version** in `Cargo.toml` (and `build/kong-installer.nsi`'s
   `PRODUCT_VERSION`), then **update `CHANGELOG.md`** with the new version's
   entry (newest first), and commit on `main`. The README download links never
   need touching — they point at version-less `releases/latest/download/…`
   permalinks (see *Version-less alias assets* above).

2. **Build + upload the Windows and macOS assets** (these are local builds, not
   yet in CI):
   - Windows: `cargo build --release` (the NSIS installer is produced by
     `build.rs`), then upload `build/Kong-<version>-windows-x64-setup.exe`.
   - macOS: `./buildOSX.sh`, then upload `dist/Kong-<version>-macos-<arch>.dmg`.

   The easiest path is to create the release first and attach these two —
   **plus their version-less aliases** (renamed copies of the same bytes):

   ```bash
   cp build/Kong-X.Y.Z-windows-x64-setup.exe build/Kong-windows-x64-setup.exe
   cp dist/Kong-X.Y.Z-macos-arm64.dmg        dist/Kong-macos-arm64.dmg

   gh release create vX.Y.Z \
     build/Kong-X.Y.Z-windows-x64-setup.exe build/Kong-windows-x64-setup.exe \
     dist/Kong-X.Y.Z-macos-arm64.dmg        dist/Kong-macos-arm64.dmg \
     --title "KONG vX.Y.Z" --notes "..."
   ```

3. **Push the tag** — this triggers the Linux workflow, which builds the static
   musl binary and **appends** `Kong-<version>-linux-x64.tar.gz` (+ a
   `.sha256`) to the release created in step 2:

   ```bash
   git tag vX.Y.Z
   git push origin vX.Y.Z
   ```

   > If you ran `gh release create vX.Y.Z` in step 2 it already created+pushed
   > the tag, which fires the workflow automatically — no separate
   > `git push origin vX.Y.Z` is needed in that case.

4. **Verify** the Releases page shows all three versioned assets **and their
   three version-less aliases** (`Kong-windows-x64-setup.exe`,
   `Kong-macos-arm64.dmg`, `Kong-linux-x64.tar.gz` + `.sha256`). Smoke-test a
   `releases/latest/download/Kong-linux-x64.tar.gz` fetch — that is the URL the
   README hands users, and a missing alias 404s it. The README download links
   never change between releases (they are version-less permalinks).

## The Linux CI build

`.github/workflows/release-linux.yml`:

- **Trigger:** push of a `v*` tag (same trigger the Win/macOS assets live under).
  Also `workflow_dispatch` for an on-demand build that uploads a workflow
  artifact instead of a release asset.
- **Target:** `x86_64-unknown-linux-musl` → a fully **static** binary (no glibc
  version dependency; runs on old/assorted Ubuntu VMs). KONG already pins
  `reqwest` to `rustls-tls` (`default-features = false`) so the static link
  needs no system OpenSSL.
- **Flags:** `--no-default-features` disables the Slint GUI → no
  fontconfig/X11/OpenGL needed (headless build).
- **Asset:** `Kong-<version>-linux-x64.tar.gz` (a `kong` binary + the two
  LICENSE files), plus `Kong-<version>-linux-x64.tar.gz.sha256`. Remember to
  also upload the version-less aliases `Kong-linux-x64.tar.gz` (+ `.sha256`) so
  the README's `releases/latest/download/…` link resolves (see *Version-less
  alias assets* above).

It does **not** touch the Windows or macOS jobs — it only adds the Linux asset.

## Local repro of the Linux build (WSL / Ubuntu)

```bash
rustup target add x86_64-unknown-linux-musl
sudo apt-get install -y musl-tools
cargo build --release --no-default-features --target x86_64-unknown-linux-musl
file target/x86_64-unknown-linux-musl/release/kong   # → "statically linked"
target/x86_64-unknown-linux-musl/release/kong --version
```
