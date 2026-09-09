// Build the embedded frontend before the controller crate compiles, so
// `rust-embed` picks up the current SPA bundle from `frontend/dist`, and
// inject the build-info env vars the metrics endpoint reports.
//
// The frontend is a Vue 3 + Vite project (see crates/controller/frontend).
// On a development box with node/npm present we build it here so the daemon
// always embeds fresh assets; in a strict offline/CI build that never touched
// the frontend, this step is skipped and the committed `dist/` is embedded as
// shipped (see AGENTS.md: commit built artifacts for the feed).

use std::path::Path;
use std::process::Command;

fn main() {
    // Version / revision / toolchain for /metrics mielofon_build_info.
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "unknown".into());
    let rev = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".into());
    let rustc = Command::new("rustc")
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".into());

    println!("cargo:rustc-env=MIELOFON_GIT_REV={rev}");
    println!("cargo:rustc-env=MIELOFON_RUSTC={rustc}");
    println!("cargo:rustc-env=MIELOFON_VERSION={version}");
    println!("cargo:rerun-if-changed=build.rs");

    // Frontend build (embeds the SPA).
    println!("cargo:rerun-if-changed=frontend/src");
    println!("cargo:rerun-if-changed=frontend/package.json");
    println!("cargo:rerun-if-changed=frontend/vite.config.js");
    println!("cargo:rerun-if-changed=frontend/index.html");

    let dist = Path::new("frontend/dist");
    let has_bundle = dist.join("index.html").is_file();

    // When a committed bundle exists (feed/CI path), embed it as-is and do NOT
    // require the frontend toolchain — CI's Rust jobs don't run npm.
    if has_bundle {
        println!("cargo:rustc-env=MIELOFON_FRONTEND_BUILT=1");
        return;
    }

    // No committed bundle: we are expected to build it. Require npm.
    let npm = Command::new("npm").arg("--version").output().is_ok();
    if !npm {
        panic!(
            "frontend/dist is missing and npm is unavailable — commit the built \
             bundle (npm run build) or install the frontend toolchain"
        );
    }
    let status = Command::new("npm")
        .args(["run", "build"])
        .current_dir("frontend")
        .status()
        .expect("failed to run the frontend build (npm run build)");
    assert!(status.success(), "frontend build failed");
}
