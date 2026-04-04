use std::path::Path;

use anyhow::{Context, Result};
use tracing::debug;

/// A parsed dependency from Cargo.lock.
#[derive(Debug, Clone)]
pub struct RustDep {
    pub name: String,
    pub version: String,
    pub _checksum: Option<String>,
}

/// Detect and parse Rust dependency files in a project directory.
pub fn detect_and_parse(project_dir: &Path) -> Result<Vec<RustDep>> {
    let cargo_lock = project_dir.join("Cargo.lock");
    if cargo_lock.exists() {
        debug!("Found Cargo.lock");
        return parse_cargo_lock(&cargo_lock);
    }

    Ok(vec![])
}

/// Parse Cargo.lock using the cargo-lock crate.
pub fn parse_cargo_lock(path: &Path) -> Result<Vec<RustDep>> {
    let lockfile = cargo_lock::Lockfile::load(path)
        .with_context(|| format!("failed to parse Cargo.lock: {}", path.display()))?;

    let deps = lockfile
        .packages
        .iter()
        .filter(|pkg| {
            // Skip the root package (source is None for path dependencies)
            pkg.source.is_some()
        })
        .map(|pkg| RustDep {
            name: pkg.name.as_str().to_string(),
            version: pkg.version.to_string(),
            _checksum: pkg.checksum.as_ref().map(|c| c.to_string()),
        })
        .collect();

    Ok(deps)
}

#[cfg(test)]
mod tests {
    // Cargo.lock parsing tests require fixture files — see kong-test skill
}
