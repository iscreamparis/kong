mod cli;
mod config;
mod download;
mod extract;
mod link;
mod node;
mod python;
mod rust_eco;
mod store;

use anyhow::Result;
use clap::Parser;
use tracing::info;

use cli::{Cli, Commands, StoreAction};

fn main() -> Result<()> {
    let cli = Cli::parse();

    let filter = if cli.verbose { "kong=trace" } else { "kong=info" };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    match cli.command {
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
            let rules = config::read_rules(&cmd.rules_path)?;
            let project_dir = cmd
                .rules_path
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."));

            if cmd.clean {
                link::clean_environments(project_dir)?;
            }

            if let Some(ref py) = rules.python {
                python::venv::build_venv(project_dir, py, &store::store_root()?, &rules)?;
                info!("Python .venv created");
            }
            if let Some(ref node) = rules.node {
                node::modules::build_node_modules(project_dir, node, &store::store_root()?)?;
                info!("Node node_modules created");
            }
            if let Some(ref rs) = rules.rust {
                rust_eco::source::configure_source_replacement(project_dir, rs, &store::store_root()?)?;
                info!("Rust source replacement configured");
            }
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
