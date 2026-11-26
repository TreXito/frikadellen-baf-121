//! JSON files that survive restarts: session times, profit stats, flip tracker.
//!
//! Moved out of main.rs unchanged; see `app/mod.rs`.

#[allow(unused_imports)]
use crate::*;

/// A single session-time entry stored in `session_times.json`.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub(crate) struct SessionTimeEntry {
    /// Accumulated running seconds for this session.
    pub(crate) secs: u64,
    /// Unix timestamp (seconds) when this entry was last saved.
    pub(crate) saved_at: u64,
}

/// Persisted profit totals for a single account in `profit_stats.json`.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default)]
pub(crate) struct ProfitStatsEntry {
    /// Cumulative AH profit in coins.
    pub(crate) ah_total: i64,
    /// Cumulative Bazaar profit in coins.
    pub(crate) bz_total: i64,
    /// Unix seconds when this entry was last saved — profit follows the same
    /// session rule as uptime: a quick restart (< MAX_SESSION_GAP_SECS) or a
    /// humanization rest break of any length resumes it, any other gap starts a
    /// fresh session at 0.
    #[serde(default)]
    pub(crate) saved_at: u64,
}

/// Load per-account profit stats from disk.
pub(crate) fn load_profit_stats(path: &std::path::Path) -> HashMap<String, ProfitStatsEntry> {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return HashMap::new(),
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

/// Persist the current profit totals for a given IGN.
pub(crate) fn save_profit_stats(
    path: &std::path::Path,
    ign: &str,
    tracker: &frikadellen_baf::profit::ProfitTracker,
) {
    let (ah, bz) = tracker.totals();
    let mut map = load_profit_stats(path);
    map.insert(
        ign.to_string(),
        ProfitStatsEntry {
            ah_total: ah,
            bz_total: bz,
            saved_at: unix_now(),
        },
    );
    if let Ok(json) = serde_json::to_string_pretty(&map) {
        if let Err(e) = std::fs::write(path, json) {
            tracing::warn!("[Profit] Failed to save profit stats: {}", e);
        }
    }
}

/// How long a bought-but-unsold flip stays in the persisted tracker. Hypixel
/// auctions run at most 6 days, so anything older than this can never produce a
/// sale we would still want to attribute.
pub(crate) const MAX_TRACKED_FLIP_AGE_SECS: u64 = 7 * 24 * 60 * 60;

/// A bought-but-unsold flip persisted in `flip_tracker.json`.
///
/// The in-memory tracker keys sell time off an `Instant`, which is monotonic and
/// therefore meaningless once the process exits. Humanization rest breaks
/// deliberately restart the process, so without this file every item bought
/// before a break sold afterwards with no buy price, no profit, no ROI and no
/// "Time to Sell": the sold webhook silently degraded to a bare price.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub(crate) struct TrackedFlipEntry {
    pub(crate) flip: Flip,
    pub(crate) buy_price: u64,
    /// Unix seconds when the item was bought (wall clock, re-anchored on load).
    pub(crate) purchased_at: u64,
    /// Unix seconds when the flip was received from the finder/COFL socket.
    pub(crate) received_at: u64,
}

/// Load every account's persisted flip tracker.
pub(crate) fn load_flip_tracker_file(
    path: &std::path::Path,
) -> HashMap<String, HashMap<String, TrackedFlipEntry>> {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return HashMap::new(),
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

/// Restore this account's bought-but-unsold flips into an in-memory tracker map,
/// converting persisted wall-clock stamps back into `Instant`s so `elapsed()`
/// keeps counting from the real purchase, not from process start.
pub(crate) fn restore_flip_tracker(
    path: &std::path::Path,
    ign: &str,
) -> HashMap<String, (Flip, u64, Instant, Instant)> {
    let now = unix_now();
    let saved = load_flip_tracker_file(path);
    let entries = match saved.get(ign) {
        Some(e) => e,
        None => return HashMap::new(),
    };
    let mut out = HashMap::new();
    let anchor = |ts: u64| -> Instant {
        let age = Duration::from_secs(now.saturating_sub(ts));
        Instant::now().checked_sub(age).unwrap_or_else(Instant::now)
    };
    for (key, entry) in entries {
        if now.saturating_sub(entry.purchased_at) > MAX_TRACKED_FLIP_AGE_SECS {
            continue;
        }
        out.insert(
            key.clone(),
            (
                entry.flip.clone(),
                entry.buy_price,
                anchor(entry.purchased_at),
                anchor(entry.received_at),
            ),
        );
    }
    if !out.is_empty() {
        info!(
            "[FlipTracker] Restored {} bought-but-unsold flip(s) for {}, so sell time and profit survive the restart",
            out.len(),
            ign
        );
    }
    out
}

/// Persist this account's bought-but-unsold flips.
///
/// Only entries with a real buy price are written: an entry with `buy_price == 0`
/// is a flip that was offered but never actually purchased, and keeping those
/// would grow the file without ever producing a sale.
pub(crate) fn save_flip_tracker(path: &std::path::Path, ign: &str, tracker: &FlipTrackerMap) {
    let now = unix_now();
    let snapshot: HashMap<String, TrackedFlipEntry> = match tracker.lock() {
        Ok(t) => t
            .iter()
            .filter(|(_, (_, buy_price, _, _))| *buy_price > 0)
            .map(|(key, (flip, buy_price, purchased, received))| {
                (
                    key.clone(),
                    TrackedFlipEntry {
                        flip: flip.clone(),
                        buy_price: *buy_price,
                        purchased_at: now.saturating_sub(purchased.elapsed().as_secs()),
                        received_at: now.saturating_sub(received.elapsed().as_secs()),
                    },
                )
            })
            .collect(),
        Err(e) => {
            warn!("[FlipTracker] Lock failed while saving: {}", e);
            return;
        }
    };
    let mut map = load_flip_tracker_file(path);
    if snapshot.is_empty() {
        map.remove(ign);
    } else {
        map.insert(ign.to_string(), snapshot);
    }
    if let Ok(json) = serde_json::to_string_pretty(&map) {
        if let Err(e) = std::fs::write(path, json) {
            warn!("[FlipTracker] Failed to save flip tracker: {}", e);
        }
    }
}

/// Return the current Unix timestamp in seconds.
pub(crate) fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Load the session-times map from disk.  Gracefully handles the **old**
/// format (`{ign: u64}`) by treating those entries as expired (secs=0).
pub(crate) fn load_session_times(path: &std::path::Path) -> HashMap<String, SessionTimeEntry> {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return HashMap::new(),
    };
    // Try new format first.
    if let Ok(map) = serde_json::from_str::<HashMap<String, SessionTimeEntry>>(&raw) {
        return map;
    }
    // Fallback: old `{ign: u64}` format — treat all entries as expired.
    if let Ok(old) = serde_json::from_str::<HashMap<String, u64>>(&raw) {
        return old
            .into_keys()
            .map(|k| {
                (
                    k,
                    SessionTimeEntry {
                        secs: 0,
                        saved_at: 0,
                    },
                )
            })
            .collect();
    }
    HashMap::new()
}

/// Persist the accumulated session time for a given IGN.
/// Existing entries for other accounts are preserved.
pub(crate) fn save_session_time(path: &std::path::Path, ign: &str, total_secs: u64) {
    let mut times = load_session_times(path);
    times.insert(
        ign.to_string(),
        SessionTimeEntry {
            secs: total_secs,
            saved_at: unix_now(),
        },
    );
    if let Ok(json) = serde_json::to_string_pretty(&times) {
        if let Err(e) = std::fs::write(path, json) {
            tracing::warn!("[SessionTime] Failed to save session times: {}", e);
        }
    }
}

/// Clear the session time for a given IGN (reset to 0).
/// Used on account switch so the outgoing account starts fresh next time.
pub(crate) fn clear_session_time(path: &std::path::Path, ign: &str) {
    let mut times = load_session_times(path);
    times.remove(ign);
    if let Ok(json) = serde_json::to_string_pretty(&times) {
        let _ = std::fs::write(path, json);
    }
}
