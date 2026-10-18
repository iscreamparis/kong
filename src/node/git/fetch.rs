//! Git subprocesses and lockfile-aware commit resolution for Node dependencies.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Result};
use tracing::{debug, info, warn};

use super::spec::{commit12, is_full_commit, repo_key, GitRef, GitSource};
use crate::config::GitPackageEntry;

/// Credentials travel only in child environments; secrets also cover diagnostics.
#[derive(Debug, Clone, Default)]
pub struct GitEnv {
    pub vars: Vec<(String, String)>,
    pub secrets: Vec<String>,
}

/// Match the CLI's lookup so Git for Windows works even when absent from PATH.
pub fn git_program() -> PathBuf {
    if Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        return PathBuf::from("git");
    }
    for candidate in [
        r"C:\Program Files\Git\cmd\git.exe",
        r"C:\Program Files (x86)\Git\cmd\git.exe",
    ] {
        if Path::new(candidate).exists() {
            return PathBuf::from(candidate);
        }
    }
    PathBuf::from("git")
}

/// Disable interactive authentication without changing Kong's process environment.
pub fn run_git(args: &[&str], cwd: Option<&Path>, env: &GitEnv) -> Result<String> {
    let mut command = Command::new(git_program());
    command
        .args(args)
        .envs(env.vars.iter().map(|(k, v)| (k, v)))
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never");
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let output = command.output().map_err(|error| {
        anyhow::anyhow!(
            "git {} failed to start: {}",
            redact(&args.join(" "), env),
            redact(&error.to_string(), env)
        )
    })?;
    if !output.status.success() {
        bail!(
            "git {} failed (exit {}): {}",
            redact(&args.join(" "), env),
            output.status.code().unwrap_or(-1),
            redact(String::from_utf8_lossy(&output.stderr).trim(), env)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Scrub both known credentials and URL userinfo before exposing diagnostics.
pub fn redact(text: &str, env: &GitEnv) -> String {
    let mut text = text.to_owned();
    let mut secrets: Vec<_> = env.secrets.iter().filter(|s| !s.is_empty()).collect();
    // Redact complete headers before tokens that might be contained in them.
    secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
    for secret in secrets {
        text = text.replace(secret, "***");
    }

    let mut cursor = 0;
    while let Some(offset) = text[cursor..].find("://") {
        let colon = cursor + offset;
        let scheme_start = text[..colon]
            .rfind(|c: char| !c.is_ascii_alphanumeric() && !"+.-".contains(c))
            .map_or(0, |i| i + text[i..].chars().next().unwrap().len_utf8());
        let valid_scheme = text.as_bytes()[scheme_start..colon]
            .first()
            .is_some_and(u8::is_ascii_alphabetic);
        let start = colon + 3;
        let end = text[start..]
            .find(|c: char| c.is_whitespace() || "/?#\"'<>".contains(c))
            .map_or(text.len(), |i| start + i);
        if valid_scheme {
            if let Some(at) = text[start..end].rfind('@') {
                text.replace_range(start..start + at, "***");
                cursor = start + 4; // Skip the replacement and its '@'.
                continue;
            }
        }
        cursor = start;
    }
    text
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    Reused,
    Pinned,
    Remote,
}

/// Keep a matching rules entry pinned until the caller explicitly requests update.
pub fn resolve_commit(
    src: &GitSource,
    previous: Option<&GitPackageEntry>,
    update: bool,
    env: &GitEnv,
) -> Result<(String, Resolution)> {
    let name = redact(&src.name, env);
    if let Some(previous) = previous.filter(|_| !update) {
        info!(pkg = %name, commit = %redact(&previous.commit, env), "Reused git dependency commit");
        return Ok((previous.commit.clone(), Resolution::Reused));
    }
    if let GitRef::Commit(commit) = &src.reference {
        info!(pkg = %name, commit = %redact(commit, env), "Pinned git dependency commit");
        return Ok((commit.clone(), Resolution::Pinned));
    }

    let output = run_git(&["ls-remote", "--", &src.repo], None, env)
        .map_err(|error| resolution_error(src, "ls-remote failed", &error.to_string(), env))?;
    let refs = match &src.reference {
        GitRef::Name(r) => vec![
            format!("refs/tags/{r}^{{}}"),
            format!("refs/tags/{r}"),
            format!("refs/heads/{r}"),
        ],
        GitRef::Default => vec!["HEAD".to_owned()],
        GitRef::Commit(_) => unreachable!(),
    };
    for reference in refs {
        for line in output.lines() {
            let mut fields = line.split_whitespace();
            if let (Some(commit), Some(remote_ref)) = (fields.next(), fields.next()) {
                if remote_ref == reference && is_full_commit(commit) {
                    info!(pkg = %name, commit = %redact(commit, env), "Resolved git dependency commit from remote");
                    return Ok((commit.to_owned(), Resolution::Remote));
                }
            }
        }
    }
    Err(resolution_error(
        src,
        "could not resolve ref",
        "no matching ref advertised by repository",
        env,
    ))
}

fn resolution_error(src: &GitSource, what: &str, reason: &str, env: &GitEnv) -> anyhow::Error {
    dependency_error(
        src,
        what,
        None,
        &[("ref", reference_label(src)), ("reason", reason.to_owned())],
        "Check the repository URL, ref and Git credentials, then run `kong rules` again.",
        env,
    )
}

fn reference_label(src: &GitSource) -> String {
    src.reference.as_option().unwrap_or_else(|| "HEAD".into())
}

/// Shared git-dependency diagnostic: every failure names the dependency, its declared
/// source, the commit and the declaring manifest entry, then one hint line.
fn dependency_error(
    src: &GitSource,
    what: &str,
    commit: Option<&str>,
    facts: &[(&str, String)],
    hint: &str,
    env: &GitEnv,
) -> anyhow::Error {
    let mut text = format!(
        "git dependency '{}' {what}:\n  source      : {}\n  commit      : {}\n  declared by : {}\n",
        src.name,
        src.spec,
        commit.unwrap_or("(unresolved)"),
        src.origin
    );
    for (label, value) in facts {
        // Keep multi-line git stderr inside its labelled column.
        let value = value.replace('\n', &format!("\n{:16}", ""));
        text.push_str(&format!("  {label:<11} : {value}\n"));
    }
    text.push_str(hint);
    anyhow::anyhow!(redact(&text, env))
}

const CLONE_MARKER: &str = ".kong-cloned";
/// After `clone` these persist in the checkout's config, so the checkout and later
/// `git status` calls never rewrite line endings; before `submodule` git passes them
/// on to the submodule clones.
const CLONE_CONFIG: [&str; 4] = [
    "-c",
    "core.autocrlf=false",
    "-c",
    "advice.detachedHead=false",
];

/// One directory per repository and commit, shared by every project using it.
pub fn commit_dir(store_root: &Path, repo: &str, commit: &str) -> PathBuf {
    store_root
        .join("node")
        .join("git")
        .join(repo_key(repo))
        .join(commit12(commit))
}

/// The `commit_dir` location relative to the store, portable in `kong.rules`.
pub fn store_path_for(repo: &str, commit: &str) -> String {
    format!("node/git/{}/{}", repo_key(repo), commit12(commit))
}

#[derive(Debug, Clone)]
pub struct CheckoutOutcome {
    pub commit_dir: PathBuf,
    /// `commit_dir/src`: the working tree at the commit, `.git` included.
    pub checkout: PathBuf,
    /// False when an earlier or concurrent run had already cloned this commit.
    pub cloned: bool,
    /// Clone duration: measured by this call when `cloned`, else read from `.kong-cloned`.
    pub seconds: f64,
}

/// Clone each commit at most once. The marker is written before the atomic rename,
/// so a commit dir holding `.kong-cloned` is always a complete checkout.
pub fn ensure_checkout(
    store_root: &Path,
    src: &GitSource,
    commit: &str,
    env: &GitEnv,
) -> Result<CheckoutOutcome> {
    if !is_full_commit(commit) {
        return Err(dependency_error(
            src,
            "has an invalid commit",
            Some(commit),
            &[("ref", reference_label(src))],
            "Use a full 40-hex commit (fix it in kong.rules or package.json), then run `kong rules` again.",
            env,
        ));
    }
    let dir = commit_dir(store_root, &src.repo, commit);
    if dir.join(CLONE_MARKER).exists() {
        return reuse_checkout(src, commit, dir, env);
    }

    let started = Instant::now();
    let store_hint = "Check that the kong store is writable, then run `kong rules` again.";
    let parent = dir
        .parent()
        .expect("a commit dir always has a repository dir");
    fs::create_dir_all(parent).map_err(|error| {
        checkout_error(
            src,
            commit,
            &dir,
            "could not create its store directory",
            error.to_string(),
            store_hint,
            env,
        )
    })?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let tmp = TmpClone(parent.join(format!(
        ".tmp-{}-{}-{nanos}",
        commit12(commit),
        std::process::id()
    )));
    fs::create_dir(&tmp.0).map_err(|error| {
        checkout_error(
            src,
            commit,
            &dir,
            "could not create its clone directory",
            error.to_string(),
            store_hint,
            env,
        )
    })?;

    let checkout = tmp.0.join("src");
    clone_at_commit(src, commit, &dir, &checkout, env)?;
    if checkout.join(".gitmodules").is_file() {
        let mut args = CLONE_CONFIG.to_vec();
        args.extend([
            "submodule",
            "update",
            "--init",
            "--recursive",
            "--depth",
            "1",
        ]);
        run_git(&args, Some(&checkout), env).map_err(|error| {
            checkout_error(
                src,
                commit,
                &dir,
                "could not fetch its submodules",
                error.to_string(),
                "Check the submodule URLs and Git credentials, then run `kong rules` again.",
                env,
            )
        })?;
    }

    let seconds = started.elapsed().as_secs_f64();
    let marker = serde_json::json!({
        "repo": src.repo,
        "commit": commit,
        "seconds": seconds,
        "cloned_at": chrono::Utc::now().to_rfc3339(),
    });
    fs::write(tmp.0.join(CLONE_MARKER), format!("{marker:#}\n")).map_err(|error| {
        checkout_error(
            src,
            commit,
            &dir,
            "could not write its clone marker",
            error.to_string(),
            store_hint,
            env,
        )
    })?;
    publish(tmp, dir, src, commit, seconds, env)
}

/// Move a finished clone into place; losing the race to another process is not an error.
fn publish(
    tmp: TmpClone,
    dir: PathBuf,
    src: &GitSource,
    commit: &str,
    seconds: f64,
    env: &GitEnv,
) -> Result<CheckoutOutcome> {
    let name = redact(&src.name, env);
    if let Err(error) = fs::rename(&tmp.0, &dir) {
        drop(tmp);
        if dir.join(CLONE_MARKER).is_file() {
            info!(pkg = %name, commit = %commit12(commit), "Another kong process cloned this commit first; reusing it");
            return reuse_checkout(src, commit, dir, env);
        }
        let hint = format!("Remove {} and run `kong rules` again.", dir.display());
        return Err(checkout_error(
            src,
            commit,
            &dir,
            "could not move its clone into the store",
            error.to_string(),
            &hint,
            env,
        ));
    }
    info!(
        pkg = %name,
        commit = %commit12(commit),
        seconds,
        "Cloned {name} {} in {seconds:.1} s",
        commit12(commit)
    );
    Ok(CheckoutOutcome {
        checkout: dir.join("src"),
        commit_dir: dir,
        cloned: true,
        seconds,
    })
}

/// A shallow clone of the named ref is enough while the ref still points at the
/// pinned commit; otherwise (moved tag, pinned commit) the full history is needed.
fn clone_at_commit(
    src: &GitSource,
    commit: &str,
    dir: &Path,
    checkout: &Path,
    env: &GitEnv,
) -> Result<()> {
    let dest = checkout.to_string_lossy();
    let clone = |extra: &[&str]| {
        let mut args = vec!["clone"];
        args.extend(CLONE_CONFIG);
        args.extend(["--quiet", "--no-checkout"]);
        args.extend(extra);
        args.extend(["--", src.repo.as_str(), dest.as_ref()]);
        run_git(&args, None, env)
    };
    let name = redact(&src.name, env);
    let shallow = match &src.reference {
        GitRef::Commit(_) => false,
        reference => {
            let mut extra = vec!["--depth", "1"];
            if let GitRef::Name(r) = reference {
                extra.extend(["--branch", r.as_str()]);
            }
            match clone(&extra).and_then(|_| run_git(&["rev-parse", "HEAD"], Some(checkout), env)) {
                Ok(head) if head == commit => true,
                outcome => {
                    let head = outcome.unwrap_or_else(|error| error.to_string());
                    debug!(pkg = %name, commit = %commit12(commit), head = %head, "Shallow clone is not at the pinned commit; cloning the full history");
                    let _ = crate::link::remove_dir_all_robust(checkout);
                    false
                }
            }
        }
    };
    if !shallow {
        clone(&[]).map_err(|error| {
            checkout_error(
                src,
                commit,
                dir,
                "could not clone its repository",
                error.to_string(),
                "Check the repository URL and Git credentials, then run `kong rules` again.",
                env,
            )
        })?;
    }
    run_git(&["checkout", "--quiet", "--detach", commit], Some(checkout), env).map_err(|error| {
        let what = if shallow { "could not check out its commit" } else { "commit not found in repository" };
        checkout_error(
            src,
            commit,
            dir,
            what,
            error.to_string(),
            "Check that the commit is reachable from a branch or tag of the repository, or fix the reference in package.json.",
            env,
        )
    })?;
    Ok(())
}

fn reuse_checkout(
    src: &GitSource,
    commit: &str,
    dir: PathBuf,
    env: &GitEnv,
) -> Result<CheckoutOutcome> {
    let marker_path = dir.join(CLONE_MARKER);
    let marker = read_marker(&marker_path);
    let recorded = marker.as_ref().and_then(|m| m["commit"].as_str());
    let hint = format!("Remove {} and run `kong rules` again.", dir.display());
    if recorded != Some(commit) {
        let reason = format!(
            "{} records commit {}",
            marker_path.display(),
            recorded.unwrap_or("(unreadable)")
        );
        return Err(checkout_error(
            src,
            commit,
            &dir,
            "store checkout is for another commit",
            reason,
            &hint,
            env,
        ));
    }
    let checkout = dir.join("src");
    if !checkout.is_dir() {
        let reason = format!("{} is missing", checkout.display());
        return Err(checkout_error(
            src,
            commit,
            &dir,
            "store checkout is incomplete",
            reason,
            &hint,
            env,
        ));
    }
    debug!(pkg = %redact(&src.name, env), commit = %commit12(commit), "Reusing store checkout");
    Ok(CheckoutOutcome {
        commit_dir: dir,
        checkout,
        cloned: false,
        seconds: marker.and_then(|m| m["seconds"].as_f64()).unwrap_or(0.0),
    })
}

/// Markers are advisory JSON: an unreadable one reads as absent.
pub fn read_marker(path: &Path) -> Option<serde_json::Value> {
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

fn checkout_error(
    src: &GitSource,
    commit: &str,
    dir: &Path,
    what: &str,
    reason: String,
    hint: &str,
    env: &GitEnv,
) -> anyhow::Error {
    let facts = [
        ("ref", reference_label(src)),
        ("store dir", dir.display().to_string()),
        ("reason", reason),
    ];
    dependency_error(src, what, Some(commit), &facts, hint, env)
}

/// An unfinished clone, removed on every exit path; after the rename it no longer exists.
struct TmpClone(PathBuf);

impl Drop for TmpClone {
    fn drop(&mut self) {
        if let Err(error) = crate::link::remove_dir_all_robust(&self.0) {
            warn!(path = %self.0.display(), error = %error, "Could not remove an unfinished clone");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::git::proof_support::{git, GitFixture};
    use crate::node::git::spec::parse_git_spec;

    fn source(fx: &GitFixture, reference: GitRef) -> GitSource {
        let fragment = reference
            .as_option()
            .map(|r| format!("#{r}"))
            .unwrap_or_default();
        let spec = format!("{}{fragment}", fx.url);
        GitSource {
            name: "@fx/core".into(),
            repo: parse_git_spec(&spec).unwrap().0,
            origin: format!(
                "package.json dependencies \"@fx/core\": \"{spec}\" (kong.git \"@fx/core\")"
            ),
            spec,
            reference,
            path: String::new(),
            build: None,
            rust_targets: Vec::new(),
            declared_by: "package.json".into(),
        }
    }

    fn previous(src: &GitSource, commit: &str) -> GitPackageEntry {
        GitPackageEntry {
            name: src.name.clone(),
            version: "1.0.0".into(),
            source: src.spec.clone(),
            repo: src.repo.clone(),
            reference: src.reference.as_option(),
            commit: commit.into(),
            path: src.path.clone(),
            build: None,
            rust_targets: Vec::new(),
            declared_by: src.declared_by.clone(),
            store_path: "node/git/fixture/commit".into(),
            lock_dir: None,
            packages: Vec::new(),
            local: Vec::new(),
        }
    }

    #[test]
    fn annotated_tag_resolves_to_peeled_commit_before_tag_or_branch() {
        let fx = GitFixture::new(&[("index.js", "initial")]);
        fx.tag_push("v1.0.0", false);
        let peeled = git(&fx.work, &["rev-parse", "v1.0.0^{commit}"]);
        assert_ne!(peeled, git(&fx.work, &["rev-parse", "refs/tags/v1.0.0"]));
        fx.write("index.js", "new branch commit");
        let branch_commit = fx.commit_push("advance branch");
        git(&fx.work, &["branch", "v1.0.0"]);
        git(&fx.work, &["push", "origin", "refs/heads/v1.0.0"]);
        assert_ne!(peeled, branch_commit);

        let src = source(&fx, GitRef::Name("v1.0.0".into()));
        assert_eq!(
            resolve_commit(&src, None, false, &GitEnv::default()).unwrap(),
            (peeled, Resolution::Remote)
        );
    }

    #[test]
    fn branch_name_resolves() {
        let fx = GitFixture::new(&[("index.js", "initial")]);
        git(&fx.work, &["branch", "feature/test"]);
        git(&fx.work, &["push", "origin", "feature/test"]);
        let src = source(&fx, GitRef::Name("feature/test".into()));
        assert_eq!(
            resolve_commit(&src, None, false, &GitEnv::default()).unwrap(),
            (git(&fx.work, &["rev-parse", "HEAD"]), Resolution::Remote)
        );
    }

    #[test]
    fn lightweight_tag_resolves_before_branch() {
        let fx = GitFixture::new(&[("index.js", "initial")]);
        let tagged = git(&fx.work, &["rev-parse", "HEAD"]);
        git(&fx.work, &["tag", "release"]);
        git(&fx.work, &["push", "origin", "refs/tags/release"]);
        fx.write("index.js", "new branch commit");
        fx.commit_push("advance branch");
        git(&fx.work, &["branch", "release"]);
        git(&fx.work, &["push", "origin", "refs/heads/release"]);
        let src = source(&fx, GitRef::Name("release".into()));
        assert_eq!(
            resolve_commit(&src, None, false, &GitEnv::default()).unwrap(),
            (tagged, Resolution::Remote)
        );
    }

    #[test]
    fn default_resolves_to_main_head() {
        let fx = GitFixture::new(&[("index.js", "initial")]);
        let src = source(&fx, GitRef::Default);
        assert_eq!(
            resolve_commit(&src, None, false, &GitEnv::default()).unwrap(),
            (git(&fx.work, &["rev-parse", "main"]), Resolution::Remote)
        );
    }

    #[test]
    fn full_commit_is_pinned_without_contacting_remote() {
        let fx = GitFixture::new(&[("index.js", "initial")]);
        let commit = "abcdef0123456789abcdef0123456789abcdef01";
        let mut src = source(&fx, GitRef::Commit(commit.into()));
        src.repo.push_str(".missing");
        for update in [false, true] {
            assert_eq!(
                resolve_commit(&src, None, update, &GitEnv::default()).unwrap(),
                (commit.into(), Resolution::Pinned)
            );
        }
        let old = previous(&src, "0123456789abcdef0123456789abcdef01234567");
        assert_eq!(
            resolve_commit(&src, Some(&old), false, &GitEnv::default()).unwrap(),
            (old.commit, Resolution::Reused)
        );
    }

    #[test]
    fn previous_commit_is_reused_after_force_move_until_update() {
        let fx = GitFixture::new(&[("index.js", "initial")]);
        fx.tag_push("v1.0.0", false);
        let src = source(&fx, GitRef::Name("v1.0.0".into()));
        let env = GitEnv::default();
        let (first, _) = resolve_commit(&src, None, false, &env).unwrap();
        let old = previous(&src, &first);
        fx.write("index.js", "moved tag");
        let moved = fx.commit_push("move tag");
        fx.tag_push("v1.0.0", true);
        assert_ne!(first, moved);

        // An unavailable remote makes an accidental ls-remote call fail visibly.
        let unavailable = fx.root.path().join("unavailable.git");
        std::fs::rename(&fx.bare, &unavailable).unwrap();
        assert_eq!(
            resolve_commit(&src, Some(&old), false, &env).unwrap(),
            (first, Resolution::Reused)
        );
        std::fs::rename(&unavailable, &fx.bare).unwrap();
        assert_eq!(
            resolve_commit(&src, Some(&old), true, &env).unwrap(),
            (moved, Resolution::Remote)
        );
    }

    #[test]
    fn unknown_ref_reports_source_and_unresolved_commit() {
        let fx = GitFixture::new(&[("index.js", "initial")]);
        let src = source(&fx, GitRef::Name("unknown-ref".into()));
        let error = resolve_commit(&src, None, false, &GitEnv::default())
            .unwrap_err()
            .to_string();
        assert!(error.starts_with("git dependency '@fx/core' could not resolve ref:"));
        assert!(error.contains(&format!("  source      : {}", src.spec)));
        assert!(error.contains("  commit      : (unresolved)"));
        assert!(error.contains(&format!("  declared by : {}", src.origin)));
        assert!(error.contains("  ref         : unknown-ref"));
        assert!(error.lines().last().unwrap().contains("kong rules"));
    }

    #[test]
    fn failed_ls_remote_has_dependency_context_and_redacts_diagnostics() {
        let fx = GitFixture::new(&[("index.js", "initial")]);
        let mut src = source(&fx, GitRef::Default);
        src.repo.push_str("/sentinel-secret");
        src.spec = format!("git+{}", src.repo);
        src.origin = format!("package.json dependencies: {}", src.spec);
        let env = GitEnv {
            secrets: vec!["sentinel-secret".into()],
            ..GitEnv::default()
        };
        let error = resolve_commit(&src, None, false, &env)
            .unwrap_err()
            .to_string();
        assert!(error.starts_with("git dependency '@fx/core' ls-remote failed:"));
        assert!(error.contains(&redact(&src.spec, &env)));
        assert!(error.contains("  commit      : (unresolved)"));
        assert!(error.contains(&redact(&src.origin, &env)));
        assert!(error.contains("  ref         : HEAD"));
        assert!(error.contains("git ls-remote --"));
        assert!(!error.contains("sentinel-secret"));
    }

    #[test]
    fn runner_sets_child_environment_and_trims_stdout() {
        let fx = GitFixture::new(&[("index.js", "initial")]);
        let env = GitEnv {
            vars: vec![
                ("GIT_CONFIG_COUNT".into(), "1".into()),
                ("GIT_CONFIG_KEY_0".into(), "kong.fetch-test".into()),
                ("GIT_CONFIG_VALUE_0".into(), "child-value".into()),
                ("GIT_TERMINAL_PROMPT".into(), "1".into()),
                ("GCM_INTERACTIVE".into(), "always".into()),
            ],
            secrets: Vec::new(),
        };
        let parent: Vec<_> = env
            .vars
            .iter()
            .map(|(key, _)| std::env::var_os(key))
            .collect();
        assert_eq!(
            run_git(
                &["config", "--get", "kong.fetch-test"],
                Some(&fx.work),
                &env
            )
            .unwrap(),
            "child-value"
        );
        for (key, expected) in [("GIT_TERMINAL_PROMPT", "0"), ("GCM_INTERACTIVE", "never")] {
            assert_eq!(
                run_git(
                    &[
                        &format!("--config-env=kong.prompt={key}"),
                        "config",
                        "--get",
                        "kong.prompt"
                    ],
                    Some(&fx.work),
                    &env,
                )
                .unwrap(),
                expected
            );
        }
        assert_eq!(
            run_git(&["rev-parse", "HEAD"], Some(&fx.work), &env).unwrap(),
            git(&fx.work, &["rev-parse", "HEAD"])
        );
        for ((key, _), before) in env.vars.iter().zip(parent) {
            assert_eq!(std::env::var_os(key), before);
        }
    }

    #[test]
    fn runner_redacts_failed_command_arguments_and_stderr() {
        let fx = GitFixture::new(&[("index.js", "initial")]);
        let env = GitEnv {
            secrets: vec!["sentinel-secret".into()],
            ..GitEnv::default()
        };
        let error = run_git(
            &["show", "https://user:pw@host/sentinel-secret"],
            Some(&fx.work),
            &env,
        )
        .unwrap_err()
        .to_string();
        assert!(error.starts_with("git show https://***@host/*** failed (exit 128): "));
        assert!(error.contains("fatal:"));
        assert!(!error.contains("sentinel-secret"));
        assert!(!error.contains("user:pw"));
    }

    #[test]
    fn redaction_masks_secrets_and_url_userinfo() {
        let env = GitEnv {
            secrets: vec!["token".into(), "header token".into(), "".into()],
            ..GitEnv::default()
        };
        assert_eq!(
            redact("token header token token https://user:pw@host/repo", &env),
            "*** *** *** https://***@host/repo"
        );
        assert_eq!(
            redact(
                "é: https://user@host/a ssh://git@host/b git+https://a:b@host/c",
                &env
            ),
            "é: https://***@host/a ssh://***@host/b git+https://***@host/c"
        );
        assert_eq!(
            redact("https://host/path@rev file:///tmp/repo user@host", &env),
            "https://host/path@rev file:///tmp/repo user@host"
        );
    }

    fn tmp_entries(store: &Path, src: &GitSource) -> Vec<String> {
        let parent = store.join("node").join("git").join(repo_key(&src.repo));
        fs::read_dir(parent)
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|n| n.starts_with(".tmp-"))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn tracked_changes(checkout: &Path) -> String {
        git(checkout, &["status", "--porcelain", "--untracked-files=no"])
    }

    #[test]
    fn tag_clone_creates_store_checkout_once() {
        let fx = GitFixture::new(&[("index.js", "initial"), ("crlf.txt", "a\r\nb\r\n")]);
        fx.tag_push("v1.0.0", false);
        let commit = git(&fx.work, &["rev-parse", "v1.0.0^{commit}"]);
        let store = tempfile::TempDir::new().unwrap();
        let src = source(&fx, GitRef::Name("v1.0.0".into()));
        let env = GitEnv::default();

        let first = ensure_checkout(store.path(), &src, &commit, &env).unwrap();
        let expected = store
            .path()
            .join("node")
            .join("git")
            .join(repo_key(&src.repo))
            .join(&commit[..12]);
        assert_eq!(first.commit_dir, expected);
        assert_eq!(first.checkout, expected.join("src"));
        assert!(first.cloned);
        assert_eq!(git(&first.checkout, &["rev-parse", "HEAD"]), commit);
        assert_eq!(
            git(&first.checkout, &["rev-parse", "--is-shallow-repository"]),
            "true"
        );
        assert_eq!(tracked_changes(&first.checkout), "");
        assert_eq!(
            fs::read(first.checkout.join("crlf.txt")).unwrap(),
            b"a\r\nb\r\n"
        );
        assert_eq!(
            run_git(
                &["config", "--get", "core.autocrlf"],
                Some(&first.checkout),
                &env
            )
            .unwrap(),
            "false"
        );
        let marker = read_marker(&expected.join(".kong-cloned")).unwrap();
        assert_eq!(marker["repo"], src.repo.as_str());
        assert_eq!(marker["commit"], commit.as_str());
        assert_eq!(marker["seconds"].as_f64(), Some(first.seconds));
        chrono::DateTime::parse_from_rfc3339(marker["cloned_at"].as_str().unwrap()).unwrap();
        assert!(tmp_entries(store.path(), &src).is_empty());

        let second = ensure_checkout(store.path(), &src, &commit, &env).unwrap();
        assert!(!second.cloned);
        assert_eq!(second.commit_dir, first.commit_dir);
        assert_eq!(second.checkout, first.checkout);
        assert_eq!(second.seconds, first.seconds);
    }

    #[test]
    fn full_commit_that_is_not_the_tip_is_cloned() {
        let fx = GitFixture::new(&[("index.js", "initial")]);
        let old = git(&fx.work, &["rev-parse", "HEAD"]);
        fx.write("index.js", "second");
        let tip = fx.commit_push("second");
        assert_ne!(old, tip);
        let store = tempfile::TempDir::new().unwrap();
        let src = source(&fx, GitRef::Commit(old.clone()));

        let out = ensure_checkout(store.path(), &src, &old, &GitEnv::default()).unwrap();
        assert!(out.cloned);
        assert_eq!(git(&out.checkout, &["rev-parse", "HEAD"]), old);
        assert_eq!(
            fs::read_to_string(out.checkout.join("index.js")).unwrap(),
            "initial"
        );
        assert_eq!(tracked_changes(&out.checkout), "");
        assert!(tmp_entries(store.path(), &src).is_empty());
    }

    #[test]
    fn moved_tag_falls_back_to_full_clone_and_default_stays_shallow() {
        let fx = GitFixture::new(&[("index.js", "initial")]);
        fx.tag_push("v1.0.0", false);
        let pinned = git(&fx.work, &["rev-parse", "v1.0.0^{commit}"]);
        fx.write("index.js", "moved");
        let tip = fx.commit_push("move tag");
        fx.tag_push("v1.0.0", true);
        let store = tempfile::TempDir::new().unwrap();
        let env = GitEnv::default();

        let tagged = source(&fx, GitRef::Name("v1.0.0".into()));
        let out = ensure_checkout(store.path(), &tagged, &pinned, &env).unwrap();
        assert_eq!(git(&out.checkout, &["rev-parse", "HEAD"]), pinned);
        assert_eq!(
            git(&out.checkout, &["rev-parse", "--is-shallow-repository"]),
            "false"
        );
        assert_eq!(tracked_changes(&out.checkout), "");

        let default = source(&fx, GitRef::Default);
        let out = ensure_checkout(store.path(), &default, &tip, &env).unwrap();
        assert_eq!(
            out.commit_dir,
            commit_dir(store.path(), &default.repo, &tip)
        );
        assert_eq!(git(&out.checkout, &["rev-parse", "HEAD"]), tip);
        assert_eq!(
            git(&out.checkout, &["rev-parse", "--is-shallow-repository"]),
            "true"
        );
        assert!(tmp_entries(store.path(), &default).is_empty());
    }

    #[test]
    fn missing_commit_and_unreachable_repo_fail_without_leftovers() {
        let fx = GitFixture::new(&[("index.js", "initial")]);
        let store = tempfile::TempDir::new().unwrap();
        let env = GitEnv::default();
        let missing = "0123456789abcdef0123456789abcdef01234567";
        let src = source(&fx, GitRef::Commit(missing.into()));

        let error = ensure_checkout(store.path(), &src, missing, &env)
            .unwrap_err()
            .to_string();
        assert!(error.starts_with("git dependency '@fx/core' commit not found in repository:"));
        assert!(error.contains(&format!("  source      : {}", src.spec)));
        assert!(error.contains(&format!("  commit      : {missing}")));
        assert!(error.contains(&format!("  declared by : {}", src.origin)));
        assert!(!commit_dir(store.path(), &src.repo, missing).exists());
        assert!(tmp_entries(store.path(), &src).is_empty());

        let mut gone = source(&fx, GitRef::Name("v1.0.0".into()));
        gone.repo.push_str(".missing");
        let error = ensure_checkout(store.path(), &gone, missing, &env)
            .unwrap_err()
            .to_string();
        assert!(error.starts_with("git dependency '@fx/core' could not clone its repository:"));
        assert!(error.contains("  ref         : v1.0.0"));
        assert!(tmp_entries(store.path(), &gone).is_empty());

        let error = ensure_checkout(store.path(), &src, "v1.0.0", &env)
            .unwrap_err()
            .to_string();
        assert!(error.starts_with("git dependency '@fx/core' has an invalid commit:"));
    }

    #[test]
    fn tampered_marker_commit_is_a_mismatch_error() {
        let fx = GitFixture::new(&[("index.js", "initial")]);
        let commit = git(&fx.work, &["rev-parse", "HEAD"]);
        let store = tempfile::TempDir::new().unwrap();
        let src = source(&fx, GitRef::Default);
        let env = GitEnv::default();
        let out = ensure_checkout(store.path(), &src, &commit, &env).unwrap();

        let other = format!("{}0000000000000000000000000000", &commit[..12]);
        let marker_path = out.commit_dir.join(".kong-cloned");
        let mut marker = read_marker(&marker_path).unwrap();
        marker["commit"] = other.as_str().into();
        fs::write(&marker_path, marker.to_string()).unwrap();

        let error = ensure_checkout(store.path(), &src, &commit, &env)
            .unwrap_err()
            .to_string();
        assert!(
            error.starts_with("git dependency '@fx/core' store checkout is for another commit:")
        );
        assert!(error.contains(&format!("  commit      : {commit}")));
        assert!(error.contains(&format!("records commit {other}")));
        assert!(error
            .lines()
            .last()
            .unwrap()
            .contains(&out.commit_dir.display().to_string()));
    }

    #[test]
    fn losing_the_rename_race_reuses_the_complete_commit_dir() {
        let fx = GitFixture::new(&[("index.js", "initial")]);
        let commit = git(&fx.work, &["rev-parse", "HEAD"]);
        let store = tempfile::TempDir::new().unwrap();
        let src = source(&fx, GitRef::Default);
        let env = GitEnv::default();
        let winner = ensure_checkout(store.path(), &src, &commit, &env).unwrap();

        let loser = winner
            .commit_dir
            .with_file_name(format!(".tmp-{}-1-2", &commit[..12]));
        fs::create_dir_all(loser.join("src")).unwrap();
        fs::write(loser.join(".kong-cloned"), "{}").unwrap();
        let out = publish(
            TmpClone(loser.clone()),
            winner.commit_dir.clone(),
            &src,
            &commit,
            9.0,
            &env,
        )
        .unwrap();
        assert!(!out.cloned);
        assert_eq!(out.checkout, winner.checkout);
        assert_eq!(out.seconds, winner.seconds);
        assert!(!loser.exists());
    }

    #[test]
    fn store_path_uses_forward_slashes_and_matches_commit_dir() {
        let repo = "https://github.com/Etcito/raa.git";
        let commit = "3f2c0a9d81b7aaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let rel = store_path_for(repo, commit);
        assert_eq!(rel, format!("node/git/{}/3f2c0a9d81b7", repo_key(repo)));
        assert!(!rel.contains('\\'));
        let store = Path::new("store");
        assert_eq!(commit_dir(store, repo, commit), store.join(&rel));
    }
}
