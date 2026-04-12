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
use tracing::info;

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
                let deps: Vec<crate::brew::parser::BrewDep> = brew
                    .packages
                    .iter()
                    .map(|e| crate::brew::parser::BrewDep {
                        name: e.name.clone(),
                        kind: match e.kind.as_str() {
                            "cask" => crate::brew::parser::BrewDepKind::Cask,
                            "tap" => crate::brew::parser::BrewDepKind::Tap,
                            _ => crate::brew::parser::BrewDepKind::Formula,
                        },
                    })
                    .collect();
                crate::brew::installer::ensure_installed(&deps)?;
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
                info!(count = brew.packages.len(), "Ensuring Homebrew packages");
                let deps: Vec<crate::brew::parser::BrewDep> = brew
                    .packages
                    .iter()
                    .map(|e| crate::brew::parser::BrewDep {
                        name: e.name.clone(),
                        kind: match e.kind.as_str() {
                            "cask" => crate::brew::parser::BrewDepKind::Cask,
                            "tap" => crate::brew::parser::BrewDepKind::Tap,
                            _ => crate::brew::parser::BrewDepKind::Formula,
                        },
                    })
                    .collect();
                let installed = crate::brew::installer::ensure_installed(&deps)?;
                if installed.is_empty() {
                    info!("All Homebrew packages already installed");
                } else {
                    info!(packages = ?installed, "Newly installed Homebrew packages");
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
            runner::run(&cmd.script, &cmd.args, &project_dir)?;
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
