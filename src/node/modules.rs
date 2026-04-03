use std::path::Path;

use anyhow::{Context, Result};
use tracing::{debug, info};

use crate::config::NodeSection;
use crate::link;

/// Build a flat node_modules structure from the store using deep hard links.
///
/// All packages land directly at `node_modules/<name>` (hoisted, like npm).
/// We use deep hard links rather than directory junctions so that Node's
/// module resolution sees project-local paths for every file's `__dirname`.
/// Junctions on Windows are transparent to path resolution, so a junction
/// pointing into the store would cause `require()` look-ups to walk the store
/// tree instead of the project tree — breaking peer-dep resolution.
pub fn build_node_modules(
    project_dir: &Path,
    node: &NodeSection,
    store_root: &Path,
) -> Result<()> {
    let nm = project_dir.join("node_modules");
    std::fs::create_dir_all(&nm)?;

    for pkg in &node.packages {
        let src = store_root.join(&pkg.store_path);
        if !src.exists() {
            tracing::warn!(pkg = %pkg.name, "Store path missing, skipping: {}", src.display());
            continue;
        }

        // npm tarballs unpack with a `package/` subdirectory; use that as the content root.
        let content_dir = if src.join("package").is_dir() {
            src.join("package")
        } else {
            src.clone()
        };

        // Destination: node_modules/<name> (handle @scope/name)
        let top_level = nm.join(&pkg.name);
        if top_level.exists() {
            debug!(pkg = %pkg.name, "Already linked, skipping");
            continue;
        }

        // Create @scope/ parent if needed
        if pkg.name.contains('/') {
            if let Some(parent) = top_level.parent() {
                std::fs::create_dir_all(parent)?;
            }
        }

        link::link_package(&content_dir, &top_level)
            .with_context(|| format!("failed to link node package {}", pkg.name))?;

        debug!(pkg = %pkg.name, "Linked into node_modules");
    }

    info!("node_modules ready at {}", nm.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    // node_modules builder tests require tempdir + store mock — see kong-test skill
}
