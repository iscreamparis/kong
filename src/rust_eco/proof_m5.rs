//! Milestone 5 proof: Kong keeps the Cargo registry and the build cache warm.
//! - `backcompat`: a project without Cargo.lock gets the same `kong.rules` as
//!   before, byte for byte except `generated`, and no `.rust-toolchain/`.
//! - `offline_build`: a tiny crate whose one dependency is in the store builds
//!   through `.rust-toolchain/bin/cargo build --offline` with the network blocked
//!   and nothing fetched into the env's CARGO_HOME.
//! - `shared_target`: a second worktree at the same Cargo.lock reuses the
//!   dependency artifacts of the shared target dir (no `Compiling kongdep`).
//! - `hostile_home`: a `$HOME/.cargo/config.toml` that replaces crates-io with a
//!   bogus directory registry breaks the bare store cargo, while Kong's wrapper
//!   (its `--config` flag) still runs `cargo test --offline`.
//!
//! The fixture is created once; every child cargo runs with a cleared
//! environment, so no test sets a process variable.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use tempfile::TempDir;

use crate::config::{KongRules, PackageEntry};
use crate::node::git::proof_support::{check, selected};
use crate::rust_eco::{parser, source};

/// The Cargo.lock checksum of the fixture dependency (any 64 hex: the vendored
/// registry writes it into `.cargo-checksum.json`, and Cargo compares the two).
const DEP_SUM: &str = "6b1f0c2e9d8a7b3c4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5";
/// Proxies pointing at a closed loopback port: any download attempt fails.
const DEAD_PROXY: &str = "http://127.0.0.1:9";
const SYS_PATH: &str = "/usr/local/bin:/usr/bin:/bin";

fn write(path: &Path, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

/// (1) No Cargo.lock: the rules generator gives the pre-M5 `kong.rules`.
fn backcompat() {
    let root = TempDir::new().unwrap();
    let project = root.path().join("plain");
    write(
        &project.join("Cargo.toml"),
        "[package]\nname = \"plain\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    write(&project.join("src/lib.rs"), "");
    let opts = crate::config::RulesOptions { name: Some("m5-plain".into()), ..Default::default() };
    let mut rules = crate::config::generate_rules_with_options(&project, &opts).unwrap();
    rules.generated = "<generated>".into();
    let path = project.join("kong.rules");
    crate::config::write_rules(&rules, &path).unwrap();
    let bytes = std::fs::read_to_string(&path).unwrap();
    let expected = "{\n  \"version\": 1,\n  \"project\": \"m5-plain\",\n  \"generated\": \"<generated>\"\n}";

    let env_dir = root.path().join("rulez/m5-plain");
    std::fs::create_dir_all(&env_dir).unwrap();
    crate::link::create_project_junctions(&project, &env_dir, &rules).unwrap();
    let untouched = !project.join(".rust-toolchain").exists() && !project.join(".cargo").exists();

    let same = bytes == expected;
    let measured = match (same, untouched) {
        (true, true) => "identical".to_string(),
        (false, _) => bytes.replace('\n', " "),
        (true, false) => "kong use created .rust-toolchain or .cargo".to_string(),
    };
    check("backcompat", measured, "kong.rules == pre-change bytes, no .rust-toolchain", same && untouched);
}

/// A complete real toolchain: the one building these tests (`CARGO`), else the
/// `rustc --print sysroot` of PATH.
fn real_sysroot() -> Option<PathBuf> {
    let complete = |d: &Path| ["bin/cargo", "bin/rustc", "bin/rustdoc", "lib"].iter().all(|f| d.join(f).exists());
    let from_cargo = Path::new(env!("CARGO")).parent()?.parent()?.to_path_buf();
    if complete(&from_cargo) {
        return Some(from_cargo);
    }
    let out = Command::new("rustc").args(["--print", "sysroot"]).output().ok()?;
    let dir = PathBuf::from(String::from_utf8(out.stdout).ok()?.trim());
    complete(&dir).then_some(dir)
}

/// The store: `kongdep 0.1.0` unpacked like `client::fetch_and_download` leaves
/// it, and a toolchain `rust/toolchain/0.0.1` made of links to the real one.
fn seed_store(store: &Path, sysroot: &Path) -> PackageEntry {
    let dir = store.join("rust/crates/kongdep-0.1.0/kongdep-0.1.0");
    write(
        &dir.join("Cargo.toml"),
        "[package]\nname = \"kongdep\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    write(&dir.join("src/lib.rs"), "pub fn answer() -> u32 { 42 }\n");
    let tc = store.join("rust/toolchain/0.0.1");
    std::fs::create_dir_all(tc.join("bin")).unwrap();
    for exe in ["cargo", "rustc", "rustdoc"] {
        link_any(&sysroot.join("bin").join(exe), &tc.join("bin").join(exe));
    }
    link_any(&sysroot.join("lib"), &tc.join("lib"));
    PackageEntry {
        name: "kongdep".into(),
        version: "0.1.0".into(),
        hash: Some(DEP_SUM.into()),
        store_path: "rust/crates/kongdep-0.1.0".into(),
        source_url: None,
        path: None,
    }
}

#[cfg(unix)]
fn link_any(src: &Path, dst: &Path) {
    std::os::unix::fs::symlink(src, dst).unwrap();
}
#[cfg(windows)]
fn link_any(src: &Path, dst: &Path) {
    if src.is_dir() {
        crate::link::link_dir(src, dst).unwrap();
    } else {
        crate::link::link_file(src, dst).unwrap();
    }
}

/// A worktree of the fixture project, with its committed Cargo.lock.
fn write_project(dir: &Path) {
    write(
        &dir.join("Cargo.toml"),
        "[package]\nname = \"kongproj\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
         [dependencies]\nkongdep = \"0.1\"\n\n[workspace]\n",
    );
    write(
        &dir.join("src/lib.rs"),
        "pub fn answer() -> u32 { kongdep::answer() }\n\n#[test]\nfn answers() { assert_eq!(answer(), 42); }\n",
    );
    write(
        &dir.join("Cargo.lock"),
        &format!(
            "version = 4\n\n[[package]]\nname = \"kongdep\"\nversion = \"0.1.0\"\n\
             source = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"{DEP_SUM}\"\n\n\
             [[package]]\nname = \"kongproj\"\nversion = \"0.1.0\"\ndependencies = [\n \"kongdep\",\n]\n"
        ),
    );
}

/// The kong.rules `kong rules` writes for a worktree (slug differs per worktree,
/// workspace and lock hash come from its Cargo.lock).
fn rules_for(project: &Path, slug: &str, dep: &PackageEntry) -> KongRules {
    let lock = project.join("Cargo.lock");
    serde_json::from_value(serde_json::json!({
        "version": 1, "project": slug, "generated": "proof_m5",
        "runtimes": {"rust": {"version": "0.0.1", "store_path": "rust/toolchain/0.0.1"}},
        "rust": {
            "packages": [dep],
            "workspace": parser::workspace_members(&lock).unwrap(),
            "lock_hash": parser::lock_hash(&lock).unwrap(),
        }
    }))
    .unwrap()
}

/// `kong use` for one worktree: the env in RULEZ, mirrored into the project.
fn kong_use(tmp: &Path, project: &Path, slug: &str, store: &Path, dep: &PackageEntry) -> crate::rust_eco::cargo_env::CargoEnv {
    let rules = rules_for(project, slug, dep);
    let env_dir = tmp.join("rulez").join(slug);
    let env = source::configure_with(&env_dir, rules.rust.as_ref().unwrap(), store, &rules, &tmp.join("cache")).unwrap();
    crate::link::create_project_junctions(project, &env_dir, &rules).unwrap();
    env
}

/// A child process with a cleared environment: HOME, a system PATH and dead
/// proxies, so any network access fails.
fn command(program: &Path, cwd: &Path, home: &Path, path: &str) -> Command {
    let mut c = Command::new(program);
    c.current_dir(cwd).env_clear().env("HOME", home).env("PATH", path);
    for var in ["HTTPS_PROXY", "HTTP_PROXY", "ALL_PROXY", "https_proxy", "http_proxy", "all_proxy"] {
        c.env(var, DEAD_PROXY);
    }
    c
}

/// Success, combined output and seconds.
fn run(c: &mut Command) -> (bool, String, f64) {
    let start = Instant::now();
    let out = c.output().unwrap();
    let seconds = start.elapsed().as_secs_f64();
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    (out.status.success(), text, seconds)
}

/// Cargo lines that mean a network fetch or a registry download happened.
fn fetched(text: &str) -> bool {
    text.lines().any(|l| {
        let l = l.trim_start();
        l.starts_with("Updating") || l.starts_with("Downloading") || l.starts_with("Downloaded")
    })
}

/// A crate archive or a registry index in the env's CARGO_HOME means Cargo
/// fetched something (it only keeps lock files there when it reads the
/// vendored directory source).
fn downloaded(cargo_home: &Path) -> bool {
    walkdir::WalkDir::new(cargo_home).into_iter().flatten().any(|e| {
        let name = e.file_name().to_string_lossy();
        name.ends_with(".crate") || (e.file_type().is_dir() && name == "index")
    })
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The bogus registry and the `$HOME/.cargo/config.toml` that points crates-io
/// at it: same crate, a checksum that does not match the lock.
fn plant_hostile_home(home: &Path) -> PathBuf {
    let bogus = home.join("bogus-registry");
    write(
        &bogus.join("kongdep-0.1.0/Cargo.toml"),
        "[package]\nname = \"kongdep\"\nversion = \"0.1.0\"\n",
    );
    write(&bogus.join("kongdep-0.1.0/src/lib.rs"), "");
    write(
        &bogus.join("kongdep-0.1.0/.cargo-checksum.json"),
        &format!(r#"{{"files":{{}},"package":"{}"}}"#, "0".repeat(64)),
    );
    let config = home.join(".cargo/config.toml");
    write(
        &config,
        &format!(
            "[source.crates-io]\nreplace-with = \"sandbox-vendor\"\n\n[source.sandbox-vendor]\ndirectory = {}\n",
            toml::Value::String(bogus.display().to_string())
        ),
    );
    config
}

#[test]
fn proof_m5() {
    if selected("backcompat") {
        backcompat();
    }
    let want_build = selected("offline_build");
    let want_shared = selected("shared_target");
    let want_hostile = selected("hostile_home");
    if !(want_build || want_shared || want_hostile) {
        return;
    }
    let Some(sysroot) = real_sysroot() else {
        let label = if want_build { "offline_build" } else if want_shared { "shared_target" } else { "hostile_home" };
        check(label, "no complete Rust toolchain (bin/cargo, rustc, rustdoc, lib)", "a real toolchain", false);
        return;
    };

    // Worktrees live under HOME, so a `$HOME/.cargo/config.toml` is also a
    // parent-directory config for them (the sandbox layout).
    let tmp = TempDir::new().unwrap();
    let (store, home) = (tmp.path().join("store"), tmp.path().join("home"));
    let dep = seed_store(&store, &sysroot);
    let wt1 = home.join("work/wt1/kongproj");
    write_project(&wt1);
    let env1 = kong_use(tmp.path(), &wt1, "kongproj-wt1", &store, &dep);
    let wrapper1 = wt1.join(".rust-toolchain/bin/cargo");
    let path1 = format!("{}:{SYS_PATH}", wt1.join(".rust-toolchain/bin").display());

    let mut first_s = 0.0;
    if want_build || want_shared {
        let (ok, text, s) = run(command(&wrapper1, &wt1, &home, &path1).args(["build", "--offline"]));
        first_s = s;
        let no_fetch = !fetched(&text) && !downloaded(&env1.cargo_home());
        let compiled = text.contains("Compiling kongdep");
        let good = ok && no_fetch && compiled && !wt1.join("target").exists() && !wt1.join(".cargo").exists();
        let measured = if good {
            format!("exit=0 fetched=0 seconds={s:.1}")
        } else {
            format!("exit_ok={ok} no_fetch={no_fetch} compiled_dep={compiled} output: {}", one_line(&text))
        };
        if want_build {
            check("offline_build", measured, "cargo build --offline via .rust-toolchain/bin/cargo exits 0, no fetch", good);
        } else {
            assert!(good, "first worktree build failed: {measured}");
        }
    }

    if want_shared {
        let wt2 = home.join("work/wt2/kongproj");
        write_project(&wt2);
        let env2 = kong_use(tmp.path(), &wt2, "kongproj-wt2", &store, &dep);
        let wrapper2 = wt2.join(".rust-toolchain/bin/cargo");
        let path2 = format!("{}:{SYS_PATH}", wt2.join(".rust-toolchain/bin").display());
        let (ok, text, s) = run(command(&wrapper2, &wt2, &home, &path2).args(["build", "--offline"]));
        let same_dir = env1.target_dir == env2.target_dir;
        let recompiled = text.lines().filter(|l| l.contains("Compiling kongdep")).count();
        let good = ok && same_dir && recompiled == 0 && !fetched(&text);
        let measured = if good {
            format!("kongdep_recompiled=0 first_seconds={first_s:.1} second_seconds={s:.1}")
        } else {
            format!("exit_ok={ok} same_target_dir={same_dir} kongdep_recompiled={recompiled} output: {}", one_line(&text))
        };
        check("shared_target", measured, "second worktree, same lock: 0 dependency crates recompiled", good);
    }

    if want_hostile {
        let config = plant_hostile_home(&home);
        // Control: the bare store cargo, with $HOME/.cargo as CARGO_HOME and parent dir.
        let sys_path = format!("{}:{SYS_PATH}", sysroot.join("bin").display());
        let (bare_ok, bare_text, _) = run(command(&env1.store_cargo, &wt1, &home, &sys_path)
            .args(["check", "--offline"])
            .env("CARGO_TARGET_DIR", tmp.path().join("control-target")));
        let bare_broken = !bare_ok && bare_text.contains("kongdep");
        // Kong's wrapper, then its exec line with CARGO_HOME at the hostile dir.
        let (wrap_ok, wrap_text, s) = run(command(&wrapper1, &wt1, &home, &path1).args(["test", "--offline"]));
        let wrap_good = wrap_ok && wrap_text.contains("test answers ... ok") && !fetched(&wrap_text);
        let (home_ok, home_text, _) = run(command(&env1.store_cargo, &wt1, &home, &path1)
            .arg("--config")
            .arg(env1.config_path())
            .args(["check", "--offline"])
            .env("CARGO_HOME", home.join(".cargo")));
        let good = bare_broken && wrap_good && home_ok;
        let measured = if good {
            let why = bare_text.lines().find(|l| l.contains("error")).unwrap_or("").trim().to_string();
            format!(
                "bare_store_cargo=FAIL({why}) kong_cargo_test=PASS seconds={s:.1} overridden={} by=--config {}",
                config.display(),
                env1.config_path().display()
            )
        } else {
            format!(
                "bare_broken={bare_broken} wrapper_ok={wrap_good} cargo_home_override_ok={home_ok} \
                 bare: {} | wrapper: {} | cargo_home: {}",
                one_line(&bare_text),
                one_line(&wrap_text),
                one_line(&home_text)
            )
        };
        check("hostile_home", measured, "bare store cargo fails, kong cargo test --offline passes", good);
    }
}
