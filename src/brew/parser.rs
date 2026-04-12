use std::path::Path;
use anyhow::Result;
use tracing::debug;

/// A single Homebrew dependency parsed from a Brewfile.
#[derive(Debug, Clone)]
pub struct BrewDep {
    pub name: String,
    /// "brew", "cask", or "tap"
    pub kind: BrewDepKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BrewDepKind {
    Formula,
    Cask,
    Tap,
}

/// Detect and parse a `Brewfile` in `project_dir`.
/// Returns an empty vec if no Brewfile exists.
pub fn detect_and_parse(project_dir: &Path) -> Result<Vec<BrewDep>> {
    let brewfile = project_dir.join("Brewfile");
    if !brewfile.exists() {
        return Ok(Vec::new());
    }
    debug!(path = %brewfile.display(), "Found Brewfile");
    parse_brewfile(&brewfile)
}

/// Parse a Brewfile into a list of dependencies.
///
/// Supported directives:
///   brew "name"
///   cask "name"
///   tap "owner/repo"
fn parse_brewfile(path: &Path) -> Result<Vec<BrewDep>> {
    let content = std::fs::read_to_string(path)?;
    let mut deps = Vec::new();

    for line in content.lines() {
        let trimmed = line.trim();
        // Skip comments and blank lines
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        if let Some(dep) = parse_line(trimmed) {
            deps.push(dep);
        }
    }

    Ok(deps)
}

/// Parse a single Brewfile line like `brew "postgresql@17"` or `cask "docker"`.
fn parse_line(line: &str) -> Option<BrewDep> {
    let (kind, rest) = if let Some(rest) = line.strip_prefix("brew ") {
        (BrewDepKind::Formula, rest)
    } else if let Some(rest) = line.strip_prefix("cask ") {
        (BrewDepKind::Cask, rest)
    } else if let Some(rest) = line.strip_prefix("tap ") {
        (BrewDepKind::Tap, rest)
    } else {
        return None;
    };

    // Extract the quoted name: `"name"` or `'name'`
    // Also handle trailing options like `, restart_service: true`
    let rest = rest.trim();
    let name = if rest.starts_with('"') {
        rest.trim_start_matches('"')
            .split('"')
            .next()?
    } else if rest.starts_with('\'') {
        rest.trim_start_matches('\'')
            .split('\'')
            .next()?
    } else {
        rest.split(',').next()?.split_whitespace().next()?
    };

    if name.is_empty() {
        return None;
    }

    Some(BrewDep {
        name: name.to_string(),
        kind,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_formula() {
        let dep = parse_line(r#"brew "postgresql@17""#).unwrap();
        assert_eq!(dep.name, "postgresql@17");
        assert_eq!(dep.kind, BrewDepKind::Formula);
    }

    #[test]
    fn parse_cask() {
        let dep = parse_line(r#"cask "docker""#).unwrap();
        assert_eq!(dep.name, "docker");
        assert_eq!(dep.kind, BrewDepKind::Cask);
    }

    #[test]
    fn parse_tap() {
        let dep = parse_line(r#"tap "homebrew/cask-fonts""#).unwrap();
        assert_eq!(dep.name, "homebrew/cask-fonts");
        assert_eq!(dep.kind, BrewDepKind::Tap);
    }

    #[test]
    fn parse_with_options() {
        let dep = parse_line(r#"brew "redis", restart_service: true"#).unwrap();
        assert_eq!(dep.name, "redis");
    }

    #[test]
    fn skip_comment() {
        assert!(parse_line("# this is a comment").is_none());
    }

    #[test]
    fn skip_empty() {
        assert!(parse_line("").is_none());
    }
}
