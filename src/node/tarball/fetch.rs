//! Downloading, verifying and extracting an https tarball dependency into the store.
//!
//! One download per distinct tarball (keyed by its sha512), verified against the
//! declared or recorded integrity, extracted once into
//! `<store>/node/tarballs/<name>/<sha512 12 hex>/`.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256, Sha512};
use tracing::{debug, info, warn};

use crate::gitdep::credential::Credential;

// ── Integrity ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Algo {
    Sha256,
    Sha512,
}

impl Algo {
    fn name(self) -> &'static str {
        match self {
            Algo::Sha256 => "sha256",
            Algo::Sha512 => "sha512",
        }
    }
    fn len(self) -> usize {
        match self {
            Algo::Sha256 => 32,
            Algo::Sha512 => 64,
        }
    }
}

/// A digest with its algorithm, as declared in a spec fragment or recorded in kong.rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Integrity {
    pub algo: Algo,
    pub digest: Vec<u8>,
}

impl Integrity {
    /// `sha512-<base64>` (the npm / SRI spelling, what kong.rules records).
    pub fn sri(&self) -> String {
        format!("{}-{}", self.algo.name(), b64_encode(&self.digest))
    }

    pub fn hex(&self) -> String {
        hex::encode(&self.digest)
    }

    /// Parse `sha256-<b64>`, `sha512-<b64>`, `sha256=<hex>` or `sha512=<hex>`.
    pub fn parse(s: &str) -> Option<Integrity> {
        let s = s.trim();
        for algo in [Algo::Sha256, Algo::Sha512] {
            let name = algo.name();
            if let Some(rest) = s.strip_prefix(&format!("{name}=")) {
                let digest = hex::decode(rest).ok()?;
                return (digest.len() == algo.len()).then_some(Integrity { algo, digest });
            }
            if let Some(rest) = s.strip_prefix(&format!("{name}-")) {
                let digest = b64_decode(rest)?;
                return (digest.len() == algo.len()).then_some(Integrity { algo, digest });
            }
        }
        None
    }
}

/// Both digests of one download, so either declared algorithm can be checked.
#[derive(Debug, Clone)]
pub struct Digests {
    pub sha256: Vec<u8>,
    pub sha512: Vec<u8>,
}

impl Digests {
    pub fn of(bytes: &[u8]) -> Digests {
        Digests {
            sha256: Sha256::digest(bytes).to_vec(),
            sha512: Sha512::digest(bytes).to_vec(),
        }
    }

    pub fn matches(&self, i: &Integrity) -> bool {
        match i.algo {
            Algo::Sha256 => self.sha256 == i.digest,
            Algo::Sha512 => self.sha512 == i.digest,
        }
    }

    pub fn of_algo(&self, algo: Algo) -> Integrity {
        Integrity {
            algo,
            digest: match algo {
                Algo::Sha256 => self.sha256.clone(),
                Algo::Sha512 => self.sha512.clone(),
            },
        }
    }
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn b64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(B64[((n >> (18 - 6 * i)) & 0x3f) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn b64_decode(s: &str) -> Option<Vec<u8>> {
    let s = s.trim_end_matches('=');
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.bytes() {
        let v = B64.iter().position(|&x| x == c)? as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

// ── Spec ────────────────────────────────────────────────────────────────────

/// True for a dependency value that is a plain http(s) URL (a tarball), i.e. not a git spec.
pub fn is_tarball_spec(spec: &str) -> bool {
    let base = spec.split('#').next().unwrap_or(spec);
    (spec.starts_with("https://") || spec.starts_with("http://"))
        && !base.trim_end_matches('/').ends_with(".git")
}

/// The spec with any `user:password@` replaced by `***@`, for messages.
pub fn mask_userinfo(spec: &str) -> String {
    let Some((scheme, rest)) = spec.split_once("://") else {
        return spec.to_string();
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    match rest[..end].rfind('@') {
        Some(at) => format!("{scheme}://***@{}", &rest[at + 1..]),
        None => spec.to_string(),
    }
}

/// Split a tarball spec into its URL (no fragment) and the integrity the fragment declares.
pub fn parse_tarball_spec(spec: &str) -> Result<(String, Option<Integrity>)> {
    if !is_tarball_spec(spec) {
        bail!("not an http(s) tarball URL: {spec}");
    }
    let (url, fragment) = match spec.split_once('#') {
        Some((u, f)) => (u, Some(f)),
        None => (spec, None),
    };
    let after_scheme = url.split_once("://").map(|(_, r)| r).unwrap_or("");
    let authority = after_scheme.split(['/', '?']).next().unwrap_or("");
    if authority.is_empty() {
        bail!("the URL has no host");
    }
    if authority.contains('@') {
        bail!(
            "the URL carries credentials (user:password@host). Credentials belong in the \
             environment (KONG_GIT_TOKEN, or GITHUB_TOKEN / GH_TOKEN for github.com), never in the spec"
        );
    }
    let declared = match fragment {
        None | Some("") => None,
        Some(f) => Some(Integrity::parse(f).ok_or_else(|| {
            anyhow!(
                "unsupported URL fragment '#{f}'. A declared integrity is `#sha256=<64 hex>`, \
                 `#sha512=<128 hex>`, `#sha256-<base64>` or `#sha512-<base64>`"
            )
        })?),
    };
    Ok((url.to_string(), declared))
}

// ── HTTP ────────────────────────────────────────────────────────────────────

const MAX_REDIRECTS: usize = 10;

fn origin_of(u: &reqwest::Url) -> (String, String, Option<u16>) {
    (
        u.scheme().to_string(),
        u.host_str().unwrap_or("").to_ascii_lowercase(),
        u.port_or_known_default(),
    )
}

/// An HTTP status the server answered, kept typed so callers can retry with a credential.
#[derive(Debug)]
pub(crate) struct HttpStatus {
    pub code: u16,
    pub message: String,
}

impl std::fmt::Display for HttpStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for HttpStatus {}

fn is_auth_failure(e: &anyhow::Error) -> bool {
    e.downcast_ref::<HttpStatus>().is_some_and(|s| matches!(s.code, 401 | 403 | 404))
}

fn client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .user_agent("kong-dependency-manager/0.1")
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(600))
        .connect_timeout(std::time::Duration::from_secs(30))
        .build()
        .context("failed to build the HTTP client")
}

/// GET `url` and return the body. `token`, when given, is sent as
/// `Authorization: Bearer <token>` to the ORIGINAL origin only and only over
/// https: redirects are followed by hand and never carry the header to another
/// origin (GitHub answers a release-asset request with a redirect to a signed
/// URL on another host). `token_over_plain_http` is false everywhere but in the
/// tests, which can only reach a local http server.
pub(crate) fn get_with(url: &str, token: Option<&str>, token_over_plain_http: bool) -> Result<Vec<u8>> {
    get_auth(url, token.map(|t| format!("Bearer {t}")).as_deref(), token_over_plain_http)
}

/// Like `get_with` with a ready `Authorization` header value.
fn get_auth(url: &str, authorization: Option<&str>, token_over_plain_http: bool) -> Result<Vec<u8>> {
    let client = client()?;
    let first = reqwest::Url::parse(url).with_context(|| format!("invalid URL {url}"))?;
    let first_origin = origin_of(&first);
    let mut current = first.clone();
    for _ in 0..=MAX_REDIRECTS {
        let mut req = client.get(current.clone()).header("Accept", "application/octet-stream");
        let same_origin = origin_of(&current) == first_origin;
        let may_send = same_origin && (current.scheme() == "https" || token_over_plain_http);
        let sent_token = match (authorization, may_send) {
            (Some(a), true) => {
                req = req.header("Authorization", a);
                true
            }
            _ => false,
        };
        let resp = req
            .send()
            .map_err(|e| anyhow!("HTTP request to {} failed: {}", host_of(&current), e.without_url()))?;
        let status = resp.status();
        if status.is_redirection() {
            let loc = resp
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| anyhow!("HTTP {status} from {} without a Location header", host_of(&current)))?;
            let next = current
                .join(loc)
                .map_err(|_| anyhow!("HTTP {status} from {} with an invalid Location", host_of(&current)))?;
            if current.scheme() == "https" && next.scheme() != "https" {
                bail!(
                    "{} redirected an https download to plain http ({}); refusing the downgrade",
                    host_of(&current),
                    host_of(&next)
                );
            }
            // Never print the redirect target: it is usually a signed URL.
            debug!(from = %host_of(&current), to = %host_of(&next), "following redirect");
            current = next;
            continue;
        }
        if !status.is_success() {
            let note = if matches!(status.as_u16(), 401 | 403 | 404) {
                if sent_token {
                    " (a credential was sent and refused or the file does not exist)"
                } else if authorization.is_some() {
                    " (a credential is available but was not sent: https is required)"
                } else {
                    " (no credential was sent: for a private asset log in with git (Git Credential Manager \
                     or any credential helper) or `gh auth login`, or set KONG_GIT_TOKEN, or GITHUB_TOKEN / \
                     GH_TOKEN for github.com; otherwise the URL does not exist)"
                }
            } else {
                ""
            };
            return Err(anyhow::Error::new(HttpStatus {
                code: status.as_u16(),
                message: format!("HTTP {status} from {}{note}", host_of(&current)),
            }));
        }
        let mut body = Vec::new();
        let mut resp = resp;
        resp.read_to_end(&mut body)
            .map_err(|e| anyhow!("failed to read the response body from {}: {e}", host_of(&current)))?;
        return Ok(body);
    }
    bail!("too many redirects (more than {MAX_REDIRECTS}) starting from {}", host_of(&first))
}

// ── Authentication policy ───────────────────────────────────────────────────

/// How a download may authenticate.
pub struct Net<'a> {
    /// A token from the environment (KONG_GIT_TOKEN, GITHUB_TOKEN, GH_TOKEN): wins over everything.
    pub token: Option<&'a str>,
    /// The credential git (or gh) holds for a host; asked lazily, only after an anonymous refusal.
    pub credential_for: &'a dyn Fn(&str) -> Option<Credential>,
    /// Tests only: allow sending credentials over plain http to the local test server.
    pub allow_plain_http_token: bool,
    /// Tests only: `(host, api base)` standing in for github.com / api.github.com.
    pub github_override: Option<(&'a str, &'a str)>,
}

type ReleaseRef = (String, String, String, String, String);

/// `(api base, owner, repo, tag, asset name)` of a GitHub release-download URL.
fn github_release(url: &str, net: &Net<'_>) -> Option<ReleaseRef> {
    let u = reqwest::Url::parse(url).ok()?;
    let host_port = match u.port() {
        Some(p) => format!("{}:{p}", u.host_str()?.to_ascii_lowercase()),
        None => u.host_str()?.to_ascii_lowercase(),
    };
    let api = if u.scheme() == "https" && host_port == "github.com" {
        "https://api.github.com".to_string()
    } else if let Some((h, api)) = net.github_override {
        (host_port == h.to_ascii_lowercase()).then(|| api.trim_end_matches('/').to_string())?
    } else {
        return None;
    };
    let seg: Vec<&str> = u.path_segments()?.collect();
    match seg.as_slice() {
        [owner, repo, "releases", "download", tag, asset] if !asset.is_empty() => {
            Some((api, owner.to_string(), repo.to_string(), tag.to_string(), asset.to_string()))
        }
        _ => None,
    }
}

fn authorization_for(cred: &Credential, github: bool) -> String {
    match (&cred.username, github) {
        (Some(u), false) => format!("Basic {}", b64_encode(format!("{u}:{}", cred.password).as_bytes())),
        _ => format!("Bearer {}", cred.password),
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Some(v) = s.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Download a private release asset through the REST API (a bearer token is not
/// accepted on the `releases/download/...` URL): resolve the asset by tag and name,
/// then ask for `application/octet-stream`; the redirect to the signed URL is
/// followed WITHOUT the Authorization header.
fn github_api_download(rel: ReleaseRef, authorization: &str, plain_ok: bool) -> Result<Vec<u8>> {
    let (api, owner, repo, tag, asset) = rel;
    let client = client()?;
    if api.starts_with("http://") && !plain_ok {
        bail!("refusing to send a credential to {api} over plain http");
    }
    let api_get = |url: &str, accept: &str| -> Result<reqwest::blocking::Response> {
        client
            .get(url)
            .header("Accept", accept)
            .header("Authorization", authorization)
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .map_err(|e| anyhow!("GitHub API request failed: {}", e.without_url()))
    };
    let check = |resp: &reqwest::blocking::Response, what: &str| -> Result<()> {
        let st = resp.status();
        if st.is_success() || st.is_redirection() {
            return Ok(());
        }
        let note = match st.as_u16() {
            401 | 403 => " (the credential was refused: it needs read access to the repository's releases, scope `repo`)",
            404 => " (release or repository not found, or the credential cannot see this private repository)",
            _ => "",
        };
        Err(anyhow::Error::new(HttpStatus {
            code: st.as_u16(),
            message: format!("HTTP {st} from the GitHub API while {what}{note}"),
        }))
    };
    let resp = api_get(&format!("{api}/repos/{owner}/{repo}/releases/tags/{tag}"), "application/vnd.github+json")?;
    check(&resp, "resolving the release")?;
    let release: serde_json::Value = resp.json().context("the GitHub API returned an unreadable release")?;
    let wanted = percent_decode(&asset);
    let id = release
        .get("assets")
        .and_then(|a| a.as_array())
        .and_then(|a| {
            a.iter()
                .find(|x| x.get("name").and_then(|n| n.as_str()).is_some_and(|n| n == wanted || n == asset))
        })
        .and_then(|x| x.get("id").and_then(|i| i.as_u64()))
        .ok_or_else(|| anyhow!("release {tag} of {owner}/{repo} has no asset named '{wanted}'"))?;
    let resp = api_get(&format!("{api}/repos/{owner}/{repo}/releases/assets/{id}"), "application/octet-stream")?;
    check(&resp, "downloading the asset")?;
    if resp.status().is_redirection() {
        let loc = resp
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| anyhow!("the GitHub API redirected without a Location header"))?
            .to_string();
        // The signed URL carries its own authorization: never send ours there.
        return get_auth(&loc, None, plain_ok);
    }
    let mut body = Vec::new();
    let mut resp = resp;
    resp.read_to_end(&mut body).map_err(|e| anyhow!("failed to read the asset: {e}"))?;
    Ok(body)
}

fn host_name_of(url: &str) -> String {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
        .unwrap_or_default()
}

fn scrub(e: anyhow::Error, secrets: &[String]) -> anyhow::Error {
    let mut m = format!("{e:#}");
    for s in secrets.iter().filter(|s| !s.is_empty()) {
        m = m.replace(s.as_str(), "***");
    }
    match e.downcast::<HttpStatus>() {
        Ok(h) => anyhow::Error::new(HttpStatus { code: h.code, message: m }),
        Err(_) => anyhow!(m),
    }
}

/// Download `url`, authenticating as little and as late as possible:
/// 1. a GitHub release URL: anonymous first (public repos need nothing), then the
///    REST API with the environment token or the credential git holds;
/// 2. any other URL with an environment token: sent to the original origin (https);
/// 3. any other URL: anonymous first, then once with the credential git holds.
pub(crate) fn get_authed(url: &str, net: &Net<'_>) -> Result<Vec<u8>> {
    let plain = net.allow_plain_http_token;
    let https = url.starts_with("https://") || plain;
    let env_secrets: Vec<String> = net.token.map(String::from).into_iter().collect();
    if let Some(rel) = github_release(url, net) {
        let first_err = match get_auth(url, None, false) {
            Ok(b) => return Ok(b),
            Err(e) if is_auth_failure(&e) => e,
            Err(e) => return Err(e),
        };
        let cred = if net.token.is_some() || !https { None } else { (net.credential_for)(&host_name_of(url)) };
        let auth = match (net.token, &cred) {
            (Some(t), _) => format!("Bearer {t}"),
            (None, Some(c)) => authorization_for(c, true),
            (None, None) => return Err(first_err),
        };
        let secrets: Vec<String> = cred.iter().map(|c| c.password.clone()).chain(env_secrets).collect();
        return github_api_download(rel, &auth, plain).map_err(|e| scrub(e, &secrets));
    }
    if net.token.is_some() {
        return get_with(url, net.token, plain).map_err(|e| scrub(e, &env_secrets));
    }
    match get_auth(url, None, false) {
        Err(e) if is_auth_failure(&e) && https => {
            let Some(cred) = (net.credential_for)(&host_name_of(url)) else {
                return Err(e);
            };
            let secrets = vec![cred.password.clone()];
            get_auth(url, Some(&authorization_for(&cred, false)), plain).map_err(|e| scrub(e, &secrets))
        }
        other => other,
    }
}

fn host_of(u: &reqwest::Url) -> String {
    match u.port() {
        Some(p) => format!("{}:{p}", u.host_str().unwrap_or("?")),
        None => u.host_str().unwrap_or("?").to_string(),
    }
}

// ── Extraction ──────────────────────────────────────────────────────────────

/// Extract a gzip tarball into `dest`, refusing paths that escape it and skipping links.
pub fn extract_safe(bytes: &[u8], dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    let gz = flate2::read::GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(gz);
    for entry in archive.entries().context("not a readable gzip tarball")? {
        let mut entry = entry.context("corrupt tarball entry")?;
        let kind = entry.header().entry_type();
        if kind.is_symlink() || kind.is_hard_link() {
            warn!(entry = %entry.path()?.display(), "skipping a link inside a dependency tarball");
            continue;
        }
        if !(kind.is_file() || kind.is_dir()) {
            continue;
        }
        let shown = entry.path()?.display().to_string();
        if !entry.unpack_in(dest).with_context(|| format!("failed to unpack {shown}"))? {
            bail!("the tarball holds an unsafe path ('{shown}') that would escape the extraction directory");
        }
    }
    Ok(())
}

// ── Store ───────────────────────────────────────────────────────────────────

const FETCHED_MARKER: &str = ".kong-fetched";

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '/' => '+',
            c if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+' | '@') => c,
            _ => '_',
        })
        .collect()
}

/// Store-relative directory of one tarball (`node/tarballs/<name>/<sha512 12 hex>`).
pub fn store_rel_for(name: &str, sha512: &Integrity) -> String {
    format!("node/tarballs/{}/{}", sanitize(name), &sha512.hex()[..12])
}

/// What `ensure_pristine` found or fetched.
#[derive(Debug, Clone)]
pub struct Pristine {
    pub rel: String,
    pub dir: PathBuf,
    /// Always the sha512 (the recorded form).
    pub integrity: Integrity,
    pub manifest: serde_json::Value,
    pub downloaded: bool,
    pub seconds: f64,
}

impl Pristine {
    pub fn version(&self) -> String {
        self.manifest
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or("0.0.0")
            .to_string()
    }
}

/// How the integrity of one tarball is established.
#[derive(Debug, Clone, Default)]
pub struct Expect {
    /// Declared in the spec fragment: wins over the recorded one.
    pub declared: Option<Integrity>,
    /// Recorded in kong.rules by an earlier run (sha512).
    pub recorded: Option<Integrity>,
}

impl Expect {
    fn effective(&self) -> Option<&Integrity> {
        self.declared.as_ref().or(self.recorded.as_ref())
    }
}

fn mismatch(url: &str, what: &str, expected: &Integrity, actual: &Integrity) -> anyhow::Error {
    anyhow!(
        "integrity mismatch for {url}:\n  {what:<11} : {}\n  expected    : {}\n  actual      : {}\n\
         The file is not the one that was pinned. If the asset was replaced on purpose, update the \
         declared `#{}=...` in the spec and/or delete this dependency's entry from kong.rules, then \
         run `kong rules` again.",
        expected.algo.name(),
        expected.sri(),
        actual.sri(),
        expected.algo.name()
    )
}

fn read_manifest(dir: &Path) -> Result<serde_json::Value> {
    let root = crate::node::modules::package_content_root(dir);
    let manifest = root.join("package.json");
    let text = std::fs::read_to_string(&manifest).map_err(|_| {
        anyhow!("the tarball has no package.json (looked in {}): it is not an npm package", root.display())
    })?;
    serde_json::from_str(&text).with_context(|| format!("unreadable {}", manifest.display()))
}

/// Make sure the tarball is in the store, downloading it only when needed.
///
/// With a recorded integrity and a complete store directory nothing touches the
/// network. Otherwise the file is downloaded once, checked against the declared
/// (else recorded) integrity, and extracted atomically.
pub fn ensure_pristine(
    store_root: &Path,
    name: &str,
    url: &str,
    expect: &Expect,
    net: &Net<'_>,
    force: bool,
) -> Result<Pristine> {
    // Offline path: the lock names the file, the store holds it.
    if let (Some(rec), false) = (&expect.recorded, force) {
        let rel = store_rel_for(name, rec);
        let dir = store_root.join(&rel);
        if let Ok(text) = std::fs::read_to_string(dir.join(FETCHED_MARKER)) {
            if let Ok(m) = serde_json::from_str::<serde_json::Value>(&text) {
                let s512 = m.get("sha512").and_then(|v| v.as_str()).and_then(Integrity::parse);
                let s256 = m.get("sha256").and_then(|v| v.as_str()).and_then(Integrity::parse);
                if s512.as_ref() == Some(rec) {
                    if let Some(decl) = &expect.declared {
                        let have = match decl.algo {
                            Algo::Sha256 => s256.clone(),
                            Algo::Sha512 => s512.clone(),
                        };
                        if let Some(have) = have {
                            if &have != decl {
                                return Err(mismatch(url, "declared", decl, &have));
                            }
                        } else {
                            // Marker without the declared algorithm: fetch again to verify.
                            return download(store_root, name, url, expect, net);
                        }
                    }
                    debug!(pkg = %name, "tarball already in the store, no network");
                    return Ok(Pristine {
                        rel,
                        manifest: read_manifest(&dir)?,
                        dir,
                        integrity: rec.clone(),
                        downloaded: false,
                        seconds: 0.0,
                    });
                }
            }
        }
    }
    download(store_root, name, url, expect, net)
}

fn download(
    store_root: &Path,
    name: &str,
    url: &str,
    expect: &Expect,
    net: &Net<'_>,
) -> Result<Pristine> {
    let started = Instant::now();
    let bytes = get_authed(url, net)?;
    let digests = Digests::of(&bytes);
    if let Some(want) = expect.effective() {
        if !digests.matches(want) {
            let what = if expect.declared.is_some() { "declared" } else { "recorded" };
            return Err(mismatch(url, what, want, &digests.of_algo(want.algo)));
        }
    }
    let sha512 = digests.of_algo(Algo::Sha512);
    let rel = store_rel_for(name, &sha512);
    let dir = store_root.join(&rel);
    let seconds;
    if dir.join(FETCHED_MARKER).is_file() {
        seconds = started.elapsed().as_secs_f64();
        debug!(pkg = %name, "tarball content already extracted in the store");
    } else {
        let parent = dir.parent().ok_or_else(|| anyhow!("bad store path"))?;
        std::fs::create_dir_all(parent)?;
        let tmp = parent.join(format!(
            ".tmp-{}-{}-{}",
            &sha512.hex()[..12],
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let result = (|| -> Result<()> {
            extract_safe(&bytes, &tmp)?;
            read_manifest(&tmp)?;
            let marker = serde_json::json!({
                "url": url,
                "sha512": sha512.sri(),
                "sha256": digests.of_algo(Algo::Sha256).sri(),
                "bytes": bytes.len(),
                "seconds": started.elapsed().as_secs_f64(),
                "fetched_at": chrono::Utc::now().to_rfc3339(),
            });
            std::fs::write(tmp.join(FETCHED_MARKER), serde_json::to_string_pretty(&marker)?)?;
            Ok(())
        })();
        if let Err(e) = result {
            let _ = crate::link::remove_dir_all_robust(&tmp);
            return Err(e);
        }
        if dir.exists() {
            // A concurrent kong won the race, or a previous run was cut short before the marker.
            if dir.join(FETCHED_MARKER).is_file() {
                let _ = crate::link::remove_dir_all_robust(&tmp);
            } else {
                crate::link::remove_dir_all_robust(&dir)?;
                std::fs::rename(&tmp, &dir)?;
            }
        } else if let Err(e) = std::fs::rename(&tmp, &dir) {
            let _ = crate::link::remove_dir_all_robust(&tmp);
            if !dir.join(FETCHED_MARKER).is_file() {
                return Err(e).with_context(|| format!("failed to move the extracted tarball to {}", dir.display()));
            }
        }
        seconds = started.elapsed().as_secs_f64();
        info!(
            pkg = %name, bytes = bytes.len(), integrity = %sha512.sri(),
            "Downloaded tarball {} in {:.1} s", name, seconds
        );
    }
    Ok(Pristine {
        rel,
        manifest: read_manifest(&dir)?,
        dir,
        integrity: sha512,
        downloaded: true,
        seconds,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integrity_parses_all_four_spellings() {
        let d = Sha256::digest(b"x");
        let hex = hex::encode(d);
        let b64 = b64_encode(&d);
        let a = Integrity::parse(&format!("sha256={hex}")).unwrap();
        let b = Integrity::parse(&format!("sha256-{b64}")).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.sri(), format!("sha256-{b64}"));
        let d5 = Sha512::digest(b"x");
        assert_eq!(
            Integrity::parse(&format!("sha512={}", hex::encode(d5))),
            Integrity::parse(&format!("sha512-{}", b64_encode(&d5)))
        );
        assert!(Integrity::parse("sha256=abcd").is_none());
        assert!(Integrity::parse("md5=abcd").is_none());
    }

    #[test]
    fn base64_round_trips() {
        for n in 0..40usize {
            let data: Vec<u8> = (0..n as u8).map(|i| i.wrapping_mul(37)).collect();
            assert_eq!(b64_decode(&b64_encode(&data)).unwrap(), data);
        }
    }

    #[test]
    fn tarball_specs_are_told_apart_from_git_specs() {
        assert!(is_tarball_spec("https://github.com/E/haa/releases/download/v0.1.0/e-haa-0.1.0.tgz"));
        assert!(is_tarball_spec("http://127.0.0.1:8000/a.tgz#sha256=00"));
        assert!(!is_tarball_spec("https://github.com/E/haa.git"));
        assert!(!is_tarball_spec("git+https://github.com/E/haa.git"));
        assert!(!is_tarball_spec("^1.2.3"));
        assert!(!is_tarball_spec("github:E/haa"));
    }

    #[test]
    fn spec_fragment_is_the_declared_integrity() {
        let hex64 = "ab".repeat(32);
        let (url, decl) = parse_tarball_spec(&format!("https://h/x.tgz#sha256={hex64}")).unwrap();
        assert_eq!(url, "https://h/x.tgz");
        assert_eq!(decl.unwrap().hex(), hex64);
        assert!(parse_tarball_spec("https://h/x.tgz").unwrap().1.is_none());
        let e = parse_tarball_spec("https://h/x.tgz#nonsense").unwrap_err().to_string();
        assert!(e.contains("unsupported URL fragment"), "{e}");
        let e = parse_tarball_spec("https://user:pw@h/x.tgz").unwrap_err().to_string();
        assert!(e.contains("credentials"), "{e}");
    }

    #[test]
    fn extraction_refuses_paths_that_escape() {
        let mut raw = Vec::new();
        {
            let gz = flate2::write::GzEncoder::new(&mut raw, flate2::Compression::fast());
            let mut b = tar::Builder::new(gz);
            let mut h = tar::Header::new_gnu();
            h.set_size(1);
            h.set_mode(0o644);
            // `set_path` rejects `..`, so write the raw name bytes.
            h.as_old_mut().name[..12].copy_from_slice(b"../evil.txt\0");
            h.set_cksum();
            b.append(&h, &b"x"[..]).unwrap();
            b.into_inner().unwrap().finish().unwrap();
        }
        let dir = tempfile::TempDir::new().unwrap();
        let dest = dir.path().join("out");
        let err = extract_safe(&raw, &dest).unwrap_err().to_string();
        assert!(err.contains("unsafe path"), "{err}");
        assert!(!dir.path().join("evil.txt").exists());
    }
}
