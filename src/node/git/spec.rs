//! Pure parsing and stable keys for Node git dependencies.
//!
//! Keeping a credential-free canonical repository URL lets the store share one
//! checkout across references without ever persisting credentials.

use anyhow::{bail, Result};
use sha2::{Digest, Sha256};

/// A remote HEAD, named ref, or immutable full commit selected for a dependency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitRef {
    Default,
    Name(String),
    Commit(String),
}

impl GitRef {
    /// Preserve the stored reference while representing remote HEAD as absence.
    pub fn as_option(&self) -> Option<String> {
        match self {
            Self::Default => None,
            Self::Name(s) | Self::Commit(s) => Some(s.clone()),
        }
    }

    /// Restore the default reference when a stored optional ref is absent.
    pub fn from_option(s: Option<&str>) -> Self {
        match s {
            None => Self::Default,
            Some(s) if is_full_commit(s) => Self::Commit(s.to_owned()),
            Some(s) => Self::Name(s.to_owned()),
        }
    }
}

/// A parsed dependency source, retaining its declaration for useful errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitSource {
    pub name: String,
    pub spec: String,
    pub repo: String,
    pub reference: GitRef,
    pub path: String,
    pub build: Option<String>,
    pub rust_targets: Vec<String>,
    pub origin: String,
    pub declared_by: String,
}

/// Recognize only the npm Git URL forms Kong handles specially.
pub fn is_git_spec(spec: &str) -> bool {
    spec.starts_with("github:")
        || [
            "git+https://",
            "git+http://",
            "git+ssh://",
            "git+file://",
            "git://",
        ]
        .iter()
        .any(|p| spec.starts_with(p))
}

/// Canonicalize an accepted npm Git spec without resolving its ref remotely.
pub fn parse_git_spec(spec: &str) -> Result<(String, GitRef)> {
    if !is_git_spec(spec) {
        bail!("unsupported git dependency spec '{spec}'");
    }
    let (source, fragment) = match spec.split_once('#') {
        Some((u, f)) => (u, Some(f)),
        None => (spec, None),
    };
    let mut repo = if let Some(path) = source.strip_prefix("github:") {
        format!("https://github.com/{path}")
    } else {
        source.strip_prefix("git+").unwrap_or(source).to_owned()
    };
    if let Some(f) = fragment {
        if f.starts_with("semver:") {
            bail!("#semver: references are not supported in git dependency specs");
        }
        if f.contains("::") {
            bail!("'::' is not supported in a git dependency fragment");
        }
        if (7..40).contains(&f.len()) && f.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("abbreviated commit '{f}' is not supported; use the full 40-hex commit or a tag/branch");
        }
    }
    if repo.starts_with("https://") || repo.starts_with("http://") {
        let authority = repo
            .split_once("://")
            .map(|(_, r)| r.split('/').next().unwrap_or(""))
            .unwrap_or("");
        if authority.contains('@') {
            bail!("userinfo in HTTP(S) git URLs is not allowed; credentials belong in the environment or the git credential helper");
        }
    }
    if repo.ends_with('/') {
        repo.pop();
    }
    if !repo.starts_with("file://") && !repo.ends_with(".git") {
        repo.push_str(".git");
    }
    let reference = match fragment {
        None | Some("") => GitRef::Default,
        Some(f) if is_full_commit(f) => GitRef::Commit(f.to_owned()),
        Some(f) => GitRef::Name(f.to_owned()),
    };
    Ok((repo, reference))
}

/// Produce a readable, collision-resistant repository directory name.
pub fn repo_key(repo: &str) -> String {
    let tail = repo
        .rsplit('/')
        .next()
        .unwrap_or(repo)
        .strip_suffix(".git")
        .unwrap_or_else(|| repo.rsplit('/').next().unwrap_or(repo));
    let safe: String = tail
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._-".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!(
        "{}-{}",
        safe,
        &hex::encode(Sha256::digest(repo.as_bytes()))[..8]
    )
}

/// Hash a build command so marker names do not contain arbitrary command text.
pub fn build_key(build: &str) -> String {
    hex::encode(Sha256::digest(build.as_bytes()))[..12].to_owned()
}

/// Return the stable short commit label used in logs and store paths.
pub fn commit12(commit: &str) -> &str {
    &commit[..commit.len().min(12)]
}

/// Accept only a full lowercase hexadecimal object id.
pub fn is_full_commit(s: &str) -> bool {
    s.len() == 40
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepted_forms_and_canonical_keys() {
        for spec in [
            "github:Etcito/raa#v1",
            "git+https://github.com/Etcito/raa.git#v1",
            "git+http://host/repo",
            "git+ssh://git@host/repo",
            "git+ssh://host/repo.git",
            "git+file:///tmp/x/repo",
            "git://host/repo",
        ] {
            assert!(parse_git_spec(spec).is_ok(), "{spec}");
        }
        let a = parse_git_spec("github:Etcito/raa#v1").unwrap().0;
        let b = parse_git_spec("git+https://github.com/Etcito/raa.git#v1")
            .unwrap()
            .0;
        assert_eq!(a, b);
        assert_eq!(repo_key(&a), repo_key(&b));
        assert_eq!(
            parse_git_spec("git+file:///tmp/x/repo").unwrap().0,
            "file:///tmp/x/repo"
        );
    }

    #[test]
    fn rejects_bad_refs_and_userinfo() {
        for spec in [
            "git+https://user:pass@host/repo",
            "github:org/repo#semver:^1",
            "github:org/repo#a::b",
            "github:org/repo#abcdef0",
        ] {
            assert!(parse_git_spec(spec).is_err(), "{spec}");
        }
    }

    #[test]
    fn classifies_refs_and_commit_helpers() {
        let full = "abcdef0123456789abcdef0123456789abcdef01";
        assert_eq!(GitRef::from_option(None), GitRef::Default);
        assert_eq!(
            parse_git_spec("git://host/repo#").unwrap().1,
            GitRef::Default
        );
        assert_eq!(
            parse_git_spec("git://host/repo#v1").unwrap().1,
            GitRef::Name("v1".into())
        );
        assert_eq!(
            parse_git_spec(&format!("git://host/repo#{full}"))
                .unwrap()
                .1,
            GitRef::Commit(full.into())
        );
        assert_eq!(commit12(full), &full[..12]);
        assert!(is_full_commit(full));
        assert!(!is_full_commit(&full.to_uppercase()));
        assert_eq!(
            GitRef::Commit(full.into()).as_option().as_deref(),
            Some(full)
        );
        assert_eq!(build_key("echo hi").len(), 12);
    }
}
