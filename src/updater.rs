//! In-process self-update.
//!
//! Mirrors the `FrikadellenBAF-loader` logic (see `src/loader.rs`) so the bot
//! can download the latest release and swap itself in from the web GUI —
//! without the user dropping to a console or relying on the external loader.
//!
//! The flow is split in two so the web handler can flush its HTTP response
//! before the process is replaced:
//!   1. [`download_latest`] — query GitHub, download the newer binary, and stage
//!      it (swap in place on Unix, write a `_new` sidecar on Windows). Returns
//!      without exiting.
//!   2. [`finish_update_restart`] — apply the staged update and restart: on
//!      Windows spawn the swap `.bat` and exit; on Unix exec the freshly-swapped
//!      binary. Diverges (never returns).

use std::path::{Path, PathBuf};

const GITHUB_REPO: &str = "TreXito/frikadellen-baf-121";
const GITHUB_API_BASE: &str = "https://api.github.com";

/// The release asset name for the main binary on the current platform.
/// Mirrors the loader so both updaters resolve the same asset.
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

/// The on-disk filename of the main binary on the current platform.
fn main_binary_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "frikadellen_baf.exe"
    } else {
        "frikadellen_baf"
    }
}

/// Directory containing the running executable (falls back to `.`).
fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Path of the staged (downloaded, not-yet-applied) binary on Windows, where the
/// running `.exe` is locked and can only be swapped after the process exits.
#[cfg(target_os = "windows")]
fn staged_binary_path(dir: &Path) -> PathBuf {
    dir.join("frikadellen_baf_new.exe")
}

fn read_local_version(dir: &Path) -> Option<String> {
    std::fs::read_to_string(dir.join(".version"))
        .ok()
        .map(|s| s.trim().to_string())
}

fn write_local_version(dir: &Path, version: &str) {
    let _ = std::fs::write(dir.join(".version"), version.as_bytes());
}

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

/// Outcome of [`download_latest`].
pub enum UpdateStatus {
    /// The local `.version` already matches the latest release — nothing staged.
    UpToDate { version: String },
    /// A newer release was downloaded and staged; call [`finish_update_restart`].
    Updated { version: String },
}

/// Query GitHub for the latest release and, if it is newer than the local
/// `.version`, download and stage the new binary. Does **not** restart or exit —
/// the caller flushes its HTTP response first, then calls
/// [`finish_update_restart`].
pub async fn download_latest() -> anyhow::Result<UpdateStatus> {
    let dir = exe_dir();

    let asset_name = platform_asset_name()
        .ok_or_else(|| anyhow::anyhow!("unsupported platform — no release asset to download"))?;

    let client = reqwest::Client::builder()
        .user_agent("FrikadellenBAF-selfupdate/1.0")
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let url = format!("{}/repos/{}/releases/latest", GITHUB_API_BASE, GITHUB_REPO);
    let resp = client.get(&url).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("GitHub API returned {}", resp.status());
    }
    let release: GithubRelease = resp.json().await?;
    let latest_tag = release.tag_name.trim().to_string();

    if read_local_version(&dir).as_deref() == Some(latest_tag.as_str()) {
        return Ok(UpdateStatus::UpToDate {
            version: latest_tag,
        });
    }

    let asset = release
        .assets
        .iter()
        .find(|a| a.name == asset_name)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "release {} has no asset named '{}'",
                latest_tag,
                asset_name
            )
        })?;

    let dl = client.get(&asset.browser_download_url).send().await?;
    if !dl.status().is_success() {
        // Never write a non-success body (error page / rate-limit notice) over
        // the binary we are about to run — keep the existing one instead.
        anyhow::bail!("download returned {}", dl.status());
    }
    let bytes = dl.bytes().await?;

    stage_binary(&dir, &bytes)?;
    write_local_version(&dir, &latest_tag);

    Ok(UpdateStatus::Updated {
        version: latest_tag,
    })
}

/// Write the downloaded bytes into place (Unix) or a sidecar (Windows).
#[cfg(not(target_os = "windows"))]
fn stage_binary(dir: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    // Stage into the same directory so the rename is atomic (same filesystem).
    let tmp = dir.join("frikadellen_baf_new");
    std::fs::write(&tmp, bytes)?;
    let mut perms = std::fs::metadata(&tmp)?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&tmp, perms)?;
    // On Unix the running binary can be replaced while executing — the process
    // keeps the old (now-unlinked) inode until it re-execs.
    std::fs::rename(&tmp, dir.join(main_binary_name()))?;
    Ok(())
}

#[cfg(target_os = "windows")]
fn stage_binary(dir: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    // The running .exe is locked; write a sidecar and swap it after we exit
    // (see finish_update_restart).
    std::fs::write(staged_binary_path(dir), bytes)?;
    Ok(())
}

/// Apply a staged update and restart into the new binary. Diverges.
///
/// - Unix: the binary was already swapped in [`download_latest`], so just
///   re-exec via [`crate::utils::restart_process`].
/// - Windows: spawn a `.bat` that waits for this process to exit, moves the
///   `_new` sidecar over the locked `.exe`, and relaunches it — then exit.
pub fn finish_update_restart() -> ! {
    #[cfg(not(target_os = "windows"))]
    {
        crate::utils::restart_process();
    }

    #[cfg(target_os = "windows")]
    {
        let dir = exe_dir();
        let main_bin = dir.join(main_binary_name());
        let tmp = staged_binary_path(&dir);
        let bat = dir.join("_baf_update.bat");
        // Paths come from current_exe() + fixed filenames — no untrusted input.
        let script = format!(
            "@echo off\r\ntimeout /t 1 /nobreak >nul\r\nmove /y \"{tmp}\" \"{main}\" >nul 2>&1\r\nstart \"\" \"{main}\"\r\ndel \"%~f0\"\r\n",
            tmp = tmp.display(),
            main = main_bin.display()
        );
        if std::fs::write(&bat, script).is_ok() {
            let _ = std::process::Command::new("cmd")
                .args(["/c", &bat.to_string_lossy()])
                .spawn();
        } else {
            tracing::error!("[Update] Failed to write swap script — restarting without swap");
        }
        // Exit so the .bat can replace the locked binary and relaunch it.
        std::process::exit(0);
    }
}
