//! Flip-intake diagnostics — answers "why am I not getting flips?".
//!
//! The COFL websocket loop in `main.rs` applies a series of gates to every
//! incoming AH flip (AH flips disabled, intake paused, Coflnet not yet
//! authenticated, startup still running, inventory full, …). Each of those
//! gates used to `continue` at `debug!` level, so a bot that silently dropped
//! every flip looked perfectly healthy on the default INFO log and in the
//! panel. This tracker records the drops, surfaces the reason in the web panel
//! and the `/ping` command, and logs a throttled, VISIBLE line so the operator
//! can see what is happening without drowning in per-flip spam.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use tracing::warn;

/// Why an incoming AH flip was not acted on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FlipDropReason {
    /// `enable_ah_flips` is false (toggled off in the panel / config).
    AhDisabled,
    /// The Disconnect button paused intake for this session.
    IntakePaused,
    /// A Coflnet flip arrived before Coflnet auth was confirmed.
    CoflUnauthenticated,
    /// The startup workflow is still running.
    StartupInProgress,
    /// The bot is in the `Startup` state (not yet ready to interact).
    StartupState,
    /// Inventory is full — the bot is in selling mode.
    InventoryFull,
}

impl FlipDropReason {
    const COUNT: usize = 6;

    fn index(self) -> usize {
        match self {
            FlipDropReason::AhDisabled => 0,
            FlipDropReason::IntakePaused => 1,
            FlipDropReason::CoflUnauthenticated => 2,
            FlipDropReason::StartupInProgress => 3,
            FlipDropReason::StartupState => 4,
            FlipDropReason::InventoryFull => 5,
        }
    }

    /// Short human-readable reason.
    pub fn as_str(self) -> &'static str {
        match self {
            FlipDropReason::AhDisabled => "AH flips are disabled",
            FlipDropReason::IntakePaused => "flip intake is paused (Disconnect)",
            FlipDropReason::CoflUnauthenticated => "Coflnet is not authenticated yet",
            FlipDropReason::StartupInProgress => "the startup workflow is still running",
            FlipDropReason::StartupState => "the bot is still starting up",
            FlipDropReason::InventoryFull => "the inventory is full (selling mode)",
        }
    }

    /// Actionable hint shown alongside the reason.
    pub fn hint(self) -> &'static str {
        match self {
            FlipDropReason::AhDisabled => "enable AH flips in the panel / config",
            FlipDropReason::IntakePaused => "press Connect to resume intake",
            FlipDropReason::CoflUnauthenticated => "sign in to Coflnet (waiting for auth)",
            FlipDropReason::StartupInProgress => "waiting for startup to finish — this usually clears on its own",
            FlipDropReason::StartupState => "waiting for the bot to reach the island",
            FlipDropReason::InventoryFull => "the bot is selling to free space, then it resumes",
        }
    }
}

/// How long to stay quiet before re-logging the SAME drop reason. A *changed*
/// reason always logs immediately (see `record_drop`), so the first sign of a
/// problem is never delayed; this only rate-limits a persistent reason (e.g.
/// AH flips deliberately left off) so it doesn't spam the log.
const LOG_THROTTLE_SECS: u64 = 120;

struct Inner {
    last_accepted_at: Option<Instant>,
    last_drop: Option<(FlipDropReason, Instant)>,
    /// Reason last logged and when, so repeated drops don't spam the log.
    last_logged: Option<(FlipDropReason, Instant)>,
}

/// Thread-safe flip-intake diagnostics shared between the intake loop, the web
/// panel status endpoint and the `/ping` command.
pub struct FlipDiagnostics {
    accepted: AtomicU64,
    dropped_total: AtomicU64,
    dropped: [AtomicU64; FlipDropReason::COUNT],
    inner: Mutex<Inner>,
}

impl Default for FlipDiagnostics {
    fn default() -> Self {
        Self::new()
    }
}

impl FlipDiagnostics {
    pub fn new() -> Self {
        Self {
            accepted: AtomicU64::new(0),
            dropped_total: AtomicU64::new(0),
            dropped: std::array::from_fn(|_| AtomicU64::new(0)),
            inner: Mutex::new(Inner {
                last_accepted_at: None,
                last_drop: None,
                last_logged: None,
            }),
        }
    }

    /// A flip was queued for purchase — intake is healthy.
    pub fn record_accepted(&self) {
        self.accepted.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut inner) = self.inner.lock() {
            inner.last_accepted_at = Some(Instant::now());
        }
    }

    /// A flip was dropped for `reason`. Increments counters, remembers the most
    /// recent drop, and logs a VISIBLE (warn-level) line — but at most once per
    /// [`LOG_THROTTLE_SECS`] per reason so a busy feed doesn't flood the log.
    pub fn record_drop(&self, reason: FlipDropReason, item_name: &str) {
        self.dropped_total.fetch_add(1, Ordering::Relaxed);
        self.dropped[reason.index()].fetch_add(1, Ordering::Relaxed);

        let now = Instant::now();
        let should_log = if let Ok(mut inner) = self.inner.lock() {
            inner.last_drop = Some((reason, now));
            let due = match inner.last_logged {
                Some((prev, at)) => {
                    prev != reason || now.duration_since(at).as_secs() >= LOG_THROTTLE_SECS
                }
                None => true,
            };
            if due {
                inner.last_logged = Some((reason, now));
            }
            due
        } else {
            false
        };

        if should_log {
            let count = self.dropped[reason.index()].load(Ordering::Relaxed);
            warn!(
                "[FlipDrop] Not buying flips — {} ({} dropped for this reason). Fix: {}. Latest: {}",
                reason.as_str(),
                count,
                reason.hint(),
                item_name,
            );
        }
    }

    /// Total flips accepted (queued) this session.
    pub fn accepted_total(&self) -> u64 {
        self.accepted.load(Ordering::Relaxed)
    }

    /// Total flips dropped this session across all reasons.
    pub fn dropped_total(&self) -> u64 {
        self.dropped_total.load(Ordering::Relaxed)
    }

    /// Seconds since the most recent accepted flip, if any.
    pub fn secs_since_accepted(&self) -> Option<u64> {
        self.inner
            .lock()
            .ok()
            .and_then(|i| i.last_accepted_at.map(|t| t.elapsed().as_secs()))
    }

    /// The most recent drop reason and how many seconds ago it happened.
    pub fn last_drop(&self) -> Option<(FlipDropReason, u64)> {
        self.inner
            .lock()
            .ok()
            .and_then(|i| i.last_drop.map(|(r, t)| (r, t.elapsed().as_secs())))
    }

    /// One-line human summary for the `/ping` command and panel tooltips.
    pub fn summary_line(&self) -> String {
        let accepted = self.accepted_total();
        let dropped = self.dropped_total();
        match self.last_drop() {
            Some((reason, secs_ago)) if dropped > 0 => format!(
                "flips: {accepted} bought / {dropped} skipped — last skip {secs_ago}s ago because {} ({})",
                reason.as_str(),
                reason.hint(),
            ),
            _ if accepted > 0 => format!("flips: {accepted} bought, none skipped — intake healthy"),
            _ => "flips: none seen yet this session".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_accepted_and_dropped() {
        let d = FlipDiagnostics::new();
        assert_eq!(d.accepted_total(), 0);
        assert_eq!(d.dropped_total(), 0);
        assert!(d.last_drop().is_none());
        assert!(d.summary_line().contains("none seen"));

        d.record_drop(FlipDropReason::AhDisabled, "Test Sword");
        d.record_drop(FlipDropReason::AhDisabled, "Test Bow");
        assert_eq!(d.dropped_total(), 2);
        let (reason, _secs_ago) = d.last_drop().expect("a drop was recorded");
        assert_eq!(reason, FlipDropReason::AhDisabled);
        assert!(d.summary_line().contains("skipped"));

        d.record_accepted();
        assert_eq!(d.accepted_total(), 1);
    }

    #[test]
    fn healthy_summary_when_only_accepted() {
        let d = FlipDiagnostics::new();
        d.record_accepted();
        let s = d.summary_line();
        assert!(s.contains("bought"), "got: {s}");
        assert!(s.contains("healthy"), "got: {s}");
    }
}
