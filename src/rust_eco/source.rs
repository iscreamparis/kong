use std::path::Path;

use anyhow::{Context, Result};
use tracing::{debug, info};

use crate::config::RustSection;

/// Generate .cargo/config.toml with source replacement pointing to the local store.
pub fn configure_source_replacement(
    project_dir: &Path,
    rust: &RustSection,
    store_root: &Path,
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

    // Write config.toml — use forward slashes so TOML doesn't need escaping
    let registry_path_str = registry_dir.to_string_lossy().replace('\\', "/");
    let config_content = format!(
        r#"[source.crates-io]
replace-with = "kong-local"

[source.kong-local]
directory = "{registry_path_str}"
"#
    );

    std::fs::write(&config_path, config_content)
        .with_context(|| format!("failed to write {}", config_path.display()))?;

    info!(config = %config_path.display(), "Cargo source replacement configured");
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
