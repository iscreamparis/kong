# KONG — Workspace Instructions

## Project Identity
KONG is a unified dependency manager for Python, Node.js, and Rust. It is a **100% Rust CLI tool** — no other languages in the implementation.

## Hard Rules — Always Enforce
1. **No external package managers.** Never call `pip`, `uv`, `npm`, `yarn`, `pnpm`, or `cargo install` as subprocesses (`std::process::Command`). All downloads and metadata must go through `reqwest` against official registries.
2. **Rust only.** Every module, test, and script is Rust. Use the crates listed in `Cargo.toml` — no ad-hoc additions without justification.
3. **Windows-first.** Default to NTFS junctions (`junction` crate) and hard links. Gate Unix symlinks behind `#[cfg(unix)]`.
4. **Content-addressable store.** Packages live in `~/.kong/{python,node,rust}/libs/<pkg>-<ver>[-tags]/`. Deduplicate by SHA-256 of the downloaded archive.
5. **Idempotent operations.** Every command must be safe to re-run. Check existence before creating links/files.
6. **No full solver.** V1 relies on existing lockfiles or exact versions — do not build a dependency resolver.

## Code Conventions
- Use `anyhow` for application errors, `thiserror` for library error types.
- Use `tracing` (not `println!` / `log`) for all diagnostics.
- Use `clap` derive API for CLI definitions.
- Prefer `reqwest::blocking` unless the module already uses `tokio`.
- Keep modules focused: one file per concern (parser, client, linker, builder).

## Registry APIs
- **PyPI:** `https://pypi.org/pypi/<package>/json` → pick best wheel for platform.
- **npm:** `https://registry.npmjs.org/<package>/<version>` → `dist.tarball` URL.
- **crates.io:** tarball at `https://static.crates.io/crates/<crate>/<crate>-<version>.crate`.

## Testing
- Unit tests in the same file (`#[cfg(test)] mod tests`).
- Integration tests in `tests/` folder.
- Use `tempfile` for any test that touches the filesystem.
