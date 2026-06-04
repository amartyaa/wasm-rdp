use std::env;
use std::process::Command;

fn main() {
    // In CI release builds, RELEASE_VERSION is set to the git tag (e.g. "v1.2.3").
    // Use it directly so --version shows the release tag. In dev, fall back to
    // CARGO_PKG_VERSION + short commit hash.
    let version = if let Ok(tag) = env::var("RELEASE_VERSION") {
        tag.trim().trim_start_matches('v').to_string()
    } else {
        let pkg_version = env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "unknown".to_string());
        let git_hash = Command::new("git")
            .args(["rev-parse", "--short", "HEAD"])
            .output()
            .ok()
            .and_then(|o| if o.status.success() { String::from_utf8(o.stdout).ok() } else { None })
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| "unknown".to_string());
        format!("{} (commit: {})", pkg_version, git_hash)
    };

    println!("cargo:rustc-env=APP_VERSION={}", version);
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/index");
}
