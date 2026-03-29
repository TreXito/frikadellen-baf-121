use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=FRIKADELLEN_VERSION");

    // Prefer an explicit version supplied by CI or the user.
    if let Ok(v) = std::env::var("FRIKADELLEN_VERSION") {
        if !v.is_empty() {
            println!("cargo:rustc-env=FRIKADELLEN_VERSION={v}");
            return;
        }
    }

    // Fall back to computing `build-YYYYMMDD-SHORTSHA` from git,
    // matching the format used by the GitHub Actions release workflow.
    let sha = Command::new("git")
        .args(["rev-parse", "--short=7", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".into());

    let date = Command::new("git")
        .args(["log", "-1", "--format=%cd", "--date=format:%Y%m%d"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".into());

    println!("cargo:rustc-env=FRIKADELLEN_VERSION=build-{date}-{sha}");

    // Rebuild when git HEAD changes (new commit / branch switch).
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/heads");
}
