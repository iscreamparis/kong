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
    /// Clone a git repository and optionally run `kong rules` + `kong use`
    Clone(CloneCmd),
    /// Parse manifests and create/update kong.rules
    Rules(RulesCmd),
    /// Create virtual environments from kong.rules
    Use(UseCmd),
    /// Run a named script in the kong-managed environment
    Run(RunCmd),
    /// Clone + rules + use + run scripts — full end-to-end setup & smoke test
    Super(SuperCmd),
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

    /// Skip automatic cargo build when target binary is missing
    #[arg(long)]
    pub no_build: bool,
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

#[derive(Parser)]
pub struct CloneCmd {
    /// Repository URL to clone (e.g. https://github.com/owner/repo)
    pub url: String,

    /// Destination directory (defaults to the repository name)
    pub directory: Option<PathBuf>,

    /// Automatically run `kong rules` + `kong use` after clone
    #[arg(long)]
    pub setup: bool,
}

#[derive(Parser)]
pub struct SuperCmd {
    /// Repository URL to clone (e.g. https://github.com/owner/repo)
    pub url: String,

    /// Destination directory (defaults to the repository name)
    pub directory: Option<PathBuf>,

    /// Scripts to run after setup (defaults to all scripts in kong.rules)
    #[arg(short, long)]
    pub run: Vec<String>,

    /// Skip automatic cargo build when target binary is missing
    #[arg(long)]
    pub no_build: bool,
}
