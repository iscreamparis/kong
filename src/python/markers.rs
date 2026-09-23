//! PEP 508 environment markers — the `; sys_platform == "win32"` tail of a
//! requirement.
//!
//! kong used to STRIP every marker and keep the dependency, so a Windows-only
//! transitive dep was resolved on Linux: `mcp` declares
//! `pywin32>=310; sys_platform == "win32"`, and `kong rules` on Linux aborted
//! with `no suitable file for pywin32==312 (target cp310)` (pywin32 ships only
//! win wheels). This module evaluates the platform part of a marker against the
//! host kong runs on.
//!
//! Evaluation is three-valued on purpose: a variable kong does not know here
//! (`python_version`, `extra`, `platform_release`, ...) or an unparseable marker
//! yields UNKNOWN, and an UNKNOWN marker keeps the dependency — exactly the old
//! behaviour. Only a marker that is DEFINITELY false for this host drops a
//! dependency, so this change can only remove deps that cannot apply here.

/// The marker variables kong can answer for the host platform.
#[derive(Debug, Clone)]
pub struct MarkerEnv {
    pub sys_platform: &'static str,
    pub platform_system: &'static str,
    pub os_name: &'static str,
    pub platform_machine: &'static str,
}

impl MarkerEnv {
    /// The environment of the machine kong is running on (kong installs for its
    /// own host; the runtime it manages is CPython from python-build-standalone).
    pub fn host() -> Self {
        Self::for_target(std::env::consts::OS, std::env::consts::ARCH)
    }

    /// Values Python itself reports (`sys.platform`, `platform.system()`,
    /// `os.name`, `platform.machine()`) for a Rust (os, arch) pair.
    pub fn for_target(os: &str, arch: &str) -> Self {
        let (sys_platform, platform_system, os_name) = match os {
            "windows" => ("win32", "Windows", "nt"),
            "macos" => ("darwin", "Darwin", "posix"),
            "linux" => ("linux", "Linux", "posix"),
            "freebsd" => ("freebsd", "FreeBSD", "posix"),
            _ => ("", "", ""),
        };
        let platform_machine = match (os, arch) {
            ("windows", "x86_64") => "AMD64",
            ("windows", "aarch64") => "ARM64",
            ("macos", "aarch64") => "arm64",
            (_, "x86_64") => "x86_64",
            (_, "aarch64") => "aarch64",
            _ => "",
        };
        MarkerEnv { sys_platform, platform_system, os_name, platform_machine }
    }

    fn lookup(&self, var: &str) -> Option<&'static str> {
        let v = match var {
            "sys_platform" | "sys.platform" => self.sys_platform,
            "platform_system" => self.platform_system,
            "os_name" | "os.name" => self.os_name,
            "platform_machine" | "platform.machine" => self.platform_machine,
            "implementation_name" => "cpython",
            "platform_python_implementation" | "platform.python_implementation" => "CPython",
            _ => return None,
        };
        if v.is_empty() { None } else { Some(v) }
    }
}

/// Should a dependency carrying `marker` (the text after `;`) be installed on
/// this host? False only when the marker is definitely false.
pub fn marker_applies(marker: &str) -> bool {
    marker_applies_in(marker, &MarkerEnv::host())
}

pub fn marker_applies_in(marker: &str, env: &MarkerEnv) -> bool {
    marker_applies_full(marker, env, &[])
}

/// Same, for a dependency of a package installed WITH `extras` (PEP 508
/// `extra == "name"` clauses are true only for a requested extra). With no
/// extras requested, an `extra == ...` clause is false: optional deps are
/// skipped exactly as before.
pub fn marker_applies_with_extras(marker: &str, extras: &[String]) -> bool {
    marker_applies_full(marker, &MarkerEnv::host(), extras)
}

pub fn marker_applies_full(marker: &str, env: &MarkerEnv, extras: &[String]) -> bool {
    let mentions_extra = marker.contains("extra");
    let tokens = match tokenize(marker) {
        Some(t) => t,
        // Unparseable: keep the dependency (old behaviour) unless it is extra-gated
        // (old behaviour skipped anything mentioning `extra ==`).
        None => return !mentions_extra,
    };
    let mut p = Parser { tokens, pos: 0, env, extras };
    match p.or_expr() {
        Some(value) if p.pos == p.tokens.len() => value != Some(false),
        _ => !mentions_extra,
    }
}

/// PEP 685 extra-name normalization: lowercase, runs of `-_.` -> `-`.
pub fn normalize_extra(raw: &str) -> String {
    let mut out = String::new();
    let mut last_sep = false;
    for c in raw.trim().chars() {
        if c == '-' || c == '_' || c == '.' {
            if !last_sep && !out.is_empty() {
                out.push('-');
            }
            last_sep = true;
        } else {
            out.extend(c.to_lowercase());
            last_sep = false;
        }
    }
    out.trim_end_matches('-').to_string()
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Ident(String),
    Str(String),
    Op(String),
    LParen,
    RParen,
}

fn tokenize(s: &str) -> Option<Vec<Tok>> {
    let chars: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
        } else if c == '(' {
            out.push(Tok::LParen);
            i += 1;
        } else if c == ')' {
            out.push(Tok::RParen);
            i += 1;
        } else if c == '"' || c == '\'' {
            let end = chars[i + 1..].iter().position(|&d| d == c)? + i + 1;
            out.push(Tok::Str(chars[i + 1..end].iter().collect()));
            i = end + 1;
        } else if "<>=!~".contains(c) {
            let mut j = i;
            while j < chars.len() && "<>=!~".contains(chars[j]) {
                j += 1;
            }
            out.push(Tok::Op(chars[i..j].iter().collect()));
            i = j;
        } else if c.is_alphanumeric() || c == '_' || c == '.' {
            let mut j = i;
            while j < chars.len() && (chars[j].is_alphanumeric() || chars[j] == '_' || chars[j] == '.') {
                j += 1;
            }
            out.push(Tok::Ident(chars[i..j].iter().collect()));
            i = j;
        } else {
            return None;
        }
    }
    Some(out)
}

/// Some(true) / Some(false) = known; None = unknown for this host.
type Tri = Option<bool>;

struct Parser<'a> {
    tokens: Vec<Tok>,
    pos: usize,
    env: &'a MarkerEnv,
    extras: &'a [String],
}

/// One side of a comparison.
enum Val {
    Known(String),
    Unknown,
    /// The `extra` variable: compared by membership in the requested extras.
    Extra,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<&Tok> {
        self.tokens.get(self.pos)
    }

    fn keyword(&mut self, kw: &str) -> bool {
        if matches!(self.peek(), Some(Tok::Ident(w)) if w == kw) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    // Outer Option = parse success; inner Tri = value.
    fn or_expr(&mut self) -> Option<Tri> {
        let mut acc = self.and_expr()?;
        while self.keyword("or") {
            let rhs = self.and_expr()?;
            acc = match (acc, rhs) {
                (Some(true), _) | (_, Some(true)) => Some(true),
                (Some(false), Some(false)) => Some(false),
                _ => None,
            };
        }
        Some(acc)
    }

    fn and_expr(&mut self) -> Option<Tri> {
        let mut acc = self.atom()?;
        while self.keyword("and") {
            let rhs = self.atom()?;
            acc = match (acc, rhs) {
                (Some(false), _) | (_, Some(false)) => Some(false),
                (Some(true), Some(true)) => Some(true),
                _ => None,
            };
        }
        Some(acc)
    }

    fn atom(&mut self) -> Option<Tri> {
        if self.peek() == Some(&Tok::LParen) {
            self.pos += 1;
            let v = self.or_expr()?;
            if self.peek() != Some(&Tok::RParen) {
                return None;
            }
            self.pos += 1;
            return Some(v);
        }
        let lhs = self.value()?;
        let op = self.op()?;
        let rhs = self.value()?;
        Some(match (&lhs, &rhs) {
            (Val::Extra, Val::Known(name)) | (Val::Known(name), Val::Extra) => {
                let hit = self.extras.iter().any(|e| *e == normalize_extra(name));
                match op.as_str() {
                    "==" | "===" => Some(hit),
                    "!=" => Some(!hit),
                    _ => None,
                }
            }
            (Val::Known(l), Val::Known(r)) => compare(l, &op, r),
            _ => None,
        })
    }

    fn value(&mut self) -> Option<Val> {
        let tok = self.peek()?.clone();
        self.pos += 1;
        match tok {
            Tok::Str(s) => Some(Val::Known(s)),
            Tok::Ident(name) if name == "extra" => Some(Val::Extra),
            Tok::Ident(name) if name != "and" && name != "or" && name != "in" && name != "not" => {
                Some(match self.env.lookup(&name) {
                    Some(v) => Val::Known(v.to_string()),
                    None => Val::Unknown,
                })
            }
            _ => None,
        }
    }

    fn op(&mut self) -> Option<String> {
        match self.peek()?.clone() {
            Tok::Op(o) => {
                self.pos += 1;
                Some(o)
            }
            Tok::Ident(w) if w == "in" => {
                self.pos += 1;
                Some("in".into())
            }
            Tok::Ident(w) if w == "not" => {
                self.pos += 1;
                if self.keyword("in") { Some("not in".into()) } else { None }
            }
            _ => None,
        }
    }
}

fn compare(l: &str, op: &str, r: &str) -> Tri {
    match op {
        "==" | "===" => Some(l == r),
        "!=" => Some(l != r),
        "in" => Some(r.contains(l)),
        "not in" => Some(!r.contains(l)),
        // Ordered comparisons are version comparisons (python_version etc.),
        // which kong does not evaluate here.
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn linux() -> MarkerEnv { MarkerEnv::for_target("linux", "x86_64") }
    fn windows() -> MarkerEnv { MarkerEnv::for_target("windows", "x86_64") }
    fn mac() -> MarkerEnv { MarkerEnv::for_target("macos", "aarch64") }

    #[test]
    fn windows_only_dep_is_dropped_on_linux_and_kept_on_windows() {
        // The real `mcp` Requires-Dist marker that broke `kong rules` on Linux.
        let m = "sys_platform == \"win32\"";
        assert!(!marker_applies_in(m, &linux()));
        assert!(!marker_applies_in(m, &mac()));
        assert!(marker_applies_in(m, &windows()));
    }

    #[test]
    fn single_quotes_not_equal_and_platform_system() {
        assert!(marker_applies_in("sys_platform != 'win32'", &linux()));
        assert!(!marker_applies_in("sys_platform != 'win32'", &windows()));
        assert!(marker_applies_in("platform_system == 'Darwin'", &mac()));
        assert!(!marker_applies_in("platform_system == 'Darwin'", &linux()));
        assert!(marker_applies_in("os_name == 'nt'", &windows()));
        assert!(!marker_applies_in("os_name == 'nt'", &linux()));
    }

    #[test]
    fn and_or_parentheses() {
        let m = "(sys_platform == 'win32' or sys_platform == 'cygwin') and platform_machine == 'AMD64'";
        assert!(marker_applies_in(m, &windows()));
        assert!(!marker_applies_in(m, &linux()));
        // uvloop's real marker
        let u = "sys_platform != 'win32' and (sys_platform != 'cygwin' and platform_python_implementation != 'PyPy')";
        assert!(marker_applies_in(u, &linux()));
        assert!(!marker_applies_in(u, &windows()));
    }

    #[test]
    fn unknown_variables_keep_the_dependency() {
        // python_version is not evaluated: unknown -> keep (old behaviour).
        assert!(marker_applies_in("python_version < '3.11'", &linux()));
        assert!(marker_applies_in("python_version >= '3.8' and sys_platform == 'linux'", &linux()));
        // ...but a definitely-false platform clause still drops it under `and`.
        assert!(!marker_applies_in("python_version < '3.11' and sys_platform == 'win32'", &linux()));
        // ...and an unknown clause under `or` with a false one stays unknown -> keep.
        assert!(marker_applies_in("python_version < '3.8' or sys_platform == 'win32'", &linux()));
    }

    #[test]
    fn membership_and_garbage() {
        assert!(marker_applies_in("'linux' in sys_platform", &linux()));
        assert!(!marker_applies_in("'linux' not in sys_platform", &linux()));
        assert!(marker_applies_in("sys_platform == ", &linux())); // unparseable -> keep
        assert!(marker_applies_in("", &linux()));
    }

    #[test]
    fn extras_are_membership_in_the_requested_set() {
        let crypto = vec!["crypto".to_string()];
        let env = linux();
        assert!(!marker_applies_full("extra == \"crypto\"", &env, &[]));
        assert!(marker_applies_full("extra == \"crypto\"", &env, &crypto));
        assert!(marker_applies_full("extra == 'Crypto'", &env, &crypto), "PEP 685 normalization");
        assert!(!marker_applies_full("extra == 'cli'", &env, &crypto));
        // uvicorn[standard]'s real platform-gated extra.
        let std = vec!["standard".to_string()];
        let m = "(sys_platform != 'win32' and (sys_platform != 'cygwin' and platform_python_implementation != 'PyPy')) and extra == 'standard'";
        assert!(marker_applies_full(m, &env, &std));
        assert!(!marker_applies_full(m, &windows(), &std));
        assert!(!marker_applies_full(m, &env, &[]));
        assert_eq!(normalize_extra("Dev__Tools."), "dev-tools");
    }

    #[test]
    fn machine_names_match_python() {
        assert!(marker_applies_in("platform_machine == 'x86_64'", &linux()));
        assert!(marker_applies_in("platform_machine == 'arm64'", &mac()));
        assert!(marker_applies_in("platform_machine == 'AMD64'", &windows()));
    }
}
