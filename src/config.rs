use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

// ── kong.rules JSON schema ──────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
pub struct KongRules {
    pub version: u32,
    pub project: String,
    pub generated: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtimes: Option<RuntimeSection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub python: Option<PythonSection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<NodeSection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rust: Option<RustSection>,
    /// Named runnable scripts (merged from package.json + pyproject.toml scripts sections).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub scripts: HashMap<String, String>,
}

/// Pinned runtime versions managed by KONG (no system Python/Node/Rust needed).
#[derive(Debug, Serialize, Deserialize)]
pub struct RuntimeSection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub python: Option<RuntimeEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<RuntimeEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rust: Option<RuntimeEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RuntimeEntry {
    pub version: String,
    pub store_path: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PythonSection {
    pub version: String,    // actual CPython version e.g. "3.12.9"
    pub platform: String,   // wheel platform tag e.g. "win_amd64"
    pub packages: Vec<PackageEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct NodeSection {
    pub packages: Vec<PackageEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RustSection {
    pub packages: Vec<PackageEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageEntry {
    pub name: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    pub store_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
}

// ── Read / Write ────────────────────────────────────────────────────────────

pub fn read_rules(path: &Path) -> Result<KongRules> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read rules file: {}", path.display()))?;
    let rules: KongRules = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse rules file: {}", path.display()))?;
    Ok(rules)
}

pub fn write_rules(rules: &KongRules, path: &Path) -> Result<()> {
    let json = serde_json::to_string_pretty(rules)?;
    std::fs::write(path, json)
        .with_context(|| format!("failed to write rules file: {}", path.display()))?;
    Ok(())
}

// ── Rules generation ────────────────────────────────────────────────────────

pub fn generate_rules(project_dir: &Path, force: bool) -> Result<KongRules> {
    let project_name = project_dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let store_root = crate::store::store_root()?;
    let now = chrono::Utc::now().to_rfc3339();
    let platform = platform_tag();

    debug!(project = %project_name, "Detecting manifests");

    // ── Python ──────────────────────────────────────────────────────────────
    let python_deps = crate::python::parser::detect_and_parse(project_dir)?;
    let (python_runtime, python_section) = if !python_deps.is_empty() {
        info!("Found Python dependencies — ensuring runtime");
        let runtime = crate::python::runtime::ensure_runtime(&store_root, "latest")?;
        let py_tag = short_python_tag(&runtime.version); // e.g. "cp312"

        info!(count = python_deps.len(), version = %runtime.version, "Processing Python packages");
        let mut packages = Vec::new();

        // BFS queue — seed with direct deps, then expand transitives
        use std::collections::VecDeque;
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut queue: VecDeque<crate::python::parser::PythonDep> = python_deps.into_iter().collect();

        while let Some(dep) = queue.pop_front() {
            let key = format!("{}-{}", dep.name.to_lowercase().replace('-', "_"), dep.version);
            if !seen.insert(key) {
                continue; // already processed
            }

            let store_path = format!(
                "python/libs/{}-{}-{}-{}",
                dep.name, dep.version, py_tag, platform
            );
            let full_store_path = store_root.join(&store_path);
            let transitive = if !full_store_path.exists() || force {
                let (file_info, trans) = crate::python::client::fetch_and_download(
                    &dep.name, &dep.version, &full_store_path,
                )?;
                packages.push(PackageEntry {
                    name: dep.name.clone(),
                    version: dep.version.clone(),
                    hash: Some(file_info.hash),
                    store_path,
                    source_url: Some(file_info.url),
                });
                trans
            } else {
                debug!(pkg = %dep.name, ver = %dep.version, "Already in store, skipping");
                packages.push(PackageEntry {
                    name: dep.name.clone(),
                    version: dep.version.clone(),
                    hash: None,
                    store_path,
                    source_url: None,
                });
                // Still need transitive deps — read from already-extracted METADATA
                read_transitive_from_store(&store_root.join(format!(
                    "python/libs/{}-{}-{}-{}",
                    dep.name, dep.version, py_tag, platform
                )))
            };

            // Enqueue transitive deps not yet seen
            for t in transitive {
                let t_key = t.name.to_lowercase().replace('-', "_");
                // Exact name match (ignoring version) — don't re-process any version of same pkg
                if seen.iter().any(|s| {
                    let s_name = s.split('-').next().unwrap_or(s);
                    s_name == t_key
                }) {
                    continue;
                }
                // version is empty when no == pin found — resolve latest from PyPI
                let version = if t.version.is_empty() {
                    match crate::python::client::resolve_latest_version(&t.name) {
                        Ok(v) => v,
                        Err(e) => { tracing::warn!(pkg = %t.name, "Could not resolve version: {e}"); continue; }
                    }
                } else {
                    t.version
                };
                queue.push_back(crate::python::parser::PythonDep { name: t.name, version });
            }
        }
        let section = PythonSection {
            version: runtime.version.clone(),
            platform: platform.clone(),
            packages,
        };
        let entry = RuntimeEntry { version: runtime.version, store_path: runtime.store_path };
        (Some(entry), Some(section))
    } else {
        (None, None)
    };

    // ── Node ────────────────────────────────────────────────────────────────
    let node_deps = crate::node::parser::detect_and_parse(project_dir)?;
    let (node_runtime, node_section) = if !node_deps.is_empty() {
        info!("Found Node dependencies — ensuring runtime");
        let runtime = crate::node::runtime::ensure_runtime(&store_root, "lts")?;

        info!(count = node_deps.len(), version = %runtime.version, "Processing Node packages");
        let mut packages = Vec::new();
        for dep in &node_deps {
            let safe_name = dep.name.replace('/', "+");
            let store_path = format!("node/libs/{}-{}", safe_name, dep.version);
            let full_store_path = store_root.join(&store_path);
            if !full_store_path.exists() || force {
                let file_info = crate::node::client::fetch_and_download(
                    &dep.name, &dep.version, &full_store_path,
                )?;
                packages.push(PackageEntry {
                    name: dep.name.clone(),
                    version: dep.version.clone(),
                    hash: Some(file_info.hash),
                    store_path,
                    source_url: Some(file_info.url),
                });
            } else {
                debug!(pkg = %dep.name, ver = %dep.version, "Already in store, skipping");
                packages.push(PackageEntry {
                    name: dep.name.clone(),
                    version: dep.version.clone(),
                    hash: None,
                    store_path,
                    source_url: None,
                });
            }
        }
        let entry = RuntimeEntry { version: runtime.version, store_path: runtime.store_path };
        (Some(entry), Some(NodeSection { packages }))
    } else {
        (None, None)
    };

    // ── Rust ────────────────────────────────────────────────────────────────
    let rust_deps = crate::rust_eco::parser::detect_and_parse(project_dir)?;
    let (rust_runtime, rust_section) = if !rust_deps.is_empty() {
        info!("Found Rust dependencies — ensuring toolchain");
        let runtime = crate::rust_eco::runtime::ensure_runtime(&store_root)?;

        info!(count = rust_deps.len(), version = %runtime.version, "Processing Rust crates");
        let mut packages = Vec::new();
        for dep in &rust_deps {
            let store_path = format!("rust/crates/{}-{}", dep.name, dep.version);
            let full_store_path = store_root.join(&store_path);
            if !full_store_path.exists() || force {
                let file_info = crate::rust_eco::client::fetch_and_download(
                    &dep.name, &dep.version, &full_store_path,
                )?;
                packages.push(PackageEntry {
                    name: dep.name.clone(),
                    version: dep.version.clone(),
                    hash: Some(file_info.hash),
                    store_path,
                    source_url: Some(file_info.url),
                });
            } else {
                debug!(pkg = %dep.name, ver = %dep.version, "Already in store, skipping");
                packages.push(PackageEntry {
                    name: dep.name.clone(),
                    version: dep.version.clone(),
                    hash: None,
                    store_path,
                    source_url: None,
                });
            }
        }
        let entry = RuntimeEntry { version: runtime.version, store_path: runtime.store_path };
        (Some(entry), Some(RustSection { packages }))
    } else {
        (None, None)
    };

    let runtimes = if python_runtime.is_some() || node_runtime.is_some() || rust_runtime.is_some() {
        Some(RuntimeSection { python: python_runtime, node: node_runtime, rust: rust_runtime })
    } else {
        None
    };

    // ── Scripts (merged from package.json + pyproject.toml) ─────────────────
    let scripts = collect_scripts(project_dir);

    Ok(KongRules {
        version: 1,
        project: project_name,
        generated: now,
        runtimes,
        python: python_section,
        node: node_section,
        rust: rust_section,
        scripts,
    })
}

// ── Platform helpers ────────────────────────────────────────────────────────

/// Return the wheel platform tag for the current OS/arch.
pub fn platform_tag() -> String {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => "win_amd64".to_string(),
        ("linux", "x86_64")   => "manylinux2014_x86_64".to_string(),
        ("macos", "x86_64")   => "macosx_10_9_x86_64".to_string(),
        ("macos", "aarch64")  => "macosx_11_0_arm64".to_string(),
        (os, arch)            => format!("{os}_{arch}"),
    }
}

/// Convert "3.12.9" → "cp312" for use in wheel store path.
fn short_python_tag(full_version: &str) -> String {
    let mut parts = full_version.splitn(3, '.');
    let major = parts.next().unwrap_or("3");
    let minor = parts.next().unwrap_or("0");
    format!("cp{major}{minor}")
}

/// Read `Requires-Dist` from an already-extracted wheel in the store.
/// Used when the package is already cached so we don't re-download it.
fn read_transitive_from_store(store_path: &std::path::Path) -> Vec<crate::python::client::TransitiveDep> {
    // The wheel is extracted flat: <store_path>/<PkgName>-<ver>.dist-info/METADATA
    let dist_info = match std::fs::read_dir(store_path) {
        Ok(rd) => rd,
        Err(_) => return Vec::new(),
    };
    for entry in dist_info.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.ends_with(".dist-info") {
            let metadata_path = entry.path().join("METADATA");
            if let Ok(content) = std::fs::read_to_string(&metadata_path) {
                let requires: Vec<String> = content
                    .lines()
                    .filter(|l| l.starts_with("Requires-Dist:"))
                    .map(|l| l["Requires-Dist:".len()..].trim().to_string())
                    .collect();
                return crate::python::client::parse_requires_dist_pub(&requires);
            }
        }
    }
    Vec::new()
}

/// Collect runnable scripts from manifests in `project_dir`.
/// Currently reads `package.json` → `scripts` and `pyproject.toml` → `[tool.taskipy.tasks]`.
/// Results are stored in `kong.rules` so `kong run <script>` works offline.
pub fn collect_scripts(project_dir: &Path) -> HashMap<String, String> {
    let mut scripts: HashMap<String, String> = HashMap::new();

    // ── package.json scripts ─────────────────────────────────────────────────
    let pkg_json = project_dir.join("package.json");
    if let Ok(content) = std::fs::read_to_string(&pkg_json) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&content) {
            if let Some(obj) = v.get("scripts").and_then(|s| s.as_object()) {
                for (k, v) in obj {
                    if let Some(cmd) = v.as_str() {
                        scripts.insert(k.clone(), cmd.to_string());
                    }
                }
            }
        }
    }

    // ── pyproject.toml [tool.taskipy.tasks] ─────────────────────────────────
    let pyproject = project_dir.join("pyproject.toml");
    if let Ok(content) = std::fs::read_to_string(&pyproject) {
        if let Ok(v) = content.parse::<toml::Value>() {
            if let Some(tasks) = v
                .get("tool")
                .and_then(|t| t.get("taskipy"))
                .and_then(|t| t.get("tasks"))
                .and_then(|t| t.as_table())
            {
                for (k, v) in tasks {
                    if let Some(cmd) = v.as_str() {
                        // Don't overwrite package.json scripts
                        scripts.entry(k.clone()).or_insert_with(|| cmd.to_string());
                    }
                }
            }
        }
    }

    scripts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_rules() {
        let rules = KongRules {
            version: 1,
            project: "test".to_string(),
            generated: "2026-01-01T00:00:00Z".to_string(),
            runtimes: Some(RuntimeSection {
                python: Some(RuntimeEntry {
                    version: "3.12.9".to_string(),
                    store_path: "python/runtime/3.12.9".to_string(),
                }),
                node: Some(RuntimeEntry {
                    version: "22.11.0".to_string(),
                    store_path: "node/runtime/22.11.0".to_string(),
                }),
            }),
            python: Some(PythonSection {
                version: "3.12.9".to_string(),
                platform: "win_amd64".to_string(),
                packages: vec![PackageEntry {
                    name: "requests".to_string(),
                    version: "2.31.0".to_string(),
                    hash: Some("sha256:abc".to_string()),
                    store_path: "python/libs/requests-2.31.0-cp312-win_amd64".to_string(),
                    source_url: None,
                }],
            }),
            node: None,
            rust: None,
            scripts: HashMap::new(),
        };

        let json = serde_json::to_string_pretty(&rules).unwrap();
        let parsed: KongRules = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.version, 1);
        assert_eq!(parsed.runtimes.unwrap().python.unwrap().version, "3.12.9");
        assert_eq!(parsed.python.unwrap().packages.len(), 1);
    }
}
