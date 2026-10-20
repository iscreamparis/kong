//! A small npm-style semver range matcher, enough to resolve the registry
//! dependencies of a tarball dependency without an external solver.
//!
//! Supported: exact versions, `^`, `~`, `>`, `>=`, `<`, `<=`, `=`, x-ranges
//! (`1.x`, `1.2.*`, `*`), hyphen ranges (`1.2 - 2.0`), `||` unions and
//! space-joined comparator sets. Pre-releases only satisfy a set that names a
//! pre-release of the same `major.minor.patch`, as in npm.

use std::cmp::Ordering;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pre: Vec<Ident>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Ident {
    Num(u64),
    Alpha(String),
}

impl Ord for Ident {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Ident::Num(a), Ident::Num(b)) => a.cmp(b),
            (Ident::Num(_), Ident::Alpha(_)) => Ordering::Less,
            (Ident::Alpha(_), Ident::Num(_)) => Ordering::Greater,
            (Ident::Alpha(a), Ident::Alpha(b)) => a.cmp(b),
        }
    }
}
impl PartialOrd for Ident {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => self.pre.cmp(&other.pre),
            })
    }
}
impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn parse_pre(s: &str) -> Option<Vec<Ident>> {
    s.split('.')
        .map(|p| {
            if p.is_empty() {
                None
            } else if p.bytes().all(|b| b.is_ascii_digit()) {
                p.parse().ok().map(Ident::Num)
            } else {
                Some(Ident::Alpha(p.to_string()))
            }
        })
        .collect()
}

impl Version {
    fn new(major: u64, minor: u64, patch: u64) -> Self {
        Version { major, minor, patch, pre: Vec::new() }
    }

    /// `-0`: the lowest pre-release, used as an exclusive upper bound.
    fn floor(major: u64, minor: u64, patch: u64) -> Self {
        Version { major, minor, patch, pre: vec![Ident::Num(0)] }
    }

    pub fn is_prerelease(&self) -> bool {
        !self.pre.is_empty()
    }

    /// Parse a full `1.2.3[-pre][+build]` version (a leading `v` or `=` is accepted).
    pub fn parse(s: &str) -> Option<Version> {
        let s = s.trim().trim_start_matches('=').trim_start_matches('v');
        let s = s.split('+').next()?;
        let (core, pre) = match s.split_once('-') {
            Some((c, p)) => (c, Some(p)),
            None => (s, None),
        };
        let mut it = core.split('.');
        let major = it.next()?.parse().ok()?;
        let minor = it.next()?.parse().ok()?;
        let patch = it.next()?.parse().ok()?;
        if it.next().is_some() {
            return None;
        }
        let pre = match pre {
            Some(p) => parse_pre(p)?,
            None => Vec::new(),
        };
        Some(Version { major, minor, patch, pre })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
}

#[derive(Debug, Clone)]
struct Comparator {
    op: Op,
    v: Version,
}

impl Comparator {
    fn matches(&self, v: &Version) -> bool {
        let c = v.cmp(&self.v);
        match self.op {
            Op::Lt => c == Ordering::Less,
            Op::Le => c != Ordering::Greater,
            Op::Gt => c == Ordering::Greater,
            Op::Ge => c != Ordering::Less,
            Op::Eq => c == Ordering::Equal,
        }
    }
}

/// `*` / `x` / empty, or a partial like `1`, `1.2`, optionally with a pre-release.
struct Partial {
    major: Option<u64>,
    minor: Option<u64>,
    patch: Option<u64>,
    pre: Vec<Ident>,
}

fn is_wild(s: &str) -> bool {
    matches!(s, "" | "*" | "x" | "X")
}

fn parse_partial(s: &str) -> Option<Partial> {
    let s = s.trim().trim_start_matches('v');
    let s = s.split('+').next()?;
    let (core, pre) = match s.split_once('-') {
        Some((c, p)) => (c, parse_pre(p)?),
        None => (s, Vec::new()),
    };
    let mut parts = core.split('.');
    let mut num = |p: Option<&str>| -> Option<Option<u64>> {
        match p {
            None => Some(None),
            Some(p) if is_wild(p) => Some(None),
            Some(p) => p.parse().ok().map(Some),
        }
    };
    let major = num(parts.next())?;
    let minor = num(parts.next())?;
    let patch = num(parts.next())?;
    if parts.next().is_some() {
        return None;
    }
    // A wildcard hides everything after it.
    let minor = major.and(minor);
    let patch = minor.and(patch);
    Some(Partial { major, minor, patch, pre })
}

type Set = Vec<Comparator>;

fn cmp(op: Op, v: Version) -> Comparator {
    Comparator { op, v }
}

fn lower(p: &Partial) -> Version {
    Version {
        major: p.major.unwrap_or(0),
        minor: p.minor.unwrap_or(0),
        patch: p.patch.unwrap_or(0),
        pre: p.pre.clone(),
    }
}

/// `1.2.x` style / bare partial -> `>=lo <hi`.
fn x_range(p: &Partial) -> Set {
    match (p.major, p.minor, p.patch) {
        (None, _, _) => vec![],
        (Some(a), None, _) => vec![
            cmp(Op::Ge, Version::new(a, 0, 0)),
            cmp(Op::Lt, Version::floor(a + 1, 0, 0)),
        ],
        (Some(a), Some(b), None) => vec![
            cmp(Op::Ge, Version::new(a, b, 0)),
            cmp(Op::Lt, Version::floor(a, b + 1, 0)),
        ],
        (Some(_), Some(_), Some(_)) => vec![cmp(Op::Eq, lower(p))],
    }
}

fn caret(p: &Partial) -> Set {
    let Some(a) = p.major else {
        return vec![];
    };
    let hi = match (a, p.minor, p.patch) {
        (0, Some(0), Some(c)) => Version::floor(0, 0, c + 1),
        (0, Some(0), None) => Version::floor(0, 1, 0),
        (0, Some(b), _) => Version::floor(0, b + 1, 0),
        (0, None, _) => Version::floor(1, 0, 0),
        (a, _, _) => Version::floor(a + 1, 0, 0),
    };
    vec![cmp(Op::Ge, lower(p)), cmp(Op::Lt, hi)]
}

fn tilde(p: &Partial) -> Set {
    let Some(a) = p.major else { return vec![] };
    let hi = match p.minor {
        Some(b) => Version::floor(a, b + 1, 0),
        None => Version::floor(a + 1, 0, 0),
    };
    vec![cmp(Op::Ge, lower(p)), cmp(Op::Lt, hi)]
}

fn parse_comparator(tok: &str) -> Option<Set> {
    let (op, rest) = if let Some(r) = tok.strip_prefix(">=") {
        (">=", r)
    } else if let Some(r) = tok.strip_prefix("<=") {
        ("<=", r)
    } else if let Some(r) = tok.strip_prefix('>') {
        (">", r)
    } else if let Some(r) = tok.strip_prefix('<') {
        ("<", r)
    } else if let Some(r) = tok.strip_prefix('^') {
        ("^", r)
    } else if let Some(r) = tok.strip_prefix('~') {
        ("~", r.trim_start_matches('>'))
    } else if let Some(r) = tok.strip_prefix('=') {
        ("", r)
    } else {
        ("", tok)
    };
    let p = parse_partial(rest)?;
    Some(match op {
        "" => x_range(&p),
        "^" => caret(&p),
        "~" => tilde(&p),
        ">=" => {
            if p.major.is_none() {
                vec![]
            } else {
                vec![cmp(Op::Ge, lower(&p))]
            }
        }
        ">" => match (p.major, p.minor, p.patch) {
            (None, _, _) => vec![cmp(Op::Lt, Version::new(0, 0, 0))],
            (Some(a), None, _) => vec![cmp(Op::Ge, Version::new(a + 1, 0, 0))],
            (Some(a), Some(b), None) => vec![cmp(Op::Ge, Version::new(a, b + 1, 0))],
            _ => vec![cmp(Op::Gt, lower(&p))],
        },
        "<=" => match (p.major, p.minor, p.patch) {
            (None, _, _) => vec![],
            (Some(a), None, _) => vec![cmp(Op::Lt, Version::floor(a + 1, 0, 0))],
            (Some(a), Some(b), None) => vec![cmp(Op::Lt, Version::floor(a, b + 1, 0))],
            _ => vec![cmp(Op::Le, lower(&p))],
        },
        "<" => {
            if p.major.is_none() {
                vec![cmp(Op::Lt, Version::new(0, 0, 0))]
            } else {
                vec![cmp(Op::Lt, lower(&p))]
            }
        }
        _ => return None,
    })
}

/// Normalise `>= 1.2` / `~ 1.2` (operator separated from its version by spaces).
fn tokens(set: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in set.split_whitespace() {
        if let Some(last) = out.last_mut() {
            if matches!(last.as_str(), ">" | ">=" | "<" | "<=" | "=" | "^" | "~") {
                last.push_str(t);
                continue;
            }
        }
        out.push(t.to_string());
    }
    out
}

fn parse_set(s: &str) -> Option<Set> {
    // Hyphen range: `a - b`.
    if let Some((a, b)) = s.split_once(" - ") {
        let lo = parse_partial(a)?;
        let hi = parse_partial(b)?;
        let mut set = Vec::new();
        if lo.major.is_some() {
            set.push(cmp(Op::Ge, lower(&lo)));
        }
        match (hi.major, hi.minor, hi.patch) {
            (None, _, _) => {}
            (Some(a), None, _) => set.push(cmp(Op::Lt, Version::floor(a + 1, 0, 0))),
            (Some(a), Some(b), None) => set.push(cmp(Op::Lt, Version::floor(a, b + 1, 0))),
            _ => set.push(cmp(Op::Le, lower(&hi))),
        }
        return Some(set);
    }
    let mut set = Vec::new();
    for tok in tokens(s) {
        set.extend(parse_comparator(&tok)?);
    }
    Some(set)
}

/// A parsed range: a union (`||`) of comparator sets.
#[derive(Debug, Clone)]
pub struct Range {
    sets: Vec<Set>,
}

impl Range {
    /// `None` when `s` is not a semver range (e.g. a dist-tag such as `latest`).
    pub fn parse(s: &str) -> Option<Range> {
        let mut sets = Vec::new();
        for part in s.split("||") {
            sets.push(parse_set(part.trim())?);
        }
        Some(Range { sets })
    }

    pub fn satisfies(&self, v: &Version) -> bool {
        self.sets.iter().any(|set| {
            if !set.iter().all(|c| c.matches(v)) {
                return false;
            }
            if !v.is_prerelease() {
                return true;
            }
            set.iter().any(|c| {
                c.v.is_prerelease()
                    && (c.v.major, c.v.minor, c.v.patch) == (v.major, v.minor, v.patch)
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(range: &str, version: &str) -> bool {
        Range::parse(range)
            .unwrap_or_else(|| panic!("range {range:?} must parse"))
            .satisfies(&Version::parse(version).unwrap())
    }

    #[test]
    fn caret_tilde_and_exact() {
        assert!(ok("^1.2.3", "1.9.0"));
        assert!(!ok("^1.2.3", "2.0.0"));
        assert!(!ok("^1.2.3", "1.2.2"));
        assert!(ok("^0.2.3", "0.2.9"));
        assert!(!ok("^0.2.3", "0.3.0"));
        assert!(ok("^0.0.3", "0.0.3"));
        assert!(!ok("^0.0.3", "0.0.4"));
        assert!(ok("~1.2.3", "1.2.9"));
        assert!(!ok("~1.2.3", "1.3.0"));
        assert!(ok("~1", "1.9.9"));
        assert!(ok("1.2.3", "1.2.3"));
        assert!(!ok("1.2.3", "1.2.4"));
        assert!(ok("=1.2.3", "1.2.3"));
    }

    #[test]
    fn comparators_ranges_and_wildcards() {
        assert!(ok(">=1.2.0 <2.0.0", "1.5.0"));
        assert!(!ok(">=1.2.0 <2.0.0", "2.0.0"));
        assert!(ok(">= 1.2.0", "3.0.0"));
        assert!(ok("1.x", "1.7.2"));
        assert!(!ok("1.x", "2.0.0"));
        assert!(ok("1.2.*", "1.2.8"));
        assert!(ok("*", "9.9.9"));
        assert!(ok("", "0.0.1"));
        assert!(ok("1.2.3 - 2.3.4", "2.0.0"));
        assert!(ok("1.2.3 - 2.3", "2.3.9"));
        assert!(!ok("1.2.3 - 2.3", "2.4.0"));
        assert!(ok("^1.0.0 || ^3.0.0", "3.4.0"));
        assert!(!ok("^1.0.0 || ^3.0.0", "2.0.0"));
        assert!(ok(">1.2", "1.3.0"));
        assert!(!ok(">1.2", "1.2.9"));
        assert!(ok("<=1.2", "1.2.9"));
    }

    #[test]
    fn prereleases_need_an_explicit_opt_in() {
        assert!(!ok("^1.2.3", "1.3.0-beta.1"));
        assert!(ok("^1.2.3-beta.1", "1.2.3-beta.2"));
        assert!(!ok("^1.2.3-beta.1", "1.2.4-beta.2"));
        assert!(ok("^1.2.3-beta.1", "1.2.3"));
    }

    #[test]
    fn version_ordering_follows_semver() {
        let v = |s| Version::parse(s).unwrap();
        assert!(v("1.0.0-alpha") < v("1.0.0-alpha.1"));
        assert!(v("1.0.0-alpha.1") < v("1.0.0-alpha.beta"));
        assert!(v("1.0.0-beta") < v("1.0.0"));
        assert!(v("1.0.0") < v("1.0.1"));
        assert!(v("1.10.0") > v("1.9.0"));
    }

    #[test]
    fn non_ranges_do_not_parse() {
        assert!(Range::parse("latest").is_none());
        assert!(Range::parse("next").is_none());
        assert!(Version::parse("1.2").is_none());
    }
}
