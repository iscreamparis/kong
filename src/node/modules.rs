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

    // ── Create .bin/ shims for executables ───────────────────────────────────
    // npm creates node_modules/.bin/<name> → ../<pkg>/bin/script.js for every
    // "bin" entry in a package's package.json. We replicate that here.
    let bin_dir = nm.join(".bin");
    std::fs::create_dir_all(&bin_dir)?;

    for pkg in &node.packages {
        let top_level = nm.join(&pkg.name);
        if !top_level.exists() {
            continue;
        }
        let pkg_json_path = top_level.join("package.json");
        let content = match std::fs::read_to_string(&pkg_json_path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let v: serde_json::Value = match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let bin = match v.get("bin") {
            Some(b) => b,
            None => continue,
        };

        // "bin" can be a string (single binary named after the package) or an object.
        let entries: Vec<(String, String)> = if let Some(s) = bin.as_str() {
            let bin_name = pkg.name.split('/').last().unwrap_or(&pkg.name).to_string();
            vec![(bin_name, s.to_string())]
        } else if let Some(obj) = bin.as_object() {
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        } else {
            continue
        };

        for (bin_name, rel_path) in entries {
            let shim = bin_dir.join(&bin_name);
            if shim.exists() {
                continue;
            }
            // Target is relative to node_modules/ root, e.g. ../vite/bin/vite.js
            let target = top_level.join(&rel_path);
            if !target.exists() {
                debug!(bin = %bin_name, "bin target missing, skipping");
                continue;
            }
            // Make target executable and symlink into .bin/
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Ok(meta) = std::fs::metadata(&target) {
                    let mut perms = meta.permissions();
                    perms.set_mode(perms.mode() | 0o111);
                    let _ = std::fs::set_permissions(&target, perms);
                }
                std::os::unix::fs::symlink(&target, &shim)
                    .with_context(|| format!("failed to create .bin/{bin_name} shim"))?;
            }
            #[cfg(windows)]
            {
                // On Windows npm creates .cmd wrappers; create a simple one.
                let target_str = target.to_string_lossy().replace('/', "\\");
                let cmd_path = bin_dir.join(format!("{bin_name}.cmd"));
                if !cmd_path.exists() {
                    std::fs::write(&cmd_path, format!("@node \"{target_str}\" %*\r\n"))?;
                }
            }
            debug!(bin = %bin_name, "Created .bin shim");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    // node_modules builder tests require tempdir + store mock — see kong-test skill
}
