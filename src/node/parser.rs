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
    /// `Some` for a LOCAL package (`file:`/`link:` dependency, npm `link: true`
    /// entry, workspace member): it is NOT fetched from the registry and NOT
    /// copied into the store — `kong use` links `node_modules/<name>` straight
    /// at the directory, like npm does. `None` for every registry package.
    pub local: Option<LocalSource>,
}

/// Where a local (non-registry) Node package lives, as declared by the project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalSource {
    /// The path exactly as declared, with `/` separators: relative to the
    /// directory holding the manifest (npm's convention for both the lockfile
    /// `resolved` field and a `file:` spec), or absolute.
    pub path: String,
    /// Human-readable origin (which file, which entry) — quoted verbatim in
    /// errors so a broken local dep can be traced back to its declaration.
    pub origin: String,
}

impl NodeDep {
    /// True for a local (`file:`/`link:`) package, false for a registry package.
    pub fn is_local(&self) -> bool {
        self.local.is_some()
    }

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
        let lock_name = path.file_name().map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "package-lock.json".into());
        for (key, value) in packages {
            // Skip the root entry (empty key)
            if key.is_empty() {
                continue;
            }

            // Symlink entries ("link": true). Two very different things share
            // this shape:
            //  - pnpm virtual-store redirects embedded in an npm lockfile
            //    (`resolved: "node_modules/.pnpm/…"`): the target is itself a
            //    `node_modules/…` entry parsed as a registry package below, so
            //    the link adds nothing — skip it (unchanged behaviour);
            //  - a LOCAL package: `file:`/`link:` dependency or workspace member
            //    (`resolved: "../vendor/atoms"`). npm symlinks
            //    `node_modules/<name>` to that directory; so does `kong use`.
            if value.get("link").and_then(|v| v.as_bool()).unwrap_or(false) {
                if let Some(dep) = local_dep_from_link(key, value, packages, &lock_name) {
                    deps.push(dep);
                }
                continue;
            }

            // A key that does not START with `node_modules/` is not something
            // npm installs into this project's tree: it is a link TARGET or a
            // workspace member spelled as its path relative to the project
            // (`"../vendor/atoms"`, `"packages/lib"`), or a package inside such
            // a directory's own tree (`"../lib/node_modules/x"`). Its metadata
            // describes the directory a `link: true` entry points at — it is
            // never fetched from the registry. Stale targets that no link
            // references any more (`"../../CRM_Atoms": {"extraneous": true, …}`,
            // left behind after the `file:` spec moved) land here too and are
            // dropped. Before this guard kong asked npm for
            // `../../CRM_Atoms@0.1.0` and aborted the whole `kong rules`.
            //
            // One exception keeps today's behaviour: a workspace member's own
            // nested install inside the project (`"packages/lib/node_modules/x"`)
            // is still hoisted flat by its leaf name, as before.
            if !key.starts_with("node_modules/") {
                let inside_project_nested = key.contains("/node_modules/")
                    && !key.starts_with("../")
                    && !key.starts_with('/')
                    && !has_drive_prefix(key);
                if !inside_project_nested {
                    debug!(key = %key, "Skipping lockfile entry outside node_modules (link target / workspace path)");
                    continue;
                }
            }

            // A `node_modules/<name>` entry installed from a local path rather
            // than the registry (`resolved: "file:…"`, what npm writes with
            // `install-links=true`). Asking npm for `<name>@<version>` would at
            // best fail and at worst download an unrelated namesake.
            if let Some(res) = value.get("resolved").and_then(|v| v.as_str()) {
                if let Some(p) = res.strip_prefix("file:") {
                    let name = leaf_name(key);
                    if is_tarball_path(p) {
                        bail!(
                            "{lock_name}: entry \"{key}\" is a local tarball dependency \
                             (resolved: \"{res}\") — kong links local DIRECTORIES but does not \
                             install local .tgz files yet. Depend on the unpacked directory \
                             (\"file:<dir>\") or publish the package to a registry."
                        );
                    }
                    let version = value.get("version").and_then(|v| v.as_str())
                        .unwrap_or("0.0.0").to_string();
                    deps.push(NodeDep {
                        name,
                        version,
                        _resolved: Some(res.to_string()),
                        _integrity: None,
                        os: Vec::new(),
                        cpu: Vec::new(),
                        libc: Vec::new(),
                        optional: false,
                        local: Some(LocalSource {
                            path: p.replace('\\', "/"),
                            origin: format!("{lock_name} entry \"{key}\" (resolved: \"{res}\")"),
                        }),
                    });
                    continue;
                }
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
                    local: None,
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

        // v1 lockfiles spell a local dependency as `"version": "file:../lib"`.
        // It is a directory to link, never a registry version.
        if let Some(p) = version.strip_prefix("file:").or_else(|| version.strip_prefix("link:")) {
            if !is_tarball_path(p) {
                out.push(NodeDep {
                    name: name.clone(),
                    version: "0.0.0".into(),
                    _resolved: None,
                    _integrity: None,
                    os: Vec::new(),
                    cpu: Vec::new(),
                    libc: Vec::new(),
                    optional: false,
                    local: Some(LocalSource {
                        path: p.replace('\\', "/"),
                        origin: format!("package-lock.json (v1) dependency \"{name}\": \"{version}\""),
                    }),
                });
            }
            continue;
        }

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
                local: None,
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

                // `file:<dir>` / `link:<dir>`: a local package. npm (default
                // `install-links=false`) symlinks node_modules/<name> to the
                // directory; the path is relative to this package.json.
                if let Some(p) = version_str
                    .strip_prefix("file:")
                    .or_else(|| version_str.strip_prefix("link:"))
                {
                    if is_tarball_path(p) {
                        bail!(
                            "{}: {section} \"{name}\": \"{version_str}\" is a local tarball — kong links \
                             local DIRECTORIES but does not install local .tgz files yet. Depend on the \
                             unpacked directory (\"file:<dir>\") or publish the package to a registry.",
                            path.display()
                        );
                    }
                    deps.push(NodeDep {
                        name: name.clone(),
                        version: "0.0.0".into(),
                        _resolved: None,
                        _integrity: None,
                        os: Vec::new(),
                        cpu: Vec::new(),
                        libc: Vec::new(),
                        optional: false,
                        local: Some(LocalSource {
                            path: p.replace('\\', "/"),
                            origin: format!("package.json {section} \"{name}\": \"{version_str}\""),
                        }),
                    });
                    continue;
                }
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
                        local: None,
                    });
                }
            }
        }
    }

    Ok(deps)
}

/// Package name from a lockfile key: the text after the LAST `node_modules/`.
fn leaf_name(key: &str) -> String {
    match key.rfind("node_modules/") {
        Some(pos) => key[pos + "node_modules/".len()..].to_string(),
        None => key.to_string(),
    }
}

/// `C:/…` or `C:\\…` — a Windows absolute path.
fn has_drive_prefix(p: &str) -> bool {
    let b = p.as_bytes();
    b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'/' || b[2] == b'\\')
}

/// A local spec pointing at a packed tarball rather than a directory.
fn is_tarball_path(p: &str) -> bool {
    let l = p.to_ascii_lowercase();
    l.ends_with(".tgz") || l.ends_with(".tar.gz") || l.ends_with(".tar")
}

/// Turn an npm `link: true` lockfile entry into a local dependency, or `None`
/// when the link is not a local package.
///
/// `resolved` is the link target, relative to the lockfile's directory. When it
/// points back INSIDE a `node_modules` tree (pnpm's `.pnpm` virtual store
/// embedded in an npm lockfile), the target is itself a registry entry parsed
/// on its own, so the link is skipped exactly as before. Otherwise the target
/// is a local directory (a `file:`/`link:` dep or a workspace member), whose
/// own entry — keyed by that same path — carries the version.
fn local_dep_from_link(
    key: &str,
    value: &serde_json::Value,
    packages: &serde_json::Map<String, serde_json::Value>,
    lock_name: &str,
) -> Option<NodeDep> {
    // Only a link installed at the project's top level is placed by kong
    // (flat layout). A nested one would need nested placement; skip it loudly.
    let resolved = value.get("resolved").and_then(|v| v.as_str())?;
    let norm = resolved.replace('\\', "/");
    if norm.starts_with("node_modules/") || norm.contains("/node_modules/") {
        return None;
    }
    if !key.starts_with("node_modules/") || key["node_modules/".len()..].contains("/node_modules/") {
        tracing::warn!(
            key = %key, resolved = %resolved,
            "Nested local link in {lock_name} is not placed by kong (flat node_modules); skipping"
        );
        return None;
    }
    let version = packages
        .get(resolved)
        .and_then(|t| t.get("version"))
        .and_then(|v| v.as_str())
        .unwrap_or("0.0.0")
        .to_string();
    Some(NodeDep {
        name: leaf_name(key),
        version,
        _resolved: Some(resolved.to_string()),
        _integrity: None,
        os: Vec::new(),
        cpu: Vec::new(),
        libc: Vec::new(),
        optional: value.get("optional").and_then(|v| v.as_bool()).unwrap_or(false),
        local: Some(LocalSource {
            path: norm,
            origin: format!("{lock_name} entry \"{key}\" (link → \"{resolved}\")"),
        }),
    })
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

    /// The real nomorechat shape: a `file:` dep (link + its target entry), a
    /// stale extraneous target left from an older link, and registry packages.
    const LOCAL_LOCK: &str = r#"{
        "name": "nomorechat-app",
        "lockfileVersion": 3,
        "packages": {
            "": { "name": "nomorechat-app", "dependencies": { "@real3d/atoms": "file:../vendor/atoms", "vue": "^3.5.0" } },
            "../../CRM_Atoms": {
                "name": "@real3d/atoms", "version": "0.1.0", "extraneous": true,
                "dependencies": { "qrcode": "^1.5.4" }
            },
            "../vendor/atoms": {
                "name": "@real3d/atoms", "version": "0.1.0",
                "peerDependencies": { "vue": "^3.5.0" }
            },
            "../vendor/atoms/node_modules/vue": { "version": "3.5.34", "dev": true },
            "node_modules/@real3d/atoms": { "resolved": "../vendor/atoms", "link": true },
            "node_modules/vue": {
                "version": "3.5.13",
                "resolved": "https://registry.npmjs.org/vue/-/vue-3.5.13.tgz"
            },
            "node_modules/@vue/shared": {
                "version": "3.5.13",
                "resolved": "https://registry.npmjs.org/@vue/shared/-/shared-3.5.13.tgz"
            }
        }
    }"#;

    #[test]
    fn lockfile_local_link_target_and_stale_extraneous_target() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), LOCAL_LOCK).unwrap();
        let deps = parse_package_lock(tmp.path()).unwrap();

        let registry: Vec<(&str, &str)> = deps.iter().filter(|d| !d.is_local())
            .map(|d| (d.name.as_str(), d.version.as_str())).collect();
        assert_eq!(registry, vec![("@vue/shared", "3.5.13"), ("vue", "3.5.13")]);

        let locals: Vec<&NodeDep> = deps.iter().filter(|d| d.is_local()).collect();
        assert_eq!(locals.len(), 1, "exactly one local dep: {deps:?}");
        assert_eq!(locals[0].name, "@real3d/atoms");
        assert_eq!(locals[0].version, "0.1.0", "version read from the link target entry");
        let src = locals[0].local.as_ref().unwrap();
        assert_eq!(src.path, "../vendor/atoms");
        assert!(src.origin.contains("node_modules/@real3d/atoms"), "{}", src.origin);

        // Nothing path-shaped may ever reach the registry client.
        assert!(deps.iter().all(|d| !d.name.starts_with('.')), "{deps:?}");
    }

    #[test]
    fn pnpm_virtual_store_links_are_still_skipped() {
        let json = r#"{
            "lockfileVersion": 3,
            "packages": {
                "": { "name": "app" },
                "node_modules/express": { "resolved": "node_modules/.pnpm/express@4.18.2/node_modules/express", "link": true },
                "node_modules/.pnpm/express@4.18.2/node_modules/express": { "version": "4.18.2" }
            }
        }"#;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), json).unwrap();
        let deps = parse_package_lock(tmp.path()).unwrap();
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].name, "express");
        assert!(!deps[0].is_local());
    }

    #[test]
    fn workspace_member_nested_install_keeps_legacy_flat_behaviour() {
        // `packages/lib/node_modules/x` (inside the project) was hoisted flat
        // before this change; that must not regress.
        let json = r#"{
            "lockfileVersion": 3,
            "packages": {
                "": { "name": "app", "workspaces": ["packages/lib"] },
                "node_modules/lib": { "resolved": "packages/lib", "link": true },
                "packages/lib": { "name": "lib", "version": "1.0.0" },
                "packages/lib/node_modules/is-number": { "version": "7.0.0" }
            }
        }"#;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), json).unwrap();
        let deps = parse_package_lock(tmp.path()).unwrap();
        let reg: Vec<&str> = deps.iter().filter(|d| !d.is_local()).map(|d| d.name.as_str()).collect();
        assert_eq!(reg, vec!["is-number"]);
        let loc: Vec<(&str, &str)> = deps.iter().filter_map(|d| d.local.as_ref().map(|l| (d.name.as_str(), l.path.as_str()))).collect();
        assert_eq!(loc, vec![("lib", "packages/lib")]);
    }

    #[test]
    fn install_links_style_file_resolved_is_local() {
        // `npm install --install-links` writes the package as a real entry
        // with `resolved: "file:../lib"`; its deps nest under it.
        let json = r#"{
            "lockfileVersion": 3,
            "packages": {
                "": { "name": "app" },
                "node_modules/mylib": { "version": "1.0.0", "resolved": "file:../lib" },
                "node_modules/mylib/node_modules/is-number": { "version": "7.0.0", "resolved": "https://registry.npmjs.org/is-number/-/is-number-7.0.0.tgz" }
            }
        }"#;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), json).unwrap();
        let deps = parse_package_lock(tmp.path()).unwrap();
        let mylib = deps.iter().find(|d| d.name == "mylib").unwrap();
        assert_eq!(mylib.local.as_ref().unwrap().path, "../lib");
        assert!(deps.iter().any(|d| d.name == "is-number" && !d.is_local()));
    }

    #[test]
    fn local_tarball_errors_with_the_entry_named() {
        let json = r#"{"lockfileVersion":3,"packages":{"":{},"node_modules/x":{"version":"1.0.0","resolved":"file:../x-1.0.0.tgz"}}}"#;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), json).unwrap();
        let err = parse_package_lock(tmp.path()).unwrap_err().to_string();
        assert!(err.contains("node_modules/x") && err.contains("tarball"), "{err}");
    }

    #[test]
    fn package_json_file_and_link_specs_are_local() {
        let json = r#"{
            "name": "embedded",
            "dependencies": {
                "@real3d/atoms": "file:Q:/WebProjects/nomorechat/vendor/atoms",
                "vue": "^3.5.0"
            },
            "devDependencies": { "tool": "link:../tool", "vite": "^8.0.11" }
        }"#;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), json).unwrap();
        let deps = parse_package_json(tmp.path()).unwrap();
        let atoms = deps.iter().find(|d| d.name == "@real3d/atoms").unwrap();
        assert_eq!(atoms.local.as_ref().unwrap().path, "Q:/WebProjects/nomorechat/vendor/atoms");
        let tool = deps.iter().find(|d| d.name == "tool").unwrap();
        assert_eq!(tool.local.as_ref().unwrap().path, "../tool");
        assert!(tool.local.as_ref().unwrap().origin.contains("devDependencies"));
        let vue = deps.iter().find(|d| d.name == "vue").unwrap();
        assert!(!vue.is_local());
        assert_eq!(vue.version, "3.5.0");
    }

    #[test]
    fn v1_lockfile_file_spec_is_local() {
        let json = r#"{"lockfileVersion":1,"dependencies":{
            "mylib":{"version":"file:../lib"},
            "is-number":{"version":"7.0.0","resolved":"https://r/is-number-7.0.0.tgz"}}}"#;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), json).unwrap();
        let deps = parse_package_lock(tmp.path()).unwrap();
        assert_eq!(deps.iter().find(|d| d.name == "mylib").unwrap().local.as_ref().unwrap().path, "../lib");
        assert!(!deps.iter().find(|d| d.name == "is-number").unwrap().is_local());
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
            local: None,
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
