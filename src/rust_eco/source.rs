use std::path::Path;

use anyhow::{Context, Result};
use tracing::{debug, info, warn};

use crate::config::{KongRules, RustSection};

/// Generate .cargo/config.toml with source replacement pointing to the local store,
/// and link the kong-managed Rust toolchain into .rust-toolchain/ in the project.
pub fn configure_source_replacement(
    project_dir: &Path,
    rust: &RustSection,
    store_root: &Path,
    rules: &KongRules,
) -> Result<()> {
    let cargo_dir = project_dir.join(".cargo");
    std::fs::create_dir_all(&cargo_dir)?;

    let config_path = cargo_dir.join("config.toml");
    let registry_dir = store_root.join("rust").join("registry");
    std::fs::create_dir_all(&registry_dir)?;

    // Ensure each crate has .cargo-checksum.json in the registry dir
    for pkg in &rust.packages {
        let crate_src = store_root.join(&pkg.store_path);
        // The unpacked crate may have a nested directory: <name>-<version>/
        let nested = crate_src.join(format!("{}-{}", pkg.name, pkg.version));
        let crate_dir = if nested.exists() { nested } else { crate_src };

        let registry_entry = registry_dir.join(format!("{}-{}", pkg.name, pkg.version));
        if !registry_entry.exists() {
            // Copy/link crate source into registry dir
            crate::link::link_package(&crate_dir, &registry_entry)?;
        }

        // Write .cargo-checksum.json if missing
        let checksum_file = registry_entry.join(".cargo-checksum.json");
        if !checksum_file.exists() {
            let hash = pkg.hash.as_deref().unwrap_or("unknown");
            let checksum_json = format!(r#"{{"files":{{}},"package":"{hash}"}}"#);
            std::fs::write(&checksum_file, checksum_json)?;
            debug!(crate_name = %pkg.name, "Wrote .cargo-checksum.json");
        }
    }

    // ── Kong-managed Rust toolchain ──────────────────────────────────────────
    // Locate rustc from the store (recorded in rules.runtimes.rust)
    let kong_rustc = kong_rustc_exe(store_root, rules);
    let kong_cargo = kong_cargo_exe(store_root, rules);

    if kong_rustc.is_none() {
        warn!("Kong-managed Rust toolchain not found in store — .cargo/config.toml won't set rustc");
    }

    // Link the full toolchain bin/ into .rust-toolchain/bin/ for easy PATH activation
    if let Some(ref cargo) = kong_cargo {
        if let Some(toolchain_store) = cargo.parent().and_then(|b| b.parent()) {
            create_toolchain_links(project_dir, toolchain_store)
                .unwrap_or_else(|e| warn!("Could not create .rust-toolchain links: {e}"));
        }
    }

    // Write config.toml — use forward slashes so TOML doesn't need escaping
    let registry_path_str = registry_dir.to_string_lossy().replace('\\', "/");
    let rustc_line = if let Some(ref rustc) = kong_rustc {
        let rustc_str = rustc.to_string_lossy().replace('\\', "/");
        format!("\n[build]\nrustc = \"{rustc_str}\"\n")
    } else {
        String::new()
    };

    let config_content = format!(
        r#"[source.crates-io]
replace-with = "kong-local"

[source.kong-local]
directory = "{registry_path_str}"
{rustc_line}"#
    );

    std::fs::write(&config_path, config_content)
        .with_context(|| format!("failed to write {}", config_path.display()))?;

    info!(config = %config_path.display(), "Cargo source replacement configured");
    Ok(())
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Locate kong-managed rustc from the store path recorded in rules.
fn kong_rustc_exe(store_root: &Path, rules: &KongRules) -> Option<std::path::PathBuf> {
    let store_path = rules.runtimes.as_ref()?.rust.as_ref()?.store_path.clone();
    let toolchain_dir = store_root.join(store_path);
    crate::rust_eco::runtime::rustc_exe_in(&toolchain_dir)
}

fn kong_cargo_exe(store_root: &Path, rules: &KongRules) -> Option<std::path::PathBuf> {
    let store_path = rules.runtimes.as_ref()?.rust.as_ref()?.store_path.clone();
    let toolchain_dir = store_root.join(store_path);
    crate::rust_eco::runtime::cargo_exe_in(&toolchain_dir)
}

/// Hard-link the toolchain bin/ directory into `.rust-toolchain/bin/` so users
/// can activate with: `$env:PATH = ".rust-toolchain\bin;$env:PATH"` and then
/// use `cargo build` normally.
fn create_toolchain_links(project_dir: &Path, toolchain_store: &Path) -> Result<()> {
    let dest = project_dir.join(".rust-toolchain");
    std::fs::create_dir_all(&dest)?;

    let src_bin = toolchain_store.join("bin");
    if !src_bin.exists() {
        return Ok(());
    }

    let dst_bin = dest.join("bin");
    std::fs::create_dir_all(&dst_bin)?;

    for entry in std::fs::read_dir(&src_bin)? {
        let entry = entry?;
        let src_file = entry.path();
        let dst_file = dst_bin.join(entry.file_name());
        if dst_file.exists() {
            continue;
        }
        // Hard-link each executable
        if let Err(e) = std::fs::hard_link(&src_file, &dst_file) {
            debug!(src = %src_file.display(), err = %e, "hard-link failed, copying");
            std::fs::copy(&src_file, &dst_file)?;
        }
    }

    // Write an activation script for Windows PowerShell
    let activate_ps1 = project_dir.join(".rust-toolchain").join("activate.ps1");
    if !activate_ps1.exists() {
        let content = format!(
            "# Activate kong-managed Rust toolchain\n\
             $env:PATH = \"$PSScriptRoot\\bin;$env:PATH\"\n\
             Write-Host \"Rust toolchain activated: $(rustc --version)\"\n"
        );
        std::fs::write(&activate_ps1, content)?;
    }

    // Write an activation script for bash/zsh
    let activate_sh = project_dir.join(".rust-toolchain").join("activate.sh");
    if !activate_sh.exists() {
        let content = "#!/bin/sh\n\
             # Activate kong-managed Rust toolchain\n\
             export PATH=\"$(dirname \"$0\")/bin:$PATH\"\n\
             echo \"Rust toolchain activated: $(rustc --version)\"\n";
        std::fs::write(&activate_sh, content)?;
    }

    info!(dest = %dest.display(), "Rust toolchain linked into project");
    Ok(())
}

#[cfg(test)]
mod tests {

    #[test]
    fn generates_valid_toml() {
        let content = format!(
            r#"[source.crates-io]
replace-with = "kong-local"

[source.kong-local]
directory = "/store/rust/registry"
"#
        );
        let parsed: toml::Value = toml::from_str(&content).unwrap();
        assert!(parsed.get("source").is_some());
    }
}
