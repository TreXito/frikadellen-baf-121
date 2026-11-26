//! Rest-break / maintenance restart marker and waiting logic.
//!
//! Moved out of main.rs unchanged; see `app/mod.rs`.

#[allow(unused_imports)]
use crate::*;

/// Path to the rest-break marker sidecar (next to the executable).
pub(crate) fn rest_break_marker_path() -> std::path::PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("rest_break.json")))
        .unwrap_or_else(|| std::path::PathBuf::from("rest_break.json"))
}

/// Persist a "resting until this UNIX time" marker so a humanization rest break
/// is honored across the process restart that starts it. The bot stays FULLY
/// offline (no Hypixel, no COFL) until the deadline instead of lingering
/// in-process, where the ECS client / AFK handler / reconnect loop could drift
/// it back into the lobby.
/// `maintenance` marks a SkyBlock-maintenance break: the next start waits on the
/// status page instead of a fixed timer, and `until` is only the hard cap.
pub(crate) fn write_rest_break_marker(ign: &str, until_unix: u64, maintenance: bool) {
    let mut v = serde_json::json!({ "ign": ign, "until": until_unix });
    if maintenance {
        v["reason"] = serde_json::json!("maintenance");
    }
    if let Err(e) = std::fs::write(rest_break_marker_path(), v.to_string()) {
        tracing::warn!("[Humanization] Failed to write rest-break marker: {}", e);
    }
}

/// Remaining seconds to stay offline for a pending rest break for `ign`, if any.
/// Returns `None` when there is no marker, it belongs to a different account, or
/// the break has already elapsed.
pub(crate) fn pending_rest_break_secs(ign: &str) -> Option<u64> {
    let contents = std::fs::read_to_string(rest_break_marker_path()).ok()?;
    let v: serde_json::Value = serde_json::from_str(&contents).ok()?;
    // Only honor a marker for THIS account — a different configured account must
    // not inherit a leftover break.
    if v.get("ign").and_then(|x| x.as_str()) != Some(ign) {
        return None;
    }
    let until = v.get("until").and_then(|x| x.as_u64())?;
    let now = unix_now();
    (until > now).then(|| until - now)
}

/// True when the pending break marker for `ign` is a SkyBlock-maintenance break.
pub(crate) fn pending_break_is_maintenance(ign: &str) -> bool {
    let Ok(contents) = std::fs::read_to_string(rest_break_marker_path()) else {
        return false;
    };
    serde_json::from_str::<serde_json::Value>(&contents)
        .ok()
        .map(|v| {
            v.get("ign").and_then(|x| x.as_str()) == Some(ign)
                && v.get("reason").and_then(|x| x.as_str()) == Some("maintenance")
        })
        .unwrap_or(false)
}

/// Shared rest-break teardown: pause flip intake and the command queue, persist
/// profit / flip tracker / session time so all three survive the restart, write
/// the break-until marker and restart the process. The fresh process waits out
/// the remaining break AFTER its web panel is up (see the "wait out the
/// remaining rest break" block in main) and then reconnects. Used by BOTH the
/// automatic humanization scheduler and the web panel's break-on-demand button.
///
/// Never returns: the process is replaced by `restart_process`.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn take_rest_break_and_restart(
    ign: &str,
    break_secs: u64,
    macro_paused: &AtomicBool,
    flip_intake_paused: &AtomicBool,
    command_queue: &frikadellen_baf::state::CommandQueue,
    profit_tracker: &frikadellen_baf::profit::ProfitTracker,
    profit_path: &std::path::Path,
    flip_tracker: &FlipTrackerMap,
    flip_tracker_path: &std::path::Path,
    session_times_path: &std::path::Path,
    total_secs: u64,
    maintenance: bool,
) -> ! {
    // A rest break must leave the account genuinely offline for the whole
    // duration. Dropping the connection in-process and sleeping proved
    // unreliable — the ECS client / AFK handler / reconnect loop could
    // bring the bot back and leave it idling in the lobby (never actually
    // "resting"). Instead we persist a break-until marker and restart NOW:
    // the fresh process waits out the remaining break offline, with the web
    // panel still up, before it reconnects.
    flip_intake_paused.store(true, std::sync::atomic::Ordering::Relaxed);
    macro_paused.store(true, std::sync::atomic::Ordering::Relaxed);
    command_queue.clear();

    // Save profit + session time so both survive the restart/break. Session
    // time is saved right before restart so the gap is near-zero.
    save_profit_stats(profit_path, ign, profit_tracker);
    // Items bought before the break keep selling while we are offline
    // (the auctions stay up), so their purchase baselines have to cross
    // the restart or the sold webhook loses time-to-sell and ROI.
    save_flip_tracker(flip_tracker_path, ign, flip_tracker);
    save_session_time(session_times_path, ign, total_secs);

    // Persist the break deadline, then restart. The next process start stays
    // offline until this time and sends the "break over" webhook.
    let until = unix_now() + break_secs;
    write_rest_break_marker(ign, until, maintenance);
    if maintenance {
        info!(
            "[Maintenance] Going offline — saved session {}s, restarting to wait on the status page",
            total_secs
        );
    } else {
        info!(
            "[Humanization] Rest break ({:.1}m) — saved session {}s, restarting offline until break ends",
            break_secs as f64 / 60.0, total_secs
        );
    }
    restart_process()
}

/// Stay offline until the status page says SkyBlock is back, then return why.
///
/// Runs on the fresh process after a maintenance break, before any Minecraft
/// connection. Only plain HTTPS polls of status.hypixel.net go out, every 2-3
/// minutes. `cap_secs` is a hard ceiling from the marker. See
/// `hypixel_status::reconnect_decision` for the rules.
pub(crate) async fn wait_out_maintenance(cap_secs: u64) -> &'static str {
    use frikadellen_baf::hypixel_status::{
        fetch_skyblock_status, reconnect_decision, SkyblockStatus,
    };
    use rand::Rng;

    info!("[Maintenance] Offline until SkyBlock is back — polling status.hypixel.net");
    let started = std::time::Instant::now();
    let mut saw_down = false;
    let mut last: Option<SkyblockStatus> = None;
    loop {
        let status = fetch_skyblock_status().await;
        saw_down |= matches!(status, SkyblockStatus::Down { .. });
        let offline_secs = started.elapsed().as_secs();
        if last.as_ref() != Some(&status) {
            info!(
                "[Maintenance] Status page: {:?} (offline {}m)",
                status,
                offline_secs / 60
            );
        }
        if let Some(reason) = reconnect_decision(&status, saw_down, offline_secs, unix_now()) {
            info!("[Maintenance] Reconnecting — {}", reason.describe());
            return reason.describe();
        }
        if offline_secs >= cap_secs {
            info!("[Maintenance] Offline cap reached — reconnecting");
            return "offline cap reached";
        }
        last = Some(status);
        let pause = rand::rng().random_range(120..=180u64);
        tokio::time::sleep(Duration::from_secs(
            pause.min(cap_secs.saturating_sub(offline_secs).max(1)),
        ))
        .await;
    }
}

/// True when a rest-break marker exists for this account, i.e. the previous
/// process exited to take a humanization break rather than being stopped by the
/// user. Unlike `pending_rest_break_secs` this ignores whether the break window
/// has already elapsed — it answers "was this restart our own break?", which is
/// what decides whether uptime and profit carry over across the gap.
pub(crate) fn resumed_from_rest_break(ign: &str) -> bool {
    let Ok(contents) = std::fs::read_to_string(rest_break_marker_path()) else {
        return false;
    };
    serde_json::from_str::<serde_json::Value>(&contents)
        .ok()
        .and_then(|v| v.get("ign").and_then(|x| x.as_str()).map(|s| s == ign))
        .unwrap_or(false)
}

/// Delete the rest-break marker (break finished or no longer applicable).
pub(crate) fn clear_rest_break_marker() {
    let _ = std::fs::remove_file(rest_break_marker_path());
}
