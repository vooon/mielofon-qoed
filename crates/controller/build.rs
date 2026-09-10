// Emit the build-info env vars the /metrics mielofon_build_info gauge reports.
//
// The frontend SPA is NOT built or embedded here: the OpenWrt package builds
// `frontend/dist` in its own Build/Compile step and installs it into
// `[frontend] root_dir`, from where the daemon serves it at runtime. build.rs
// only needs the version/rev/toolchain fingerprints.

use std::process::Command;

fn main() {
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
}
