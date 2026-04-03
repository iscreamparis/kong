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
}

/// Pinned runtime versions managed by KONG (no system Python/Node needed).
#[derive(Debug, Serialize, Deserialize)]
pub struct RuntimeSection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub python: Option<RuntimeEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<RuntimeEntry>,
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
        for dep in &python_deps {
            let store_path = format!(
                "python/libs/{}-{}-{}-{}",
                dep.name, dep.version, py_tag, platform
            );
            let full_store_path = store_root.join(&store_path);
            if !full_store_path.exists() || force {
                let file_info = crate::python::client::fetch_and_download(
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
    let rust_section = if !rust_deps.is_empty() {
        info!(count = rust_deps.len(), "Found Rust dependencies");
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
        Some(RustSection { packages })
    } else {
        None
    };

    let runtimes = if python_runtime.is_some() || node_runtime.is_some() {
        Some(RuntimeSection { python: python_runtime, node: node_runtime })
    } else {
        None
    };

    Ok(KongRules {
        version: 1,
        project: project_name,
        generated: now,
        runtimes,
        python: python_section,
        node: node_section,
        rust: rust_section,
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
        };

        let json = serde_json::to_string_pretty(&rules).unwrap();
        let parsed: KongRules = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.version, 1);
        assert_eq!(parsed.runtimes.unwrap().python.unwrap().version, "3.12.9");
        assert_eq!(parsed.python.unwrap().packages.len(), 1);
    }
}
