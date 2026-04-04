use std::path::Path;

use anyhow::{Context, Result};
use tracing::debug;

/// A parsed dependency from a Node.js manifest.
#[derive(Debug, Clone)]
pub struct NodeDep {
    pub name: String,
    pub version: String,
    pub _resolved: Option<String>,
    pub _integrity: Option<String>,
}

/// Detect and parse Node.js dependency files in a project directory.
/// Priority: package-lock.json > package.json
pub fn detect_and_parse(project_dir: &Path) -> Result<Vec<NodeDep>> {
    let lock = project_dir.join("package-lock.json");
    if lock.exists() {
        debug!("Found package-lock.json");
        return parse_package_lock(&lock);
    }

    let pkg = project_dir.join("package.json");
    if pkg.exists() {
        debug!("Found package.json");
        return parse_package_json(&pkg);
    }

    Ok(vec![])
}

/// Parse package-lock.json (v2/v3 format with `packages` key).
pub fn parse_package_lock(path: &Path) -> Result<Vec<NodeDep>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let doc: serde_json::Value = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse JSON: {}", path.display()))?;

    let mut deps = Vec::new();

    // v3 / v2 format: "packages" map
    if let Some(packages) = doc.get("packages").and_then(|p| p.as_object()) {
        for (key, value) in packages {
            // Skip the root entry (empty key)
            if key.is_empty() {
                continue;
            }

            // Skip symlink entries ("link": true) — these are pnpm virtual-store
            // redirects that have no version or tarball of their own.
            if value.get("link").and_then(|v| v.as_bool()).unwrap_or(false) {
                continue;
            }

            // Derive the real package name from the key.
            //
            // Standard npm v3:  "node_modules/express"
            //                   "node_modules/@scope/name"
            //
            // pnpm virtual store (embedded in npm lockfile):
            //   "node_modules/.pnpm/express@4.18.0/node_modules/express"
            //   "node_modules/.pnpm/@scope+name@1.0.0/node_modules/@scope/name"
            //
            // The real package name is always the last segment after the
            // final occurrence of "node_modules/".
            let name = if let Some(pos) = key.rfind("node_modules/") {
                key[pos + "node_modules/".len()..].to_string()
            } else {
                key.clone()
            };

            let version = value
                .get("version")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();

            let resolved = value
                .get("resolved")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            let integrity = value
                .get("integrity")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            if !version.is_empty() {
                deps.push(NodeDep {
                    name,
                    version,
                    _resolved: resolved,
                    _integrity: integrity,
                });
            }
        }
    }
    // v1 fallback: "dependencies" map
    else if let Some(dependencies) = doc.get("dependencies").and_then(|d| d.as_object()) {
        parse_v1_deps(dependencies, &mut deps);
    }

    Ok(deps)
}

fn parse_v1_deps(deps_map: &serde_json::Map<String, serde_json::Value>, out: &mut Vec<NodeDep>) {
    for (name, value) in deps_map {
        let version = value
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();

        let resolved = value
            .get("resolved")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let integrity = value
            .get("integrity")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        if !version.is_empty() {
            out.push(NodeDep {
                name: name.clone(),
                version,
                _resolved: resolved,
                _integrity: integrity,
            });
        }

        // Recurse into nested dependencies
        if let Some(nested) = value.get("dependencies").and_then(|d| d.as_object()) {
            parse_v1_deps(nested, out);
        }
    }
}

/// Parse package.json — extract dependencies + devDependencies (versions may be ranges).
pub fn parse_package_json(path: &Path) -> Result<Vec<NodeDep>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let doc: serde_json::Value = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse JSON: {}", path.display()))?;

    let mut deps = Vec::new();

    for section in &["dependencies", "devDependencies"] {
        if let Some(map) = doc.get(section).and_then(|d| d.as_object()) {
            for (name, version) in map {
                let version_str = version.as_str().unwrap_or_default();
                // Strip common range prefixes for best-effort
                let cleaned = version_str
                    .trim_start_matches('^')
                    .trim_start_matches('~')
                    .trim_start_matches(">=")
                    .to_string();

                if !cleaned.is_empty() {
                    deps.push(NodeDep {
                        name: name.clone(),
                        version: cleaned,
                        _resolved: None,
                        _integrity: None,
                    });
                }
            }
        }
    }

    Ok(deps)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_package_lock_v3() {
        let json = r#"{
            "name": "test-app",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": { "name": "test-app", "version": "1.0.0" },
                "node_modules/express": {
                    "version": "4.18.2",
                    "resolved": "https://registry.npmjs.org/express/-/express-4.18.2.tgz",
                    "integrity": "sha512-abc123"
                },
                "node_modules/@types/node": {
                    "version": "20.10.0",
                    "resolved": "https://registry.npmjs.org/@types/node/-/node-20.10.0.tgz"
                }
            }
        }"#;

        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), json).unwrap();
        let deps = parse_package_lock(tmp.path()).unwrap();

        assert_eq!(deps.len(), 2);
        let names: Vec<&str> = deps.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"express"));
        assert!(names.contains(&"@types/node"));
        let express = deps.iter().find(|d| d.name == "express").unwrap();
        assert_eq!(express.version, "4.18.2");
    }

    #[test]
    fn parse_package_json_basic() {
        let json = r#"{
            "name": "my-app",
            "dependencies": { "express": "^4.18.2" },
            "devDependencies": { "typescript": "~5.3.0" }
        }"#;

        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), json).unwrap();
        let deps = parse_package_json(tmp.path()).unwrap();

        assert_eq!(deps.len(), 2);
        assert_eq!(deps[0].name, "express");
        assert_eq!(deps[0].version, "4.18.2");
        assert_eq!(deps[1].name, "typescript");
        assert_eq!(deps[1].version, "5.3.0");
    }
}
