---
name: kong-scaffold
description: "Scaffold the KONG Rust project from scratch. Use when: initializing the project, setting up Cargo.toml, creating the module skeleton, wiring up clap CLI, or adding a new top-level module. Produces: complete Cargo.toml, main.rs with clap subcommands, module stubs."
argument-hint: "Describe which part of the skeleton to generate (e.g. 'full project', 'add new subcommand')"
---

# KONG Project Scaffolding

## When to Use
- Starting the KONG project from zero
- Adding a new CLI subcommand or top-level module
- Resetting or regenerating `Cargo.toml` dependencies

## Required Crates (Cargo.toml)
Include ALL of these — they are the approved dependency set:

| Crate | Purpose |
|-------|---------|
| `clap` (derive) | CLI argument parsing |
| `serde`, `serde_json`, `toml` | Serialization for .rules, manifests, configs |
| `cargo-lock` | Parse Cargo.lock files |
| `reqwest` (blocking + rustls-tls) | HTTP downloads from registries |
| `sha2`, `hex` | SHA-256 hash verification |
| `zip` | Extract .whl (zip) archives |
| `tar`, `flate2` | Extract .tgz / .crate archives |
| `junction` | NTFS junctions on Windows |
| `dirs` | Locate home directory cross-platform |
| `walkdir` | Recursive directory traversal |
| `tempfile` | Temp dirs for downloads and tests |
| `fs_extra` | Bulk copy/move operations |
| `anyhow`, `thiserror` | Error handling |
| `tracing`, `tracing-subscriber` | Structured logging |
| `indicatif` | Progress bars (optional) |

## Module Layout
```
src/
├── main.rs          # CLI entry point (clap App + subcommands)
├── cli.rs           # Clap derive structs for commands
├── config.rs        # .rules file read/write + store paths
├── store.rs         # Central store operations (paths, dedup, verify)
├── download.rs      # HTTP download + hash verify pipeline
├── extract.rs       # Archive extraction (zip, tar.gz, .crate)
├── link.rs          # Hard links + junctions/symlinks (platform-aware)
├── python/
│   ├── mod.rs
│   ├── parser.rs    # requirements.txt, pyproject.toml, lockfiles
│   ├── client.rs    # PyPI JSON API client
│   └── venv.rs      # .venv builder
├── node/
│   ├── mod.rs
│   ├── parser.rs    # package.json, package-lock.json
│   ├── client.rs    # npm registry client
│   └── modules.rs   # node_modules builder (pnpm-style)
└── rust/
    ├── mod.rs
    ├── parser.rs    # Cargo.toml + Cargo.lock parser
    ├── client.rs    # crates.io client
    └── source.rs    # .cargo/config.toml source replacement
```

## Procedure

### Full project scaffold
1. Create `Cargo.toml` with all approved crates (see table above). Use `edition = "2021"`, `name = "kong"`, `version = "0.1.0"`.
2. Create `src/main.rs` with:
   - `use clap::Parser` and `#[derive(Parser)]` for the top-level app
   - Subcommands: `Rules`, `Use`, `Store`, `Doctor`
   - `tracing_subscriber::fmt::init()` at startup
3. Create `src/cli.rs` with clap derive structs:
   - `RulesCmd { force: bool }`
   - `UseCmd { rules_path: PathBuf, clean: bool }`
   - `StoreCmd` (subcommand: `path`)
   - `DoctorCmd`
4. Create stub modules for each file in the layout above, each exporting placeholder functions.
5. Run `cargo check` to verify everything compiles.

### Add new subcommand
1. Add the clap derive struct to `cli.rs`.
2. Add the variant to the `Commands` enum.
3. Wire the match arm in `main.rs`.
4. Create the implementation module.

## Hard Constraints
- Never add crates not in the approved list without explicit justification.
- `main.rs` must use `tracing`, never `println!` for diagnostics.
- All error handling via `anyhow::Result` at the application boundary.
