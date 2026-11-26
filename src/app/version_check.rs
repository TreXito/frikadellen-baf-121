//! Startup check against the latest GitHub release.
//!
//! Moved out of main.rs unchanged; see `app/mod.rs`.

#[allow(unused_imports)]
use crate::*;

/// GitHub release response (subset of fields).
#[derive(serde::Deserialize)]
pub(crate) struct GithubRelease {
    pub(crate) tag_name: String,
    pub(crate) published_at: Option<String>,
}

/// Check the GitHub releases API to see if the current binary is outdated.
/// Logs a prominent warning if the latest release tag differs from the local
/// version.  The local version is determined by:
///   1. The `.version` file next to the binary (written by the loader), or
///   2. The hardcoded `VERSION` constant (protocol version, as a last resort).
///
/// This avoids false "outdated" warnings when the loader has already updated
/// the binary to the latest release.
pub(crate) async fn check_version_outdated() {
    let client = match reqwest::Client::builder()
        .user_agent("FrikadellenBAF/version-check")
        .timeout(std::time::Duration::from_secs(8))
        .build()
    {
        Ok(c) => c,
        Err(_) => return,
    };
    let url = format!(
        "https://api.github.com/repos/{}/releases/latest",
        GITHUB_REPO
    );
    let resp = match client.get(&url).send().await {
        Ok(r) if r.status().is_success() => r,
        _ => return,
    };
    let release: GithubRelease = match resp.json().await {
        Ok(r) => r,
        Err(_) => return,
    };
    let latest_tag = release.tag_name.trim();

    // Read the `.version` file that the loader writes next to the binary.
    // When present and matching the latest release, the binary is up-to-date
    // regardless of the hardcoded VERSION constant.
    let loader_version: Option<String> = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|p| p.join(".version")))
        .and_then(|path| std::fs::read_to_string(path).ok())
        .map(|s| s.trim().to_string());

    let local_version = loader_version.as_deref().unwrap_or(VERSION);

    if latest_tag == local_version {
        return; // Up to date
    }
    let date_info = release
        .published_at
        .as_deref()
        .and_then(|d| d.split('T').next())
        .unwrap_or("unknown date");
    warn!("========================================");
    warn!("YOU ARE USING AN OUTDATED CLIENT, BUG REPORTS ARE NOT VALID FOR OUTDATED CLIENTS");
    warn!(
        "Current version: {}  |  Latest release: {} ({})",
        local_version, latest_tag, date_info
    );
    warn!("Download the latest release or use the FrikadellenBAF-loader for automatic updates.");
    warn!("========================================");
}
