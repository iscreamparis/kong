---
name: kong-doctor
description: "Implement the 'kong doctor' diagnostic command. Use when: building health checks, verifying store integrity, detecting broken links/junctions, checking Python/Node/Rust availability, diagnosing missing dependencies, or validating kong.rules against the store."
argument-hint: "Which checks to implement (store, links, runtime, rules, or all)"
---

# Kong Doctor — Diagnostic Command

## When to Use
- Implementing the `kong doctor` CLI subcommand
- Adding a new health check to the diagnostic suite
- Debugging a broken environment (corrupt store, stale junctions, missing runtimes)
- Validating that `kong use` created a correct environment

## Checks to Implement

### 1. Store Integrity
- Verify `~/.kong/` (or `KONG_STORE`) directory exists and is writable
- For each entry in store, check `.kong-verified` marker exists
- Optionally re-hash stored contents against the recorded SHA-256
- Report total disk usage per ecosystem

```rust
struct StoreCheck {
    path: PathBuf,
    exists: bool,
    writable: bool,
    python_packages: usize,
    node_packages: usize,
    rust_crates: usize,
    total_size_bytes: u64,
    corrupt_entries: Vec<String>,  // entries missing .kong-verified
}
```

### 2. Link Validation
- Walk `.venv/Lib/site-packages/` — verify each hard link target still exists
- Walk `node_modules/.pnpm/` — verify each junction target still exists
- Check `.cargo/config.toml` source replacement path exists
- Report broken links (target deleted from store)

```rust
fn check_links(project_dir: &Path) -> Result<LinkReport> {
    let mut broken = Vec::new();
    // For each hard link: std::fs::metadata() fails if target missing
    // For each junction: junction::exists() + target path exists check
    // ...
    Ok(LinkReport { total: count, broken })
}
```

### 3. Runtime Detection
- **Python:** Find `python3`/`python` on PATH, report version, confirm it matches kong.rules python.version
- **Node:** Find `node` on PATH, report version
- **Rust:** Find `rustc` on PATH, report version and target triple
- Warn if runtime version doesn't match what kong.rules expects

### 4. Rules Validation
- Parse `kong.rules` if present
- For each listed package, verify the store path exists
- Flag packages listed in rules but missing from store (need `kong rules` re-run)
- Flag packages in store but not in rules (orphans — informational only)

### 5. Platform Checks
- Windows: Verify hard link support (NTFS required, not FAT32)
- Windows: Verify junction creation works (test create + delete in temp dir)
- Unix: Verify symlink creation works
- Report filesystem type if detectable

## Output Format
Use colored terminal output via `tracing` or direct ANSI codes:

```
kong doctor
  ✓ Store: C:\kong (3.2 GB, 142 packages)
  ✓ Python: 3.11.7 (matches kong.rules)
  ✓ Node: 20.10.0
  ✓ Rust: 1.75.0 (x86_64-pc-windows-msvc)
  ✓ Links: 847 verified, 0 broken
  ✗ Rules: 2 packages missing from store
      - flask==3.0.0 (not downloaded)
      - numpy==1.26.2 (not downloaded)
    Run 'kong rules' to download missing packages.
```

Use `✓` for pass, `✗` for fail, `⚠` for warning.

## Procedure
1. Define a `DoctorReport` struct containing all check results.
2. Run all checks sequentially (each check is independent but fast).
3. Print summary with pass/fail markers.
4. Exit code: `0` if all pass, `1` if any fail.

## Error Handling
Doctor should **never** panic or abort on a check failure — collect all results and report at the end. Wrap each check in a `match` or `Result` handler that captures the failure as a report entry.
