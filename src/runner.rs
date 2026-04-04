//! `kong run <script>` — execute a named script inside the kong-managed environment.
//!
//! Resolution order:
//!   1. `kong.rules` `scripts` section (set by `kong rules`)
//!   2. `package.json` scripts (live read, in case rules is out of date)
//!
//! The process inherits the current environment with these additions:
//!   - PATH is prepended with `<env_dir>/node_modules/.bin` and `<env_dir>/.venv/Scripts`
//!   - `VIRTUAL_ENV` is set to `<env_dir>/.venv`
//!   - `NODE_PATH` is set to `<env_dir>/node_modules`

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use tracing::{debug, info};

use crate::config::KongRules;
use crate::store;

/// Run `script` (with optional extra `args`) for the project at `project_dir`.
pub fn run(script: &str, args: &[String], project_dir: &Path) -> Result<()> {
    // ── Locate kong.rules ────────────────────────────────────────────────────
    let rules_path = project_dir.join("kong.rules");
    let rules: Option<KongRules> = if rules_path.exists() {
        Some(crate::config::read_rules(&rules_path)?)
    } else {
        None
    };

    // ── Resolve script command ───────────────────────────────────────────────
    let cmd_str = resolve_script(script, rules.as_ref(), project_dir)
        .with_context(|| format!("script '{script}' not found in kong.rules or package.json"))?;

    // ── Derive project name ──────────────────────────────────────────────────
    let project_name = project_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "project".to_string());

    let env_dir = store::rulez_dir(&project_name)?;

    // ── Build augmented PATH ─────────────────────────────────────────────────
    let augmented_path = build_path(&env_dir);

    // ── Append extra args to command ─────────────────────────────────────────
    let full_cmd = if args.is_empty() {
        cmd_str.clone()
    } else {
        format!("{cmd_str} {}", args.join(" "))
    };

    info!(script = %script, cmd = %full_cmd, env_dir = %env_dir.display(), "Running script");

    // ── Spawn ────────────────────────────────────────────────────────────────
    let status = spawn(&full_cmd, project_dir, &augmented_path, &env_dir)?;

    if !status.success() {
        let code = status.code().unwrap_or(1);
        bail!("script '{script}' exited with code {code}");
    }

    Ok(())
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Try to find the named script in kong.rules, then package.json.
fn resolve_script(script: &str, rules: Option<&KongRules>, project_dir: &Path) -> Option<String> {
    // 1. kong.rules scripts section
    if let Some(r) = rules {
        if let Some(cmd) = r.scripts.get(script) {
            debug!(script, cmd, "Resolved from kong.rules");
            return Some(cmd.clone());
        }
    }

    // 2. package.json live read
    let pkg_json = project_dir.join("package.json");
    if let Ok(content) = std::fs::read_to_string(&pkg_json) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&content) {
            if let Some(cmd) = v
                .get("scripts")
                .and_then(|s| s.get(script))
                .and_then(|c| c.as_str())
            {
                debug!(script, cmd, "Resolved from package.json");
                return Some(cmd.to_string());
            }
        }
    }

    None
}

/// Build the PATH string: bin dirs from env_dir prepended before the current PATH.
fn build_path(env_dir: &Path) -> String {
    let current = std::env::var("PATH").unwrap_or_default();

    #[cfg(windows)]
    let extra: Vec<PathBuf> = vec![
        env_dir.join("node_modules").join(".bin"),
        env_dir.join(".venv").join("Scripts"),
    ];
    #[cfg(not(windows))]
    let extra: Vec<PathBuf> = vec![
        env_dir.join("node_modules").join(".bin"),
        env_dir.join(".venv").join("bin"),
    ];

    let separator = if cfg!(windows) { ";" } else { ":" };

    let mut parts: Vec<String> = extra
        .into_iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    if !current.is_empty() {
        parts.push(current);
    }
    parts.join(separator)
}

/// Spawn the command using the OS shell so that shell builtins and PATH resolution work.
fn spawn(
    cmd: &str,
    cwd: &Path,
    path: &str,
    env_dir: &Path,
) -> Result<std::process::ExitStatus> {
    #[cfg(windows)]
    let (shell, flag) = ("cmd.exe", "/C");
    #[cfg(not(windows))]
    let (shell, flag) = ("/bin/sh", "-c");

    let venv_path = env_dir.join(".venv");
    let node_modules = env_dir.join("node_modules");

    let mut extra_env: HashMap<&str, String> = HashMap::new();
    extra_env.insert("PATH", path.to_string());
    if venv_path.exists() {
        extra_env.insert("VIRTUAL_ENV", venv_path.to_string_lossy().into_owned());
    }
    if node_modules.exists() {
        extra_env.insert("NODE_PATH", node_modules.to_string_lossy().into_owned());
    }

    debug!(shell, cmd, cwd = %cwd.display(), "Spawning");

    let status = std::process::Command::new(shell)
        .arg(flag)
        .arg(cmd)
        .current_dir(cwd)
        .envs(&extra_env)
        .status()
        .with_context(|| format!("failed to spawn shell '{shell}' for command: {cmd}"))?;

    Ok(status)
}
