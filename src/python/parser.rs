use std::path::Path;

use anyhow::{Context, Result};
use tracing::debug;

/// A parsed dependency from a Python manifest.
#[derive(Debug, Clone)]
pub struct PythonDep {
    pub name: String,
    pub version: String,
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
        let line = line.trim();

        // Skip empty, comments, flags, includes
        if line.is_empty() || line.starts_with('#') || line.starts_with('-') {
            continue;
        }

        // Parse name==version
        if let Some((name, version)) = line.split_once("==") {
            deps.push(PythonDep {
                name: normalize_python_name(name.trim()),
                version: version.trim().to_string(),
            });
        }
        // TODO: handle >= and other specifiers as warnings
    }

    Ok(deps)
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
                if let Some(dep) = parse_pep508_simple(s) {
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
                let version = match value {
                    toml::Value::String(v) => v.trim_start_matches('^').trim_start_matches('~').to_string(),
                    toml::Value::Table(t) => t
                        .get("version")
                        .and_then(|v| v.as_str())
                        .map(|v| v.trim_start_matches('^').trim_start_matches('~').to_string())
                        .unwrap_or_default(),
                    _ => continue,
                };
                if !version.is_empty() {
                    deps.push(PythonDep {
                        name: normalize_python_name(name),
                        version,
                    });
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
                    });
                }
            }
        }
    }

    Ok(deps)
}

/// Very simple PEP 508 parser: extract name and version from "name>=version" or "name==version".
fn parse_pep508_simple(spec: &str) -> Option<PythonDep> {
    let spec = spec.split(';').next()?.trim(); // strip environment markers

    for op in &["==", ">=", "~=", "!=", "<="] {
        if let Some((name, version)) = spec.split_once(op) {
            return Some(PythonDep {
                name: normalize_python_name(name.trim()),
                version: version.split(',').next()?.trim().to_string(),
            });
        }
    }

    None
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
    fn parse_pep508() {
        let dep = parse_pep508_simple("requests>=2.28.0").unwrap();
        assert_eq!(dep.name, "requests");
        assert_eq!(dep.version, "2.28.0");

        let dep = parse_pep508_simple("flask==3.0.0; python_version >= '3.8'").unwrap();
        assert_eq!(dep.name, "flask");
        assert_eq!(dep.version, "3.0.0");
    }
}
