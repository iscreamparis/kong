---
description: "Rust module conventions for KONG. Use when: editing any .rs file in the project. Enforces: anyhow/thiserror error handling, tracing diagnostics, no println!, test patterns."
applyTo: "src/**/*.rs"
---

# Rust Module Conventions

## Error Handling
- Application boundaries (`main.rs`, command handlers): return `anyhow::Result<()>`
- Library modules (parsers, clients, store): define `thiserror` error enums
- Never use `.unwrap()` in non-test code — use `?` or `.context("...")?`

## Diagnostics
- Use `tracing::{info, warn, error, debug, trace}` — never `println!` or `eprintln!`
- Add `#[tracing::instrument]` on public functions that might fail
- Use `debug!` for internal progress, `info!` for user-visible status

## Module Structure
- One file per concern — don't put parser + client in same file
- Public API at top of file, private helpers below
- `#[cfg(test)] mod tests { ... }` at the bottom of every file with logic

## Testing
- Use `tempfile::TempDir` for any filesystem test
- Use recorded JSON fixtures (in `tests/fixtures/`) — no real HTTP in unit tests
- Test both success and error paths

## Platform
- Default to Windows paths and NTFS junctions
- Gate Unix-specific code behind `#[cfg(unix)]`
- Use `std::fs::hard_link` for files, `junction::create` for directories on Windows
