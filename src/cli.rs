use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// KONG — Unified dependency manager for Python, Node.js, and Rust
#[derive(Parser)]
#[command(name = "kong", version, about, long_about = None)]
pub struct Cli {
    /// Enable verbose output
    #[arg(short, long, global = true)]
    pub verbose: bool,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Parse manifests and create/update kong.rules
    Rules(RulesCmd),
    /// Create virtual environments from kong.rules
    Use(UseCmd),
    /// Run a named script in the kong-managed environment
    Run(RunCmd),
    /// Manage the central store
    Store(StoreCmd),
    /// Run diagnostic checks
    Doctor(DoctorCmd),
}

#[derive(Parser)]
pub struct RulesCmd {
    /// Force re-download even if packages are already cached
    #[arg(short, long)]
    pub force: bool,

    /// Path to project directory (defaults to current directory)
    #[arg(short, long)]
    pub path: Option<PathBuf>,
}

#[derive(Parser)]
pub struct UseCmd {
    /// Path to kong.rules file (defaults to ./kong.rules)
    #[arg(default_value = "kong.rules")]
    pub rules_path: PathBuf,

    /// Remove existing virtual environments before rebuilding
    #[arg(long)]
    pub clean: bool,
}

#[derive(Parser)]
pub struct RunCmd {
    /// Name of the script to run (e.g. dev, build, test)
    pub script: String,

    /// Extra arguments forwarded to the script
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,

    /// Path to project directory (defaults to current directory)
    #[arg(short, long)]
    pub path: Option<PathBuf>,
}

#[derive(Parser)]
pub struct StoreCmd {
    #[command(subcommand)]
    pub action: StoreAction,
}

#[derive(Subcommand)]
pub enum StoreAction {
    /// Print the store root path
    Path,
}

#[derive(Parser)]
pub struct DoctorCmd;
