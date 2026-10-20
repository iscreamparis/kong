//! Reuse the credential git already holds on this machine (Git Credential Manager, a
//! credential helper, the OS keyring), with `gh auth token` as a second source for
//! github.com. Used by the tarball download when no environment token applies.
//!
//! The secret lives in memory only: it is cached for the life of the process, never put
//! in an argv, a URL, `kong.rules`, a marker, a log or an error, and never written to disk.
//! Nothing here may prompt or hang: `GIT_TERMINAL_PROMPT=0`, `GCM_INTERACTIVE=never`,
//! no stdin for `gh`, and a hard timeout that kills the child.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use tracing::debug;

/// What a credential helper returned. `Debug` never shows the values.
#[derive(Clone, PartialEq, Eq)]
pub struct Credential {
    pub username: Option<String>,
    pub password: String,
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Credential(***)")
    }
}

const TIMEOUT: Duration = Duration::from_secs(20);

/// Parse the `key=value` lines of `git credential fill`.
pub fn parse_fill(output: &str) -> Option<Credential> {
    let (mut user, mut pass) = (None, None);
    for line in output.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(v) = line.strip_prefix("password=") {
            pass = Some(v.to_string());
        } else if let Some(v) = line.strip_prefix("username=") {
            user = Some(v.to_string());
        }
    }
    let password = pass.filter(|p| !p.trim().is_empty())?;
    Some(Credential { username: user.filter(|u| !u.is_empty()), password })
}

/// Run `program args` with `stdin_data`, never prompting, killed after `timeout`.
/// Returns stdout on exit 0. stderr is discarded (it may echo secrets).
fn run_quiet(program: &Path, args: &[&str], stdin_data: &str, timeout: Duration) -> Option<String> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = cmd.spawn().ok()?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(stdin_data.as_bytes());
    }
    let mut stdout = child.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stdout.read_to_string(&mut s);
        let _ = tx.send(s);
    });
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let out = rx.recv_timeout(Duration::from_secs(2)).unwrap_or_default();
                return status.success().then_some(out);
            }
            Ok(None) if started.elapsed() < timeout => std::thread::sleep(Duration::from_millis(25)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// `git credential fill` for `https://<host>` using the given git program.
pub fn git_fill(git: &Path, host: &str, timeout: Duration) -> Option<Credential> {
    let input = format!("protocol=https\nhost={host}\n\n");
    let out = run_quiet(git, &["credential", "fill"], &input, timeout)?;
    parse_fill(&out)
}

/// `gh auth token` (github.com only).
pub fn gh_token(gh: &Path, timeout: Duration) -> Option<Credential> {
    let out = run_quiet(gh, &["auth", "token", "--hostname", "github.com"], "", timeout)?;
    let t = out.trim();
    (!t.is_empty() && !t.contains(char::is_whitespace))
        .then(|| Credential { username: None, password: t.to_string() })
}

/// git first, then (github.com only) the gh CLI when it exists.
pub fn lookup_with(git: &Path, gh: &Path, host: &str, timeout: Duration) -> Option<Credential> {
    if let Some(c) = git_fill(git, host, timeout) {
        debug!(host = host, source = "git credential", "download credential found");
        return Some(c);
    }
    if host.eq_ignore_ascii_case("github.com") {
        if let Some(c) = gh_token(gh, timeout) {
            debug!(host = host, source = "gh auth token", "download credential found");
            return Some(c);
        }
    }
    None
}

/// An in-process cache keyed by host, negative answers included (so a missing
/// credential costs one helper call per run, not one per dependency).
#[derive(Default)]
pub struct Cache(Mutex<HashMap<String, Option<Credential>>>);

impl Cache {
    pub fn get_or_fill(&self, host: &str, fill: impl FnOnce() -> Option<Credential>) -> Option<Credential> {
        let key = host.to_ascii_lowercase();
        if let Some(hit) = self.0.lock().unwrap().get(&key) {
            return hit.clone();
        }
        let found = fill();
        self.0.lock().unwrap().insert(key, found.clone());
        found
    }
}

/// The machine credential for an https host (process-wide cache, real git / gh).
pub fn machine_credential(host: &str) -> Option<Credential> {
    static CACHE: OnceLock<Cache> = OnceLock::new();
    CACHE.get_or_init(Cache::default).get_or_fill(host, || {
        lookup_with(&crate::node::git::fetch::git_program(), Path::new("gh"), host, TIMEOUT)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn parses_password_and_username() {
        let c = parse_fill("protocol=https\nhost=github.com\nusername=octo\r\npassword=SENT-1\n").unwrap();
        assert_eq!(c.username.as_deref(), Some("octo"));
        assert_eq!(c.password, "SENT-1");
        assert!(parse_fill("protocol=https\nhost=h\n").is_none());
        assert!(parse_fill("password=  \n").is_none());
        assert!(!format!("{c:?}").contains("SENT-1"));
    }

    #[test]
    fn cache_asks_once_per_host_even_for_a_miss() {
        let calls = AtomicUsize::new(0);
        let cache = Cache::default();
        for _ in 0..3 {
            let r = cache.get_or_fill("Github.com", || {
                calls.fetch_add(1, Ordering::SeqCst);
                None
            });
            assert!(r.is_none());
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// A fake executable (sh script on unix, .cmd on windows).
    fn fake(dir: &Path, name: &str, body_unix: &str, body_win: &str) -> std::path::PathBuf {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let p = dir.join(name);
            std::fs::write(&p, format!("#!/bin/sh\n{body_unix}\n")).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
            let _ = body_win;
            p
        }
        #[cfg(windows)]
        {
            let p = dir.join(format!("{name}.cmd"));
            std::fs::write(&p, format!("@echo off\r\n{body_win}\r\n")).unwrap();
            let _ = body_unix;
            p
        }
    }

    #[test]
    fn git_fill_sends_protocol_and_host_on_stdin_and_reads_the_password() {
        let d = tempfile::TempDir::new().unwrap();
        let log = d.path().join("seen.txt");
        let l = log.display();
        let g = fake(
            d.path(),
            "fakegit",
            &format!("cat > '{l}'\necho \"args=$*\" >> '{l}'\nprintf 'username=u\\npassword=SENTINEL-PW\\n'"),
            &format!("more > \"{l}\"\r\necho args=%* >> \"{l}\"\r\necho username=u\r\necho password=SENTINEL-PW"),
        );
        let c = git_fill(&g, "example.test", Duration::from_secs(10)).unwrap();
        assert_eq!(c.password, "SENTINEL-PW");
        let seen = std::fs::read_to_string(&log).unwrap();
        assert!(seen.contains("protocol=https") && seen.contains("host=example.test"), "{seen}");
        assert!(seen.contains("credential fill"), "{seen}");
    }

    #[test]
    fn a_failing_or_hanging_helper_yields_nothing_quickly() {
        let d = tempfile::TempDir::new().unwrap();
        let bad = fake(d.path(), "badgit", "exit 1", "exit /b 1");
        assert!(git_fill(&bad, "h", Duration::from_secs(5)).is_none());
        let none = d.path().join("does-not-exist");
        assert!(git_fill(&none, "h", Duration::from_secs(5)).is_none());
        let slow = fake(d.path(), "slowgit", "sleep 30", "ping -n 30 127.0.0.1 >nul");
        let t = Instant::now();
        assert!(git_fill(&slow, "h", Duration::from_millis(500)).is_none());
        assert!(t.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn gh_is_the_fallback_for_github_only() {
        let d = tempfile::TempDir::new().unwrap();
        let bad = fake(d.path(), "nogit", "exit 1", "exit /b 1");
        let gh = fake(d.path(), "fakegh", "echo SENTINEL-GH", "echo SENTINEL-GH");
        let c = lookup_with(&bad, &gh, "github.com", Duration::from_secs(10)).unwrap();
        assert_eq!(c.password, "SENTINEL-GH");
        assert!(lookup_with(&bad, &gh, "example.test", Duration::from_secs(10)).is_none());
    }
}
