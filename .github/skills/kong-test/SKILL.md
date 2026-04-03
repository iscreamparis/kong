---
name: kong-test
description: "Testing patterns and strategies for the KONG project. Use when: writing unit tests, integration tests, creating test fixtures, mocking HTTP responses, testing filesystem operations (links, junctions), or setting up CI test infrastructure."
argument-hint: "What to test (parser, registry-client, store, linker, cli, or integration)"
---

# KONG Testing Patterns

## When to Use
- Writing unit tests for any KONG module
- Creating integration tests for end-to-end workflows
- Mocking registry HTTP responses
- Testing filesystem operations (hard links, junctions)
- Setting up test fixtures for manifest files

## Testing Principles
1. **Unit tests** live in the same file: `#[cfg(test)] mod tests { ... }`
2. **Integration tests** live in `tests/` directory
3. **No real network calls** in unit tests — use recorded fixtures
4. **Use `tempfile`** for all filesystem tests — never write to real project dirs
5. **Test idempotency** — run operations twice, assert same result

## Unit Test Patterns

### Parser Tests
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_requirements_txt() {
        let input = "requests==2.31.0\nflask==3.0.0\n# comment\n-r other.txt\n";
        let deps = parse_requirements(input).unwrap();
        assert_eq!(deps.len(), 2);
        assert_eq!(deps[0].name, "requests");
        assert_eq!(deps[0].version, "2.31.0");
    }

    #[test]
    fn normalize_python_package_name() {
        assert_eq!(normalize("Requests"), "requests");
        assert_eq!(normalize("my-package"), "my_package");
        assert_eq!(normalize("my.package"), "my_package");
    }
}
```

### Registry Client Tests (Mocked)
Create JSON fixtures in `tests/fixtures/`:
```
tests/
├── fixtures/
│   ├── pypi/
│   │   └── requests.json       # Recorded PyPI API response
│   ├── npm/
│   │   └── express-4.18.2.json # Recorded npm version response
│   └── cargo/
│       └── serde-1.0.193.lock  # Cargo.lock excerpt
└── integration/
    └── full_workflow.rs
```

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(path: &str) -> String {
        std::fs::read_to_string(format!("tests/fixtures/{path}")).unwrap()
    }

    #[test]
    fn parse_pypi_response() {
        let json = fixture("pypi/requests.json");
        let info: PypiPackageInfo = serde_json::from_str(&json).unwrap();
        assert!(info.releases.contains_key("2.31.0"));
    }

    #[test]
    fn select_wheel_prefers_binary() {
        let json = fixture("pypi/requests.json");
        let info: PypiPackageInfo = serde_json::from_str(&json).unwrap();
        let wheel = select_best_wheel(&info, "3.11", "win_amd64").unwrap();
        assert!(wheel.filename.ends_with(".whl"));
    }
}
```

### Store / Filesystem Tests
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn extract_wheel_to_store() {
        let tmp = TempDir::new().unwrap();
        let store_path = tmp.path().join("requests-2.31.0-py3-none-any");
        extract_wheel(Path::new("tests/fixtures/requests-2.31.0.whl"), &store_path).unwrap();
        assert!(store_path.join("requests").exists());
        assert!(store_path.join(".kong-verified").exists());
    }

    #[test]
    fn hard_link_file() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("source.txt");
        std::fs::write(&src, "hello").unwrap();
        let dst = tmp.path().join("link.txt");
        link_file(&src, &dst).unwrap();
        assert_eq!(std::fs::read_to_string(&dst).unwrap(), "hello");
    }

    #[cfg(windows)]
    #[test]
    fn junction_directory() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("source_dir");
        std::fs::create_dir(&src).unwrap();
        std::fs::write(src.join("file.txt"), "content").unwrap();
        let dst = tmp.path().join("junction_dir");
        link_dir(&src, &dst).unwrap();
        assert!(junction::exists(&dst).unwrap());
        assert_eq!(std::fs::read_to_string(dst.join("file.txt")).unwrap(), "content");
    }

    #[test]
    fn idempotent_extract() {
        let tmp = TempDir::new().unwrap();
        let store_path = tmp.path().join("pkg-1.0.0");
        // First run
        extract_and_verify(&archive, &store_path, &expected_hash).unwrap();
        // Second run should succeed without error
        extract_and_verify(&archive, &store_path, &expected_hash).unwrap();
    }
}
```

## Integration Tests

### Full Workflow Test
```rust
// tests/integration/full_workflow.rs
use tempfile::TempDir;

#[test]
fn kong_rules_then_use_python() {
    let project = TempDir::new().unwrap();
    let store = TempDir::new().unwrap();
    
    // Write a requirements.txt
    std::fs::write(project.path().join("requirements.txt"), "requests==2.31.0\n").unwrap();
    
    // Run kong rules (programmatically, not CLI)
    let rules = generate_rules(project.path(), store.path()).unwrap();
    assert_eq!(rules.python.as_ref().unwrap().packages.len(), 1);
    
    // Run kong use
    apply_rules(&rules, project.path(), store.path()).unwrap();
    
    // Verify .venv was created
    assert!(project.path().join(".venv/pyvenv.cfg").exists());
    assert!(project.path().join(".venv/Lib/site-packages/requests").exists());
}
```

## Recording Fixtures
To create test fixtures from real API responses (run once manually):
```bash
# PyPI
curl -o tests/fixtures/pypi/requests.json https://pypi.org/pypi/requests/json

# npm
curl -o tests/fixtures/npm/express-4.18.2.json https://registry.npmjs.org/express/4.18.2

# Trim large responses to only the fields/versions needed for tests
```

## Running Tests
```bash
cargo test                    # all tests
cargo test -- --nocapture     # with stdout
cargo test parser             # just parser tests
cargo test --test integration # just integration tests
```
