// Build the embedded frontend before the controller crate compiles, so
// `rust-embed` picks up the current SPA bundle from `frontend/dist`, and
// inject the build-info env vars the metrics endpoint reports.
//
// The frontend is a Vue 3 + Vite project (see crates/controller/frontend),
// built either by the OpenWrt package (Build/Compile runs `npm ci && npm run
// build` into frontend/dist before cargo) or locally/CI via npm when the
// bundle is absent. `dist/` is intentionally NOT committed — the OpenWrt
// toolchain builds it (node/host, mirroring gotify's yarn UI build).

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

    // The OpenWrt build (Build/Compile) and any external build already produced
    // the bundle into frontend/dist — embed it as-is rather than rebuilding.
    if has_bundle {
        println!("cargo:rustc-env=MIELOFON_FRONTEND_BUILT=1");
        return;
    }

    // No bundle present: build it with npm (local dev / CI). dist/ is not
    // committed, so this is the normal path everywhere but the feed build.
    let npm = Command::new("npm").arg("--version").output().is_ok();
    if !npm {
        panic!(
            "frontend/dist is missing and npm is unavailable — install the frontend \
             toolchain or let the OpenWrt build produce the bundle first"
        );
    }
    let status = Command::new("npm")
        .args(["run", "build"])
        .current_dir("frontend")
        .status()
        .expect("failed to run the frontend build (npm run build)");
    assert!(status.success(), "frontend build failed");
}
