//! "Flip on a friend's island" — process-wide state for the `visitfriend`
//! config option.
//!
//! When `visitfriend` names a friend's IGN, every place that would send the bot
//! to its OWN island (`/is`) instead sends it to that friend's island via
//! `/visit <friend>` followed by a click on slot 11 (the "Visit player island"
//! ender-eye in the visit GUI). The friend's island becomes the bot's default
//! "home" so the AFK / stall guards return it there rather than teleporting it
//! back to its own island.
//!
//! If the friend has guest visits disabled, Hypixel answers the slot-11 click
//! with "Couldn't warp you!" / "This island doesn't allow everyone to guest!".
//! In that case the feature is disabled for the rest of the session (the bot
//! flips on its own island instead) and a webhook is sent.
//!
//! Modelled after the module-global `REMOVE_DRILL_PARTS` flag: set once from
//! config at startup, read from both the bot event loop and `main.rs`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::RwLock;
use std::time::Instant;

/// Configured friend IGN, or `None` when the feature is off.
static FRIEND: RwLock<Option<String>> = RwLock::new(None);
/// Set true when the friend's island refuses the bot; it then falls back to its
/// own island until the next process restart.
static DISABLED_FOR_SESSION: AtomicBool = AtomicBool::new(false);
/// Monotonic time of the most recent `/visit` attempt. The chat-failure
/// detector only reacts to warp failures that follow a visit we initiated, so a
/// stray "Couldn't warp you!" from an unrelated command can't disable the
/// feature.
static LAST_ATTEMPT: RwLock<Option<Instant>> = RwLock::new(None);

/// Apply the config value at startup. An empty / whitespace-only string turns
/// the feature off. Also resets the per-session disable + attempt state so a
/// restart starts clean.
pub fn configure(friend: Option<String>) {
    let normalized = friend
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    if let Ok(mut f) = FRIEND.write() {
        *f = normalized.clone();
    }
    DISABLED_FOR_SESSION.store(false, Ordering::Relaxed);
    if let Ok(mut a) = LAST_ATTEMPT.write() {
        *a = None;
    }
    if let Some(name) = normalized {
        tracing::info!("[VisitFriend] Enabled — flipping on {}'s island", name);
    }
}

/// The configured friend IGN regardless of the session-disable state (for
/// messages / webhooks). `None` when the feature is off.
pub fn configured_friend() -> Option<String> {
    FRIEND.read().ok().and_then(|f| f.clone())
}

/// The friend whose island the bot should currently go to: `Some(name)` only
/// when the feature is configured AND not disabled for this session. This is the
/// single decision point for "`/visit <friend>` vs `/is`".
pub fn active_friend() -> Option<String> {
    if DISABLED_FOR_SESSION.load(Ordering::Relaxed) {
        return None;
    }
    configured_friend()
}

/// Disable the friend visit for the rest of this session (island refused us).
pub fn disable_for_session() {
    DISABLED_FOR_SESSION.store(true, Ordering::Relaxed);
}

/// Record that a `/visit` was just attempted.
pub fn note_attempt() {
    if let Ok(mut a) = LAST_ATTEMPT.write() {
        *a = Some(Instant::now());
    }
}

/// True when a `/visit` was attempted within the last `within_secs` seconds.
pub fn recently_attempted(within_secs: u64) -> bool {
    LAST_ATTEMPT
        .read()
        .ok()
        .and_then(|a| *a)
        .map(|t| t.elapsed().as_secs() <= within_secs)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Single test: these functions share process-global state, so exercising
    // the whole lifecycle in one test avoids races with a parallel test.
    #[test]
    fn lifecycle() {
        // Empty / whitespace turns the feature off.
        configure(Some("   ".to_string()));
        assert!(configured_friend().is_none());
        assert!(active_friend().is_none());

        // Configured → active.
        configure(Some(" Notch ".to_string()));
        assert_eq!(configured_friend().as_deref(), Some("Notch"));
        assert_eq!(active_friend().as_deref(), Some("Notch"));

        // Refused this session → still configured, but not active.
        disable_for_session();
        assert!(active_friend().is_none());
        assert_eq!(configured_friend().as_deref(), Some("Notch"));

        // Reconfiguring (e.g. a restart) clears the session-disable.
        configure(Some("Notch".to_string()));
        assert_eq!(active_friend().as_deref(), Some("Notch"));

        // Attempt tracking.
        note_attempt();
        assert!(recently_attempted(5));

        // Reset to off so this global state doesn't leak into other tests.
        configure(None);
        assert!(configured_friend().is_none());
        assert!(active_friend().is_none());
    }
}
