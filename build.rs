use std::process::Command;

fn main() {
    // ── Slint UI compilation ────────────────────────────────────────────────
    slint_build::compile("ui/kong.slint").expect("Slint build failed");

    // ── Windows NSIS installer (release builds only) ────────────────────────
    // Only build the installer on Windows release builds.
    // The CARGO_PROFILE env var is set by Cargo.
    let profile = std::env::var("PROFILE").unwrap_or_default();
    if profile != "release" {
        return;
    }

    // Only run on Windows.
    if !cfg!(target_os = "windows") {
        return;
    }

    let nsis = r"C:\Program Files (x86)\NSIS\makensis.exe";
    if !std::path::Path::new(nsis).exists() {
        println!("cargo:warning=NSIS not found at {nsis} — skipping installer build");
        return;
    }

    let nsi = std::path::Path::new("build/kong-installer.nsi")
        .canonicalize()
        .expect("build/kong-installer.nsi not found");

    println!("cargo:warning=Building NSIS installer...");

    let status = Command::new(nsis)
        .arg(nsi)
        .current_dir("build")
        .status()
        .expect("failed to run makensis");

    if !status.success() {
        panic!("makensis failed with exit code: {status}");
    }

    println!("cargo:warning=Installer written to build/kong-*.exe");

    // Re-run this script if the NSI script changes.
    println!("cargo:rerun-if-changed=build/kong-installer.nsi");
    // Re-run if the binary itself changes (it's bundled into the installer).
    println!("cargo:rerun-if-changed=target/release/kong.exe");
}
