//! Cross-restart session markers shared between the binary (`main.rs`) and the
//! web control panel (`web/server.rs`).
//!
//! A restart caused by an **account switch** must start a FRESH session for the
//! incoming account: profit and uptime both reset to 0. A crash, manual
//! relaunch, or humanization rest break must instead CARRY those totals across
//! the gap. The two cases cannot be told apart at startup from timing alone (a
//! rapid switch looks exactly like a quick restart within the 5-minute session
//! window), so the switch path drops a marker naming the incoming account and
//! the next startup consumes it.

use std::path::PathBuf;

/// Path to the account-switch marker, kept next to the executable alongside
/// `session_times.json` / `profit_stats.json` / `rest_break.json`.
pub fn account_switch_marker_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("account_switch.json")))
        .unwrap_or_else(|| PathBuf::from("account_switch.json"))
}

/// Record that the NEXT process start is an account switch to `ign`. Written by
/// the web panel's switch button and the automatic account-rotation timer right
/// before they restart the process.
pub fn write_account_switch_marker(ign: &str) {
    let v = serde_json::json!({ "ign": ign });
    if let Err(e) = std::fs::write(account_switch_marker_path(), v.to_string()) {
        tracing::warn!("[AccountSwitch] Failed to write account-switch marker: {}", e);
    }
}

/// Consume the account-switch marker: returns `true` when a marker exists for
/// `ign`, deleting it so the reset happens exactly once. A marker naming a
/// different account is left in place (defensive — the switch always targets the
/// account that is about to start).
pub fn take_account_switch(ign: &str) -> bool {
    let path = account_switch_marker_path();
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return false;
    };
    let matches = serde_json::from_str::<serde_json::Value>(&contents)
        .ok()
        .and_then(|v| v.get("ign").and_then(|x| x.as_str()).map(|s| s == ign))
        .unwrap_or(false);
    if matches {
        let _ = std::fs::remove_file(&path);
    }
    matches
}
