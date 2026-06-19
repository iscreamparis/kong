//! npm-style platform filtering for Node packages.
//!
//! npm-published platform-native packages (e.g. `@esbuild/linux-x64`,
//! `@rspack/binding-linux-x64-musl`) declare `os`, `cpu`, and sometimes `libc`
//! in their own `package.json`. npm installs such a package **only** when those
//! fields match the host — and because they are virtually always
//! `optionalDependencies`, a non-matching one is *silently skipped* (no error).
//!
//! Without this filter KONG fetched every platform variant the lockfile listed
//! (a CRM store carried `@rspack/binding` for SEVEN platforms, ~340 MB, when
//! only `linux-x64` is usable). This module reproduces npm's eligibility rule so
//! KONG keeps only the host's variant.
//!
//! The matcher is **host-driven and fully general** — it is fed nothing but the
//! package's own `os`/`cpu`/`libc` arrays and a [`HostTriple`]; no package names
//! are special-cased.

/// The host platform expressed in npm's vocabulary.
///
/// `os` ∈ {linux, darwin, win32, …}, `cpu` ∈ {x64, arm64, ia32, …},
/// `libc` ∈ {glibc, musl} (only meaningful on Linux).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostTriple {
    pub os: String,
    pub cpu: String,
    /// Some on Linux ("glibc" | "musl"); None elsewhere (npm `libc` is Linux-only).
    pub libc: Option<String>,
}

impl HostTriple {
    /// Derive the host triple from the Rust runtime, mapping Rust's identifiers
    /// to npm's: `macos`→`darwin`, `windows`→`win32`, `x86_64`→`x64`,
    /// `aarch64`→`arm64`, `x86`→`ia32`. libc is detected only on Linux
    /// (musl vs glibc), defaulting to glibc when undetermined.
    pub fn current() -> Self {
        Self {
            os: map_os(std::env::consts::OS),
            cpu: map_arch(std::env::consts::ARCH),
            libc: current_libc(),
        }
    }
}

/// Map a Rust `std::env::consts::OS` value to npm's `process.platform`.
pub fn map_os(rust_os: &str) -> String {
    match rust_os {
        "macos" => "darwin",
        "windows" => "win32",
        other => other, // "linux", "freebsd", "openbsd", … pass through
    }
    .to_string()
}

/// Map a Rust `std::env::consts::ARCH` value to npm's `process.arch`.
pub fn map_arch(rust_arch: &str) -> String {
    match rust_arch {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        "powerpc64" => "ppc64",
        "s390x" => "s390x",
        other => other, // "riscv64", "arm", … pass through
    }
    .to_string()
}

/// Detect the host C library family on Linux ("musl" | "glibc"); `None` off Linux.
///
/// Mirrors what the wheel/platform code keys on: musllinux vs manylinux. We probe
/// the dynamic loader path — a musl system has `/lib/ld-musl-*.so.*` and no glibc
/// `ld-linux`. When indeterminate we default to glibc (the common case), matching
/// npm's behaviour of treating an unknown host libc as glibc.
#[cfg(target_os = "linux")]
fn current_libc() -> Option<String> {
    // A musl rootfs ships its loader as /lib/ld-musl-<arch>.so.1 and lacks the
    // glibc loader. Detect by presence of a musl loader.
    if let Ok(entries) = std::fs::read_dir("/lib") {
        for e in entries.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("ld-musl-") {
                return Some("musl".to_string());
            }
        }
    }
    Some("glibc".to_string())
}

#[cfg(not(target_os = "linux"))]
fn current_libc() -> Option<String> {
    None
}

/// npm eligibility: a package is installable on `host` iff each declared
/// constraint matches. A constraint is satisfied when the field is absent/empty,
/// or the host value is allowed by the list. Negation (`"!win32"`) is honored:
/// a list of only-negations means "anything except these"; a positive entry
/// means "must be one of these".
///
/// `os`/`cpu`/`libc` are the package's declared arrays (each may be empty/absent).
pub fn is_compatible(
    host: &HostTriple,
    os: &[String],
    cpu: &[String],
    libc: &[String],
) -> bool {
    list_allows(os, &host.os)
        && list_allows(cpu, &host.cpu)
        // libc only constrains when the host even has a libc notion (Linux).
        // npm ignores a `libc` field on non-Linux hosts.
        && match &host.libc {
            Some(host_libc) => list_allows(libc, host_libc),
            None => true,
        }
}

/// True if `value` is allowed by an npm os/cpu/libc list, honoring `!` negation.
///
/// npm semantics:
/// - empty/absent list → no constraint → allowed.
/// - any positive entry present → the list is an allow-list: `value` must be
///   listed positively (and not negated).
/// - only negations present → `value` allowed unless it is negated.
pub fn list_allows(list: &[String], value: &str) -> bool {
    if list.is_empty() {
        return true;
    }

    let mut has_positive = false;
    let mut positive_match = false;
    let mut negated = false;

    for raw in list {
        let entry = raw.trim();
        if let Some(neg) = entry.strip_prefix('!') {
            if neg == value {
                negated = true;
            }
        } else {
            has_positive = true;
            if entry == value {
                positive_match = true;
            }
        }
    }

    // An explicit "!value" always disqualifies.
    if negated {
        return false;
    }
    // With positive entries, value must be among them.
    if has_positive {
        return positive_match;
    }
    // Only negations present and none matched → allowed.
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(os: &str, cpu: &str, libc: Option<&str>) -> HostTriple {
        HostTriple {
            os: os.to_string(),
            cpu: cpu.to_string(),
            libc: libc.map(|s| s.to_string()),
        }
    }

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn no_constraints_always_kept() {
        let h = host("linux", "x64", Some("glibc"));
        assert!(is_compatible(&h, &[], &[], &[]));
    }

    #[test]
    fn darwin_os_skipped_on_linux() {
        let h = host("linux", "x64", Some("glibc"));
        assert!(!is_compatible(&h, &v(&["darwin"]), &[], &[]));
    }

    #[test]
    fn arm64_cpu_skipped_on_x64() {
        let h = host("linux", "x64", Some("glibc"));
        assert!(!is_compatible(&h, &v(&["linux"]), &v(&["arm64"]), &[]));
    }

    #[test]
    fn musl_libc_skipped_on_glibc_host() {
        let h = host("linux", "x64", Some("glibc"));
        // os+cpu match, but libc=musl on a glibc host → skip
        assert!(!is_compatible(&h, &v(&["linux"]), &v(&["x64"]), &v(&["musl"])));
    }

    #[test]
    fn glibc_libc_kept_on_glibc_host() {
        let h = host("linux", "x64", Some("glibc"));
        assert!(is_compatible(&h, &v(&["linux"]), &v(&["x64"]), &v(&["glibc"])));
    }

    #[test]
    fn negation_kept_on_linux() {
        let h = host("linux", "x64", Some("glibc"));
        // os: ["!win32"] → anything but win32 → linux allowed
        assert!(is_compatible(&h, &v(&["!win32"]), &[], &[]));
    }

    #[test]
    fn negation_skipped_when_host_matches_negated() {
        let h = host("win32", "x64", None);
        assert!(!is_compatible(&h, &v(&["!win32"]), &[], &[]));
    }

    #[test]
    fn libc_field_ignored_off_linux() {
        // A darwin host has no libc notion; npm ignores a libc field there.
        let h = host("darwin", "arm64", None);
        assert!(is_compatible(&h, &v(&["darwin"]), &v(&["arm64"]), &v(&["glibc"])));
    }

    // ── non-linux host parameterized cases ───────────────────────────────────

    #[test]
    fn win32_x64_keeps_matching_native() {
        let h = host("win32", "x64", None);
        assert!(is_compatible(&h, &v(&["win32"]), &v(&["x64"]), &[]));
        // a linux-only package is skipped on win32
        assert!(!is_compatible(&h, &v(&["linux"]), &v(&["x64"]), &[]));
    }

    #[test]
    fn darwin_arm64_keeps_matching_native() {
        let h = host("darwin", "arm64", None);
        assert!(is_compatible(&h, &v(&["darwin"]), &v(&["arm64"]), &[]));
        // darwin-x64 (Intel mac) skipped on arm64 host
        assert!(!is_compatible(&h, &v(&["darwin"]), &v(&["x64"]), &[]));
    }

    #[test]
    fn rspack_seven_platforms_only_one_kept_on_linux_glibc() {
        // The live bug: @rspack/binding for 7 platforms, host = linux-x64-glibc.
        let h = host("linux", "x64", Some("glibc"));
        let variants: &[(&[&str], &[&str], &[&str])] = &[
            (&["linux"], &["x64"], &["musl"]),   // linux-x64-musl   → skip
            (&["linux"], &["arm64"], &["musl"]), // linux-arm64-musl → skip
            (&["linux"], &["x64"], &["glibc"]),  // linux-x64-gnu    → KEEP
            (&["darwin"], &["x64"], &[]),        // darwin-x64       → skip
            (&["win32"], &["x64"], &[]),         // win32-x64        → skip
            (&["linux"], &["arm64"], &["glibc"]),// linux-arm64-gnu  → skip
            (&["darwin"], &["arm64"], &[]),      // darwin-arm64     → skip
        ];
        let kept: Vec<bool> = variants
            .iter()
            .map(|(os, cpu, libc)| is_compatible(&h, &v(os), &v(cpu), &v(libc)))
            .collect();
        assert_eq!(kept, vec![false, false, true, false, false, false, false]);
        assert_eq!(kept.iter().filter(|k| **k).count(), 1);
    }

    #[test]
    fn host_triple_current_is_npm_vocab() {
        let h = HostTriple::current();
        // os/cpu are mapped to npm vocab, never raw rust identifiers.
        assert!(!h.os.is_empty());
        assert!(h.os != "macos" && h.os != "windows");
        assert!(h.cpu != "x86_64" && h.cpu != "aarch64");
        // libc is Some iff Linux
        #[cfg(target_os = "linux")]
        assert!(h.libc.is_some());
        #[cfg(not(target_os = "linux"))]
        assert!(h.libc.is_none());
    }

    #[test]
    fn map_helpers() {
        assert_eq!(map_os("macos"), "darwin");
        assert_eq!(map_os("windows"), "win32");
        assert_eq!(map_os("linux"), "linux");
        assert_eq!(map_arch("x86_64"), "x64");
        assert_eq!(map_arch("aarch64"), "arm64");
        assert_eq!(map_arch("x86"), "ia32");
    }
}
