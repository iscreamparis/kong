use std::path::Path;

use anyhow::{bail, Context, Result};
use tracing::{debug, info};

use super::platform::{self, HostTriple};

/// A parsed dependency from a Node.js manifest.
#[derive(Debug, Clone)]
pub struct NodeDep {
    pub name: String,
    pub version: String,
    pub _resolved: Option<String>,
    pub _integrity: Option<String>,
    /// npm `os` constraint (e.g. ["linux"], ["!win32"]); empty = unconstrained.
    pub os: Vec<String>,
    /// npm `cpu` constraint (e.g. ["x64"], ["arm64"]); empty = unconstrained.
    pub cpu: Vec<String>,
    /// npm `libc` constraint (e.g. ["glibc"], ["musl"]); empty = unconstrained.
    pub libc: Vec<String>,
    /// Whether npm marked this dep optional. A host-incompatible OPTIONAL dep is
    /// silently skipped (npm behaviour); a host-incompatible REQUIRED dep errors.
    pub optional: bool,
}

impl NodeDep {
    /// True if this dep declares any platform constraint (os/cpu/libc).
    pub fn has_platform_constraint(&self) -> bool {
        !self.os.is_empty() || !self.cpu.is_empty() || !self.libc.is_empty()
    }
}

/// Detect and parse Node.js dependency files in a project directory, returning
/// only the dependencies installable on the **current host** — mirroring npm,
/// which installs a platform-native package only when its `os`/`cpu`/`libc`
/// fields match the host (and silently skips a non-matching *optional* one).
///
/// Priority: package-lock.json > package.json
pub fn detect_and_parse(project_dir: &Path) -> Result<Vec<NodeDep>> {
    detect_and_parse_for_host(project_dir, &HostTriple::current())
}

/// Host-parameterized variant of [`detect_and_parse`] — the host triple is
/// injected so platform filtering is testable for any target.
pub fn detect_and_parse_for_host(project_dir: &Path, host: &HostTriple) -> Result<Vec<NodeDep>> {
    let raw = parse_all(project_dir)?;
    filter_for_host(raw, host)
}

/// Parse the manifest without any platform filtering (raw lockfile contents).
fn parse_all(project_dir: &Path) -> Result<Vec<NodeDep>> {
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

/// Drop dependencies that are not installable on `host`.
///
/// For each dep that declares an `os`/`cpu`/`libc` constraint and does NOT match
/// the host:
/// - if it is **optional**, skip it silently (npm treats an os/cpu/libc mismatch
///   on an optionalDependency as "not installed", no error);
/// - if it is **required**, this is a fatal install error (npm: `EBADPLATFORM`).
///
/// Deps with no platform constraint are always kept. The filter is fully
/// host-driven — no package names are special-cased.
pub fn filter_for_host(deps: Vec<NodeDep>, host: &HostTriple) -> Result<Vec<NodeDep>> {
    let mut kept = Vec::with_capacity(deps.len());
    let mut skipped = 0usize;

    for dep in deps {
        if !dep.has_platform_constraint()
            || platform::is_compatible(host, &dep.os, &dep.cpu, &dep.libc)
        {
            kept.push(dep);
            continue;
        }

        if dep.optional {
            debug!(
                pkg = %dep.name, ver = %dep.version,
                os = ?dep.os, cpu = ?dep.cpu, libc = ?dep.libc,
                "Skipping host-incompatible optional dependency"
            );
            skipped += 1;
        } else {
            bail!(
                "package '{}@{}' is not compatible with this host \
                 (os={:?} cpu={:?} libc={:?}; host os={} cpu={} libc={:?}) \
                 — it is a required dependency (EBADPLATFORM)",
                dep.name, dep.version, dep.os, dep.cpu, dep.libc,
                host.os, host.cpu, host.libc
            );
        }
    }

    if skipped > 0 {
        info!(
            skipped,
            host_os = %host.os, host_cpu = %host.cpu, host_libc = ?host.libc,
            "Filtered out platform-incompatible optional Node packages"
        );
    }

    Ok(kept)
}

/// Read an npm os/cpu/libc field, which may be a JSON array of strings.
fn read_str_array(value: &serde_json::Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
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
                    os: read_str_array(value, "os"),
                    cpu: read_str_array(value, "cpu"),
                    libc: read_str_array(value, "libc"),
                    optional: value
                        .get("optional")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
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
                os: read_str_array(value, "os"),
                cpu: read_str_array(value, "cpu"),
                libc: read_str_array(value, "libc"),
                optional: value
                    .get("optional")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
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
                        // package.json's dependency map carries no os/cpu/libc —
                        // those live in each dependency's own package.json, which
                        // we don't fetch here. Platform filtering therefore only
                        // engages from a lockfile (the common, npm-managed case).
                        name: name.clone(),
                        version: cleaned,
                        _resolved: None,
                        _integrity: None,
                        os: Vec::new(),
                        cpu: Vec::new(),
                        libc: Vec::new(),
                        optional: false,
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

    fn linux_x64_glibc() -> HostTriple {
        HostTriple {
            os: "linux".into(),
            cpu: "x64".into(),
            libc: Some("glibc".into()),
        }
    }

    fn dep(name: &str, os: &[&str], cpu: &[&str], libc: &[&str], optional: bool) -> NodeDep {
        NodeDep {
            name: name.into(),
            version: "1.0.0".into(),
            _resolved: None,
            _integrity: None,
            os: os.iter().map(|s| s.to_string()).collect(),
            cpu: cpu.iter().map(|s| s.to_string()).collect(),
            libc: libc.iter().map(|s| s.to_string()).collect(),
            optional,
        }
    }

    #[test]
    fn filter_keeps_unconstrained_and_matching_only() {
        let host = linux_x64_glibc();
        let deps = vec![
            dep("express", &[], &[], &[], false), // no constraint → keep
            dep("@esbuild/linux-x64", &["linux"], &["x64"], &[], true), // match → keep
            dep("@esbuild/darwin-x64", &["darwin"], &["x64"], &[], true), // skip
            dep("@esbuild/win32-x64", &["win32"], &["x64"], &[], true), // skip
            dep("@rspack/binding-linux-x64-musl", &["linux"], &["x64"], &["musl"], true), // skip
            dep("@rspack/binding-linux-x64-gnu", &["linux"], &["x64"], &["glibc"], true), // keep
        ];
        let kept = filter_for_host(deps, &host).unwrap();
        let names: Vec<&str> = kept.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "express",
                "@esbuild/linux-x64",
                "@rspack/binding-linux-x64-gnu"
            ]
        );
    }

    #[test]
    fn filter_errors_on_incompatible_required_dep() {
        let host = linux_x64_glibc();
        // A REQUIRED (non-optional) dep that can't run on the host must error,
        // matching npm's EBADPLATFORM.
        let deps = vec![dep("native-thing", &["win32"], &["x64"], &[], false)];
        let err = filter_for_host(deps, &host).unwrap_err();
        assert!(err.to_string().contains("not compatible"));
        assert!(err.to_string().contains("EBADPLATFORM"));
    }

    #[test]
    fn filter_skips_incompatible_optional_without_error() {
        let host = linux_x64_glibc();
        let deps = vec![dep("native-thing", &["win32"], &["x64"], &[], true)];
        let kept = filter_for_host(deps, &host).unwrap();
        assert!(kept.is_empty());
    }

    #[test]
    fn negation_kept_on_linux() {
        let host = linux_x64_glibc();
        let deps = vec![dep("not-windows", &["!win32"], &[], &[], true)];
        let kept = filter_for_host(deps, &host).unwrap();
        assert_eq!(kept.len(), 1);
    }

    #[test]
    fn lockfile_captures_platform_fields() {
        let json = r#"{
            "lockfileVersion": 3,
            "packages": {
                "": { "name": "app", "version": "1.0.0" },
                "node_modules/@esbuild/android-arm": {
                    "version": "0.27.7",
                    "resolved": "https://r/-.tgz",
                    "cpu": ["arm"],
                    "os": ["android"],
                    "optional": true
                }
            }
        }"#;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), json).unwrap();
        let deps = parse_package_lock(tmp.path()).unwrap();
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].os, vec!["android".to_string()]);
        assert_eq!(deps[0].cpu, vec!["arm".to_string()]);
        assert!(deps[0].optional);
        // …and on a linux host the parser-level filter drops it.
        let kept = filter_for_host(deps, &linux_x64_glibc()).unwrap();
        assert!(kept.is_empty());
    }
}
