//! Holds the git-dependency build step (shared §4).

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};

use crate::node::git::{BuildRecord, BuildRequest};

#[derive(Debug, Clone, Default)]
pub struct BuildTools {
    pub path_prepend: Vec<PathBuf>,
    pub env: Vec<(String, String)>,
    pub rust_version: Option<String>,
}

pub fn locate_bash() -> Result<PathBuf> {
    bail!("gitdep::build::locate_bash is not implemented yet")
}

pub fn build_argv(_checkout: &Path, _build: &str) -> Vec<String> {
    Vec::new()
}

pub fn rust_targets_for(_req: &BuildRequest) -> Vec<String> {
    Vec::new()
}

pub fn build_tools_for(_req: &BuildRequest) -> Result<BuildTools> {
    bail!("gitdep::build::build_tools_for is not implemented yet")
}

pub fn run_build_with(_req: &BuildRequest, _tools: &BuildTools) -> Result<BuildRecord> {
    bail!("gitdep::build::run_build_with is not implemented yet")
}

pub fn run_build(_req: &BuildRequest) -> Result<BuildRecord> {
    bail!("gitdep::build::run_build is not implemented yet")
}
