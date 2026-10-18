use std::path::Path;

use anyhow::{bail, Context, Result};
use tracing::{debug, info, warn};

use super::git::spec::{self, GitSource};
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
    /// Lockfile key of a NESTED install (`node_modules/a/node_modules/b`): where
    /// npm puts a second version of `b` that only `a` sees. `None` for a
    /// top-level (hoisted) entry and for every non-lockfile source.
    pub path: Option<String>,
    /// `Some` for a kong git dependency (a git spec named by a `kong.git` block):
    /// it is cloned into the store and linked from there instead of being taken
    /// from the lockfile. `None` for every registry and local package.
    pub git: Option<GitSource>,
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
        let mut deps = parse_package_lock(&lock)?;
        let pkg = project_dir.join("package.json");
        if pkg.exists() {
            prefer_absolute_manifest_paths(&mut deps, &pkg)?;
        }
        apply_git_declarations(&mut deps, project_dir)?;
        return Ok(deps);
    }

    let pkg = project_dir.join("package.json");
    if pkg.exists() {
        debug!("Found package.json");
        let mut deps = parse_package_json(&pkg)?;
        apply_git_declarations(&mut deps, project_dir)?;
        return Ok(deps);
    }

    Ok(vec![])
}

/// Replace whatever the lockfile (or package.json) says about each kong git
/// dependency by one git `NodeDep` (shared §1 "Lockfile interplay").
///
/// The project's own lockfile may still hold a stale `file:` link, an npm git
/// entry or a nested tree under `node_modules/<name>/node_modules/` from before
/// the dependency moved to git; the git dependency brings its own tree from its
/// own lockfile, so all of those are dropped. Without any declaration `deps` is
/// left untouched, so a project without `kong.git` resolves exactly as before.
fn apply_git_declarations(deps: &mut Vec<NodeDep>, project_dir: &Path) -> Result<()> {
    let sources = git_declarations(project_dir)?;
    if sources.is_empty() {
        return Ok(());
    }
    let before = deps.len();
    for src in &sources {
        let nested = format!("node_modules/{}/node_modules/", src.name);
        deps.retain(|d| {
            d.name != src.name && !d.path.as_deref().is_some_and(|p| p.starts_with(&nested))
        });
    }
    let dropped = before - deps.len();
    debug!(
        git = sources.len(),
        dropped, "Git dependencies replace their lockfile entries"
    );
    for src in sources {
        deps.push(NodeDep {
            name: src.name.clone(),
            version: "0.0.0".into(),
            _resolved: None,
            _integrity: None,
            os: Vec::new(),
            cpu: Vec::new(),
            libc: Vec::new(),
            optional: false,
            local: None,
            path: None,
            git: Some(src),
        });
    }
    Ok(())
}

/// A local (`file:`/`link:`) dependency the MANIFEST declares with an ABSOLUTE
/// path keeps that path, whatever the lockfile says.
///
/// npm always records a local dependency in package-lock.json RELATIVE to the
/// lockfile (`"resolved": "../../nomorechat/vendor/atoms"`), even when
/// package.json spells it absolute (`"file:/root/nmc/nomorechat/vendor/atoms"`).
/// The relative form breaks as soon as the project is used from another depth
/// than the one the lockfile was resolved in — a git worktree of the project, a
/// copy — and kong then aborts with "points at a directory that does not exist"
/// (measured on a nomorechat embedded project: its lockfile is committed and
/// every task runs in a worktree under another root). The absolute manifest
/// path is position-independent and is what the author wrote, so it wins.
fn prefer_absolute_manifest_paths(deps: &mut [NodeDep], manifest: &Path) -> Result<()> {
    let content = std::fs::read_to_string(manifest)
        .with_context(|| format!("failed to read {}", manifest.display()))?;
    let doc: serde_json::Value = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse JSON: {}", manifest.display()))?;
    for section in &["dependencies", "devDependencies", "optionalDependencies"] {
        let Some(map) = doc.get(section).and_then(|d| d.as_object()) else {
            continue;
        };
        for (name, spec) in map {
            let spec = spec.as_str().unwrap_or_default();
            let Some(p) = spec
                .strip_prefix("file:")
                .or_else(|| spec.strip_prefix("link:"))
            else {
                continue;
            };
            let p = p.replace('\\', "/");
            if !Path::new(&p).is_absolute() && !is_windows_absolute(&p) {
                continue;
            }
            for dep in deps.iter_mut().filter(|d| &d.name == name) {
                if let Some(local) = dep.local.as_mut() {
                    if local.path != p {
                        debug!(pkg = %name, lock = %local.path, manifest = %p, "Local dep: absolute manifest path wins over the lockfile's relative one");
                        local.path = p.clone();
                        local.origin = format!("package.json {section} \"{name}\": \"{spec}\"");
                    }
                }
            }
        }
    }
    Ok(())
}

/// `C:/x` or `C:\\x` — absolute on Windows even when this build runs elsewhere.
fn is_windows_absolute(p: &str) -> bool {
    let b = p.as_bytes();
    b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'/' || b[2] == b'\\')
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
                dep.name,
                dep.version,
                dep.os,
                dep.cpu,
                dep.libc,
                host.os,
                host.cpu,
                host.libc
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
        let lock_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
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
                    let version = value
                        .get("version")
                        .and_then(|v| v.as_str())
                        .unwrap_or("0.0.0")
                        .to_string();
                    deps.push(NodeDep {
                        git: None,
                        path: None,
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
                    git: None,
                    path: nested_install_path(key),
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
    parse_v1_level(deps_map, None, out);
}

/// One level of a v1 `dependencies` tree. `parent` is the lockfile-style key of
/// the package whose `dependencies` this is (`None` at the top level), so a
/// nested entry records `node_modules/<parent>/node_modules/<name>`.
fn parse_v1_level(
    deps_map: &serde_json::Map<String, serde_json::Value>,
    parent: Option<&str>,
    out: &mut Vec<NodeDep>,
) {
    for (name, value) in deps_map {
        let key = match parent {
            Some(p) => format!("{p}/node_modules/{name}"),
            None => format!("node_modules/{name}"),
        };
        let version = value
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();

        // v1 lockfiles spell a local dependency as `"version": "file:../lib"`.
        // It is a directory to link, never a registry version.
        if let Some(p) = version
            .strip_prefix("file:")
            .or_else(|| version.strip_prefix("link:"))
        {
            if !is_tarball_path(p) {
                out.push(NodeDep {
                    git: None,
                    path: None,
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
                        origin: format!(
                            "package-lock.json (v1) dependency \"{name}\": \"{version}\""
                        ),
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
                git: None,
                path: if parent.is_some() {
                    Some(key.clone())
                } else {
                    None
                },
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
            parse_v1_level(nested, Some(&key), out);
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
                        git: None,
                        path: None,
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
                        git: None,
                        path: None,
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

/// Dependency sections a `kong.git` entry may point into, in npm's order.
const GIT_DEP_SECTIONS: [&str; 3] = ["dependencies", "devDependencies", "optionalDependencies"];

/// Collect the kong git dependencies declared by the project: the root
/// `package.json` plus each workspace member's `package.json` (shared §1).
/// Sorted by name. A project whose manifests carry no `kong` key gets an empty
/// vec, so its resolution stays exactly what it was before git dependencies.
pub fn git_declarations(project_dir: &Path) -> Result<Vec<GitSource>> {
    let root_path = project_dir.join("package.json");
    if !root_path.is_file() {
        return Ok(Vec::new());
    }
    let content = std::fs::read_to_string(&root_path)
        .with_context(|| format!("Failed to read {}", root_path.display()))?;
    let root: serde_json::Value = serde_json::from_str(&content)
        .with_context(|| format!("Failed to parse {}", root_path.display()))?;
    let root_has_kong = root.get("kong").is_some();

    // Manifests that carry a `kong` key: (path relative to the project, JSON).
    let mut manifests: Vec<(String, serde_json::Value)> = Vec::new();
    for member in workspace_members(project_dir, &root) {
        let rel = format!("{member}/package.json");
        let path = project_dir.join(&rel);
        let parsed = std::fs::read_to_string(&path)
            .map_err(anyhow::Error::from)
            .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).map_err(Into::into));
        match parsed {
            Ok(json) if json.get("kong").is_some() => manifests.push((rel, json)),
            Ok(_) => {}
            // Before git dependencies kong never read member manifests; a broken
            // one must not fail a project that declares nothing from its root.
            Err(e) if !root_has_kong => {
                warn!(manifest = %rel, error = %e, "skipping unreadable workspace manifest");
            }
            Err(e) => return Err(e.context(format!("Failed to parse {}", path.display()))),
        }
    }
    if root_has_kong {
        manifests.insert(0, ("package.json".to_string(), root));
    }

    let mut sources: Vec<GitSource> = Vec::new();
    for (manifest, json) in &manifests {
        let Some(block) = json.get("kong").and_then(|k| k.get("git")) else {
            continue;
        };
        let Some(block) = block.as_object() else {
            bail!(
                "invalid kong.git block:\n  \
                 declared by : {manifest} (kong.git)\n  \
                 problem     : expected an object mapping dependency names to entries, found {block}"
            );
        };
        for (name, value) in block {
            let entry = spec::parse_kong_git_entry(name, value, manifest)?;
            let declared = GIT_DEP_SECTIONS.iter().find_map(|section| {
                json.get(*section)
                    .and_then(|s| s.get(name))
                    .map(|v| (*section, v))
            });
            let (section, spec_str) = match declared {
                Some((section, serde_json::Value::String(s))) if spec::is_git_spec(s) => {
                    (section, s.clone())
                }
                Some((section, v)) => bail!(
                    "git dependency '{name}' is not declared with a git spec:\n  \
                     declared by : {manifest} (kong.git \"{name}\")\n  \
                     dependency  : {manifest} {section} \"{name}\": {v}\n\
                     Use a git spec (github:ORG/REPO#ref, git+https://…, git+ssh://…, git+file://…) \
                     as the dependency value, or remove the kong.git entry."
                ),
                None => bail!(
                    "git dependency '{name}' is missing from the dependencies:\n  \
                     declared by : {manifest} (kong.git \"{name}\")\n  \
                     dependency  : not found in {manifest} dependencies, devDependencies or optionalDependencies\n\
                     Add \"{name}\": \"<git spec>\" to {manifest} dependencies, or remove the kong.git entry."
                ),
            };
            let origin =
                format!("{manifest} {section} \"{name}\": \"{spec_str}\" (kong.git \"{name}\")");
            let (repo, reference) = spec::parse_git_spec(&spec_str).map_err(|e| {
                anyhow::anyhow!(
                    "git dependency '{name}' has an invalid source:\n  \
                     source      : {spec_str}\n  \
                     commit      : (unresolved)\n  \
                     declared by : {origin}\n\
                     {e}"
                )
            })?;
            if let Some(prev) = sources.iter().find(|s| &s.name == name) {
                bail!(
                    "git dependency '{name}' is declared twice:\n  \
                     declared by : {}\n  \
                     and by      : {origin}\n\
                     Declare each git dependency in one package.json only (it is hoisted to the \
                     project's node_modules/{name}).",
                    prev.origin
                );
            }
            debug!(pkg = %name, manifest = %manifest, repo = %repo, "kong git dependency declared");
            sources.push(GitSource {
                name: name.clone(),
                spec: spec_str,
                repo,
                reference,
                path: entry.path,
                build: entry.build,
                rust_targets: entry.rust_targets,
                origin,
                declared_by: manifest.clone(),
            });
        }
    }
    sources.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(sources)
}

/// Workspace member directories (relative, `/` separators) from the root
/// `workspaces` field: an array, or an object's `packages` array. An entry is a
/// literal directory or `dir/*` (every direct subdirectory holding a
/// package.json); any other glob is ignored with a warning.
fn workspace_members(project_dir: &Path, root: &serde_json::Value) -> Vec<String> {
    let entries = match root.get("workspaces") {
        Some(serde_json::Value::Array(a)) => a.clone(),
        Some(serde_json::Value::Object(o)) => o
            .get("packages")
            .and_then(|p| p.as_array())
            .cloned()
            .unwrap_or_default(),
        _ => return Vec::new(),
    };
    let mut members: Vec<String> = Vec::new();
    for entry in entries.iter().filter_map(|e| e.as_str()) {
        let norm = entry.replace('\\', "/");
        let norm = norm.trim_start_matches("./").trim_end_matches('/');
        let mut found: Vec<String> = Vec::new();
        if let Some(parent) = norm.strip_suffix("/*").filter(|p| !has_glob(p)) {
            let Ok(read) = std::fs::read_dir(project_dir.join(parent)) else {
                continue;
            };
            for e in read.flatten() {
                if e.path().join("package.json").is_file() {
                    if let Some(n) = e.file_name().to_str() {
                        found.push(format!("{parent}/{n}"));
                    }
                }
            }
            found.sort();
        } else if has_glob(norm) {
            warn!(workspace = %entry, "unsupported workspace glob ignored for kong.git declarations (only `dir` and `dir/*` are read)");
        } else if !norm.is_empty() && project_dir.join(norm).join("package.json").is_file() {
            found.push(norm.to_string());
        }
        for m in found {
            if !members.contains(&m) {
                members.push(m);
            }
        }
    }
    members
}

fn has_glob(p: &str) -> bool {
    p.contains(['*', '?', '[', ']', '{', '}', '!'])
}

/// Install location of a NESTED lockfile entry, or `None` for a top-level one.
///
/// `node_modules/path-scurry/node_modules/lru-cache` is a second version of
/// `lru-cache` that only `path-scurry` resolves; npm keeps it there and the
/// hoisted `node_modules/lru-cache` is a different version. Only keys that
/// start with `node_modules/` and contain a further `/node_modules/` count.
/// pnpm virtual-store keys (`node_modules/.pnpm/…`) and workspace-member keys
/// keep the historical flat behaviour.
fn nested_install_path(key: &str) -> Option<String> {
    if !key.starts_with("node_modules/") || key.contains("/.pnpm/") {
        return None;
    }
    if key["node_modules/".len()..].contains("/node_modules/") {
        Some(key.to_string())
    } else {
        None
    }
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
    if !key.starts_with("node_modules/") || key["node_modules/".len()..].contains("/node_modules/")
    {
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
        git: None,
        path: None,
        name: leaf_name(key),
        version,
        _resolved: Some(resolved.to_string()),
        _integrity: None,
        os: Vec::new(),
        cpu: Vec::new(),
        libc: Vec::new(),
        optional: value
            .get("optional")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
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

        let registry: Vec<(&str, &str)> = deps
            .iter()
            .filter(|d| !d.is_local())
            .map(|d| (d.name.as_str(), d.version.as_str()))
            .collect();
        assert_eq!(registry, vec![("@vue/shared", "3.5.13"), ("vue", "3.5.13")]);

        let locals: Vec<&NodeDep> = deps.iter().filter(|d| d.is_local()).collect();
        assert_eq!(locals.len(), 1, "exactly one local dep: {deps:?}");
        assert_eq!(locals[0].name, "@real3d/atoms");
        assert_eq!(
            locals[0].version, "0.1.0",
            "version read from the link target entry"
        );
        let src = locals[0].local.as_ref().unwrap();
        assert_eq!(src.path, "../vendor/atoms");
        assert!(
            src.origin.contains("node_modules/@real3d/atoms"),
            "{}",
            src.origin
        );

        // Nothing path-shaped may ever reach the registry client.
        assert!(deps.iter().all(|d| !d.name.starts_with('.')), "{deps:?}");
    }

    /// npm writes a local dep RELATIVE in the lockfile even when package.json
    /// declares it absolute; from another depth (a worktree) the relative path
    /// points nowhere. The absolute manifest path must win.
    #[test]
    fn absolute_manifest_file_path_wins_over_relative_lockfile_path() {
        let root = tempfile::TempDir::new().unwrap();
        let vendor = root.path().join("vendor").join("atoms");
        std::fs::create_dir_all(&vendor).unwrap();
        std::fs::write(
            vendor.join("package.json"),
            r#"{"name":"@real3d/atoms","version":"0.1.0"}"#,
        )
        .unwrap();
        // The project sits at a depth the lockfile's relative path was NOT resolved from.
        let app = root.path().join("worktrees").join("7").join("12");
        std::fs::create_dir_all(&app).unwrap();
        let abs = vendor.to_string_lossy().replace('\\', "/");
        std::fs::write(
            app.join("package.json"),
            format!(
                r#"{{"name":"p","dependencies":{{"@real3d/atoms":"file:{abs}","vue":"^3.5.0"}}}}"#
            ),
        )
        .unwrap();
        std::fs::write(app.join("package-lock.json"), r#"{
            "lockfileVersion": 3,
            "packages": {
                "": { "name": "p" },
                "../../vendor/atoms": { "name": "@real3d/atoms", "version": "0.1.0" },
                "node_modules/@real3d/atoms": { "resolved": "../../vendor/atoms", "link": true },
                "node_modules/vue": { "version": "3.5.13", "resolved": "https://registry.npmjs.org/vue/-/vue-3.5.13.tgz" }
            }
        }"#).unwrap();
        let deps = parse_all(&app).unwrap();
        let atoms = deps.iter().find(|d| d.name == "@real3d/atoms").unwrap();
        let local = atoms.local.as_ref().unwrap();
        assert_eq!(local.path, abs, "absolute manifest path kept: {deps:?}");
        assert!(
            local.origin.starts_with("package.json dependencies"),
            "{}",
            local.origin
        );
        assert_eq!(
            atoms.version, "0.1.0",
            "version still read from the lockfile"
        );
        let entry = crate::config::local_entry_for(&app, &atoms.name, local).unwrap();
        assert_eq!(entry.resolve(&app), vendor);
        // registry packages untouched
        assert!(deps
            .iter()
            .any(|d| d.name == "vue" && d.version == "3.5.13" && d.local.is_none()));
    }

    /// A RELATIVE manifest path leaves the lockfile's entry exactly as before.
    #[test]
    fn relative_manifest_file_path_keeps_lockfile_path() {
        let root = tempfile::TempDir::new().unwrap();
        let app = root.path().join("app");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(
            app.join("package.json"),
            r#"{"name":"p","dependencies":{"@real3d/atoms":"file:../vendor/atoms"}}"#,
        )
        .unwrap();
        std::fs::write(
            app.join("package-lock.json"),
            r#"{
            "lockfileVersion": 3,
            "packages": {
                "": { "name": "p" },
                "../vendor/atoms": { "name": "@real3d/atoms", "version": "0.1.0" },
                "node_modules/@real3d/atoms": { "resolved": "../vendor/atoms", "link": true }
            }
        }"#,
        )
        .unwrap();
        let deps = parse_all(&app).unwrap();
        let local = deps[0].local.as_ref().unwrap();
        assert_eq!(local.path, "../vendor/atoms");
        assert!(
            local.origin.starts_with("package-lock.json"),
            "{}",
            local.origin
        );
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
        let reg: Vec<&str> = deps
            .iter()
            .filter(|d| !d.is_local())
            .map(|d| d.name.as_str())
            .collect();
        assert_eq!(reg, vec!["is-number"]);
        let loc: Vec<(&str, &str)> = deps
            .iter()
            .filter_map(|d| d.local.as_ref().map(|l| (d.name.as_str(), l.path.as_str())))
            .collect();
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
        assert!(
            err.contains("node_modules/x") && err.contains("tarball"),
            "{err}"
        );
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
        assert_eq!(
            atoms.local.as_ref().unwrap().path,
            "Q:/WebProjects/nomorechat/vendor/atoms"
        );
        let tool = deps.iter().find(|d| d.name == "tool").unwrap();
        assert_eq!(tool.local.as_ref().unwrap().path, "../tool");
        assert!(tool
            .local
            .as_ref()
            .unwrap()
            .origin
            .contains("devDependencies"));
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
        assert_eq!(
            deps.iter()
                .find(|d| d.name == "mylib")
                .unwrap()
                .local
                .as_ref()
                .unwrap()
                .path,
            "../lib"
        );
        assert!(!deps
            .iter()
            .find(|d| d.name == "is-number")
            .unwrap()
            .is_local());
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
            git: None,
            path: None,
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
            dep(
                "@rspack/binding-linux-x64-musl",
                &["linux"],
                &["x64"],
                &["musl"],
                true,
            ), // skip
            dep(
                "@rspack/binding-linux-x64-gnu",
                &["linux"],
                &["x64"],
                &["glibc"],
                true,
            ), // keep
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

    #[test]
    fn nested_installs_keep_their_lockfile_path() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            tmp.path(),
            include_str!("fixtures/nested-installs.package-lock.json"),
        )
        .unwrap();
        let deps = parse_package_lock(tmp.path()).unwrap();
        assert_eq!(deps.len(), 5);
        let find = |v: &str| {
            deps.iter()
                .find(|d| d.name == "lru-cache" && d.version == v)
                .unwrap()
        };
        assert_eq!(find("5.1.1").path, None, "top-level entry has no path");
        assert_eq!(
            find("11.5.2").path.as_deref(),
            Some("node_modules/path-scurry/node_modules/lru-cache")
        );
        let inner = deps.iter().find(|d| d.name == "@scope/inner").unwrap();
        assert_eq!(
            inner.path.as_deref(),
            Some("node_modules/@scope/outer/node_modules/@scope/inner")
        );
        assert!(deps
            .iter()
            .find(|d| d.name == "path-scurry")
            .unwrap()
            .path
            .is_none());
    }

    #[test]
    fn pnpm_virtual_store_keys_are_not_nested_installs() {
        assert_eq!(
            nested_install_path("node_modules/.pnpm/express@4.18.2/node_modules/express"),
            None
        );
        assert_eq!(nested_install_path("node_modules/express"), None);
        assert_eq!(nested_install_path("packages/lib/node_modules/x"), None);
    }

    #[test]
    fn v1_nested_dependencies_record_their_path() {
        let json = r#"{"lockfileVersion":1,"dependencies":{
            "a":{"version":"1.0.0","dependencies":{"b":{"version":"2.0.0","dependencies":{"c":{"version":"3.0.0"}}}}},
            "b":{"version":"1.5.0"}}}"#;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), json).unwrap();
        let deps = parse_package_lock(tmp.path()).unwrap();
        let p = |n: &str, v: &str| {
            deps.iter()
                .find(|d| d.name == n && d.version == v)
                .unwrap()
                .path
                .clone()
        };
        assert_eq!(p("a", "1.0.0"), None);
        assert_eq!(p("b", "1.5.0"), None);
        assert_eq!(
            p("b", "2.0.0").as_deref(),
            Some("node_modules/a/node_modules/b")
        );
        assert_eq!(
            p("c", "3.0.0").as_deref(),
            Some("node_modules/a/node_modules/b/node_modules/c")
        );
    }

    fn write_manifest(dir: &Path, rel: &str, json: &str) {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, json).unwrap();
    }

    #[test]
    fn git_declarations_from_root() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_manifest(
            tmp.path(),
            "package.json",
            r#"{ "name": "app",
                 "dependencies": { "vue": "^3.5.0", "@fx/core": "git+https://github.com/fx/core.git#v1.0.0" },
                 "kong": { "git": { "@fx/core": { "path": "./packages/core/", "build": "scripts/build.sh" } } } }"#,
        );
        let srcs = git_declarations(tmp.path()).unwrap();
        assert_eq!(srcs.len(), 1);
        let s = &srcs[0];
        assert_eq!(s.name, "@fx/core");
        assert_eq!(s.repo, "https://github.com/fx/core.git");
        assert_eq!(s.reference, spec::GitRef::Name("v1.0.0".into()));
        assert_eq!(s.path, "packages/core");
        assert_eq!(s.build.as_deref(), Some("scripts/build.sh"));
        assert_eq!(s.declared_by, "package.json");
        assert_eq!(
            s.origin,
            r#"package.json dependencies "@fx/core": "git+https://github.com/fx/core.git#v1.0.0" (kong.git "@fx/core")"#
        );
    }

    #[test]
    fn git_declarations_from_workspace_member() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_manifest(
            tmp.path(),
            "package.json",
            r#"{ "name": "mono", "workspaces": ["apps/*", "packages/**"] }"#,
        );
        write_manifest(tmp.path(), "apps/api/package.json", r#"{ "name": "api" }"#);
        write_manifest(
            tmp.path(),
            "apps/web/package.json",
            r#"{ "name": "web",
                 "devDependencies": { "@fx/raa": "github:fx/raa#main" },
                 "kong": { "git": { "@fx/raa": {} } } }"#,
        );
        let srcs = git_declarations(tmp.path()).unwrap();
        assert_eq!(srcs.len(), 1);
        assert_eq!(srcs[0].declared_by, "apps/web/package.json");
        assert_eq!(srcs[0].path, "");
        assert_eq!(
            srcs[0].origin,
            r#"apps/web/package.json devDependencies "@fx/raa": "github:fx/raa#main" (kong.git "@fx/raa")"#
        );
    }

    #[test]
    fn git_declarations_duplicate_across_manifests_errors() {
        let tmp = tempfile::TempDir::new().unwrap();
        let decl = r#""dependencies": { "@fx/core": "github:fx/core" },
                      "kong": { "git": { "@fx/core": {} } }"#;
        write_manifest(
            tmp.path(),
            "package.json",
            &format!(
                r#"{{ "name": "mono", "workspaces": {{ "packages": ["apps/web"] }}, {decl} }}"#
            ),
        );
        write_manifest(
            tmp.path(),
            "apps/web/package.json",
            &format!(r#"{{ "name": "web", {decl} }}"#),
        );
        let err = format!("{:#}", git_declarations(tmp.path()).unwrap_err());
        assert!(err.contains("@fx/core"), "{err}");
        assert!(err.contains("package.json dependencies"), "{err}");
        assert!(err.contains("apps/web/package.json"), "{err}");
    }

    #[test]
    fn git_declarations_name_missing_from_dependencies_errors() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_manifest(
            tmp.path(),
            "package.json",
            r#"{ "dependencies": { "vue": "^3.5.0" }, "kong": { "git": { "@fx/core": {} } } }"#,
        );
        let err = format!("{:#}", git_declarations(tmp.path()).unwrap_err());
        assert!(err.contains("@fx/core"), "{err}");
        assert!(
            err.contains("package.json (kong.git \"@fx/core\")"),
            "{err}"
        );

        // Present, but not a git spec: the error names the entry and the dependency.
        write_manifest(
            tmp.path(),
            "package.json",
            r#"{ "dependencies": { "@fx/core": "^1.0.0" }, "kong": { "git": { "@fx/core": {} } } }"#,
        );
        let err = format!("{:#}", git_declarations(tmp.path()).unwrap_err());
        assert!(err.contains("kong.git \"@fx/core\""), "{err}");
        assert!(err.contains("dependencies \"@fx/core\""), "{err}");
    }

    #[test]
    fn git_declarations_without_kong_key_is_empty() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_manifest(
            tmp.path(),
            "package.json",
            r#"{ "workspaces": ["apps/*"], "dependencies": { "x": "github:fx/x" } }"#,
        );
        write_manifest(tmp.path(), "apps/web/package.json", r#"{ "name": "web" }"#);
        assert!(git_declarations(tmp.path()).unwrap().is_empty());
        let empty = tempfile::TempDir::new().unwrap();
        assert!(git_declarations(empty.path()).unwrap().is_empty());
    }

    #[test]
    fn git_declarations_github_and_git_https_give_same_repo() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_manifest(
            tmp.path(),
            "package.json",
            r#"{ "dependencies": { "b": "github:Fx/Core#v1", "a": "git+https://github.com/Fx/Core.git#v1" },
                 "kong": { "git": { "b": {}, "a": {} } } }"#,
        );
        let srcs = git_declarations(tmp.path()).unwrap();
        let names: Vec<&str> = srcs.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["a", "b"]);
        assert_eq!(srcs[0].repo, srcs[1].repo);
        assert_eq!(srcs[0].repo, "https://github.com/Fx/Core.git");
    }
    /// A project lockfile from before `@fx/core` moved to git: a `link: true`
    /// entry, its target, and a nested install under the old link.
    const STALE_GIT_LOCK: &str = r#"{
        "name": "app",
        "lockfileVersion": 3,
        "packages": {
            "": { "name": "app", "dependencies": { "@fx/core": "file:../core", "vue": "^3.5.0" } },
            "../core": { "name": "@fx/core", "version": "0.9.0" },
            "node_modules/@fx/core": { "resolved": "../core", "link": true },
            "node_modules/@fx/core/node_modules/x": {
                "version": "1.0.0",
                "resolved": "https://registry.npmjs.org/x/-/x-1.0.0.tgz"
            },
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

    const GIT_MANIFEST: &str = r#"{ "name": "app",
        "dependencies": { "@fx/core": "git+file:///tmp/r#v1.0.0", "vue": "^3.5.0" },
        "kong": { "git": { "@fx/core": { "path": "packages/core" } } } }"#;

    /// (name, version, path, local?, git?) of every dep, sorted, for comparisons.
    fn summary(deps: &[NodeDep]) -> Vec<(String, String, Option<String>, bool, bool)> {
        let mut v: Vec<_> = deps
            .iter()
            .map(|d| {
                (
                    d.name.clone(),
                    d.version.clone(),
                    d.path.clone(),
                    d.is_local(),
                    d.git.is_some(),
                )
            })
            .collect();
        v.sort();
        v
    }

    fn assert_single_git_core(deps: &[NodeDep]) {
        let core: Vec<&NodeDep> = deps.iter().filter(|d| d.name == "@fx/core").collect();
        assert_eq!(core.len(), 1, "{deps:?}");
        let g = core[0].git.as_ref().expect("git source");
        assert_eq!(g.repo, "file:///tmp/r");
        assert_eq!(g.path, "packages/core");
        assert_eq!(core[0].version, "0.0.0");
        assert!(core[0].local.is_none() && core[0].path.is_none());
        assert!(!core[0].has_platform_constraint());
        assert!(
            deps.iter().all(|d| !d
                .path
                .as_deref()
                .is_some_and(|p| p.starts_with("node_modules/@fx/core/"))),
            "{deps:?}"
        );
    }

    #[test]
    fn git_declaration_replaces_stale_lockfile_entries() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_manifest(tmp.path(), "package-lock.json", STALE_GIT_LOCK);
        write_manifest(
            tmp.path(),
            "package.json",
            r#"{ "name": "app", "dependencies": { "@fx/core": "file:../core", "vue": "^3.5.0" } }"#,
        );
        let before = detect_and_parse_for_host(tmp.path(), &linux_x64_glibc()).unwrap();
        assert!(before.iter().any(|d| d.name == "@fx/core" && d.is_local()));

        write_manifest(tmp.path(), "package.json", GIT_MANIFEST);
        let deps = detect_and_parse_for_host(tmp.path(), &linux_x64_glibc()).unwrap();
        assert_single_git_core(&deps);

        // Every other dep is exactly what it was before the declaration.
        let others = |d: &[NodeDep]| {
            summary(d)
                .into_iter()
                .filter(|s| s.0 != "@fx/core" && s.0 != "x")
                .collect::<Vec<_>>()
        };
        assert_eq!(others(&deps), others(&before));
        assert_eq!(deps.len(), 3, "{deps:?}");
    }

    #[test]
    fn git_declaration_in_package_json_only_branch() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_manifest(tmp.path(), "package.json", GIT_MANIFEST);
        let deps = detect_and_parse_for_host(tmp.path(), &linux_x64_glibc()).unwrap();
        assert_single_git_core(&deps);
        assert_eq!(
            summary(&deps),
            vec![
                (
                    "@fx/core".to_string(),
                    "0.0.0".to_string(),
                    None,
                    false,
                    true
                ),
                ("vue".to_string(), "3.5.0".to_string(), None, false, false),
            ]
        );
    }

    #[test]
    fn undeclared_git_spec_is_parsed_as_before() {
        let manifest = r#"{ "name": "app",
            "dependencies": { "@fx/core": "git+file:///tmp/r#v1.0.0", "vue": "^3.5.0" } }"#;
        let tmp = tempfile::TempDir::new().unwrap();
        write_manifest(tmp.path(), "package.json", manifest);
        let deps = parse_all(tmp.path()).unwrap();
        let direct = parse_package_json(&tmp.path().join("package.json")).unwrap();
        assert_eq!(summary(&deps), summary(&direct));
        assert!(deps.iter().all(|d| d.git.is_none()));

        write_manifest(tmp.path(), "package-lock.json", STALE_GIT_LOCK);
        let deps = parse_all(tmp.path()).unwrap();
        let direct = parse_package_lock(&tmp.path().join("package-lock.json")).unwrap();
        assert_eq!(summary(&deps), summary(&direct));
        assert!(deps.iter().all(|d| d.git.is_none()));
    }

    #[test]
    fn project_without_kong_key_is_unchanged() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_manifest(tmp.path(), "package-lock.json", LOCAL_LOCK);
        write_manifest(
            tmp.path(),
            "package.json",
            r#"{ "name": "nomorechat-app", "dependencies": { "@real3d/atoms": "file:../vendor/atoms", "vue": "^3.5.0" } }"#,
        );
        let deps = parse_all(tmp.path()).unwrap();
        let direct = parse_package_lock(&tmp.path().join("package-lock.json")).unwrap();
        assert_eq!(summary(&deps), summary(&direct));
        assert_eq!(
            deps.iter().map(|d| d.local.clone()).collect::<Vec<_>>(),
            direct.iter().map(|d| d.local.clone()).collect::<Vec<_>>()
        );
    }
}
