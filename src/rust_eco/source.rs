use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use tracing::{debug, info, warn};

use crate::config::{KongRules, PackageEntry, RustSection};
use crate::rust_eco::cargo_env::{self, CargoEnv};
use crate::rust_eco::{runtime, target_cache};

/// Kong's Cargo environment for a project with a Cargo.lock, built in `env_dir`
/// (the RULEZ env; `link::create_project_junctions` mirrors its `.rust-toolchain/`
/// into the project):
/// - the vendored registry, hard-linked from the store crates, with each crate's
///   Cargo.lock checksum in its `.cargo-checksum.json`;
/// - the shared target dir of `target_cache`;
/// - `.rust-toolchain/` with the toolchain links, the `cargo` wrapper, `cargo-home/`
///   and the activate scripts (`cargo_env`).
///
/// No `.cargo/config.toml` is written anywhere: the wrapper's `--config` flag
/// outranks every config file, including a read-only `~/.cargo/config.toml` that
/// replaces crates-io (loaded as `CARGO_HOME` or as a parent-directory config).
pub fn configure_source_replacement(
    env_dir: &Path,
    rust: &RustSection,
    store_root: &Path,
    rules: &KongRules,
) -> Result<()> {
    let cache_root = target_cache::cache_root()?;
    configure_with(env_dir, rust, store_root, rules, &cache_root).map(|_| ())
}

/// `configure_source_replacement` with an explicit cache root, so tests never
/// depend on `KONG_CACHE` or the install root.
pub fn configure_with(
    env_dir: &Path,
    rust: &RustSection,
    store_root: &Path,
    rules: &KongRules,
    cache_root: &Path,
) -> Result<CargoEnv> {
    let env = plan_cargo_env(env_dir, rust, store_root, rules, cache_root)?;
    build_vendor_dir(store_root, &rust.packages, &env.vendor_dir)?;
    std::fs::create_dir_all(&env.target_dir).with_context(|| {
        format!("cannot create the shared target dir {}", env.target_dir.display())
    })?;
    let toolchain_dir = env
        .store_cargo
        .parent()
        .and_then(Path::parent)
        .context("store cargo has no toolchain directory")?;
    link_toolchain(env_dir, toolchain_dir)?;
    cargo_env::write_all(&env)?;
    info!(
        crates = rust.packages.len(),
        vendor = %env.vendor_dir.display(),
        target_dir = %env.target_dir.display(),
        "Cargo environment configured"
    );
    Ok(env)
}

/// The paths of Kong's Cargo environment for `env_dir`: the store cargo, the
/// vendored registry dir and the shared target dir. Nothing is written in the env
/// (`kong env` can use it); only `ensure_runtime` may fetch a toolchain when the
/// one kong.rules records is not in the store.
pub fn plan_cargo_env(
    env_dir: &Path,
    rust: &RustSection,
    store_root: &Path,
    rules: &KongRules,
    cache_root: &Path,
) -> Result<CargoEnv> {
    let (version, toolchain_dir) = resolve_toolchain(store_root, rules)?;
    let store_cargo = runtime::cargo_exe_in(&toolchain_dir).with_context(|| {
        format!("cargo not found in the Rust toolchain {}", toolchain_dir.display())
    })?;

    let vendor_key = target_cache::vendor_key(&vendor_triples(&rust.packages));
    // kong.rules written before the workspace/lock fields: fall back to the
    // project slug and the crate set, which still separate unrelated projects.
    let workspace = if rust.workspace.is_empty() {
        vec![rules.project.clone()]
    } else {
        rust.workspace.clone()
    };
    let lock = rust.lock_hash.clone().unwrap_or_else(|| vendor_key.clone());
    let key = target_cache::target_key(&workspace, &lock, &version, &runtime::host_target());

    Ok(CargoEnv {
        project_root: env_dir.to_path_buf(),
        store_cargo,
        vendor_dir: target_cache::vendor_dir(cache_root, &vendor_key),
        target_dir: cache_root.join("cargo-target").join(key),
    })
}

/// The toolchain recorded in kong.rules when it is complete in the store (no
/// network), else `ensure_runtime`, which falls back to the newest toolchain
/// already in the store when the channel is unreachable.
fn resolve_toolchain(store_root: &Path, rules: &KongRules) -> Result<(String, PathBuf)> {
    if let Some(rt) = rules.runtimes.as_ref().and_then(|r| r.rust.as_ref()) {
        let dir = store_root.join(&rt.store_path);
        if runtime::cargo_exe_in(&dir).is_some() && runtime::rustc_exe_in(&dir).is_some() {
            debug!(version = %rt.version, "Using the Rust toolchain recorded in kong.rules");
            return Ok((rt.version.clone(), dir));
        }
        warn!(version = %rt.version, path = %dir.display(),
            "Rust toolchain recorded in kong.rules is not in the store, resolving one");
    }
    let rt = runtime::ensure_runtime(store_root)?;
    Ok((rt.version.clone(), store_root.join(&rt.store_path)))
}

fn vendor_triples(packages: &[PackageEntry]) -> Vec<(String, String, String)> {
    packages
        .iter()
        .map(|p| (p.name.clone(), p.version.clone(), p.hash.clone().unwrap_or_default()))
        .collect()
}

/// The unpacked crate in the store: `.crate` archives hold a `<name>-<version>/`
/// top directory, so the source sits one level below the store path.
fn store_crate_dir(store_root: &Path, pkg: &PackageEntry) -> PathBuf {
    let crate_src = store_root.join(&pkg.store_path);
    let nested = crate_src.join(format!("{}-{}", pkg.name, pkg.version));
    if nested.is_dir() {
        nested
    } else {
        crate_src
    }
}

/// Build the vendored registry `dir` (a Cargo directory source): one
/// `<name>-<version>/` per crate, hard-linked from the store, plus a
/// `.cargo-checksum.json` whose `package` is the Cargo.lock checksum. Cargo
/// compares that value with the lock, so `"unknown"` or a wrong value fails.
///
/// The directory is keyed by the crate set and filled in a temp dir then renamed,
/// so an existing one is complete and reused as is.
pub fn build_vendor_dir(store_root: &Path, packages: &[PackageEntry], dir: &Path) -> Result<()> {
    if dir.is_dir() {
        debug!(dir = %dir.display(), "Vendored registry already built");
        return Ok(());
    }
    let parent = dir.parent().context("vendored registry dir has no parent")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("cannot create {}", parent.display()))?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let tmp = parent.join(format!(
        ".tmp-{}-{}-{nanos}",
        dir.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    let filled = fill_vendor_dir(store_root, packages, &tmp);
    if let Err(e) = filled {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(e);
    }
    if let Err(e) = std::fs::rename(&tmp, dir) {
        let _ = std::fs::remove_dir_all(&tmp);
        // Another `kong use` renamed the same crate set into place first.
        if !dir.is_dir() {
            return Err(e).with_context(|| format!("cannot move the vendored registry to {}", dir.display()));
        }
    }
    info!(dir = %dir.display(), crates = packages.len(), "Vendored registry built from the store");
    Ok(())
}

fn fill_vendor_dir(store_root: &Path, packages: &[PackageEntry], tmp: &Path) -> Result<()> {
    std::fs::create_dir_all(tmp).with_context(|| format!("cannot create {}", tmp.display()))?;
    let mut unknown = Vec::new();
    for pkg in packages {
        let entry = tmp.join(format!("{}-{}", pkg.name, pkg.version));
        if entry.exists() {
            continue;
        }
        let crate_dir = store_crate_dir(store_root, pkg);
        if !crate_dir.join("Cargo.toml").is_file() {
            bail!(
                "Rust crate '{} {}' is missing from the store:\n  \
                 expected : {}\n  \
                 rules    : kong.rules rust.packages\n\
                 run `kong rules` to fetch it",
                pkg.name,
                pkg.version,
                crate_dir.display()
            );
        }
        crate::link::link_package(&crate_dir, &entry)?;
        if pkg.hash.is_none() {
            unknown.push(format!("{} {}", pkg.name, pkg.version));
        }
        // A `null` package checksum is what Cargo expects for a lock entry
        // without one; written fresh, never through a hard link into the store.
        let checksum = serde_json::json!({ "files": {}, "package": pkg.hash });
        let checksum_file = entry.join(".cargo-checksum.json");
        let _ = std::fs::remove_file(&checksum_file);
        std::fs::write(&checksum_file, checksum.to_string())
            .with_context(|| format!("cannot write {}", checksum_file.display()))?;
    }
    if !unknown.is_empty() {
        warn!(
            count = unknown.len(),
            first = %unknown[0],
            "Crates without a Cargo.lock checksum in kong.rules (re-run `kong rules` if Cargo rejects them)"
        );
    }
    Ok(())
}

/// Link the store toolchain into `<root>/.rust-toolchain/`: every `bin/` file as a
/// hard link (`cargo_env` replaces `cargo` with its wrapper), and `lib` as a
/// directory link.
///
/// `lib` must sit beside the hard links: rustc and rustdoc load librustc_driver
/// through `$ORIGIN/../lib` and derive their sysroot from it, both resolved from
/// the link's own directory, so a hard-linked rustc alone does not start.
/// When the toolchain changes, `bin/` is rebuilt so no binary of the old one stays.
fn link_toolchain(root: &Path, toolchain_dir: &Path) -> Result<()> {
    let dest = root.join(".rust-toolchain");
    let dst_bin = dest.join("bin");
    let stamp = dest.join(".kong-toolchain");
    let wanted = toolchain_dir.display().to_string();
    if std::fs::read_to_string(&stamp).ok().as_deref() != Some(wanted.as_str()) && dst_bin.exists() {
        debug!(toolchain = %wanted, "Rust toolchain changed, relinking .rust-toolchain/bin");
        crate::link::remove_dir_all_robust(&dst_bin)?;
    }
    std::fs::create_dir_all(&dst_bin)
        .with_context(|| format!("cannot create {}", dst_bin.display()))?;

    let src_bin = toolchain_dir.join("bin");
    for entry in std::fs::read_dir(&src_bin)
        .with_context(|| format!("cannot read {}", src_bin.display()))?
    {
        let entry = entry?;
        let name = entry.file_name();
        if name == "cargo" || name == "cargo.exe" {
            continue;
        }
        let dst_file = dst_bin.join(&name);
        if std::fs::symlink_metadata(&dst_file).is_ok() {
            continue;
        }
        crate::link::link_file(&entry.path(), &dst_file)?;
    }

    let src_lib = toolchain_dir.join("lib");
    if src_lib.is_dir() {
        crate::link::ensure_dir_link(&src_lib, &dest.join("lib"))
            .with_context(|| format!("cannot link {} into {}", src_lib.display(), dest.display()))?;
    }
    std::fs::write(&stamp, &wanted).with_context(|| format!("cannot write {}", stamp.display()))?;
    debug!(dest = %dest.display(), toolchain = %wanted, "Rust toolchain linked");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const DEP_SUM: &str = "5d3a1c1e8f0b2a4c6e8d0f1a3b5c7e9f1a2b3c4d5e6f708192a3b4c5d6e7f809";

    /// `<store>/rust/crates/<n>-<v>/<n>-<v>/` like `client::fetch_and_download` leaves it.
    fn seed_crate(store: &Path, name: &str, version: &str, lib_rs: &str) -> PackageEntry {
        let dir = store.join(format!("rust/crates/{name}-{version}/{name}-{version}"));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            format!("[package]\nname = \"{name}\"\nversion = \"{version}\"\nedition = \"2021\"\n"),
        )
        .unwrap();
        std::fs::write(dir.join("src/lib.rs"), lib_rs).unwrap();
        PackageEntry {
            name: name.into(),
            version: version.into(),
            hash: Some(DEP_SUM.into()),
            store_path: format!("rust/crates/{name}-{version}"),
            source_url: None,
            path: None,
        }
    }

    fn rules_with(project: &str, packages: Vec<PackageEntry>, toolchain: &str) -> KongRules {
        serde_json::from_value(serde_json::json!({
            "version": 1, "project": project, "generated": "test",
            "runtimes": {"rust": {"version": toolchain, "store_path": format!("rust/toolchain/{toolchain}")}},
            "rust": {"packages": packages, "workspace": ["kongproj"], "lock_hash": "ab12"}
        }))
        .unwrap()
    }

    fn fake_toolchain(store: &Path, version: &str) {
        let bin = store.join(format!("rust/toolchain/{version}/bin"));
        std::fs::create_dir_all(&bin).unwrap();
        for exe in ["cargo", "rustc", "rustdoc"] {
            std::fs::write(bin.join(if cfg!(windows) { format!("{exe}.exe") } else { exe.into() }), b"x").unwrap();
        }
        std::fs::create_dir_all(store.join(format!("rust/toolchain/{version}/lib/rustlib"))).unwrap();
    }

    #[test]
    fn vendor_dir_carries_lock_checksums_and_leaves_the_store_alone() {
        let tmp = TempDir::new().unwrap();
        let store = tmp.path().join("store");
        let a = seed_crate(&store, "alpha", "1.0.0", "");
        let mut b = seed_crate(&store, "beta", "0.2.0", "");
        b.hash = None;
        let dir = tmp.path().join("cache/cargo-vendor/k1");
        build_vendor_dir(&store, &[a.clone(), b.clone()], &dir).unwrap();

        let sum: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.join("alpha-1.0.0/.cargo-checksum.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(sum, serde_json::json!({"files": {}, "package": DEP_SUM}));
        let sum_b = std::fs::read_to_string(dir.join("beta-0.2.0/.cargo-checksum.json")).unwrap();
        assert!(sum_b.contains("\"package\":null"), "{sum_b}");
        assert!(dir.join("alpha-1.0.0/src/lib.rs").is_file());
        assert!(!store.join("rust/crates/alpha-1.0.0/alpha-1.0.0/.cargo-checksum.json").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let ino = |p: PathBuf| std::fs::metadata(p).unwrap().ino();
            assert_eq!(
                ino(dir.join("alpha-1.0.0/Cargo.toml")),
                ino(store.join("rust/crates/alpha-1.0.0/alpha-1.0.0/Cargo.toml")),
                "crate files are hard links into the store"
            );
        }
        // Reused as is, and no temp dir is left behind.
        build_vendor_dir(&store, &[a, b], &dir).unwrap();
        let names: Vec<_> = std::fs::read_dir(dir.parent().unwrap()).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(names, vec![std::ffi::OsString::from("k1")]);
    }

    #[test]
    fn missing_store_crate_is_a_clear_error() {
        let tmp = TempDir::new().unwrap();
        let store = tmp.path().join("store");
        let mut ghost = seed_crate(&store, "ghost", "0.1.0", "");
        ghost.store_path = "rust/crates/ghost-9.9.9".into();
        let dir = tmp.path().join("cache/cargo-vendor/k2");
        let err = format!("{:#}", build_vendor_dir(&store, &[ghost], &dir).unwrap_err());
        assert!(err.contains("'ghost 0.1.0'") && err.contains("kong rules"), "{err}");
        assert!(!dir.exists());
        assert_eq!(std::fs::read_dir(dir.parent().unwrap()).unwrap().count(), 0);
    }

    /// Two worktrees (two RULEZ envs, two project slugs) of the same lock and
    /// toolchain share the vendored registry and the target dir, not `.rust-toolchain`.
    #[test]
    fn worktrees_share_vendor_and_target_dirs() {
        let tmp = TempDir::new().unwrap();
        let store = tmp.path().join("store");
        fake_toolchain(&store, "1.90.0");
        let pkgs = vec![seed_crate(&store, "alpha", "1.0.0", "")];
        let cache = tmp.path().join("cache");
        let r1 = rules_with("proj-1111", pkgs.clone(), "1.90.0");
        let r2 = rules_with("proj-2222", pkgs, "1.90.0");
        let e1 = configure_with(&tmp.path().join("env1"), r1.rust.as_ref().unwrap(), &store, &r1, &cache).unwrap();
        let e2 = configure_with(&tmp.path().join("env2"), r2.rust.as_ref().unwrap(), &store, &r2, &cache).unwrap();
        assert_eq!(e1.target_dir, e2.target_dir);
        assert_eq!(e1.vendor_dir, e2.vendor_dir);
        assert!(e1.target_dir.is_dir() && e1.vendor_dir.join("alpha-1.0.0").is_dir());
        assert_ne!(e1.config_path(), e2.config_path());
        assert!(e1.target_dir.starts_with(cache.join("cargo-target")));
        for env in [&e1, &e2] {
            assert!(env.config_path().is_file());
            assert!(env.toolchain_dir().join("activate.sh").is_file());
            assert!(env.bin_dir().join(if cfg!(windows) { "rustc.exe" } else { "rustc" }).is_file());
            assert!(env.toolchain_dir().join("lib/rustlib").is_dir());
            assert!(!env.project_root.join(".cargo").exists());
        }
        assert!(std::fs::read_to_string(e1.bin_dir().join("cargo")).unwrap().contains("--config"));
    }

    /// A complete real toolchain for the hostile-HOME test: the one building these
    /// tests (`CARGO`), else the `rustc --print sysroot` of PATH.
    #[cfg(unix)]
    fn real_sysroot() -> Option<PathBuf> {
        let complete = |d: &Path| {
            ["bin/cargo", "bin/rustc", "bin/rustdoc", "lib"].iter().all(|f| d.join(f).exists())
        };
        let from_cargo = Path::new(env!("CARGO")).parent()?.parent()?.to_path_buf();
        if complete(&from_cargo) {
            return Some(from_cargo);
        }
        let out = std::process::Command::new("rustc").args(["--print", "sysroot"]).output().ok()?;
        let dir = PathBuf::from(String::from_utf8(out.stdout).ok()?.trim());
        complete(&dir).then_some(dir)
    }

    /// The M5 sandbox case. `$HOME/.cargo/config.toml` replaces crates-io with a
    /// directory registry whose checksum does not match the lock, and the project
    /// sits under `$HOME`, so Cargo loads that file as the parent-directory config
    /// (and as `CARGO_HOME` when that is not set). Plain cargo fails there; through
    /// Kong's wrapper, and through activate.sh, `cargo check/test --offline` pass,
    /// because `--config <cargo-home/config.toml>` outranks both.
    #[cfg(unix)]
    #[test]
    fn hostile_home_cargo_config_is_overridden_by_the_wrapper() {
        use std::process::Command;
        let Some(sysroot) = real_sysroot() else {
            eprintln!("SKIP: no complete Rust toolchain (bin/cargo, rustc, rustdoc, lib) found");
            return;
        };
        let tmp = TempDir::new().unwrap();
        let (store, home) = (tmp.path().join("store"), tmp.path().join("home"));

        // Store: one crate and a toolchain made of links to the real one.
        let dep = seed_crate(&store, "kongdep", "0.1.0", "pub fn answer() -> u32 { 42 }\n");
        let tc = store.join("rust/toolchain/0.0.1");
        std::fs::create_dir_all(tc.join("bin")).unwrap();
        for exe in ["cargo", "rustc", "rustdoc"] {
            std::os::unix::fs::symlink(sysroot.join("bin").join(exe), tc.join("bin").join(exe)).unwrap();
        }
        std::os::unix::fs::symlink(sysroot.join("lib"), tc.join("lib")).unwrap();

        // The hostile ~/.cargo/config.toml and its mismatched registry.
        let bogus = home.join("bogus-registry/kongdep-0.1.0");
        std::fs::create_dir_all(bogus.join("src")).unwrap();
        std::fs::write(bogus.join("Cargo.toml"), "[package]\nname = \"kongdep\"\nversion = \"0.1.0\"\n").unwrap();
        std::fs::write(bogus.join("src/lib.rs"), "").unwrap();
        std::fs::write(bogus.join(".cargo-checksum.json"), format!(r#"{{"files":{{}},"package":"{}"}}"#, "0".repeat(64))).unwrap();
        std::fs::create_dir_all(home.join(".cargo")).unwrap();
        std::fs::write(
            home.join(".cargo/config.toml"),
            format!(
                "[source.crates-io]\nreplace-with = \"sandbox-vendor\"\n\n[source.sandbox-vendor]\ndirectory = {}\n",
                toml::Value::String(home.join("bogus-registry").display().to_string())
            ),
        )
        .unwrap();

        // The project, under $HOME, with its committed Cargo.lock.
        let project = home.join("work/kongproj");
        std::fs::create_dir_all(project.join("src")).unwrap();
        std::fs::write(
            project.join("Cargo.toml"),
            "[package]\nname = \"kongproj\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\nkongdep = \"0.1\"\n\n[workspace]\n",
        )
        .unwrap();
        std::fs::write(
            project.join("src/lib.rs"),
            "pub fn answer() -> u32 { kongdep::answer() }\n\n#[test]\nfn answers() { assert_eq!(answer(), 42); }\n",
        )
        .unwrap();
        std::fs::write(
            project.join("Cargo.lock"),
            format!(
                "version = 4\n\n[[package]]\nname = \"kongdep\"\nversion = \"0.1.0\"\n\
                 source = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"{DEP_SUM}\"\n\n\
                 [[package]]\nname = \"kongproj\"\nversion = \"0.1.0\"\ndependencies = [\n \"kongdep\",\n]\n"
            ),
        )
        .unwrap();

        // `kong use`: the env in RULEZ, mirrored into the project.
        let rules = rules_with("kongproj-1234", vec![dep], "0.0.1");
        let env_dir = tmp.path().join("rulez/kongproj-1234");
        let env = configure_with(&env_dir, rules.rust.as_ref().unwrap(), &store, &rules, &tmp.path().join("cache")).unwrap();
        crate::link::create_project_junctions(&project, &env_dir, &rules).unwrap();
        assert!(!project.join(".cargo").exists(), "no .cargo/config.toml in the project");

        let sys_path = "/usr/local/bin:/usr/bin:/bin";
        // A clean environment: only HOME and what each step sets on top.
        let cmd = |program: &Path| {
            let mut c = Command::new(program);
            c.current_dir(&project).env_clear().env("HOME", &home);
            c
        };
        let run = |c: &mut Command| {
            let out = c.output().unwrap();
            let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
            (out.status.success(), text)
        };

        // Control: plain store cargo, $HOME/.cargo is CARGO_HOME and a parent dir.
        let (ok, text) = run(cmd(&env.store_cargo)
            .args(["check", "--offline"])
            .env("PATH", format!("{}:{sys_path}", sysroot.join("bin").display()))
            .env("CARGO_TARGET_DIR", tmp.path().join("control-target")));
        assert!(!ok, "the hostile config must break plain cargo:\n{text}");
        assert!(text.contains("kongdep"), "{text}");

        // Through the wrapper: the hostile file is a parent-directory config.
        let wrapper = project.join(".rust-toolchain/bin/cargo");
        let bin_path = format!("{}:{sys_path}", project.join(".rust-toolchain/bin").display());
        let (ok, text) = run(cmd(&wrapper).args(["check", "--offline"]).env("PATH", &bin_path));
        assert!(ok, "cargo check through the wrapper:\n{text}");

        // The wrapper's exec line with CARGO_HOME at the hostile $HOME/.cargo.
        let (ok, text) = run(cmd(&env.store_cargo)
            .arg("--config")
            .arg(env.config_path())
            .args(["check", "--offline"])
            .env("PATH", &bin_path)
            .env("CARGO_HOME", home.join(".cargo")));
        assert!(ok, "--config must win over a hostile CARGO_HOME config:\n{text}");

        // The 3730 flow: source activate.sh, then plain cargo.
        let (ok, text) = run(cmd(Path::new("sh"))
            .arg("-c")
            .arg(". .rust-toolchain/activate.sh && command -v cargo && cargo test --offline")
            .env("PATH", sys_path));
        assert!(ok, "cargo test through activate.sh:\n{text}");
        assert!(text.lines().next().unwrap_or("").starts_with(&env_dir.display().to_string()), "{text}");
        assert!(text.contains("test answers ... ok"), "{text}");
        assert!(env.target_dir.join("debug").is_dir(), "builds land in the shared target dir");
        assert!(!project.join("target").exists());
    }
}
