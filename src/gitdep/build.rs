//! Holds the git-dependency build step (shared §4).
//!
//! A build runs once per commit and build string: its marker
//! (`<commit_dir>/.kong-built-<build_key>`) is the cache key, a lock file keeps two
//! kong processes from building the same commit at once, and the combined output is
//! kept in `<commit_dir>/build-<build_key>.log` for diagnosis.

use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{bail, Context, Result};
use tracing::{debug, info, warn};

use crate::node::git::fetch::{self, GitEnv};
use crate::node::git::spec;
use crate::node::git::{BuildRecord, BuildRequest};

#[derive(Debug, Clone, Default)]
pub struct BuildTools {
    pub path_prepend: Vec<PathBuf>,
    pub env: Vec<(String, String)>,
    pub rust_version: Option<String>,
}

/// A lock older than this belongs to a kong that died mid-build.
const STALE_LOCK: Duration = Duration::from_secs(2 * 60 * 60);
/// How long a second kong waits for another one's build of the same commit.
const LOCK_WAIT: Duration = Duration::from_secs(30 * 60);
const LOCK_POLL: Duration = Duration::from_secs(1);
/// Lines of the build log quoted in a build failure.
const LOG_TAIL: usize = 20;

/// The same build command must run on every OS, so it always goes through bash.
///
/// Unix: the first `bash` on PATH, else `sh`.
#[cfg(not(windows))]
pub fn locate_bash() -> Result<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    for program in ["bash", "sh"] {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(program);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    bail!(
        "no bash or sh found on PATH:\n  PATH        : {}\nA git dependency build step runs with bash; install bash (or sh) and run `kong rules` again.",
        path.to_string_lossy()
    )
}

/// The same build command must run on every OS, so it always goes through bash.
///
/// Windows: the bash that ships with Git for Windows, found from `git --exec-path`
/// (e.g. `C:\Program Files\Git\mingw64\libexec\git-core` → `C:\Program Files\Git`).
/// PATH is never consulted: there `bash.exe` is often WSL's `System32\bash.exe`,
/// which runs the script in a Linux VM that cannot see the Kong toolchain.
#[cfg(windows)]
pub fn locate_bash() -> Result<PathBuf> {
    let exec_path = fetch::run_git(&["--exec-path"], None, &GitEnv::default());
    match &exec_path {
        Ok(exec) => {
            let exec = PathBuf::from(exec.trim());
            // The parent itself, then at most 3 directories above it.
            if let Some(parent) = exec.parent() {
                for dir in parent.ancestors().take(4) {
                    let candidate = dir.join("bin").join("bash.exe");
                    if candidate.is_file() {
                        debug!(bash = %candidate.display(), "Found Git for Windows bash from git --exec-path");
                        return Ok(candidate);
                    }
                }
            }
        }
        Err(e) => {
            debug!(error = %format!("{e:#}"), "git --exec-path failed; trying the default Git for Windows locations")
        }
    }
    for candidate in [
        r"C:\Program Files\Git\bin\bash.exe",
        r"C:\Program Files (x86)\Git\bin\bash.exe",
    ] {
        let candidate = PathBuf::from(candidate);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    bail!(
        "Git for Windows bash not found:\n  git exec    : {}\n  tried       : <git>\\bin\\bash.exe, C:\\Program Files\\Git\\bin\\bash.exe, C:\\Program Files (x86)\\Git\\bin\\bash.exe\nA git dependency build step runs with the bash of Git for Windows (WSL's bash.exe is never used); install Git for Windows and run `kong rules` again.",
        match &exec_path {
            Ok(p) => p.clone(),
            Err(e) => format!("(failed: {e})"),
        }
    )
}

/// A build that names a script in the repository runs it as a file (no executable
/// bit needed, which Windows checkouts do not keep); anything else is a command line.
pub fn build_argv(checkout: &Path, build: &str) -> Vec<String> {
    if checkout.join(build).is_file() {
        vec![build.to_owned()]
    } else {
        vec!["-c".to_owned(), build.to_owned()]
    }
}

/// The dependency's own `rust-toolchain.toml` may need targets the declaration does
/// not repeat (e.g. `wasm32-unknown-unknown`), so both are merged.
pub fn rust_targets_for(req: &BuildRequest) -> Vec<String> {
    let mut targets = req.rust_targets.clone();
    let toolchain_file = req.checkout.join("rust-toolchain.toml");
    if toolchain_file.is_file() {
        match fs::read_to_string(&toolchain_file)
            .map_err(anyhow::Error::from)
            .and_then(|text| Ok(text.parse::<toml::Value>()?))
        {
            Ok(value) => {
                if let Some(list) = value
                    .get("toolchain")
                    .and_then(|t| t.get("targets"))
                    .and_then(|t| t.as_array())
                {
                    targets.extend(list.iter().filter_map(|t| t.as_str()).map(str::to_owned));
                }
            }
            Err(e) => warn!(
                pkg = %req.name,
                file = %toolchain_file.display(),
                error = %format!("{e:#}"),
                "Ignoring unreadable rust-toolchain.toml of {}",
                req.name
            ),
        }
    }
    targets.sort();
    targets.dedup();
    targets
}

/// The Kong-managed tools a build step runs with (shared §4).
///
/// A git dependency's build (wasm-pack, cargo, npm scripts) must not depend on what
/// the host happens to have installed, so the Kong Rust toolchain (plus every extra
/// target the dependency asks for) and the Kong Node runtime come first on PATH, and
/// cargo state goes to one CARGO_HOME shared by every build step in the store.
pub fn build_tools_for(req: &BuildRequest) -> Result<BuildTools> {
    let rust = crate::rust_eco::runtime::ensure_runtime(&req.store_root)
        .context("failed to prepare the Kong Rust toolchain")?;
    for target in rust_targets_for(req) {
        crate::rust_eco::runtime::ensure_target(&req.store_root, &rust, &target)?;
    }
    let toolchain = req.store_root.join(&rust.store_path);
    let rustc = crate::rust_eco::runtime::rustc_exe_in(&toolchain).with_context(|| {
        format!(
            "rustc not found in the Kong Rust toolchain {}",
            toolchain.display()
        )
    })?;

    let node = crate::node::runtime::ensure_runtime(&req.store_root, "lts")
        .context("failed to prepare the Kong Node runtime")?;
    // node lives in `bin/` on Unix and at the runtime root on Windows: its parent
    // is the directory that also holds npm/npx.
    let node_dir = node
        ._node_exe
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| req.store_root.join(&node.store_path));

    let cargo_home = req.store_root.join("node").join("git").join(".cargo-home");
    fs::create_dir_all(&cargo_home)
        .with_context(|| format!("Failed to create {}", cargo_home.display()))?;

    let path_prepend = prepend_order(&toolchain, &cargo_home, &node_dir, req.lock_dir.as_deref());
    debug!(pkg = %req.name, rust = %rust.version, node = %node.version,
        cargo_home = %cargo_home.display(), "Build tools for {} prepared", req.name);
    Ok(BuildTools {
        path_prepend,
        env: vec![
            (
                "CARGO_HOME".to_string(),
                cargo_home.to_string_lossy().into_owned(),
            ),
            ("RUSTC".to_string(), rustc.to_string_lossy().into_owned()),
        ],
        rust_version: Some(rust.version),
    })
}

/// PATH order of shared §4: the toolchain first (its cargo/rustc win), then binaries
/// installed with `cargo install` into the shared CARGO_HOME, then node, then the
/// dependency's own `node_modules/.bin`.
fn prepend_order(
    toolchain: &Path,
    cargo_home: &Path,
    node_dir: &Path,
    lock_dir: Option<&Path>,
) -> Vec<PathBuf> {
    let mut dirs = vec![
        toolchain.join("bin"),
        cargo_home.join("bin"),
        node_dir.to_path_buf(),
    ];
    if let Some(lock_dir) = lock_dir {
        dirs.push(lock_dir.join("node_modules").join(".bin"));
    }
    dirs
}

/// Run the build step of one git dependency at one commit, at most once.
///
/// The marker is the cache: a build that already finished (here, or in a concurrent
/// kong that held the lock) is reported with its recorded seconds and not re-run.
pub fn run_build_with(req: &BuildRequest, tools: &BuildTools) -> Result<BuildRecord> {
    let key = spec::build_key(&req.build);
    let lock_path = req.commit_dir.join(format!(".kong-build-{key}.lock"));
    let _lock = match acquire_lock(req, &lock_path)? {
        Acquired::Lock(guard) => guard,
        Acquired::BuiltElsewhere(seconds) => {
            return Ok(BuildRecord {
                seconds,
                log: req.log.clone(),
            })
        }
    };
    // Another kong may have finished between our marker check and the lock.
    if let Some(seconds) = marker_seconds(&req.marker) {
        debug!(pkg = %req.name, marker = %req.marker.display(), "Build marker appeared before the build started");
        return Ok(BuildRecord {
            seconds,
            log: req.log.clone(),
        });
    }

    let bash = locate_bash().map_err(|e| {
        build_error(
            req,
            "build step could not start",
            &[("reason", format!("{e:#}"))],
            "Install the bash the build step needs, then run `kong rules` again.",
            None,
        )
    })?;
    let argv = build_argv(&req.checkout, &req.build);
    let path = build_path(&tools.path_prepend)
        .map_err(|e| build_error(req, "build step could not start", &[("reason", format!("{e:#}"))],
            "A PATH entry contains the platform's PATH separator; fix PATH and run `kong rules` again.", None))?;

    let log = File::create(&req.log)
        .with_context(|| format!("Failed to create build log {}", req.log.display()))?;
    let log_err = log
        .try_clone()
        .with_context(|| format!("Failed to share build log {}", req.log.display()))?;

    info!(pkg = %req.name, commit = %spec::commit12(&req.commit), build = %req.build, log = %req.log.display(),
        "Building {} {}", req.name, spec::commit12(&req.commit));
    let started = Instant::now();
    let status = Command::new(&bash)
        .args(&argv)
        .current_dir(&req.checkout)
        .envs(tools.env.iter().map(|(k, v)| (k, v)))
        .env("PATH", &path)
        .env("KONG_GIT_DEP", &req.name)
        .env("KONG_GIT_COMMIT", &req.commit)
        .env("KONG_GIT_CHECKOUT", &req.checkout)
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(log_err)
        .status()
        .map_err(|e| {
            build_error(
                req,
                "build step could not start",
                &[
                    ("bash", bash.display().to_string()),
                    ("reason", e.to_string()),
                ],
                "Check that the bash above runs, then run `kong rules` again.",
                None,
            )
        })?;
    let seconds = started.elapsed().as_secs_f64();

    if !status.success() {
        let code = status
            .code()
            .map_or_else(|| "signal".to_owned(), |c| c.to_string());
        return Err(build_error(
            req,
            &format!("build step failed (exit {code})"),
            &[],
            &format!(
                "Reproduce it with `cd {} && {} {}` (KONG_GIT_DEP, KONG_GIT_COMMIT and KONG_GIT_CHECKOUT set), fix the build in the repository, then run `kong rules` again.",
                req.checkout.display(), bash.display(), argv.join(" ")
            ),
            Some(log_tail(&req.log)),
        ));
    }

    let marker = serde_json::json!({
        "build": req.build,
        "commit": req.commit,
        "seconds": seconds,
        "rust": tools.rust_version,
        "finished": chrono::Utc::now().to_rfc3339(),
    });
    fs::write(&req.marker, serde_json::to_string_pretty(&marker)?)
        .with_context(|| format!("Failed to write build marker {}", req.marker.display()))?;

    warn_on_tracked_changes(req);
    info!(pkg = %req.name, commit = %spec::commit12(&req.commit), seconds, log = %req.log.display(),
        "Built {} {} in {:.1} s", req.name, spec::commit12(&req.commit), seconds);
    Ok(BuildRecord {
        seconds,
        log: req.log.clone(),
    })
}

/// The production `BuildFn`: prepare the Kong tools, then build.
///
/// A toolchain or runtime failure is reported in the shared §6 shape so the user
/// sees which dependency needed those tools.
pub fn run_build(req: &BuildRequest) -> Result<BuildRecord> {
    let tools = build_tools_for(req).map_err(|e| {
        build_error(
            req,
            "build tools could not be prepared",
            &[("error", format!("{e:#}"))],
            "the build step needs the Kong-managed Rust toolchain and Node runtime; check network access to static.rust-lang.org and nodejs.org, then run `kong rules` again",
            None,
        )
    })?;
    run_build_with(req, &tools)
}

enum Acquired {
    Lock(LockGuard),
    BuiltElsewhere(f64),
}

/// The lock file is removed on every exit path of the build, including errors.
struct LockGuard(PathBuf);

impl Drop for LockGuard {
    fn drop(&mut self) {
        if let Err(e) = fs::remove_file(&self.0) {
            if e.kind() != ErrorKind::NotFound {
                warn!(lock = %self.0.display(), error = %e, "Failed to remove build lock");
            }
        }
    }
}

/// Take the build lock, or wait for the kong that holds it to write the marker.
///
/// `create_new` is atomic on every OS, so exactly one process builds. A waiter that
/// sees the lock vanish without a marker (the other build failed) tries to take it.
fn acquire_lock(req: &BuildRequest, lock_path: &Path) -> Result<Acquired> {
    let deadline = Instant::now() + LOCK_WAIT;
    let mut announced = false;
    loop {
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(lock_path)
        {
            Ok(mut file) => {
                // The pid only helps a human decide whether the lock is abandoned.
                let _ = write!(file, "{}", std::process::id());
                return Ok(Acquired::Lock(LockGuard(lock_path.to_path_buf())));
            }
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
            Err(e) => {
                return Err(e).with_context(|| {
                    format!("Failed to create build lock {}", lock_path.display())
                })
            }
        }
        if lock_is_stale(lock_path) {
            warn!(pkg = %req.name, lock = %lock_path.display(),
                "Removing stale build lock of {} (older than 2 h)", req.name);
            match fs::remove_file(lock_path) {
                Ok(()) => continue,
                Err(e) if e.kind() == ErrorKind::NotFound => continue,
                Err(e) => {
                    return Err(e).with_context(|| {
                        format!("Failed to remove stale build lock {}", lock_path.display())
                    })
                }
            }
        }
        if let Some(seconds) = marker_seconds(&req.marker) {
            return Ok(Acquired::BuiltElsewhere(seconds));
        }
        if Instant::now() >= deadline {
            return Err(build_error(
                req,
                "build step is still locked by another kong",
                &[("lock", lock_path.display().to_string())],
                "Another kong has been building this commit for 30 min; wait for it, or delete the lock file if no kong is running, then run `kong rules` again.",
                None,
            ));
        }
        if !announced {
            info!(pkg = %req.name, lock = %lock_path.display(),
                "Waiting for another kong to build {} {}", req.name, spec::commit12(&req.commit));
            announced = true;
        }
        std::thread::sleep(LOCK_POLL);
    }
}

fn lock_is_stale(lock_path: &Path) -> bool {
    fs::metadata(lock_path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age > STALE_LOCK)
}

/// A present marker means built; its recorded seconds are reported when readable.
fn marker_seconds(marker: &Path) -> Option<f64> {
    if !marker.is_file() {
        return None;
    }
    Some(
        fetch::read_marker(marker)
            .and_then(|m| m["seconds"].as_f64())
            .unwrap_or(0.0),
    )
}

/// Kong's tools come first so the build uses the managed toolchain, not the host's.
fn build_path(prepend: &[PathBuf]) -> Result<std::ffi::OsString> {
    let current = std::env::var_os("PATH").unwrap_or_default();
    let dirs: Vec<PathBuf> = prepend
        .iter()
        .cloned()
        .chain(std::env::split_paths(&current))
        .collect();
    std::env::join_paths(dirs).context("Failed to join the build PATH")
}

/// The last lines of the build log, redacted, for the failure message.
fn log_tail(log: &Path) -> String {
    let bytes = fs::read(log).unwrap_or_default();
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text.lines().collect();
    let tail = lines[lines.len().saturating_sub(LOG_TAIL)..].join("\n");
    fetch::redact(&tail, &GitEnv::default())
}

/// A build that edits tracked files changes the repository every project shares.
fn warn_on_tracked_changes(req: &BuildRequest) {
    match fetch::run_git(
        &["status", "--porcelain", "--untracked-files=no"],
        Some(&req.checkout),
        &GitEnv::default(),
    ) {
        Ok(status) if status.is_empty() => {}
        Ok(status) => {
            let files: Vec<&str> = status.lines().map(|l| l.get(3..).unwrap_or(l)).collect();
            warn!(pkg = %req.name, commit = %spec::commit12(&req.commit), files = %files.join(", "),
                "Build step of {} modified tracked files in the store checkout: {}; build outputs should be untracked",
                req.name, files.join(", "));
        }
        Err(e) => warn!(pkg = %req.name, error = %format!("{e:#}"),
            "Could not check the store checkout of {} for modified tracked files", req.name),
    }
}

/// Shared §6 diagnostic for a build step (the log tail is appended for failures).
fn build_error(
    req: &BuildRequest,
    what: &str,
    facts: &[(&str, String)],
    hint: &str,
    tail: Option<String>,
) -> anyhow::Error {
    let mut text = format!(
        "git dependency '{}' {what}:\n  source      : {}\n  commit      : {}\n  declared by : {}\n  log         : {}\n  build       : {}\n  checkout    : {}\n",
        req.name,
        req.source,
        req.commit,
        req.origin,
        req.log.display(),
        req.build,
        req.checkout.display()
    );
    for (label, value) in facts {
        let value = value.replace('\n', &format!("\n{:16}", ""));
        text.push_str(&format!("  {label:<11} : {value}\n"));
    }
    text.push_str(hint);
    if let Some(tail) = tail {
        text.push_str("\nlast lines of the log:\n");
        text.push_str(&tail);
    }
    anyhow::anyhow!(fetch::redact(&text, &GitEnv::default()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

    /// A request whose commit dir and checkout live in `tmp`; no git repository needed
    /// except where a test initialises one.
    fn request(tmp: &Path, build: &str) -> BuildRequest {
        let commit_dir = tmp.join("commit");
        let checkout = commit_dir.join("src");
        fs::create_dir_all(&checkout).unwrap();
        let key = spec::build_key(build);
        BuildRequest {
            name: "@fx/core".into(),
            source: "git+file:///fixture/core.git#v1.0.0".into(),
            commit: COMMIT.into(),
            origin: "package.json dependencies \"@fx/core\": \"git+file:///fixture/core.git#v1.0.0\" (kong.git \"@fx/core\")".into(),
            build: build.into(),
            package_dir: checkout.clone(),
            checkout,
            lock_dir: None,
            rust_targets: vec![],
            store_root: tmp.to_path_buf(),
            marker: commit_dir.join(format!(".kong-built-{key}")),
            log: commit_dir.join(format!("build-{key}.log")),
            commit_dir,
        }
    }

    fn write_script(req: &BuildRequest, rel: &str, body: &str) {
        let path = req.checkout.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    #[test]
    fn argv_file_form_vs_command_form() {
        let tmp = TempDir::new().unwrap();
        let req = request(tmp.path(), "scripts/build.sh");
        assert_eq!(
            build_argv(&req.checkout, "scripts/build.sh"),
            vec!["-c", "scripts/build.sh"]
        );
        write_script(&req, "scripts/build.sh", "echo hi\n");
        assert_eq!(
            build_argv(&req.checkout, "scripts/build.sh"),
            vec!["scripts/build.sh"]
        );
        assert_eq!(
            build_argv(&req.checkout, "npm run build"),
            vec!["-c", "npm run build"]
        );
        // A directory is not a script.
        assert_eq!(build_argv(&req.checkout, "scripts"), vec!["-c", "scripts"]);
    }

    #[test]
    fn successful_build_writes_marker_and_log() {
        let tmp = TempDir::new().unwrap();
        let req = request(
            tmp.path(),
            "echo built-ok; printf '%s' \"$KONG_GIT_COMMIT\" > out.txt",
        );
        let record = run_build_with(&req, &BuildTools::default()).unwrap();
        assert_eq!(record.log, req.log);
        assert!(fs::read_to_string(&req.log).unwrap().contains("built-ok"));
        assert_eq!(
            fs::read_to_string(req.checkout.join("out.txt")).unwrap(),
            COMMIT
        );
        let marker = fetch::read_marker(&req.marker).unwrap();
        assert_eq!(marker["commit"], COMMIT);
        assert_eq!(marker["build"], req.build.as_str());
        assert!(marker["rust"].is_null());
        assert!(marker["finished"].as_str().is_some());
        let lock = req
            .commit_dir
            .join(format!(".kong-build-{}.lock", spec::build_key(&req.build)));
        assert!(!lock.exists(), "the lock is removed after the build");
    }

    #[test]
    fn failing_script_error_names_dep_exit_log_and_last_line() {
        let tmp = TempDir::new().unwrap();
        let req = request(tmp.path(), "scripts/fail.sh");
        write_script(
            &req,
            "scripts/fail.sh",
            "echo first-line\necho kong-last-line >&2\nexit 3\n",
        );
        let err = run_build_with(&req, &BuildTools::default())
            .err()
            .expect("build fails");
        let text = format!("{err:#}");
        assert!(
            text.starts_with("git dependency '@fx/core' build step failed (exit 3):"),
            "{text}"
        );
        assert!(text.contains(&req.log.display().to_string()), "{text}");
        assert!(text.contains(COMMIT), "{text}");
        assert!(text.contains(&req.origin), "{text}");
        assert!(
            text.contains("last lines of the log:\nfirst-line\nkong-last-line"),
            "{text}"
        );
        assert!(!req.marker.exists(), "a failed build writes no marker");
        let lock = req
            .commit_dir
            .join(format!(".kong-build-{}.lock", spec::build_key(&req.build)));
        assert!(!lock.exists(), "the lock is removed on failure too");
    }

    #[test]
    fn existing_marker_skips_the_build() {
        let tmp = TempDir::new().unwrap();
        let req = request(tmp.path(), "touch ran.txt");
        fs::write(&req.marker, r#"{"seconds": 4.5}"#).unwrap();
        let record = run_build_with(&req, &BuildTools::default()).unwrap();
        assert_eq!(record.seconds, 4.5);
        assert!(
            !req.checkout.join("ran.txt").exists(),
            "the script must not run"
        );
    }

    #[test]
    fn stale_lock_is_removed() {
        let tmp = TempDir::new().unwrap();
        let req = request(tmp.path(), "touch ran.txt");
        let lock = req
            .commit_dir
            .join(format!(".kong-build-{}.lock", spec::build_key(&req.build)));
        let file = File::create(&lock).unwrap();
        file.set_modified(SystemTime::now() - Duration::from_secs(3 * 60 * 60))
            .unwrap();
        drop(file);
        run_build_with(&req, &BuildTools::default()).unwrap();
        assert!(
            req.checkout.join("ran.txt").exists(),
            "the build ran after removing the stale lock"
        );
        assert!(!lock.exists());
    }

    #[test]
    fn prepend_order_follows_shared_section_4() {
        let dirs = prepend_order(
            Path::new("/s/rust/toolchain/1.0.0"),
            Path::new("/s/node/git/.cargo-home"),
            Path::new("/s/node/runtime/22/bin"),
            Some(Path::new("/c/src/pkg")),
        );
        assert_eq!(
            dirs,
            vec![
                PathBuf::from("/s/rust/toolchain/1.0.0/bin"),
                PathBuf::from("/s/node/git/.cargo-home/bin"),
                PathBuf::from("/s/node/runtime/22/bin"),
                PathBuf::from("/c/src/pkg/node_modules/.bin"),
            ]
        );
        let without_lock = prepend_order(Path::new("/t"), Path::new("/h"), Path::new("/n"), None);
        assert_eq!(without_lock.len(), 3);
    }

    #[test]
    fn targets_union_sorted_and_deduplicated() {
        let tmp = TempDir::new().unwrap();
        let mut req = request(tmp.path(), "true");
        req.rust_targets = vec![
            "x86_64-unknown-linux-musl".into(),
            "wasm32-unknown-unknown".into(),
        ];
        assert_eq!(
            rust_targets_for(&req),
            vec!["wasm32-unknown-unknown", "x86_64-unknown-linux-musl"]
        );
        fs::write(
            req.checkout.join("rust-toolchain.toml"),
            "[toolchain]\nchannel = \"1.80.0\"\ntargets = [\"wasm32-unknown-unknown\", \"aarch64-apple-darwin\"]\n",
        )
        .unwrap();
        assert_eq!(
            rust_targets_for(&req),
            vec![
                "aarch64-apple-darwin",
                "wasm32-unknown-unknown",
                "x86_64-unknown-linux-musl"
            ]
        );
        fs::write(req.checkout.join("rust-toolchain.toml"), "not [valid toml").unwrap();
        assert_eq!(
            rust_targets_for(&req),
            vec!["wasm32-unknown-unknown", "x86_64-unknown-linux-musl"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn locate_bash_finds_a_shell() {
        let bash = locate_bash().unwrap();
        assert!(bash.is_file(), "{}", bash.display());
    }
}
