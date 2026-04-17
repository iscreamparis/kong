//! Service management — start, stop, and monitor background daemons
//! (postgres, redis, etc.) that are provided by brew bottles in the store.
//!
//! State lives in `RULEZ/<project>/services/<name>/`:
//!   - `data/`    — data directory (e.g. postgres cluster)
//!   - `pid`      — PID file
//!   - `log`      — stdout/stderr log
//!   - `port`     — port number (text file)

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use tracing::{debug, info};

/// Check whether a process with the given PID is alive.
#[cfg(unix)]
pub(crate) fn is_process_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[cfg(windows)]
pub(crate) fn is_process_alive(pid: u32) -> bool {
    use std::process::Command;
    Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .output()
        .map(|o| {
            let stdout = String::from_utf8_lossy(&o.stdout);
            !stdout.contains("No tasks") && stdout.contains(&pid.to_string())
        })
        .unwrap_or(false)
}

/// Terminate a process by PID.
#[cfg(unix)]
fn terminate_process(pid: u32) {
    unsafe { libc::kill(pid as i32, libc::SIGTERM); }
}

#[cfg(windows)]
fn terminate_process(pid: u32) {
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F"])
        .output();
}

use crate::config::{KongRules, ServiceEntry};
use crate::store;

/// Directory layout for a service instance.
struct ServicePaths {
    root: PathBuf,
    data_dir: PathBuf,
    pid_file: PathBuf,
    log_file: PathBuf,
    port_file: PathBuf,
}

impl ServicePaths {
    fn new(env_dir: &Path, name: &str) -> Self {
        let root = env_dir.join("services").join(name);
        Self {
            data_dir: root.join("data"),
            pid_file: root.join("pid"),
            log_file: root.join("log"),
            port_file: root.join("port"),
            root,
        }
    }

    fn ensure_dirs(&self) -> Result<()> {
        std::fs::create_dir_all(&self.root)?;
        std::fs::create_dir_all(&self.data_dir)?;
        Ok(())
    }
}

// ── Public API ──────────────────────────────────────────────────────────────

/// Start a service (or all services if `name` is None).
pub fn start(
    name: Option<&str>,
    port_override: Option<u16>,
    project_dir: &Path,
) -> Result<()> {
    let (rules, env_dir, store_root) = load_context(project_dir)?;

    let services = match_services(&rules, name)?;

    for svc in services {
        let port = port_override.unwrap_or(svc.port.unwrap_or(0));
        start_one(&svc, port, &env_dir, &store_root, &rules)?;
    }

    Ok(())
}

/// Stop a service (or all services if `name` is None).
pub fn stop(name: Option<&str>, project_dir: &Path) -> Result<()> {
    let (rules, env_dir, store_root) = load_context(project_dir)?;

    let services = match_services(&rules, name)?;

    for svc in services {
        stop_one(&svc, &env_dir, &store_root, &rules)?;
    }

    Ok(())
}

/// Print status of all services.
pub fn status(project_dir: &Path) -> Result<()> {
    let (rules, env_dir, _store_root) = load_context(project_dir)?;

    if rules.services.is_empty() {
        info!("No services defined in kong.rules");
        return Ok(());
    }

    println!("{:<12} {:<8} {:<8} {}", "SERVICE", "STATUS", "PORT", "PID");
    println!("{}", "─".repeat(48));

    for svc in &rules.services {
        let paths = ServicePaths::new(&env_dir, &svc.name);
        let (status, pid) = read_pid_status(&paths);
        let port = read_port(&paths);
        let port_str = port.map_or("—".to_string(), |p| p.to_string());
        let pid_str = pid.map_or("—".to_string(), |p| p.to_string());
        println!("{:<12} {:<8} {:<8} {}", svc.name, status, port_str, pid_str);
    }

    Ok(())
}

/// Print the last `n` lines of a service's log.
pub fn logs(name: &str, lines: usize, project_dir: &Path) -> Result<()> {
    let (rules, env_dir, _store_root) = load_context(project_dir)?;

    let svc = rules
        .services
        .iter()
        .find(|s| s.name == name)
        .with_context(|| format!("service '{name}' not found in kong.rules"))?;

    let paths = ServicePaths::new(&env_dir, &svc.name);

    if !paths.log_file.exists() {
        info!(service = %name, "No log file yet — service may not have been started");
        return Ok(());
    }

    let content = std::fs::read_to_string(&paths.log_file)?;
    let all_lines: Vec<&str> = content.lines().collect();
    let start = all_lines.len().saturating_sub(lines);
    for line in &all_lines[start..] {
        println!("{line}");
    }

    Ok(())
}

// ── Internals ───────────────────────────────────────────────────────────────

fn load_context(project_dir: &Path) -> Result<(KongRules, PathBuf, PathBuf)> {
    let rules_path = project_dir.join("kong.rules");
    if !rules_path.exists() {
        bail!("kong.rules not found in {}. Run `kong rules` first.", project_dir.display());
    }
    let rules = crate::config::read_rules(&rules_path)?;
    let project_name = project_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "project".to_string());
    let env_dir = store::rulez_dir(&project_name)?;
    let store_root = store::store_root()?;
    Ok((rules, env_dir, store_root))
}

fn match_services<'a>(rules: &'a KongRules, name: Option<&str>) -> Result<Vec<&'a ServiceEntry>> {
    if rules.services.is_empty() {
        bail!("No services defined in kong.rules. Does the project have a Brewfile with postgres/redis?");
    }

    match name {
        Some(n) => {
            let svc = rules
                .services
                .iter()
                .find(|s| s.name == n)
                .with_context(|| {
                    let available: Vec<&str> = rules.services.iter().map(|s| s.name.as_str()).collect();
                    format!("service '{n}' not found. Available: {}", available.join(", "))
                })?;
            Ok(vec![svc])
        }
        None => Ok(rules.services.iter().collect()),
    }
}

/// Build the PATH/env for running a service binary (same logic as runner.rs).
fn build_service_env(
    svc: &ServiceEntry,
    store_root: &Path,
    rules: &KongRules,
) -> HashMap<String, String> {
    let mut env: HashMap<String, String> = HashMap::new();
    let sep = if cfg!(windows) { ";" } else { ":" };

    let mut path_parts: Vec<String> = Vec::new();
    let mut lib_parts: Vec<String> = Vec::new();

    // Add all brew bin/sbin and lib dirs
    if let Some(ref brew) = rules.brew {
        for entry in &brew.packages {
            let base = store_root.join(&entry.store_path);
            let bin = base.join("bin");
            if bin.exists() {
                path_parts.push(bin.to_string_lossy().into_owned());
            }
            let sbin = base.join("sbin");
            if sbin.exists() {
                path_parts.push(sbin.to_string_lossy().into_owned());
            }
            let lib = base.join("lib");
            if lib.exists() {
                lib_parts.push(lib.to_string_lossy().into_owned());
            }
        }
    }

    // Append system PATH
    if let Ok(sys) = std::env::var("PATH") {
        path_parts.push(sys);
    }

    env.insert("PATH".to_string(), path_parts.join(sep));

    if !lib_parts.is_empty() {
        let lib_key = if cfg!(target_os = "macos") {
            "DYLD_LIBRARY_PATH"
        } else {
            "LD_LIBRARY_PATH"
        };
        // Append existing
        if let Ok(existing) = std::env::var(lib_key) {
            lib_parts.push(existing);
        }
        env.insert(lib_key.to_string(), lib_parts.join(sep));
    }

    // Service-specific: set the brew package root so shared libs resolve
    if let Some(ref brew) = rules.brew {
        if let Some(pkg) = brew.packages.iter().find(|p| p.name == svc.brew_package) {
            let prefix = store_root.join(&pkg.store_path);
            env.insert("HOMEBREW_PREFIX".to_string(), prefix.to_string_lossy().into_owned());
        }
    }

    env
}

/// Expand template variables in a command string.
/// Paths are shell-quoted to handle spaces (e.g. "Application Support").
fn expand_cmd(
    template: &str,
    paths: &ServicePaths,
    port: u16,
) -> String {
    template
        .replace("{data_dir}", &shell_quote(&paths.data_dir))
        .replace("{log_file}", &shell_quote(&paths.log_file))
        .replace("{pid_file}", &shell_quote(&paths.pid_file))
        .replace("{port}", &port.to_string())
}

/// Single-quote a path for safe use in shell commands.
fn shell_quote(path: &Path) -> String {
    let s = path.to_string_lossy();
    // If it contains spaces or special chars, wrap in single quotes
    // (escape any embedded single quotes with '\'' )
    if s.contains(' ') || s.contains('\'') || s.contains('"') {
        format!("'{}'", s.replace('\'', "'\\''"))
    } else {
        s.into_owned()
    }
}

fn start_one(
    svc: &ServiceEntry,
    port: u16,
    env_dir: &Path,
    store_root: &Path,
    rules: &KongRules,
) -> Result<()> {
    let paths = ServicePaths::new(env_dir, &svc.name);
    paths.ensure_dirs()?;

    // Check if already running
    let (status, _) = read_pid_status(&paths);
    if status == "running" {
        info!(service = %svc.name, "Already running");
        return Ok(());
    }

    let env = build_service_env(svc, store_root, rules);

    // Run init command if needed (e.g. initdb for postgres)
    if let Some(ref init_template) = svc.init_cmd {
        // Only init if data_dir is empty
        let is_empty = paths.data_dir.read_dir()
            .map(|mut d| d.next().is_none())
            .unwrap_or(true);

        if is_empty {
            let init_cmd = expand_cmd(init_template, &paths, port);
            info!(service = %svc.name, cmd = %init_cmd, "Initializing data directory");

            let output = std::process::Command::new("/bin/sh")
                .args(["-c", &init_cmd])
                .envs(&env)
                .output()
                .with_context(|| format!("failed to run init command: {init_cmd}"))?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                bail!("init command failed for {}: {stderr}", svc.name);
            }
            debug!(service = %svc.name, "Data directory initialized");
        }
    }

    // Start the service
    let start_cmd = expand_cmd(&svc.start_cmd, &paths, port);
    info!(service = %svc.name, port, cmd = %start_cmd, "Starting service");

    // For services that daemonize themselves (redis), we just run the command.
    // For services that use pg_ctl, it handles daemonization internally.
    let output = std::process::Command::new("/bin/sh")
        .args(["-c", &start_cmd])
        .envs(&env)
        .output()
        .with_context(|| format!("failed to start service {}: {start_cmd}", svc.name))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        bail!(
            "failed to start {}: {}\n{}",
            svc.name,
            stderr.trim(),
            stdout.trim()
        );
    }

    // Save the port
    std::fs::write(&paths.port_file, port.to_string())?;

    // Try to detect PID if not already written
    if !paths.pid_file.exists() || std::fs::read_to_string(&paths.pid_file).unwrap_or_default().trim().is_empty() {
        // Give the daemon a moment to write its pid
        std::thread::sleep(std::time::Duration::from_millis(500));

        // For pg_ctl, read the postmaster.pid
        if svc.name == "postgres" {
            let pm_pid = paths.data_dir.join("postmaster.pid");
            if pm_pid.exists() {
                if let Ok(content) = std::fs::read_to_string(&pm_pid) {
                    if let Some(first_line) = content.lines().next() {
                        std::fs::write(&paths.pid_file, first_line)?;
                    }
                }
            }
        }
    }

    let (_, pid) = read_pid_status(&paths);
    let pid_str = pid.map_or("unknown".to_string(), |p| p.to_string());
    info!(service = %svc.name, port, pid = %pid_str, "✓ Service started");

    Ok(())
}

fn stop_one(
    svc: &ServiceEntry,
    env_dir: &Path,
    store_root: &Path,
    rules: &KongRules,
) -> Result<()> {
    let paths = ServicePaths::new(env_dir, &svc.name);

    let (status, pid) = read_pid_status(&paths);
    if status != "running" {
        info!(service = %svc.name, "Not running");
        return Ok(());
    }

    if svc.stop_cmd.starts_with("signal:") {
        // Kill by signal
        if let Some(pid) = pid {
            info!(service = %svc.name, pid, "Sending terminate signal");
            terminate_process(pid);
            // Wait briefly for clean shutdown
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
    } else {
        // Run the stop command
        let env = build_service_env(svc, store_root, rules);
        let stop_cmd = expand_cmd(&svc.stop_cmd, &paths, 0);
        info!(service = %svc.name, cmd = %stop_cmd, "Stopping service");

        let output = std::process::Command::new("/bin/sh")
            .args(["-c", &stop_cmd])
            .envs(&env)
            .output()
            .with_context(|| format!("failed to stop service {}: {stop_cmd}", svc.name))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            tracing::warn!(service = %svc.name, "Stop command reported: {}", stderr.trim());
        }
    }

    // Clean up pid file
    let _ = std::fs::remove_file(&paths.pid_file);
    info!(service = %svc.name, "✓ Service stopped");

    Ok(())
}

/// Read PID file and check if the process is alive.
fn read_pid_status(paths: &ServicePaths) -> (&'static str, Option<u32>) {
    let pid = match std::fs::read_to_string(&paths.pid_file) {
        Ok(content) => content.trim().parse::<u32>().ok(),
        Err(_) => None,
    };

    match pid {
        Some(p) => {
            if is_process_alive(p) {
                ("running", Some(p))
            } else {
                ("dead", Some(p)) // stale pid file
            }
        }
        None => ("stopped", None),
    }
}

/// Read the saved port number.
fn read_port(paths: &ServicePaths) -> Option<u16> {
    std::fs::read_to_string(&paths.port_file)
        .ok()
        .and_then(|s| s.trim().parse().ok())
}
