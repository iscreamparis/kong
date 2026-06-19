//! A focused PEP 440 version + specifier implementation.
//!
//! KONG resolves a project's dependencies from PyPI from scratch. A dependency's
//! version constraint is rarely an exact pin — it is usually a PEP 440 specifier
//! like `>=2.10,<3` or `~=1.4`. To pick the *right* version (the highest released
//! version that SATISFIES the constraint, not the global latest) we need to:
//!   1. parse a version string into orderable components, and
//!   2. parse a comma-separated specifier set and test a version against it.
//!
//! This is intentionally a pragmatic subset of PEP 440 — enough to resolve real
//! requirements without pulling a heavy dependency:
//!   * release segments (`1.2.3`), arbitrary length
//!   * epochs (`1!2.3`)
//!   * pre-releases (`a`/`b`/`rc`, with `alpha`/`beta`/`c`/`pre`/`preview` aliases)
//!   * post-releases (`.postN`, `-N`, `revN`)
//!   * dev-releases (`.devN`)
//!   * local versions (`+abc`) — compared only for equality semantics
//!   * specifier operators `==` `!=` `>=` `<=` `>` `<` `~=` `===`, `==X.*` prefix,
//!     and comma-separated conjunctions.
//!
//! What it does NOT do: full normalization edge-cases of every legal spelling,
//! and `~=` only on 2+ release segments (per spec). Unknown spellings fail to
//! parse and the caller falls back to "latest", never crashing.

use std::cmp::Ordering;

/// Pre-release kind, ordered alpha < beta < rc.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PreKind {
    Alpha,
    Beta,
    Rc,
}

impl PreKind {
    fn rank(&self) -> u8 {
        match self {
            PreKind::Alpha => 0,
            PreKind::Beta => 1,
            PreKind::Rc => 2,
        }
    }
}

/// A parsed PEP 440 version.
///
/// Equality and ordering are defined via [`Ord`] (zero-padded release tuples and
/// phase keys), NOT structural field equality — so `1.2` and `1.2.0` compare
/// equal, matching PEP 440 semantics.
#[derive(Debug, Clone)]
pub struct Version {
    epoch: u64,
    /// Release segments, e.g. `1.2.3` → `[1, 2, 3]`.
    release: Vec<u64>,
    /// Pre-release, e.g. `rc1` → `(Rc, 1)`.
    pre: Option<(PreKind, u64)>,
    /// Post-release number, e.g. `.post2` → `Some(2)`.
    post: Option<u64>,
    /// Dev-release number, e.g. `.dev3` → `Some(3)`.
    dev: Option<u64>,
    /// Local version segment (`+local`), lower-cased. Captured for completeness
    /// (and to validate that a `+` suffix is well-formed) but intentionally not
    /// used in ordering — PyPI release keys don't carry local versions.
    #[allow(dead_code)]
    local: Option<String>,
}

impl Version {
    /// Parse a PEP 440 version string. Returns `None` if it is not parseable.
    pub fn parse(input: &str) -> Option<Version> {
        let mut s = input.trim();
        // Accept a leading `v` (e.g. `v1.2.3`) per PEP 440 normalization.
        if let Some(rest) = s.strip_prefix(['v', 'V']) {
            s = rest;
        }
        if s.is_empty() {
            return None;
        }
        let lower = s.to_ascii_lowercase();
        let mut rest = lower.as_str();

        // ── local version (everything after the first '+') ──────────────────
        let local = if let Some(idx) = rest.find('+') {
            let loc = rest[idx + 1..].to_string();
            rest = &rest[..idx];
            if loc.is_empty() {
                return None;
            }
            Some(loc)
        } else {
            None
        };

        // ── epoch (N! prefix) ───────────────────────────────────────────────
        let epoch = if let Some(idx) = rest.find('!') {
            let e = rest[..idx].parse::<u64>().ok()?;
            rest = &rest[idx + 1..];
            e
        } else {
            0
        };

        // ── release segment (digits and dots up to first non-release char) ──
        let rel_end = rest
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(rest.len());
        // A trailing '.' belongs to the suffix separator (e.g. "1.0.dev1"), not
        // the release tuple — give it back to `rest`.
        let mut split_at = rel_end;
        while split_at > 0 && rest.as_bytes()[split_at - 1] == b'.' {
            split_at -= 1;
        }
        let release_str = &rest[..split_at];
        rest = &rest[split_at..];
        if release_str.is_empty() {
            return None;
        }
        let mut release = Vec::new();
        for seg in release_str.split('.') {
            if seg.is_empty() {
                return None;
            }
            release.push(seg.parse::<u64>().ok()?);
        }

        let mut pre = None;
        let mut post = None;
        let mut dev = None;

        // ── pre-release ─────────────────────────────────────────────────────
        rest = strip_sep(rest);
        for (kw, kind) in [
            ("alpha", PreKind::Alpha),
            ("beta", PreKind::Beta),
            ("preview", PreKind::Rc),
            ("pre", PreKind::Rc),
            ("rc", PreKind::Rc),
            ("a", PreKind::Alpha),
            ("b", PreKind::Beta),
            ("c", PreKind::Rc),
        ] {
            if let Some(after) = rest.strip_prefix(kw) {
                let after = strip_sep(after);
                let (num, tail) = take_number(after);
                pre = Some((kind, num.unwrap_or(0)));
                rest = tail;
                break;
            }
        }

        // ── post-release: `.postN`, `.revN`, `.rN`, or implicit `-N` ────────
        rest = strip_sep(rest);
        for kw in ["post", "rev", "r"] {
            if let Some(after) = rest.strip_prefix(kw) {
                let after = strip_sep(after);
                let (num, tail) = take_number(after);
                post = Some(num.unwrap_or(0));
                rest = tail;
                break;
            }
        }

        // ── dev-release: `.devN` ────────────────────────────────────────────
        rest = strip_sep(rest);
        if let Some(after) = rest.strip_prefix("dev") {
            let after = strip_sep(after);
            let (num, tail) = take_number(after);
            dev = Some(num.unwrap_or(0));
            rest = tail;
        }

        rest = rest.trim_matches(['.', '-', '_']);
        if !rest.is_empty() {
            // Unrecognized trailing content → not a clean parse.
            return None;
        }

        Some(Version {
            epoch,
            release,
            pre,
            post,
            dev,
            local,
        })
    }

    /// Release segment padded to `len` with zeros (for comparisons).
    fn release_at(&self, i: usize) -> u64 {
        self.release.get(i).copied().unwrap_or(0)
    }

    /// True if this is a pre-release or dev-release (PEP 440 "pre-release").
    pub fn is_prerelease(&self) -> bool {
        self.pre.is_some() || self.dev.is_some()
    }
}

/// Strip optional leading separators (`.`, `-`, `_`) between PEP 440 suffix groups.
fn strip_sep(r: &str) -> &str {
    r.trim_start_matches(['.', '-', '_'])
}

/// Parse a leading run of digits, returning (number?, remainder).
fn take_number(s: &str) -> (Option<u64>, &str) {
    let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    if end == 0 {
        (None, s)
    } else {
        (s[..end].parse::<u64>().ok(), &s[end..])
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        // epoch
        if self.epoch != other.epoch {
            return self.epoch.cmp(&other.epoch);
        }
        // release (compare segment by segment, zero-padded)
        let max = self.release.len().max(other.release.len());
        for i in 0..max {
            let c = self.release_at(i).cmp(&other.release_at(i));
            if c != Ordering::Equal {
                return c;
            }
        }
        // Pre/dev/post ordering per PEP 440:
        //   dev < pre < (no pre/post) < post
        // We map each version to a sortable tuple of "phase" keys.
        self.phase_key().cmp(&other.phase_key())
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Version {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Version {}

impl Version {
    /// Build a comparable key for the pre/post/dev phase ordering.
    ///
    /// For the same release tuple, PEP 440 orders:
    ///   X.devN < X.aN < X.bN < X.rcN < X < X.postN  (and dev lowers any of them)
    /// We mirror the `packaging` library's sort-key trick:
    ///   * A *pure dev* release (no pre, no post) sorts BELOW any pre-release →
    ///     give it the lowest pre-phase (-1).
    ///   * A *final* release (no pre) sorts ABOVE any pre-release → pre-phase 1.
    ///   * "no post" sorts below "has post" (post_state 0 vs 1).
    ///   * "no dev" sorts ABOVE "has dev" (dev = i64::MAX) so `X` > `X.devN`.
    fn phase_key(&self) -> (i8, u8, u64, i8, i64, i64) {
        let (pre_phase, pre_rank, pre_num) = match &self.pre {
            Some((k, n)) => (0i8, k.rank(), *n),
            None => {
                if self.dev.is_some() && self.post.is_none() {
                    (-1i8, 0, 0) // pure dev release: below all pre-releases
                } else {
                    (1i8, 0, 0) // final / post: above all pre-releases
                }
            }
        };
        let (post_state, post_num) = match self.post {
            Some(n) => (1i8, n as i64),
            None => (0i8, 0),
        };
        let dev_num = match self.dev {
            Some(n) => n as i64,
            None => i64::MAX,
        };
        (pre_phase, pre_rank, pre_num, post_state, post_num, dev_num)
    }
}

/// A single PEP 440 comparison operator.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Op {
    Eq,         // ==
    EqStar,     // ==X.*  (prefix match)
    Ne,         // !=
    NeStar,     // !=X.*
    Ge,         // >=
    Le,         // <=
    Gt,         // >
    Lt,         // <
    Compatible, // ~=
    Arbitrary,  // === (string equality)
}

/// One operator + version clause, e.g. `>=2.10`.
#[derive(Debug, Clone)]
struct Clause {
    op: Op,
    /// The version text as written (for `===` arbitrary equality and `*` prefix).
    raw: String,
    /// Parsed version (None for `===` arbitrary, where any string is allowed).
    ver: Option<Version>,
}

/// A parsed set of PEP 440 specifier clauses joined by AND (comma).
/// An empty set matches everything (bare dependency).
#[derive(Debug, Clone, Default)]
pub struct SpecifierSet {
    clauses: Vec<Clause>,
}

impl SpecifierSet {
    /// Parse a specifier string like `>=2.10,<3` or `~=1.4.2` or `` (empty).
    /// Unparseable clauses are skipped with the whole set returning what parsed;
    /// a fully unparseable non-empty input yields an empty (match-all) set so the
    /// caller degrades to "latest" rather than failing.
    pub fn parse(input: &str) -> SpecifierSet {
        let mut clauses = Vec::new();
        for part in input.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            if let Some(c) = parse_clause(part) {
                clauses.push(c);
            }
        }
        SpecifierSet { clauses }
    }

    /// True if there are no constraints (matches any version).
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.clauses.is_empty()
    }

    /// True if this set is a single exact `==X.Y.Z` pin (no wildcard). Returns
    /// the pinned version string if so. Used to prefer an exact pin.
    pub fn exact_pin(&self) -> Option<String> {
        if self.clauses.len() == 1 {
            let c = &self.clauses[0];
            if c.op == Op::Eq {
                return Some(c.raw.clone());
            }
        }
        None
    }

    /// True if `version` satisfies every clause.
    pub fn matches(&self, version: &Version) -> bool {
        self.clauses.iter().all(|c| clause_matches(c, version))
    }

    /// Merge another set into this one (logical AND of all clauses). Used to
    /// combine a direct requirement with a parent's `Requires-Dist` bound.
    pub fn merge(&mut self, other: &SpecifierSet) {
        self.clauses.extend(other.clauses.iter().cloned());
    }
}

/// Parse one clause like `>=2.10` / `==1.2.*` / `===weird`.
fn parse_clause(part: &str) -> Option<Clause> {
    // Order matters: longest operators first.
    let ops: &[(&str, Op)] = &[
        ("===", Op::Arbitrary),
        ("==", Op::Eq),
        ("!=", Op::Ne),
        (">=", Op::Ge),
        ("<=", Op::Le),
        ("~=", Op::Compatible),
        (">", Op::Gt),
        ("<", Op::Lt),
    ];
    for (sym, op) in ops {
        if let Some(rest) = part.strip_prefix(sym) {
            let raw = rest.trim().to_string();
            if raw.is_empty() {
                return None;
            }
            // Wildcard handling for == / != only.
            if raw.ends_with(".*") {
                let base = &raw[..raw.len() - 2];
                let ver = Version::parse(base)?;
                let wop = match op {
                    Op::Eq => Op::EqStar,
                    Op::Ne => Op::NeStar,
                    _ => return None, // `.*` only valid with == / !=
                };
                return Some(Clause {
                    op: wop,
                    raw,
                    ver: Some(ver),
                });
            }
            if *op == Op::Arbitrary {
                // === keeps the raw string; no version parse required.
                return Some(Clause {
                    op: Op::Arbitrary,
                    raw,
                    ver: None,
                });
            }
            let ver = Version::parse(&raw)?;
            return Some(Clause {
                op: op.clone(),
                raw,
                ver: Some(ver),
            });
        }
    }
    None
}

/// Compare release-tuple prefix for `==X.*` / `~=` semantics.
/// Returns true if `v`'s release tuple starts with `prefix`'s release tuple.
fn release_prefix_matches(v: &Version, prefix: &Version) -> bool {
    if v.epoch != prefix.epoch {
        return false;
    }
    for (i, p) in prefix.release.iter().enumerate() {
        if v.release_at(i) != *p {
            return false;
        }
    }
    true
}

fn clause_matches(c: &Clause, v: &Version) -> bool {
    let target = match &c.ver {
        Some(t) => t,
        None => {
            // Arbitrary `===`: literal string match against the raw input.
            return c.op == Op::Arbitrary && version_raw_eq(&c.raw, v);
        }
    };
    match c.op {
        Op::Eq => v == target,
        Op::Ne => v != target,
        Op::EqStar => release_prefix_matches(v, target),
        Op::NeStar => !release_prefix_matches(v, target),
        Op::Ge => v >= target,
        Op::Le => v <= target,
        Op::Gt => v > target,
        Op::Lt => v < target,
        Op::Arbitrary => version_raw_eq(&c.raw, v),
        Op::Compatible => {
            // ~=X.Y  is equivalent to  >=X.Y, ==X.*
            // ~=X.Y.Z is  >=X.Y.Z, ==X.Y.*
            // i.e. >= target AND same release prefix dropping the last segment.
            if target.release.len() < 2 {
                // ~= requires at least 2 release segments; treat as >=.
                return v >= target;
            }
            if v < target {
                return false;
            }
            let prefix_len = target.release.len() - 1;
            let prefix = Version {
                epoch: target.epoch,
                release: target.release[..prefix_len].to_vec(),
                pre: None,
                post: None,
                dev: None,
                local: None,
            };
            release_prefix_matches(v, &prefix)
        }
    }
}

/// `===` arbitrary equality compares the normalized strings loosely: we compare
/// the raw clause text against the version's canonical-ish rendering. To stay
/// permissive we also accept a parse-and-equal fallback.
fn version_raw_eq(raw: &str, v: &Version) -> bool {
    if let Some(parsed) = Version::parse(raw) {
        return &parsed == v;
    }
    false
}

/// From a list of available version strings, pick the highest that satisfies the
/// specifier set. Pre-releases are only considered when (a) the set explicitly
/// targets a pre-release, or (b) there is no stable version that satisfies.
///
/// Returns the chosen version string (exactly as it appeared in `available`),
/// or `None` if nothing satisfies.
pub fn select_best<'a>(available: &'a [String], spec: &SpecifierSet) -> Option<&'a str> {
    // Parse, keep the original string alongside the parsed version.
    let mut parsed: Vec<(&'a str, Version)> = available
        .iter()
        .filter_map(|s| Version::parse(s).map(|v| (s.as_str(), v)))
        .filter(|(_, v)| spec.matches(v))
        .collect();

    if parsed.is_empty() {
        return None;
    }

    // Prefer stable releases unless the only matches are pre-releases.
    let has_stable = parsed.iter().any(|(_, v)| !v.is_prerelease());
    if has_stable {
        parsed.retain(|(_, v)| !v.is_prerelease());
    }

    parsed
        .into_iter()
        .max_by(|(_, a), (_, b)| a.cmp(b))
        .map(|(s, _)| s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap_or_else(|| panic!("parse failed: {s}"))
    }

    #[test]
    fn parses_basic_release() {
        let a = v("1.2.3");
        assert_eq!(a.release, vec![1, 2, 3]);
        assert!(!a.is_prerelease());
    }

    #[test]
    fn ordering_release() {
        assert!(v("1.2.0") < v("1.10.0"));
        assert!(v("2.0.0") > v("1.99.99"));
        assert!(v("1.2") < v("1.2.1"));
        assert_eq!(v("1.2"), v("1.2.0"));
    }

    #[test]
    fn ordering_prerelease() {
        assert!(v("1.0a1") < v("1.0b1"));
        assert!(v("1.0b1") < v("1.0rc1"));
        assert!(v("1.0rc1") < v("1.0"));
        assert!(v("1.0.dev1") < v("1.0a1"));
        assert!(v("1.0") < v("1.0.post1"));
    }

    #[test]
    fn prerelease_aliases() {
        assert_eq!(v("1.0alpha1"), v("1.0a1"));
        assert_eq!(v("1.0beta2"), v("1.0b2"));
        assert_eq!(v("1.0c1"), v("1.0rc1"));
        assert_eq!(v("1.0preview3"), v("1.0rc3"));
    }

    #[test]
    fn epoch_and_local() {
        assert!(v("1!1.0") > v("9.9.9"));
        assert!(Version::parse("1.0+ubuntu1").is_some());
    }

    #[test]
    fn specifier_ge_lt_range() {
        let spec = SpecifierSet::parse(">=2.10,<3");
        let avail = vec!["2.10.4".into(), "2.11.0".into(), "3.0.0".into()];
        assert_eq!(select_best(&avail, &spec), Some("2.11.0"));
    }

    #[test]
    fn specifier_exact_pin() {
        let spec = SpecifierSet::parse("==2.10.4");
        let avail = vec!["2.10.4".into(), "2.11.0".into(), "3.0.0".into()];
        assert_eq!(select_best(&avail, &spec), Some("2.10.4"));
        assert_eq!(spec.exact_pin().as_deref(), Some("2.10.4"));
    }

    #[test]
    fn specifier_not_equal() {
        let spec = SpecifierSet::parse("!=2.11.0,>=2.10");
        let avail = vec!["2.10.4".into(), "2.11.0".into(), "2.12.0".into()];
        // highest satisfying that isn't 2.11.0 → 2.12.0
        assert_eq!(select_best(&avail, &spec), Some("2.12.0"));
    }

    #[test]
    fn specifier_bare_picks_latest_stable() {
        let spec = SpecifierSet::parse("");
        assert!(spec.is_empty());
        let avail = vec!["1.0.0".into(), "2.0.0".into(), "1.5.0".into()];
        assert_eq!(select_best(&avail, &spec), Some("2.0.0"));
    }

    #[test]
    fn compatible_release_operator() {
        // ~=2.2 means >=2.2,==2.*
        let spec = SpecifierSet::parse("~=2.2");
        let avail = vec!["2.1.0".into(), "2.5.0".into(), "3.0.0".into()];
        assert_eq!(select_best(&avail, &spec), Some("2.5.0"));

        // ~=1.4.2 means >=1.4.2,==1.4.*
        let spec = SpecifierSet::parse("~=1.4.2");
        let avail = vec!["1.4.1".into(), "1.4.9".into(), "1.5.0".into()];
        assert_eq!(select_best(&avail, &spec), Some("1.4.9"));
    }

    #[test]
    fn equal_star_prefix() {
        let spec = SpecifierSet::parse("==1.4.*");
        let avail = vec!["1.3.0".into(), "1.4.0".into(), "1.4.7".into(), "1.5.0".into()];
        assert_eq!(select_best(&avail, &spec), Some("1.4.7"));
    }

    #[test]
    fn upper_bound_excludes_too_new() {
        // The transitive-too-new bug: parent needs <2, latest is 2.x.
        let spec = SpecifierSet::parse(">=1.0,<2.0");
        let avail = vec!["1.9.0".into(), "2.0.0".into(), "2.1.0".into()];
        assert_eq!(select_best(&avail, &spec), Some("1.9.0"));
    }

    #[test]
    fn prerelease_excluded_when_a_stable_satisfies() {
        let spec = SpecifierSet::parse(">=1.0");
        let avail = vec!["1.0.0".into(), "2.0.0rc1".into()];
        // A stable release satisfies → the pre-release is filtered out, even
        // though 2.0.0rc1 also matches >=1.0.
        assert_eq!(select_best(&avail, &spec), Some("1.0.0"));
    }

    #[test]
    fn prerelease_taken_when_only_prereleases_satisfy() {
        // No stable version satisfies (all available stables are too old); the
        // constraint targets a pre-release explicitly.
        let spec = SpecifierSet::parse(">=2.0.0rc1");
        let avail = vec!["1.0.0".into(), "2.0.0rc1".into()];
        assert_eq!(select_best(&avail, &spec), Some("2.0.0rc1"));
    }

    #[test]
    fn final_outranks_its_own_prerelease() {
        // PEP 440: 2.0.0rc1 < 2.0.0. With both present and a >= bound, pick final.
        let spec = SpecifierSet::parse(">=2.0.0rc1");
        let avail = vec!["2.0.0rc1".into(), "2.0.0".into()];
        assert_eq!(select_best(&avail, &spec), Some("2.0.0"));
    }

    #[test]
    fn nothing_satisfies_returns_none() {
        let spec = SpecifierSet::parse(">=9.0");
        let avail = vec!["1.0.0".into(), "2.0.0".into()];
        assert_eq!(select_best(&avail, &spec), None);
    }

    #[test]
    fn merge_constraints() {
        let mut spec = SpecifierSet::parse(">=2.10");
        let parent = SpecifierSet::parse("<3");
        spec.merge(&parent);
        let avail = vec!["2.10.0".into(), "2.99.0".into(), "3.0.0".into()];
        assert_eq!(select_best(&avail, &spec), Some("2.99.0"));
    }

    #[test]
    fn unparseable_clause_is_skipped_not_crashed() {
        // A clause we can't parse is dropped; remaining constraints still apply.
        let spec = SpecifierSet::parse(">=1.0,@@garbage");
        let avail = vec!["1.0.0".into(), "2.0.0".into()];
        assert_eq!(select_best(&avail, &spec), Some("2.0.0"));
    }
}
