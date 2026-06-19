use std::path::Path;

use anyhow::{Context, Result};
use tracing::debug;

/// A parsed dependency from a Python manifest.
///
/// `version` is a concrete pin when the manifest gave one (`==X.Y.Z`, or a
/// lockfile's resolved version); it is empty for a non-exact requirement, in
/// which case `spec` carries the raw PEP 440 specifier (`>=2.10,<3`, `~=1.4`)
/// so the resolver downloads the highest version that satisfies it rather than
/// the global latest. Lockfiles produce an exact `version` and an empty `spec`.
#[derive(Debug, Clone)]
pub struct PythonDep {
    pub name: String,
    pub version: String,
    /// Raw PEP 440 specifier from the manifest (empty for exact lockfile pins).
    pub spec: String,
}

/// Detect and parse Python dependency files in a project directory.
/// Priority: uv.lock > poetry.lock > Pipfile.lock > requirements.txt > pyproject.toml
pub fn detect_and_parse(project_dir: &Path) -> Result<Vec<PythonDep>> {
    // uv.lock (TOML)
    let uv_lock = project_dir.join("uv.lock");
    if uv_lock.exists() {
        debug!("Found uv.lock");
        return parse_uv_lock(&uv_lock);
    }

    // poetry.lock (TOML)
    let poetry_lock = project_dir.join("poetry.lock");
    if poetry_lock.exists() {
        debug!("Found poetry.lock");
        return parse_poetry_lock(&poetry_lock);
    }

    // Pipfile.lock (JSON)
    let pipfile_lock = project_dir.join("Pipfile.lock");
    if pipfile_lock.exists() {
        debug!("Found Pipfile.lock");
        return parse_pipfile_lock(&pipfile_lock);
    }

    // requirements.txt
    let requirements = project_dir.join("requirements.txt");
    if requirements.exists() {
        debug!("Found requirements.txt");
        return parse_requirements_txt(&requirements);
    }

    // pyproject.toml
    let pyproject = project_dir.join("pyproject.toml");
    if pyproject.exists() {
        debug!("Found pyproject.toml");
        return parse_pyproject_toml(&pyproject);
    }

    Ok(vec![])
}

/// Parse requirements.txt: extract `package==version` lines.
pub fn parse_requirements_txt(path: &Path) -> Result<Vec<PythonDep>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;

    let mut deps = Vec::new();
    for line in content.lines() {
        // Strip inline comments (`pkg==1.0  # note`) then trim.
        let line = line.split(" #").next().unwrap_or(line);
        let line = line.trim();

        // Skip empty, comments, flags, includes
        if line.is_empty() || line.starts_with('#') || line.starts_with('-') {
            continue;
        }
        // Skip URL / VCS / local-path requirements (not PyPI-resolvable here).
        if line.contains("://") || line.starts_with('.') || line.starts_with('/') {
            continue;
        }

        if let Some(dep) = parse_requirement_line(line) {
            deps.push(dep);
        }
    }

    Ok(deps)
}

/// Parse one requirement line into a name + (exact version OR raw specifier).
///
/// Honors every PEP 440 operator: an exact `==X.Y.Z` becomes a concrete
/// `version`; anything else (`>=`, `<`, `~=`, ranges, `!=`, `==X.*`) is kept as
/// the raw `spec` for the resolver to satisfy. A bare `name` (no specifier) ->
/// empty version + empty spec (resolves to latest).
fn parse_requirement_line(line: &str) -> Option<PythonDep> {
    // Strip environment markers and extras for the name; keep the specifier raw.
    let body = line.split(';').next().unwrap_or(line).trim();
    if body.is_empty() {
        return None;
    }
    // Name runs until the first specifier operator. Brackets ([extras]) and the
    // usual name chars are part of the name token.
    let name_end = body
        .find(|c: char| matches!(c, '=' | '!' | '<' | '>' | '~' | ' ' | '\t'))
        .unwrap_or(body.len());
    let raw_name = body[..name_end].trim();
    let name = match raw_name.find('[') {
        Some(b) => raw_name[..b].trim(),
        None => raw_name,
    };
    if name.is_empty() {
        return None;
    }
    let spec = body[name_end..].trim().to_string();

    // Exact single `==X.Y.Z` pin → concrete version, empty spec.
    let set = crate::python::pep440::SpecifierSet::parse(&spec);
    if let Some(pin) = set.exact_pin() {
        return Some(PythonDep {
            name: normalize_python_name(name),
            version: pin,
            spec: String::new(),
        });
    }

    Some(PythonDep {
        name: normalize_python_name(name),
        version: String::new(),
        spec,
    })
}

/// Parse pyproject.toml [project.dependencies] or [tool.poetry.dependencies].
pub fn parse_pyproject_toml(path: &Path) -> Result<Vec<PythonDep>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let doc: toml::Value = toml::from_str(&content)
        .with_context(|| format!("failed to parse TOML: {}", path.display()))?;

    let mut deps = Vec::new();

    // Try [project.dependencies] (PEP 621)
    if let Some(project_deps) = doc
        .get("project")
        .and_then(|p| p.get("dependencies"))
        .and_then(|d| d.as_array())
    {
        for entry in project_deps {
            if let Some(s) = entry.as_str() {
                // PEP 508 requirement strings are the same grammar as a
                // requirements.txt line — reuse the specifier-preserving parser.
                if let Some(dep) = parse_requirement_line(s) {
                    deps.push(dep);
                }
            }
        }
    }

    // Try [tool.poetry.dependencies]
    if deps.is_empty() {
        if let Some(poetry_deps) = doc
            .get("tool")
            .and_then(|t| t.get("poetry"))
            .and_then(|p| p.get("dependencies"))
            .and_then(|d| d.as_table())
        {
            for (name, value) in poetry_deps {
                if name == "python" {
                    continue;
                }
                let raw = match value {
                    toml::Value::String(v) => v.to_string(),
                    toml::Value::Table(t) => t
                        .get("version")
                        .and_then(|v| v.as_str())
                        .map(|v| v.to_string())
                        .unwrap_or_default(),
                    _ => continue,
                };
                if let Some(dep) = parse_poetry_constraint(name, &raw) {
                    deps.push(dep);
                }
            }
        }
    }

    Ok(deps)
}

/// Parse uv.lock (TOML with [[package]] sections).
pub fn parse_uv_lock(path: &Path) -> Result<Vec<PythonDep>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let doc: toml::Value = toml::from_str(&content)
        .with_context(|| format!("failed to parse TOML: {}", path.display()))?;

    let mut deps = Vec::new();
    if let Some(packages) = doc.get("package").and_then(|p| p.as_array()) {
        for pkg in packages {
            let name = pkg.get("name").and_then(|n| n.as_str()).unwrap_or_default();
            let version = pkg.get("version").and_then(|v| v.as_str()).unwrap_or_default();
            if !name.is_empty() && !version.is_empty() {
                deps.push(PythonDep {
                    name: normalize_python_name(name),
                    version: version.to_string(),
                    spec: String::new(),
                });
            }
        }
    }

    Ok(deps)
}

/// Parse poetry.lock (TOML with [[package]] sections).
pub fn parse_poetry_lock(path: &Path) -> Result<Vec<PythonDep>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let doc: toml::Value = toml::from_str(&content)
        .with_context(|| format!("failed to parse TOML: {}", path.display()))?;

    let mut deps = Vec::new();
    if let Some(packages) = doc.get("package").and_then(|p| p.as_array()) {
        for pkg in packages {
            let name = pkg.get("name").and_then(|n| n.as_str()).unwrap_or_default();
            let version = pkg.get("version").and_then(|v| v.as_str()).unwrap_or_default();
            if !name.is_empty() && !version.is_empty() {
                deps.push(PythonDep {
                    name: normalize_python_name(name),
                    version: version.to_string(),
                    spec: String::new(),
                });
            }
        }
    }

    Ok(deps)
}

/// Parse Pipfile.lock (JSON).
pub fn parse_pipfile_lock(path: &Path) -> Result<Vec<PythonDep>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let doc: serde_json::Value = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse JSON: {}", path.display()))?;

    let mut deps = Vec::new();

    for section in &["default", "develop"] {
        if let Some(packages) = doc.get(section).and_then(|s| s.as_object()) {
            for (name, info) in packages {
                let version = info
                    .get("version")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .trim_start_matches("==");
                if !version.is_empty() {
                    deps.push(PythonDep {
                        name: normalize_python_name(name),
                        version: version.to_string(),
                        spec: String::new(),
                    });
                }
            }
        }
    }

    Ok(deps)
}

/// Convert a Poetry version constraint into a PEP 440 specifier and parse it.
///
/// Poetry uses caret/tilde sugar that PEP 440 expresses differently:
///   `^1.2.3` → `>=1.2.3,<2.0.0` (compatible within the leftmost non-zero part)
///   `~1.2.3` → `>=1.2.3,<1.3.0`
///   `~1.2`   → `>=1.2,<1.3`
/// Plain PEP 440 constraints (`>=1.0`, `==1.2.3`, `1.2.*`) and a bare exact
/// version (`1.2.3`, which Poetry treats as `==`) pass through.
fn parse_poetry_constraint(name: &str, raw: &str) -> Option<PythonDep> {
    let raw = raw.trim();
    if raw.is_empty() || raw == "*" {
        return Some(PythonDep {
            name: normalize_python_name(name),
            version: String::new(),
            spec: String::new(),
        });
    }

    let spec = if let Some(rest) = raw.strip_prefix('^') {
        caret_to_specifier(rest.trim())
    } else if let Some(rest) = raw.strip_prefix('~') {
        tilde_to_specifier(rest.trim())
    } else if raw
        .chars()
        .next()
        .map(|c| c.is_ascii_digit())
        .unwrap_or(false)
    {
        // Bare version → Poetry exact pin.
        format!("=={raw}")
    } else {
        raw.to_string()
    };

    let set = crate::python::pep440::SpecifierSet::parse(&spec);
    if let Some(pin) = set.exact_pin() {
        return Some(PythonDep {
            name: normalize_python_name(name),
            version: pin,
            spec: String::new(),
        });
    }
    Some(PythonDep {
        name: normalize_python_name(name),
        version: String::new(),
        spec,
    })
}

/// `^X.Y.Z` → `>=X.Y.Z,<(next compatible)`. The upper bound bumps the leftmost
/// non-zero component: `^1.2.3`→`<2.0.0`, `^0.2.3`→`<0.3.0`, `^0.0.3`→`<0.0.4`.
fn caret_to_specifier(v: &str) -> String {
    let parts: Vec<u64> = v.split('.').filter_map(|p| p.parse::<u64>().ok()).collect();
    if parts.is_empty() {
        return format!(">={v}");
    }
    let (mut maj, mut min, mut patch) = (
        parts.first().copied().unwrap_or(0),
        parts.get(1).copied().unwrap_or(0),
        parts.get(2).copied().unwrap_or(0),
    );
    if maj > 0 {
        maj += 1;
        min = 0;
        patch = 0;
    } else if min > 0 {
        min += 1;
        patch = 0;
    } else {
        patch += 1;
    }
    format!(">={v},<{maj}.{min}.{patch}")
}

/// `~X.Y.Z` → `>=X.Y.Z,<X.(Y+1).0`; `~X.Y` → `>=X.Y,<X.(Y+1)`; `~X` → `>=X,<X+1`.
fn tilde_to_specifier(v: &str) -> String {
    let parts: Vec<u64> = v.split('.').filter_map(|p| p.parse::<u64>().ok()).collect();
    match parts.len() {
        0 => format!(">={v}"),
        1 => format!(">={v},<{}", parts[0] + 1),
        _ => format!(">={v},<{}.{}", parts[0], parts[1] + 1),
    }
}

/// Normalize Python package name per PEP 503: lowercase, replace -._ with _.
pub fn normalize_python_name(name: &str) -> String {
    name.to_lowercase()
        .replace('-', "_")
        .replace('.', "_")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_requirements_basic() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "requests==2.31.0\nflask==3.0.0\n# comment\n-r other.txt\n").unwrap();
        let deps = parse_requirements_txt(tmp.path()).unwrap();
        assert_eq!(deps.len(), 2);
        assert_eq!(deps[0].name, "requests");
        assert_eq!(deps[0].version, "2.31.0");
        assert_eq!(deps[1].name, "flask");
    }

    #[test]
    fn normalize_names() {
        assert_eq!(normalize_python_name("Requests"), "requests");
        assert_eq!(normalize_python_name("my-package"), "my_package");
        assert_eq!(normalize_python_name("My.Package"), "my_package");
    }

    #[test]
    fn parse_requirement_specifier_preserved() {
        // Non-exact specifier is kept raw, version stays empty for resolution.
        let dep = parse_requirement_line("requests>=2.28.0").unwrap();
        assert_eq!(dep.name, "requests");
        assert_eq!(dep.version, "");
        assert_eq!(dep.spec, ">=2.28.0");

        // Range with upper bound is preserved whole.
        let dep = parse_requirement_line("urllib3>=1.21.1,<3").unwrap();
        assert_eq!(dep.name, "urllib3");
        assert_eq!(dep.spec, ">=1.21.1,<3");

        // Exact pin → concrete version, no spec.
        let dep = parse_requirement_line("flask==3.0.0; python_version >= '3.8'").unwrap();
        assert_eq!(dep.name, "flask");
        assert_eq!(dep.version, "3.0.0");
        assert_eq!(dep.spec, "");

        // Extras are stripped from the name.
        let dep = parse_requirement_line("requests[security]>=2.0").unwrap();
        assert_eq!(dep.name, "requests");
        assert_eq!(dep.spec, ">=2.0");
    }

    #[test]
    fn parse_requirements_keeps_ranges() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            tmp.path(),
            "requests==2.31.0\nflask>=3.0,<4  # web\nhttps://x/y.whl\n# comment\n-r other.txt\n",
        )
        .unwrap();
        let deps = parse_requirements_txt(tmp.path()).unwrap();
        // requests (exact) + flask (range); the URL and comment are skipped.
        assert_eq!(deps.len(), 2);
        assert_eq!(deps[0].version, "2.31.0");
        assert_eq!(deps[1].name, "flask");
        assert_eq!(deps[1].version, "");
        assert_eq!(deps[1].spec, ">=3.0,<4");
    }

    #[test]
    fn poetry_caret_tilde_conversion() {
        let d = parse_poetry_constraint("django", "^4.2.1").unwrap();
        assert_eq!(d.spec, ">=4.2.1,<5.0.0");

        let d = parse_poetry_constraint("foo", "^0.2.3").unwrap();
        assert_eq!(d.spec, ">=0.2.3,<0.3.0");

        let d = parse_poetry_constraint("bar", "~1.4").unwrap();
        assert_eq!(d.spec, ">=1.4,<1.5");

        // Bare version is an exact Poetry pin.
        let d = parse_poetry_constraint("baz", "2.1.0").unwrap();
        assert_eq!(d.version, "2.1.0");
        assert_eq!(d.spec, "");

        // Wildcard means latest.
        let d = parse_poetry_constraint("qux", "*").unwrap();
        assert_eq!(d.version, "");
        assert_eq!(d.spec, "");
    }
}
