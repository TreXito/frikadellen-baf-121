//! FrikadellenBAF-loader
//!
//! A lightweight auto-updater for FrikadellenBAF.
//! On startup it checks GitHub for the latest release, downloads the main
//! binary if a newer version is available, and then runs it.
//!
//! Usage: just run `FrikadellenBAF-loader` instead of `frikadellen_baf` directly.

use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

const GITHUB_REPO: &str = "TreXito/frikadellen-baf-121";
const GITHUB_API_BASE: &str = "https://api.github.com";

/// The asset name for the main binary on the current platform.
fn platform_asset_name() -> Option<&'static str> {
    if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some("frikadellen_baf-linux-x86_64")
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Some("frikadellen_baf-macos-x86_64")
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some("frikadellen_baf-macos-arm64")
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        Some("frikadellen_baf-windows-x86_64.exe")
    } else {
        None
    }
}

/// The filename of the main binary on the current platform.
fn main_binary_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "frikadellen_baf.exe"
    } else {
        "frikadellen_baf"
    }
}

/// Read the version tag stored in a `.version` file next to the binary.
/// Returns `None` if the file does not exist yet.
fn read_local_version(dir: &Path) -> Option<String> {
    let path = dir.join(".version");
    fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

/// Write the version tag to `.version` next to the binary.
fn write_local_version(dir: &Path, version: &str) {
    let path = dir.join(".version");
    if let Ok(mut f) = fs::File::create(path) {
        let _ = f.write_all(version.as_bytes());
    }
}

/// Response structure for the GitHub releases API.
#[derive(serde::Deserialize)]
struct GithubRelease {
    tag_name: String,
    assets: Vec<GithubAsset>,
}

#[derive(serde::Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
}

fn check_and_update() -> anyhow::Result<()> {
    let exe_path = env::current_exe()?;
    let exe_dir = exe_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));

    let asset_name = match platform_asset_name() {
        Some(n) => n,
        None => {
            eprintln!("[Loader] Unsupported platform — skipping update check.");
            return Ok(());
        }
    };

    println!("[Loader] Checking for updates on GitHub ({})…", GITHUB_REPO);

    // Query the latest release from the GitHub API.
    let url = format!("{}/repos/{}/releases/latest", GITHUB_API_BASE, GITHUB_REPO);
    let client = reqwest::blocking::Client::builder()
        .user_agent("FrikadellenBAF-loader/1.0")
        .timeout(std::time::Duration::from_secs(15))
        .build()?;

    let resp = client.get(&url).send()?;
    if !resp.status().is_success() {
        eprintln!(
            "[Loader] GitHub API returned {}: skipping update.",
            resp.status()
        );
        return Ok(());
    }

    let release: GithubRelease = resp.json()?;
    let latest_tag = release.tag_name.trim().to_string();

    let local_version = read_local_version(&exe_dir);
    println!(
        "[Loader] Latest release: {}  |  Local version: {}",
        latest_tag,
        local_version.as_deref().unwrap_or("(unknown)")
    );

    if local_version.as_deref() == Some(latest_tag.as_str()) {
        println!("[Loader] Already up-to-date. Launching FrikadellenBAF…");
        return Ok(());
    }

    // Find the matching asset for this platform.
    let asset = match release.assets.iter().find(|a| a.name == asset_name) {
        Some(a) => a,
        None => {
            eprintln!(
                "[Loader] No asset named '{}' found in release {}. Launching existing binary.",
                asset_name, latest_tag
            );
            return Ok(());
        }
    };

    println!(
        "[Loader] Downloading {} from {}…",
        asset.name, asset.browser_download_url
    );

    let bytes = client.get(&asset.browser_download_url).send()?.bytes()?;
    let main_bin = exe_dir.join(main_binary_name());

    download_and_replace(&bytes, &main_bin, &exe_dir, &latest_tag)?;

    println!("[Loader] ✅ Updated to {} successfully.", latest_tag);
    Ok(())
}

fn download_and_replace(
    bytes: &[u8],
    main_bin: &Path,
    exe_dir: &Path,
    latest_tag: &str,
) -> anyhow::Result<()> {
    #[cfg(target_os = "windows")]
    {
        let tmp = exe_dir.join("frikadellen_baf_new.exe");
        fs::write(&tmp, bytes)?;
        // Write a small .bat that swaps the binary after the loader exits.
        // Paths come from env::current_exe() + fixed filenames, so they are
        // under user control and don't contain untrusted input.
        let bat = exe_dir.join("_baf_update.bat");
        let tmp_str = tmp
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("tmp path contains non-UTF-8 characters"))?;
        let main_str = main_bin
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("main binary path contains non-UTF-8 characters"))?;
        let script = format!(
            "@echo off\r\ntimeout /t 1 /nobreak >nul\r\nmove /y \"{tmp}\" \"{main}\" >nul 2>&1\r\nstart \"\" \"{main}\"\r\ndel \"%~f0\"\r\n",
            tmp = tmp_str,
            main = main_str
        );
        fs::write(&bat, script)?;
        write_local_version(exe_dir, latest_tag);
        let bat_str = bat
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("bat path contains non-UTF-8 characters"))?;
        Command::new("cmd").args(["/c", bat_str]).spawn()?;
        // Exit so the .bat can replace the locked binary.
        std::process::exit(0);
    }

    #[cfg(not(target_os = "windows"))]
    {
        let tmp = exe_dir.join("frikadellen_baf_new");
        fs::write(&tmp, bytes)?;

        // Make executable.
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&tmp)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&tmp, perms)?;

        // Atomic rename (same filesystem — guaranteed by using exe_dir for tmp).
        fs::rename(&tmp, main_bin)?;

        write_local_version(exe_dir, latest_tag);
    }

    Ok(())
}

fn launch_main_binary() -> ! {
    let exe_path = env::current_exe().unwrap_or_else(|_| PathBuf::from("."));
    let exe_dir = exe_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let main_bin = exe_dir.join(main_binary_name());

    if !main_bin.exists() {
        eprintln!(
            "[Loader] Main binary not found at {:?}. Please place '{}' next to the loader.",
            main_bin,
            main_binary_name()
        );
        std::process::exit(1);
    }

    println!("[Loader] Launching {}…", main_bin.display());

    // Pass through all arguments from the loader to the main binary.
    let args: Vec<String> = env::args().skip(1).collect();

    let status = Command::new(&main_bin)
        .args(&args)
        .status()
        .unwrap_or_else(|e| {
            eprintln!("[Loader] Failed to launch {:?}: {}", main_bin, e);
            std::process::exit(1);
        });

    std::process::exit(status.code().unwrap_or(1));
}

fn main() {
    if let Err(e) = check_and_update() {
        eprintln!(
            "[Loader] Update check failed: {}. Proceeding with existing binary.",
            e
        );
    }
    launch_main_binary();
}
