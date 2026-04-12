use anyhow::{bail, Context, Result};
use tracing::{debug, info, warn};

use super::parser::{BrewDep, BrewDepKind};

/// Check that `brew` is available on the system.
pub fn check_brew_available() -> Result<()> {
    let output = std::process::Command::new("brew")
        .arg("--version")
        .output()
        .context("Homebrew is not installed. Install it from https://brew.sh")?;

    if !output.status.success() {
        bail!("Homebrew is installed but `brew --version` failed");
    }
    Ok(())
}

/// Get the list of currently installed formulae and casks.
fn installed_formulae() -> Result<std::collections::HashSet<String>> {
    let output = std::process::Command::new("brew")
        .args(["list", "--formula", "-1"])
        .output()
        .context("failed to run `brew list --formula`")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut set: std::collections::HashSet<String> = stdout
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();

    // Also check versioned formulae — `brew list` shows "postgresql@17" as is
    // but let's also grab the --cask list separately
    let cask_output = std::process::Command::new("brew")
        .args(["list", "--cask", "-1"])
        .output()
        .context("failed to run `brew list --cask`")?;

    let cask_stdout = String::from_utf8_lossy(&cask_output.stdout);
    for line in cask_stdout.lines() {
        let name = line.trim();
        if !name.is_empty() {
            set.insert(name.to_string());
        }
    }

    Ok(set)
}

/// Get the list of currently tapped repos.
fn tapped_repos() -> Result<std::collections::HashSet<String>> {
    let output = std::process::Command::new("brew")
        .args(["tap"])
        .output()
        .context("failed to run `brew tap`")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect())
}

/// Ensure all brew dependencies are installed. Returns the list of packages that
/// were newly installed.
pub fn ensure_installed(deps: &[BrewDep]) -> Result<Vec<String>> {
    check_brew_available()?;

    let installed = installed_formulae()?;
    let taps = tapped_repos()?;
    let mut newly_installed = Vec::new();

    // First pass: handle taps (must happen before formulae/casks)
    for dep in deps.iter().filter(|d| d.kind == BrewDepKind::Tap) {
        if taps.contains(&dep.name) {
            debug!(tap = %dep.name, "Already tapped");
            continue;
        }
        info!(tap = %dep.name, "Tapping");
        let status = std::process::Command::new("brew")
            .args(["tap", &dep.name])
            .status()
            .with_context(|| format!("failed to tap {}", dep.name))?;
        if !status.success() {
            warn!(tap = %dep.name, "Failed to tap — continuing");
        } else {
            newly_installed.push(format!("tap:{}", dep.name));
        }
    }

    // Second pass: formulae and casks
    for dep in deps.iter().filter(|d| d.kind != BrewDepKind::Tap) {
        if installed.contains(&dep.name) {
            debug!(pkg = %dep.name, "Already installed");
            continue;
        }
        let subcmd = match dep.kind {
            BrewDepKind::Formula => "install",
            BrewDepKind::Cask => "install",
            BrewDepKind::Tap => unreachable!(),
        };
        let mut cmd = std::process::Command::new("brew");
        cmd.arg(subcmd);
        if dep.kind == BrewDepKind::Cask {
            cmd.arg("--cask");
        }
        cmd.arg(&dep.name);

        info!(pkg = %dep.name, kind = ?dep.kind, "Installing");
        let status = cmd
            .status()
            .with_context(|| format!("failed to install {}", dep.name))?;

        if !status.success() {
            warn!(pkg = %dep.name, "brew install failed — continuing with remaining packages");
        } else {
            newly_installed.push(dep.name.clone());
        }
    }

    Ok(newly_installed)
}
