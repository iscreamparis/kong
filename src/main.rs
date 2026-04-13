mod brew;
mod cli;
mod config;
mod download;
mod extract;
mod link;
mod node;
mod python;
mod runner;
mod rust_eco;
mod store;

use anyhow::Result;
use clap::Parser;
use tracing::{debug, info};

use cli::{Cli, Commands, StoreAction};

fn which_git() -> String {
    // Check PATH first, then common Windows install locations.
    if std::process::Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        return "git".to_string();
    }
    for candidate in &[
        r"C:\Program Files\Git\cmd\git.exe",
        r"C:\Program Files (x86)\Git\cmd\git.exe",
    ] {
        if std::path::Path::new(candidate).exists() {
            return candidate.to_string();
        }
    }
    "git".to_string() // let it fail with a legible OS error
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let filter = if cli.verbose { "kong=trace" } else { "kong=info" };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    match cli.command {
        Commands::Clone(cmd) => {
            // Derive destination directory from the URL if not given.
            let dest = cmd.directory.unwrap_or_else(|| {
                let repo_name = cmd.url
                    .trim_end_matches('/')
                    .rsplit('/')
                    .next()
                    .unwrap_or("repo")
                    .trim_end_matches(".git");
                std::path::PathBuf::from(repo_name)
            });

            info!(url = %cmd.url, dest = %dest.display(), "Cloning repository");

            // Use git from PATH, or fall back to common Windows install locations.
            let git = which_git();
            let status = std::process::Command::new(&git)
                .args(["clone", &cmd.url])
                .arg(&dest)
                .status()
                .map_err(|e| anyhow::anyhow!("Failed to run git ({}): {}", git, e))?;

            if !status.success() {
                anyhow::bail!("git clone failed with exit code {:?}", status.code());
            }

            if !cmd.setup {
                info!(path = %dest.display(), "Clone complete — run `kong rules` then `kong use` to set up");
                return Ok(());
            }

            // Auto-run `kong rules` + `kong use` in the cloned directory.
            info!("Running `kong rules`…");
            let rules = config::generate_rules(&dest, false)?;
            let rules_path = dest.join("kong.rules");
            config::write_rules(&rules, &rules_path)?;
            info!(path = %rules_path.display(), "kong.rules written");

            info!("Running `kong use`…");
            let project_name = dest
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "project".to_string());
            let env_dir = store::rulez_dir(&project_name)?;

            if let Some(ref py) = rules.python {
                python::venv::build_venv(&env_dir, py, &store::store_root()?, &rules)?;
            }
            if let Some(ref node) = rules.node {
                node::modules::build_node_modules(&env_dir, node, &store::store_root()?)?;
            }
            if let Some(ref rs) = rules.rust {
                rust_eco::source::configure_source_replacement(&env_dir, rs, &store::store_root()?, &rules)?;
            }
            if let Some(ref brew) = rules.brew {
                if cfg!(target_os = "macos") || cfg!(target_os = "linux") {
                    let store = store::store_root()?;
                    for entry in &brew.packages {
                        let bottle_dir = store.join(&entry.store_path);
                        if !bottle_dir.exists() {
                            info!(pkg = %entry.name, "Re-downloading missing bottle");
                            let formula = crate::brew::client::resolve_formula(&entry.name)?;
                            crate::brew::client::download_bottle(&formula, &bottle_dir)?;
                        }
                    }
                } else {
                    tracing::warn!(
                        "Skipping brew setup during `kong clone --setup`: Homebrew bottles are only supported on macOS/Linux"
                    );
                }
            }
            link::create_project_junctions(&dest, &env_dir, &rules)?;
            info!(path = %dest.display(), "Clone + setup complete. `cd {}` and you're ready.", dest.display());
        }
        Commands::Rules(cmd) => {
            let project_dir = cmd.path.unwrap_or_else(|| std::env::current_dir().unwrap());
            info!(path = %project_dir.display(), "Generating kong.rules");
            let rules = config::generate_rules(&project_dir, cmd.force)?;
            let rules_path = project_dir.join("kong.rules");
            config::write_rules(&rules, &rules_path)?;
            info!(path = %rules_path.display(), "kong.rules written");
        }
        Commands::Use(cmd) => {
            info!(rules = %cmd.rules_path.display(), clean = cmd.clean, "Applying rules");

            // Canonicalize the rules path so relative paths like ".\kong.rules"
            // resolve to the correct absolute path before we strip the filename.
            let rules_abs = cmd.rules_path
                .canonicalize()
                .unwrap_or_else(|_| cmd.rules_path.clone());
            let project_dir = rules_abs
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."));

            // Derive project name from the directory containing kong.rules.
            let project_name = project_dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "project".to_string());

            // Environments live in C:\kong\RULEZ\<project_name>\ so that hard
            // links from the store (same drive) always work, regardless of which
            // drive the project source lives on.
            let env_dir = store::rulez_dir(&project_name)?;
            info!(env_dir = %env_dir.display(), "Environments will be created in RULEZ");

            if cmd.clean {
                link::clean_environments(&env_dir)?;
                // Also remove project-dir junctions/symlinks
                link::clean_project_junctions(project_dir)?;
                if !cmd.rules_path.exists() {
                    info!("Clean complete");
                    return Ok(());
                }
            }

            let rules = config::read_rules(&cmd.rules_path)?;

            if let Some(ref py) = rules.python {
                python::venv::build_venv(&env_dir, py, &store::store_root()?, &rules)?;
                info!(path = %env_dir.join(".venv").display(), "Python .venv created");
            }
            if let Some(ref node) = rules.node {
                node::modules::build_node_modules(&env_dir, node, &store::store_root()?)?;
                info!(path = %env_dir.join("node_modules").display(), "Node node_modules created");
            }
            if let Some(ref rs) = rules.rust {
                rust_eco::source::configure_source_replacement(&env_dir, rs, &store::store_root()?, &rules)?;
                info!("Rust source replacement configured");
            }

            // ── Brew (system packages) ────────────────────────────────────
            if let Some(ref brew) = rules.brew {
                info!(count = brew.packages.len(), "Ensuring Homebrew bottles in store");
                let store = store::store_root()?;
                let mut downloaded = Vec::new();
                for entry in &brew.packages {
                    let bottle_dir = store.join(&entry.store_path);
                    if !bottle_dir.exists() {
                        info!(pkg = %entry.name, "Downloading missing bottle");
                        let formula = crate::brew::client::resolve_formula(&entry.name)?;
                        crate::brew::client::download_bottle(&formula, &bottle_dir)?;
                        downloaded.push(entry.name.clone());
                    } else {
                        debug!(pkg = %entry.name, "Bottle already in store");
                    }
                }
                if downloaded.is_empty() {
                    info!("All Homebrew bottles already in store");
                } else {
                    info!(packages = ?downloaded, "Newly downloaded Homebrew bottles");
                }
            }

            // ── Project-dir junctions → RULEZ ─────────────────────────────
            // Tools like Vite, Node, and Python resolve modules by walking up
            // the filesystem from the project dir — they never see RULEZ.
            // Create junctions so resolution just works without env hacks.
            link::create_project_junctions(project_dir, &env_dir, &rules)?;
            info!("Project-dir junctions created");
        }
        Commands::Run(cmd) => {
            let project_dir = cmd.path
                .unwrap_or_else(|| std::env::current_dir().unwrap());
            runner::run(&cmd.script, &cmd.args, &project_dir, cmd.no_build)?;
        }
        Commands::Super(cmd) => {
            // ── 1. Clone ─────────────────────────────────────────────────
            let dest = cmd.directory.unwrap_or_else(|| {
                let repo_name = cmd.url
                    .trim_end_matches('/')
                    .rsplit('/')
                    .next()
                    .unwrap_or("repo")
                    .trim_end_matches(".git");
                std::path::PathBuf::from(repo_name)
            });

            info!("════════════════════════════════════════════════════════");
            info!("  KONG SUPER — {}", cmd.url);
            info!("════════════════════════════════════════════════════════");

            if dest.exists() {
                info!(path = %dest.display(), "Directory exists, pulling latest");
                let git = which_git();
                let status = std::process::Command::new(&git)
                    .args(["pull"])
                    .current_dir(&dest)
                    .status()
                    .map_err(|e| anyhow::anyhow!("Failed to run git pull: {}", e))?;
                if !status.success() {
                    tracing::warn!("git pull failed — continuing with existing checkout");
                }
            } else {
                info!(url = %cmd.url, dest = %dest.display(), "[1/4] Cloning repository");
                let git = which_git();
                let status = std::process::Command::new(&git)
                    .args(["clone", &cmd.url])
                    .arg(&dest)
                    .status()
                    .map_err(|e| anyhow::anyhow!("Failed to run git ({}): {}", git, e))?;
                if !status.success() {
                    anyhow::bail!("git clone failed with exit code {:?}", status.code());
                }
            }

            // ── 2. Rules ─────────────────────────────────────────────────
            info!("[2/4] Generating kong.rules");
            let rules = config::generate_rules(&dest, false)?;
            let rules_path = dest.join("kong.rules");
            config::write_rules(&rules, &rules_path)?;
            info!(path = %rules_path.display(), "kong.rules written");

            // ── 3. Use ───────────────────────────────────────────────────
            info!("[3/4] Setting up environments");
            let project_name = dest
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "project".to_string());
            let env_dir = store::rulez_dir(&project_name)?;

            if let Some(ref py) = rules.python {
                python::venv::build_venv(&env_dir, py, &store::store_root()?, &rules)?;
                info!("  ✓ Python .venv");
            }
            if let Some(ref node_sec) = rules.node {
                node::modules::build_node_modules(&env_dir, node_sec, &store::store_root()?)?;
                info!("  ✓ Node node_modules");
            }
            if let Some(ref rs) = rules.rust {
                rust_eco::source::configure_source_replacement(&env_dir, rs, &store::store_root()?, &rules)?;
                info!("  ✓ Rust source replacement");
            }
            if let Some(ref brew) = rules.brew {
                let store = store::store_root()?;
                for entry in &brew.packages {
                    let bottle_dir = store.join(&entry.store_path);
                    if !bottle_dir.exists() {
                        let formula = crate::brew::client::resolve_formula(&entry.name)?;
                        crate::brew::client::download_bottle(&formula, &bottle_dir)?;
                    }
                }
                info!("  ✓ Homebrew bottles ({})", brew.packages.len());
            }
            link::create_project_junctions(&dest, &env_dir, &rules)?;
            info!("  ✓ Project junctions");

            // ── 4. Run scripts ───────────────────────────────────────────
            let scripts_to_run: Vec<String> = if !cmd.run.is_empty() {
                cmd.run
            } else {
                // Run all scripts from kong.rules
                rules.scripts.keys().cloned().collect()
            };

            if scripts_to_run.is_empty() {
                info!("No scripts to run");
            } else {
                info!("[4/4] Running {} script(s)", scripts_to_run.len());
                let mut passed = Vec::new();
                let mut failed = Vec::new();

                for script in &scripts_to_run {
                    info!("──── kong run {} ────", script);
                    match runner::run(script, &[], &dest, cmd.no_build) {
                        Ok(()) => {
                            info!("  ✓ {}", script);
                            passed.push(script.as_str());
                        }
                        Err(e) => {
                            tracing::warn!("  ✗ {} — {}", script, e);
                            failed.push(script.as_str());
                        }
                    }
                }

                info!("════════════════════════════════════════════════════════");
                info!("  RESULTS: {} passed, {} failed", passed.len(), failed.len());
                for s in &passed {
                    info!("    ✓ {}", s);
                }
                for s in &failed {
                    info!("    ✗ {}", s);
                }
                info!("════════════════════════════════════════════════════════");
            }

            info!("SUPER complete → cd {}", dest.display());
        }
        Commands::Store(cmd) => match cmd.action {
            StoreAction::Path => {
                let root = store::store_root()?;
                println!("{}", root.display());
            }
        },
        Commands::Doctor(_cmd) => {
            info!("Running diagnostics...");
            let report = store::doctor()?;
            report.print();
        }
    }

    Ok(())
}
