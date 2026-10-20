//! Node tarball dependencies: a `package.json` dependency whose spec is an
//! http(s) URL (typically a GitHub Release asset built by `npm pack`).
//!
//! The file is downloaded ONCE into `<store>/node/tarballs/<name>/<sha512 12 hex>/`,
//! integrity checked, and extracted; there is no clone and no build. Its own
//! dependencies come from its package.json, resolved like registry packages
//! (see `resolve`), and are assembled next to it in
//! `<that dir>/tree-<deps hash>/package/node_modules` so that Node, which
//! resolves links to their real path, finds them. `kong use` links
//! `node_modules/<name>` at `tree-*/package`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use tracing::{debug, info, warn};

use crate::config::{NodeSection, PackageEntry, TarballPackageEntry};
use crate::node::parser::NodeDep;
use crate::node::platform::HostTriple;

pub mod fetch;
pub mod resolve;
pub mod semver;

use fetch::{Expect, Integrity};

const INSTALL_MARKER: &str = ".kong-installed";

/// A tarball dependency as declared by a package.json.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TarballSource {
    pub name: String,
    /// The spec exactly as written (URL plus optional integrity fragment).
    pub spec: String,
    /// The URL without fragment.
    pub url: String,
    pub declared: Option<Integrity>,
    /// `<manifest> <section> "<name>": "<spec>"`, quoted in errors.
    pub origin: String,
    pub declared_by: String,
}

pub struct PrepareOptions<'a> {
    pub force: bool,
    pub host: HostTriple,
    /// The credential for a URL (policy lives with the caller, see `gitdep::auth`).
    pub token_for: &'a dyn Fn(&str) -> Option<String>,
    /// Registry metadata for one package name.
    pub registry: &'a dyn Fn(&str) -> Result<resolve::Packument>,
    /// Tests only: allow sending a token over plain http to the local test server.
    pub allow_plain_http_token: bool,
}

#[derive(Debug, Default, Clone)]
pub struct PrepareReport {
    /// Files downloaded (a re-run with an unchanged lock reports 0).
    pub downloaded: u32,
    /// Registry metadata documents fetched (0 when the lock is reused).
    pub registry_lookups: u32,
    pub download_seconds: f64,
}

/// Fetch the abbreviated packument of `name` from an npm registry.
pub fn fetch_packument(base: &str, name: &str) -> Result<resolve::Packument> {
    let url = format!("{}/{}", base.trim_end_matches('/'), name.replace('/', "%2f"));
    let client = reqwest::blocking::Client::builder()
        .user_agent("kong-dependency-manager/0.1")
        .timeout(std::time::Duration::from_secs(120))
        .connect_timeout(std::time::Duration::from_secs(30))
        .build()?;
    let resp = client
        .get(&url)
        .header("Accept", "application/vnd.npm.install-v1+json; q=1.0, application/json; q=0.8")
        .send()
        .with_context(|| format!("failed to fetch npm metadata for {name}"))?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        anyhow::bail!("package '{name}' not found on the npm registry");
    }
    if !resp.status().is_success() {
        anyhow::bail!("npm registry answered HTTP {} for '{name}'", resp.status());
    }
    let doc: serde_json::Value = resp
        .json()
        .with_context(|| format!("failed to parse npm metadata for {name}"))?;
    Ok(resolve::parse_packument(&doc))
}

/// Prepare every tarball dependency, in name order.
///
/// `previous` is the `node.tarballs` list of the last `kong.rules`: a matching
/// entry (same name and URL) keeps its recorded integrity and its resolved
/// packages, so a re-run with an unchanged lock does no network.
pub fn prepare_all(
    store_root: &Path,
    sources: &[TarballSource],
    previous: &[TarballPackageEntry],
    opts: &PrepareOptions<'_>,
) -> Result<(Vec<TarballPackageEntry>, PrepareReport)> {
    let mut ordered: Vec<&TarballSource> = sources.iter().collect();
    ordered.sort_by(|a, b| a.name.cmp(&b.name));
    let mut report = PrepareReport::default();
    let mut entries = Vec::with_capacity(ordered.len());
    for src in ordered {
        entries.push(prepare_one(store_root, src, previous, opts, &mut report)?);
    }
    Ok((entries, report))
}

/// §6-style diagnostic: names the dependency, the declared source, the integrity
/// and the manifest entry that declares it. Secrets are masked.
fn tb_error(
    src: &TarballSource,
    integrity: Option<&str>,
    what: &str,
    facts: &[(&str, String)],
    hint: &str,
    secrets: &[String],
) -> anyhow::Error {
    let mut text = format!(
        "tarball dependency '{}' {what}:\n  source      : {}\n  integrity   : {}\n  declared by : {}\n",
        src.name,
        src.spec,
        integrity.unwrap_or("(not recorded yet)"),
        src.origin
    );
    for (label, value) in facts {
        let value = value.replace('\n', &format!("\n{:16}", ""));
        text.push_str(&format!("  {label:<11} : {value}\n"));
    }
    text.push_str(hint);
    anyhow::anyhow!(redact(&text, secrets))
}

pub fn redact(text: &str, secrets: &[String]) -> String {
    let mut out = text.to_string();
    for s in secrets.iter().filter(|s| !s.is_empty()) {
        out = out.replace(s.as_str(), "***");
    }
    out
}

/// Resolution environment of one `prepare_one`: registry cache, store, lock.
struct PrepEnv<'a> {
    store_root: &'a Path,
    opts: &'a PrepareOptions<'a>,
    /// Packages recorded for this tarball by the previous run (integrity of nested tarballs).
    recorded: &'a [PackageEntry],
    cache: HashMap<String, resolve::Packument>,
    report: &'a mut PrepareReport,
    secrets: Vec<String>,
}

impl resolve::Env for PrepEnv<'_> {
    fn packument(&mut self, name: &str) -> Result<resolve::Packument> {
        if let Some(p) = self.cache.get(name) {
            return Ok(p.clone());
        }
        let pk = (self.opts.registry)(name)?;
        self.report.registry_lookups += 1;
        self.cache.insert(name.to_string(), pk.clone());
        Ok(pk)
    }

    fn tarball(&mut self, name: &str, spec: &str, requested_by: &str) -> Result<resolve::TarballNode> {
        let (url, declared) = fetch::parse_tarball_spec(spec).map_err(|e| {
            anyhow::anyhow!("tarball dependency '{name}' required by '{requested_by}' has an invalid source '{spec}': {e}")
        })?;
        let recorded = self
            .recorded
            .iter()
            .find(|p| p.name == name && p.source_url.as_deref() == Some(url.as_str()))
            .and_then(|p| p.hash.as_deref().and_then(Integrity::parse));
        let token = (self.opts.token_for)(&url);
        if let Some(t) = &token {
            self.secrets.push(t.clone());
        }
        let p = fetch::ensure_pristine(
            self.store_root,
            name,
            &url,
            &Expect { declared, recorded },
            token.as_deref(),
            self.opts.force,
            self.opts.allow_plain_http_token,
        )
        .map_err(|e| {
            anyhow::anyhow!(redact(
                &format!("tarball dependency '{name}' (required by '{requested_by}', {url}): {e:#}"),
                &self.secrets
            ))
        })?;
        if p.downloaded {
            self.report.downloaded += 1;
            self.report.download_seconds += p.seconds;
        }
        Ok(resolve::TarballNode {
            entry: PackageEntry {
                name: name.to_string(),
                version: p.version(),
                hash: Some(p.integrity.sri()),
                store_path: p.rel.clone(),
                source_url: Some(url),
                path: None,
            },
            manifest: p.manifest,
        })
    }
}

fn registry_dep(name: &str, version: &str, path: Option<String>) -> NodeDep {
    NodeDep {
        name: name.to_string(),
        version: version.to_string(),
        _resolved: None,
        _integrity: None,
        os: Vec::new(),
        cpu: Vec::new(),
        libc: Vec::new(),
        optional: false,
        local: None,
        path,
        git: None,
        tarball: None,
    }
}

fn prepare_one(
    store_root: &Path,
    src: &TarballSource,
    previous: &[TarballPackageEntry],
    opts: &PrepareOptions<'_>,
    report: &mut PrepareReport,
) -> Result<TarballPackageEntry> {
    let prev = previous.iter().find(|p| p.name == src.name && p.url == src.url);
    let recorded = prev.and_then(|p| Integrity::parse(&p.integrity));
    let token = (opts.token_for)(&src.url);
    let secrets: Vec<String> = token.iter().cloned().collect();
    let expect = Expect { declared: src.declared.clone(), recorded };

    let pristine = fetch::ensure_pristine(
        store_root,
        &src.name,
        &src.url,
        &expect,
        token.as_deref(),
        opts.force,
        opts.allow_plain_http_token,
    )
    .map_err(|e| {
        tb_error(
            src,
            prev.map(|p| p.integrity.as_str()),
            "could not be fetched",
            &[("url", src.url.clone()), ("reason", format!("{e:#}"))],
            "Fix the problem above, then run `kong rules` again.",
            &secrets,
        )
    })?;
    if pristine.downloaded {
        report.downloaded += 1;
        report.download_seconds += pristine.seconds;
    }
    let integrity = pristine.integrity.sri();
    if let Some(own) = pristine.manifest.get("name").and_then(|n| n.as_str()) {
        if own != src.name {
            warn!(
                pkg = %src.name, package_name = %own,
                "Tarball dependency {} is named '{}' in its package.json; linking it as {}",
                src.name, own, src.name
            );
        }
    }
    let version = pristine.version();

    // Own dependencies: the lock's answer while the file is the recorded one,
    // a fresh resolution otherwise.
    let same_file = prev.is_some_and(|p| p.integrity == integrity);
    let packages: Vec<PackageEntry> = if same_file {
        let prev = prev.expect("same_file implies prev");
        ensure_listed_packages(store_root, &prev.packages, opts, report, src, &integrity, &secrets)?;
        prev.packages.clone()
    } else {
        let mut env = PrepEnv {
            store_root,
            opts,
            recorded: prev.map(|p| p.packages.as_slice()).unwrap_or(&[]),
            cache: HashMap::new(),
            report,
            secrets: secrets.clone(),
        };
        let resolved = resolve::resolve_tree(
            &src.name,
            resolve::requirements(&pristine.manifest),
            &opts.host,
            &mut env,
        )
        .map_err(|e| {
            tb_error(
                src,
                Some(&integrity),
                "cannot install its own dependencies",
                &[("reason", format!("{e:#}"))],
                "Its dependencies come from its package.json; fix the problem above, then run `kong rules` again.",
                &secrets,
            )
        })?;
        let mut packages = Vec::with_capacity(resolved.len());
        for r in resolved {
            match r.source {
                resolve::Source::Tarball(entry) => packages.push(entry),
                resolve::Source::Registry => {
                    let dep = registry_dep(&r.name, &r.version, r.path.clone());
                    packages.push(
                        crate::config::node_package_entry(&dep, store_root, opts.force).map_err(|e| {
                            tb_error(
                                src,
                                Some(&integrity),
                                &format!("cannot install its dependency '{}@{}'", r.name, r.version),
                                &[("reason", format!("{e:#}"))],
                                "Fix the problem above, then run `kong rules` again.",
                                &secrets,
                            )
                        })?,
                    );
                }
            }
        }
        packages.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.path.cmp(&b.path)));
        packages
    };

    let store_path = assemble_tree(store_root, &pristine, &packages, opts.force).map_err(|e| {
        tb_error(
            src,
            Some(&integrity),
            "could not be assembled in the store",
            &[("reason", format!("{e:#}"))],
            "Fix the problem above, then run `kong rules` again.",
            &secrets,
        )
    })?;

    info!(
        pkg = %src.name, version = %version, integrity = %integrity,
        downloaded = pristine.downloaded, packages = packages.len(),
        "Prepared tarball dependency {} {}", src.name, version
    );
    Ok(TarballPackageEntry {
        name: src.name.clone(),
        version,
        source: src.spec.clone(),
        url: src.url.clone(),
        integrity,
        declared_by: src.declared_by.clone(),
        store_path,
        packages,
    })
}

/// A reused lock must still find its packages in the store: registry packages are
/// fetched when missing (exact version, no resolution), nested tarballs are
/// re-fetched against the integrity the lock recorded.
fn ensure_listed_packages(
    store_root: &Path,
    packages: &[PackageEntry],
    opts: &PrepareOptions<'_>,
    report: &mut PrepareReport,
    src: &TarballSource,
    integrity: &str,
    secrets: &[String],
) -> Result<()> {
    for pkg in packages {
        if store_root.join(&pkg.store_path).exists() && !opts.force {
            continue;
        }
        let fetched = match (&pkg.source_url, pkg.store_path.starts_with("node/tarballs/")) {
            (Some(url), true) => {
                let token = (opts.token_for)(url);
                fetch::ensure_pristine(
                    store_root,
                    &pkg.name,
                    url,
                    &Expect { declared: None, recorded: pkg.hash.as_deref().and_then(Integrity::parse) },
                    token.as_deref(),
                    opts.force,
                    opts.allow_plain_http_token,
                )
                .map(|p| {
                    if p.downloaded {
                        report.downloaded += 1;
                        report.download_seconds += p.seconds;
                    }
                })
            }
            _ => crate::config::node_package_entry(&registry_dep(&pkg.name, &pkg.version, pkg.path.clone()), store_root, opts.force)
                .map(|_| ()),
        };
        fetched.map_err(|e| {
            tb_error(
                src,
                Some(integrity),
                &format!("cannot restore its dependency '{}@{}' in the store", pkg.name, pkg.version),
                &[("reason", format!("{e:#}"))],
                "Fix the problem above, then run `kong rules` again.",
                secrets,
            )
        })?;
    }
    Ok(())
}

/// Hash of a resolved package set: two projects that resolved the same tree share one directory.
fn deps_key(packages: &[PackageEntry]) -> String {
    let mut lines: Vec<String> = packages
        .iter()
        .map(|p| {
            format!(
                "{}|{}|{}|{}",
                p.path.as_deref().unwrap_or(""),
                p.name,
                p.version,
                p.hash.as_deref().unwrap_or("")
            )
        })
        .collect();
    lines.sort();
    let digest = Sha256::digest(lines.join("\n").as_bytes());
    hex::encode(digest)[..8].to_string()
}

/// Build `<pristine>/tree-<deps hash>/package/` = the package (hard links) with
/// its own `node_modules`. Idempotent: a finished tree is left alone.
fn assemble_tree(
    store_root: &Path,
    pristine: &fetch::Pristine,
    packages: &[PackageEntry],
    force: bool,
) -> Result<String> {
    let rel = format!("{}/tree-{}", pristine.rel, deps_key(packages));
    let dir = store_root.join(&rel);
    let marker = dir.join(INSTALL_MARKER);
    if marker.is_file() && !force {
        debug!(tree = %rel, "tarball tree already assembled");
        return Ok(rel);
    }
    if dir.exists() {
        crate::link::remove_dir_all_robust(&dir)?;
    }
    let started = std::time::Instant::now();
    let package = dir.join("package");
    let content = crate::node::modules::package_content_root(&pristine.dir);
    crate::link::link_package(&content, &package)
        .with_context(|| format!("failed to link {} into {}", content.display(), package.display()))?;
    let node = NodeSection {
        packages: packages.to_vec(),
        local: Vec::new(),
        git: Vec::new(),
        tarballs: Vec::new(),
    };
    crate::node::modules::build_node_modules(&package, &package, &node, store_root)
        .context("failed to assemble the package's own node_modules")?;
    let body = serde_json::json!({
        "packages": packages.len(),
        "seconds": started.elapsed().as_secs_f64(),
    });
    std::fs::write(&marker, serde_json::to_string_pretty(&body)?)
        .with_context(|| format!("failed to write {}", marker.display()))?;
    Ok(rel)
}

/// The directory `node_modules/<name>` links to.
pub fn tarball_link_target(store_root: &Path, e: &TarballPackageEntry) -> PathBuf {
    store_root.join(&e.store_path).join("package")
}

/// Refuse an incomplete store entry before `kong use` links it into a project.
pub fn check_prepared(store_root: &Path, e: &TarballPackageEntry) -> Result<()> {
    let dir = store_root.join(&e.store_path);
    if !dir.join(INSTALL_MARKER).is_file() || !dir.join("package").join("package.json").is_file() {
        anyhow::bail!(
            "tarball dependency '{}' is missing from the store:\n  source      : {}\n  integrity   : {}\n  declared by : kong.rules node.tarballs (declared in {})\n  tree        : {}\nrun `kong rules` to fetch it",
            e.name,
            e.source,
            e.integrity,
            e.declared_by,
            dir.display(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests;
