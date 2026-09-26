//! Project interpreter selection, independent of dependency lockfiles.
use anyhow::{Context, Result};
use std::path::Path;

pub fn requested_version(project: &Path) -> Result<String> {
    let path = project.join(".python-version");
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            if let Some(line) = text
                .lines()
                .map(str::trim)
                .find(|l| !l.is_empty() && !l.starts_with('#'))
            {
                anyhow::ensure!(version_parts(line).is_some(),
                    "Invalid Python version '{line}' in {}: expected major.minor or major.minor.patch", path.display());
                return Ok(line.into());
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    }
    let path = project.join("pyproject.toml");
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            let doc: toml::Value =
                toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
            if let Some(spec) = doc
                .get("project")
                .and_then(|p| p.get("requires-python"))
                .and_then(|v| v.as_str())
            {
                if let Some(minor) = single_minor(spec) {
                    return Ok(minor);
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    }
    Ok("latest".into())
}

/// Conservatively infer a minor from a bounded intersection of stable numeric
/// PEP 440 clauses. Unsupported / ambiguous constraints leave the old default.
fn single_minor(spec: &str) -> Option<String> {
    let mut lower = [0u32; 3];
    let mut upper = [u32::MAX; 3]; // exclusive
    let mut exclusions = Vec::new();
    for clause in spec.split(',').map(str::trim) {
        let op = ["~=", "==", "!=", ">=", "<=", ">", "<"]
            .into_iter()
            .find(|op| clause.starts_with(op))?;
        let raw = clause[op.len()..].trim();
        let wildcard = raw.ends_with(".*");
        let parts = version_parts(raw.strip_suffix(".*").unwrap_or(raw))?;
        let v = [parts[0], parts[1], *parts.get(2).unwrap_or(&0)];
        let next_patch = || Some([v[0], v[1], v[2].checked_add(1)?]);
        let next_minor = || Some([v[0], v[1].checked_add(1)?, 0]);
        if wildcard && (!matches!(op, "==" | "!=") || parts.len() != 2) {
            return None;
        }
        match op {
            ">=" => lower = lower.max(v),
            ">" => lower = lower.max(next_patch()?),
            "<" => upper = upper.min(v),
            "<=" => upper = upper.min(next_patch()?),
            "==" => {
                lower = lower.max(v);
                upper = upper.min(if wildcard {
                    next_minor()?
                } else {
                    next_patch()?
                });
            }
            "~=" => {
                lower = lower.max(v);
                upper = upper.min(if parts.len() == 3 {
                    next_minor()?
                } else {
                    [v[0].checked_add(1)?, 0, 0]
                });
            }
            "!=" => exclusions.push((
                v,
                if wildcard {
                    next_minor()?
                } else {
                    next_patch()?
                },
            )),
            _ => return None,
        }
    }
    if lower >= upper {
        return None;
    }
    let mut intervals = vec![(lower, upper)];
    for (start, end) in exclusions {
        let mut remaining = Vec::new();
        for (lo, hi) in intervals {
            if end <= lo || start >= hi {
                remaining.push((lo, hi));
            } else {
                if lo < start {
                    remaining.push((lo, start));
                }
                if end < hi {
                    remaining.push((end, hi));
                }
            }
        }
        intervals = remaining;
    }
    let first = intervals.first()?.0;
    let next_minor = [first[0], first[1].checked_add(1)?, 0];
    intervals
        .iter()
        .all(|(lo, hi)| lo[..2] == first[..2] && *hi <= next_minor)
        .then(|| format!("{}.{}", first[0], first[1]))
}

pub(super) fn version_parts(value: &str) -> Option<Vec<u32>> {
    let parts: Vec<_> = value.split('.').collect();
    if !(2..=3).contains(&parts.len())
        || parts
            .iter()
            .any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    parts.into_iter().map(|p| p.parse().ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn excluded_minor_can_narrow_or_empty_range() {
        assert_eq!(single_minor(">=3.10,<3.12,!=3.10.*"), Some("3.11".into()));
        assert_eq!(single_minor("==3.11.*,!=3.11.*"), None);
        assert_eq!(single_minor("==3.11.9,!=3.11.9"), None);
        assert_eq!(single_minor(">=3.11,<3.12,!=3.11.9"), Some("3.11".into()));
        assert_eq!(single_minor(">=3.11,<=3.12"), None);
        assert_eq!(single_minor("~=3.11"), None);
        assert_eq!(single_minor(">=3.11rc1,<3.12"), None);
        assert_eq!(single_minor(""), None);
    }
    #[test]
    fn project_requires_single_minor() {
        for (spec, expected) in [
            (">=3.11,<3.12", "3.11"),
            ("==3.11.*", "3.11"),
            (">=3.11", "latest"),
            (">=3.10,<3.13", "latest"),
            ("~=3.11.2", "3.11"),
            ("==3.11.9", "3.11"),
            ("<3.12, >=3.11.2, !=3.11.5", "3.11"),
            (">=3.12,<3.11", "latest"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(
                dir.path().join("pyproject.toml"),
                format!("[project]\nrequires-python = '{spec}'\n"),
            )
            .unwrap();
            assert_eq!(requested_version(dir.path()).unwrap(), expected, "{spec}");
        }
    }
    #[test]
    fn file_missing_empty_minor_and_precedence() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(requested_version(dir.path()).unwrap(), "latest");
        std::fs::write(dir.path().join(".python-version"), "# empty\n\n").unwrap();
        assert_eq!(requested_version(dir.path()).unwrap(), "latest");
        std::fs::write(
            dir.path().join("pyproject.toml"),
            "[project]\nrequires-python = '==3.12.*'\n",
        )
        .unwrap();
        assert_eq!(requested_version(dir.path()).unwrap(), "3.12");
        std::fs::write(dir.path().join(".python-version"), " 3.11\r\n").unwrap();
        assert_eq!(requested_version(dir.path()).unwrap(), "3.11");
        for invalid in ["3", "3.11rc1", "../3.11", "3.11.1.2", "pypy3.11"] {
            std::fs::write(dir.path().join(".python-version"), invalid).unwrap();
            assert!(requested_version(dir.path())
                .unwrap_err()
                .to_string()
                .contains(invalid));
        }
    }
    #[test]
    fn version_file_takes_first_non_comment_line() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".python-version"),
            "\n # comment\n 3.11.9 \n3.12\n",
        )
        .unwrap();
        assert_eq!(requested_version(dir.path()).unwrap(), "3.11.9");
    }
}
