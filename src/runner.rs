//! `kong run <script>` — execute a named script inside the kong-managed environment.
//!
//! Resolution order:
//!   1. `kong.rules` `scripts` section (set by `kong rules`)
//!   2. `package.json` scripts (live read, in case rules is out of date)
//!
//! The process inherits the current environment with these additions:
//!   - PATH is prepended with brew package `bin/` dirs, Rust toolchain `bin/`,
//!     `<env_dir>/node_modules/.bin`, and `<env_dir>/.venv/bin`
//!   - `VIRTUAL_ENV` is set to `<env_dir>/.venv`
//!   - `NODE_PATH` is set to `<env_dir>/node_modules`
//!   - `DYLD_LIBRARY_PATH` / `LD_LIBRARY_PATH` includes brew `lib/` dirs
//!   - `PKG_CONFIG_PATH` includes brew `lib/pkgconfig` dirs
//!
//! If the resolved command references `./target/` and the binary doesn't exist,
//! kong auto-runs `cargo build --release` using the managed Rust toolchain.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{bail, Context, Result};
use tracing::{debug, info};

use crate::config::KongRules;
use crate::store;

/// Run `script` (with optional extra `args`) for the project at `project_dir`.
/// If `no_build` is true, skip automatic `cargo build`.
pub fn run(script: &str, args: &[String], project_dir: &Path, no_build: bool) -> Result<()> {
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
    let store_root = store::store_root()?;

    // ── Build augmented environment ──────────────────────────────────────────
    let augmented_path = build_path(&env_dir, &store_root, rules.as_ref());
    let lib_path = build_lib_path(&store_root, rules.as_ref());
    let pkg_config_path = build_pkg_config_path(&store_root, rules.as_ref());

    // ── Lazy cargo build ─────────────────────────────────────────────────────
    if !no_build {
        maybe_cargo_build(&cmd_str, project_dir, &augmented_path, &lib_path, &store_root, rules.as_ref())?;
    }

    // ── Append extra args to command ─────────────────────────────────────────
    let full_cmd = if args.is_empty() {
        cmd_str.clone()
    } else {
        format!("{cmd_str} {}", args.join(" "))
    };

    info!(script = %script, cmd = %full_cmd, env_dir = %env_dir.display(), "Running script");

    // ── Spawn ────────────────────────────────────────────────────────────────
    let status = spawn(&full_cmd, project_dir, &augmented_path, &lib_path, &pkg_config_path, &env_dir)?;

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

/// Build the PATH string with all kong-managed bin directories prepended.
fn build_path(env_dir: &Path, store_root: &Path, rules: Option<&KongRules>) -> String {
    let current = std::env::var("PATH").unwrap_or_default();
    let separator = if cfg!(windows) { ";" } else { ":" };

    let mut parts: Vec<String> = Vec::new();

    // 1. Brew package bin/ dirs
    if let Some(rules) = rules {
        if let Some(ref brew) = rules.brew {
            for entry in &brew.packages {
                let bin = store_root.join(&entry.store_path).join("bin");
                if bin.exists() {
                    parts.push(bin.to_string_lossy().into_owned());
                }
                // Some packages put binaries in sbin/
                let sbin = store_root.join(&entry.store_path).join("sbin");
                if sbin.exists() {
                    parts.push(sbin.to_string_lossy().into_owned());
                }
            }
        }
    }

    // 2. Rust toolchain bin/
    if let Some(rules) = rules {
        if let Some(ref runtimes) = rules.runtimes {
            if let Some(ref rust) = runtimes.rust {
                let rust_bin = store_root.join(&rust.store_path).join("bin");
                if rust_bin.exists() {
                    parts.push(rust_bin.to_string_lossy().into_owned());
                }
            }
        }
    }

    // 3. Node modules .bin/
    parts.push(env_dir.join("node_modules").join(".bin").to_string_lossy().into_owned());

    // 4. Python venv bin/
    #[cfg(windows)]
    parts.push(env_dir.join(".venv").join("Scripts").to_string_lossy().into_owned());
    #[cfg(not(windows))]
    parts.push(env_dir.join(".venv").join("bin").to_string_lossy().into_owned());

    // 5. Current PATH
    if !current.is_empty() {
        parts.push(current);
    }

    parts.join(separator)
}

/// Build DYLD_LIBRARY_PATH (macOS) / LD_LIBRARY_PATH (Linux) for brew shared libs.
fn build_lib_path(store_root: &Path, rules: Option<&KongRules>) -> String {
    let current = if cfg!(target_os = "macos") {
        std::env::var("DYLD_LIBRARY_PATH").unwrap_or_default()
    } else {
        std::env::var("LD_LIBRARY_PATH").unwrap_or_default()
    };
    let separator = if cfg!(windows) { ";" } else { ":" };

    let mut parts: Vec<String> = Vec::new();

    if let Some(rules) = rules {
        if let Some(ref brew) = rules.brew {
            for entry in &brew.packages {
                let lib = store_root.join(&entry.store_path).join("lib");
                if lib.exists() {
                    parts.push(lib.to_string_lossy().into_owned());
                }
            }
        }
    }

    if !current.is_empty() {
        parts.push(current);
    }

    parts.join(separator)
}

/// Build PKG_CONFIG_PATH for native crate compilation.
fn build_pkg_config_path(store_root: &Path, rules: Option<&KongRules>) -> String {
    let current = std::env::var("PKG_CONFIG_PATH").unwrap_or_default();
    let separator = if cfg!(windows) { ";" } else { ":" };

    let mut parts: Vec<String> = Vec::new();

    if let Some(rules) = rules {
        if let Some(ref brew) = rules.brew {
            for entry in &brew.packages {
                let pkgconfig = store_root.join(&entry.store_path).join("lib").join("pkgconfig");
                if pkgconfig.exists() {
                    parts.push(pkgconfig.to_string_lossy().into_owned());
                }
            }
        }
    }

    if !current.is_empty() {
        parts.push(current);
    }

    parts.join(separator)
}

/// If the command references `./target/` and the binary doesn't exist, auto-build with cargo.
fn maybe_cargo_build(
    cmd: &str,
    project_dir: &Path,
    path: &str,
    lib_path: &str,
    store_root: &Path,
    rules: Option<&KongRules>,
) -> Result<()> {
    // Check if the command references a target/ binary
    if !cmd.contains("./target/") && !cmd.contains(".\\target\\") {
        return Ok(());
    }

    // Extract the binary path from the command
    let binary_path = cmd
        .split_whitespace()
        .find(|w| w.contains("target/") || w.contains("target\\"))
        .map(|w| project_dir.join(w));

    let binary_path = match binary_path {
        Some(p) if !p.exists() => p,
        _ => return Ok(()), // binary exists or can't determine path
    };

    info!(binary = %binary_path.display(), "Binary not found, running cargo build");

    // Find kong-managed cargo
    let cargo_exe = rules
        .and_then(|r| r.runtimes.as_ref())
        .and_then(|rt| rt.rust.as_ref())
        .map(|rust| store_root.join(&rust.store_path).join("bin").join("cargo"))
        .filter(|p| p.exists());

    let cargo = match cargo_exe {
        Some(c) => c.to_string_lossy().into_owned(),
        None => "cargo".to_string(), // fall back to system cargo
    };

    // Determine build profile from path
    let profile_flag = if cmd.contains("release") {
        vec!["--release"]
    } else {
        vec![]
    };

    let lib_env_key = if cfg!(target_os = "macos") {
        "DYLD_LIBRARY_PATH"
    } else {
        "LD_LIBRARY_PATH"
    };

    let status = std::process::Command::new(&cargo)
        .arg("build")
        .args(&profile_flag)
        .current_dir(project_dir)
        .env("PATH", path)
        .env(lib_env_key, lib_path)
        .status()
        .with_context(|| format!("failed to run '{cargo} build'"))?;

    if !status.success() {
        bail!("cargo build failed with exit code {:?}", status.code());
    }

    info!("Cargo build complete");
    Ok(())
}

/// Spawn the command using the OS shell so that shell builtins and PATH resolution work.
fn spawn(
    cmd: &str,
    cwd: &Path,
    path: &str,
    lib_path: &str,
    pkg_config_path: &str,
    env_dir: &Path,
) -> Result<std::process::ExitStatus> {
    #[cfg(windows)]
    let (shell, flag) = ("cmd.exe", "/C");
    #[cfg(not(windows))]
    let (shell, flag) = ("/bin/sh", "-c");

    let venv_path = env_dir.join(".venv");
    let node_modules = env_dir.join("node_modules");

    let mut extra_env: HashMap<String, String> = HashMap::new();
    extra_env.insert("PATH".to_string(), path.to_string());

    if venv_path.exists() {
        extra_env.insert("VIRTUAL_ENV".to_string(), venv_path.to_string_lossy().into_owned());
    }
    if node_modules.exists() {
        extra_env.insert("NODE_PATH".to_string(), node_modules.to_string_lossy().into_owned());
    }

    // Brew library paths
    if !lib_path.is_empty() {
        let key = if cfg!(target_os = "macos") {
            "DYLD_LIBRARY_PATH"
        } else {
            "LD_LIBRARY_PATH"
        };
        extra_env.insert(key.to_string(), lib_path.to_string());
    }
    if !pkg_config_path.is_empty() {
        extra_env.insert("PKG_CONFIG_PATH".to_string(), pkg_config_path.to_string());
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
