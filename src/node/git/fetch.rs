//! Git subprocesses and lockfile-aware commit resolution for Node dependencies.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Result};
use tracing::info;

use super::spec::{is_full_commit, GitRef, GitSource};
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
    let reference = src.reference.as_option().unwrap_or_else(|| "HEAD".into());
    anyhow::anyhow!(redact(
        &format!(
            "git dependency '{}' {what}:\n  source      : {}\n  commit      : (unresolved)\n  declared by : {}\n  ref         : {reference}\n  reason      : {reason}\nCheck the repository URL, ref and Git credentials, then run `kong rules` again.",
            src.name, src.spec, src.origin
        ),
        env
    ))
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
}
