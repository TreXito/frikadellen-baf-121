//! Stealth session pacing: the join/leave history an admin can actually pull.
//!
//! Everything a macro reviewer can see about an account without any behavioural
//! data is timing: when it logged in, when it logged out, and how much of the
//! day it was online. An account that never leaves, or that naps for the same
//! few minutes on the same schedule all day, is a machine on that evidence
//! alone — no movement, chat or buying behaviour has to be flagged for it to
//! look wrong.
//!
//! This module owns the two numbers that shape that history:
//!
//! * **the online span** — how long one connection lasts, drawn from a
//!   configured range with a triangular distribution so mid-length sessions
//!   cluster the way a person's sessions do, instead of every draw landing
//!   somewhere in a flat, uniform cloud (or worse, on the same endpoint);
//! * **the daily budget** — how much online time one "day" gets, redrawn ±25%
//!   each day. Once it is spent the bot does not take another short nap, it
//!   logs off for a human-sized night: roughly 24h minus whatever was played.
//!
//! Both numbers are persisted next to `rest_break.json`, so a crash or a manual
//! restart in the middle of a day does not hand the account a fresh budget and
//! a break cannot be laundered into extra play time.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use rand::Rng;
use serde::{Deserialize, Serialize};

/// Triangular draw over `min..=max`: the average of two uniform draws.
///
/// A plain uniform draw spends as much time at the endpoints as anywhere else,
/// so a "60–300 min" range quietly turns into sessions that are frequently
/// either an hour or five hours long. The triangular shape clusters around the
/// middle: consecutive sessions still differ, but they read as one population
/// of similar-length sittings rather than two fixed extremes.
pub fn sample_range(min: u64, max: u64) -> u64 {
    if min >= max {
        return min;
    }
    let mut rng = rand::rng();
    let a = rng.random_range(min..=max);
    let b = rng.random_range(min..=max);
    (a + b) / 2
}

/// Daily online allowance drawn around the configured hint: `hint ±25%`, so no
/// two days carry the same budget. Hints below 15 minutes are floored, because
/// an allowance that small only produces machine-short days.
///
/// Returns 0 for a 0 hint, which is the "no daily cap" signal.
pub fn draw_budget_secs(hint_secs: u64) -> u64 {
    if hint_secs == 0 {
        return 0;
    }
    let lo = (hint_secs * 3 / 4).max(15 * 60);
    let hi = (hint_secs * 5 / 4).max(lo + 60);
    sample_range(lo, hi)
}

/// How long the account stays offline when the daily budget is spent.
///
/// The day should end where a person's day ends: about 24h after it started,
/// minus whatever was actually played. Clamped to 4–16h so an extreme day still
/// gets a plausible night, then jittered ±45 minutes so two consecutive nights
/// are never the same length.
pub fn long_break_secs(spent_secs: u64) -> u64 {
    const DAY: u64 = 24 * 60 * 60;
    const MIN: u64 = 4 * 60 * 60;
    const MAX: u64 = 16 * 60 * 60;
    let base = DAY.saturating_sub(spent_secs).max(MIN);
    let jitter = sample_range(0, 90 * 60) as i64 - 45 * 60;
    (base as i64 + jitter).clamp(MIN as i64, MAX as i64) as u64
}

/// Path of the stealth day state, kept next to the executable alongside
/// `session_times.json` / `profit_stats.json` / `rest_break.json`.
fn day_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("stealth_day.json")))
        .unwrap_or_else(|| PathBuf::from("stealth_day.json"))
}

/// One account's day: when it starts, how much online time it has used and
/// what it is allowed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StealthDay {
    /// Account this day belongs to. A file naming a different account starts a
    /// fresh day for the incoming one.
    pub ign: String,
    /// UNIX time the day starts counting from (set past the closing night when
    /// a day rolls over, so the offline gap itself never counts as play).
    pub anchor_unix: u64,
    /// Online seconds used out of `budget_secs`.
    pub spent_secs: u64,
    /// Allowed online seconds for this day, 0 = no cap.
    pub budget_secs: u64,
}

/// Persistence + policy for the daily budget of a single account.
pub struct StealthTracker {
    day: StealthDay,
    path: PathBuf,
    /// Configured daily hint in seconds, 0 = cap disabled. Kept so a rollover
    /// can redraw the next day's budget without re-reading the config.
    daily_hint_secs: u64,
}

impl StealthTracker {
    /// Load the persisted day for `ign`, starting a fresh one when the file is
    /// missing or corrupt, names another account, or is older than a sane day
    /// window (a box that was off overnight must not resume yesterday's spend).
    pub fn load(ign: &str, now_unix: u64, daily_hint_secs: u64) -> Self {
        Self::load_from(day_path(), ign, now_unix, daily_hint_secs)
    }

    fn load_from(path: PathBuf, ign: &str, now_unix: u64, daily_hint_secs: u64) -> Self {
        /// Anything older than this is yesterday's file, not a day in progress.
        const DAY_MAX_AGE_SECS: u64 = 36 * 60 * 60;

        let existing = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<StealthDay>(&raw).ok())
            .filter(|day| {
                day.ign == ign && now_unix.saturating_sub(day.anchor_unix) <= DAY_MAX_AGE_SECS
            });

        let mut tracker = match existing {
            Some(day) => StealthTracker {
                day,
                path,
                daily_hint_secs,
            },
            None => StealthTracker {
                day: StealthDay {
                    ign: ign.to_string(),
                    anchor_unix: now_unix,
                    spent_secs: 0,
                    budget_secs: draw_budget_secs(daily_hint_secs),
                },
                path,
                daily_hint_secs,
            },
        };
        tracker.apply_hint();
        tracker
    }

    /// Keep the persisted allowance in step with the config: turning the cap on
    /// mid-day gives today an allowance, turning it off removes it. A day that
    /// already has one keeps it — redrawing on every restart would hand the
    /// account a fresh budget after every single break.
    fn apply_hint(&mut self) {
        if self.daily_hint_secs == 0 {
            self.day.budget_secs = 0;
        } else if self.day.budget_secs == 0 {
            self.day.budget_secs = draw_budget_secs(self.daily_hint_secs);
        }
    }

    /// Spend `secs` of online time against the day. Called from the 60s uptime
    /// tick with the delta counted while the connection was up, so time spent
    /// offline (a break, a crash gap) never eats into the budget.
    pub fn add_spent(&mut self, secs: u64) {
        if secs == 0 {
            return;
        }
        self.day.spent_secs = self.day.spent_secs.saturating_add(secs);
        self.save();
    }

    pub fn spent_secs(&self) -> u64 {
        self.day.spent_secs
    }

    pub fn budget_secs(&self) -> u64 {
        self.day.budget_secs
    }

    /// True when today's allowance is used up, i.e. the next break should end
    /// the day instead of being another nap.
    pub fn exceeded(&self) -> bool {
        self.day.budget_secs > 0 && self.day.spent_secs >= self.day.budget_secs
    }

    /// Roll over to a new day that starts after `offline_secs` of being logged
    /// off, with a freshly drawn allowance.
    pub fn start_new_day(&mut self, now_unix: u64, offline_secs: u64) {
        self.day.anchor_unix = now_unix.saturating_add(offline_secs);
        self.day.spent_secs = 0;
        self.day.budget_secs = draw_budget_secs(self.daily_hint_secs);
        self.save();
    }

    fn save(&self) {
        let Ok(raw) = serde_json::to_string(&self.day) else {
            return;
        };
        if let Err(e) = std::fs::write(&self.path, raw) {
            tracing::warn!("[Stealth] Failed to persist day state: {}", e);
        }
    }
}

/// The day state shared between the uptime tick and the break scheduler.
pub type SharedTracker = Arc<Mutex<StealthTracker>>;

/// Wrap a tracker for sharing between tasks.
pub fn share(tracker: StealthTracker) -> SharedTracker {
    Arc::new(Mutex::new(tracker))
}

/// Lock the shared day state without panicking on a poisoned mutex: the value
/// inside is a few counters that every task derives the same way, so reading
/// through the poison is strictly better than killing the scheduler.
pub fn lock(tracker: &SharedTracker) -> MutexGuard<'_, StealthTracker> {
    tracker.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("stealth_day_{}_{}.json", tag, std::process::id()))
    }

    #[test]
    fn sample_range_stays_inside_the_configured_range() {
        for _ in 0..500 {
            let v = sample_range(60, 300);
            assert!((60..=300).contains(&v), "{v} is outside 60..=300");
        }
    }

    #[test]
    fn sample_range_of_an_empty_or_inverted_range_is_the_low_end() {
        assert_eq!(sample_range(120, 120), 120);
        assert_eq!(sample_range(300, 60), 300);
    }

    #[test]
    fn sample_range_clusters_around_the_middle() {
        // Triangular over 60..=300 puts ~56% of draws in the middle third
        // (140..=220) where a uniform draw would put 33%. A failure here means
        // the sampler degenerated into something flat or endpoint-heavy.
        let mut in_middle_third = 0usize;
        for _ in 0..2000 {
            let v = sample_range(60, 300);
            if (140..=220).contains(&v) {
                in_middle_third += 1;
            }
        }
        assert!(
            in_middle_third > 1000,
            "only {in_middle_third}/2000 draws landed in the middle third"
        );
    }

    #[test]
    fn budget_is_drawn_around_the_hint() {
        let hint = 600 * 60;
        for _ in 0..200 {
            let b = draw_budget_secs(hint);
            assert!((hint * 3 / 4..=hint * 5 / 4).contains(&b), "{b}");
        }
        assert_eq!(draw_budget_secs(0), 0);
    }

    #[test]
    fn a_night_off_ends_near_a_day() {
        // Any day gets a plausible night: never machine-short, never a week.
        for spent in [0, 4 * 3600, 10 * 3600, 20 * 3600, 40 * 3600] {
            let off = long_break_secs(spent);
            assert!(
                (4 * 3600..=16 * 3600).contains(&off),
                "spent {spent} → {off}"
            );
        }
        // 10h played closes a 24h day: 14h off, ±45min of jitter.
        let off = long_break_secs(10 * 3600);
        assert!(
            (14 * 3600 - 45 * 60..=14 * 3600 + 45 * 60).contains(&off),
            "{off}"
        );
    }

    #[test]
    fn a_restart_mid_day_does_not_hand_out_a_fresh_budget() {
        let path = temp_path("resume");
        let _ = std::fs::remove_file(&path);

        let mut day = StealthTracker::load_from(path.clone(), "Notch", 1_000_000, 600 * 60);
        assert!(!day.exceeded());
        assert!(day.budget_secs() > 0);
        let budget = day.budget_secs();
        day.add_spent(budget);
        assert!(day.exceeded());

        // Same account, five minutes later: the spent budget survives.
        let resumed = StealthTracker::load_from(path.clone(), "Notch", 1_000_030, 600 * 60);
        assert!(resumed.exceeded());
        assert_eq!(resumed.spent_secs(), budget);

        // A different account starts its own day.
        let other = StealthTracker::load_from(path.clone(), "Jeb", 1_000_030, 600 * 60);
        assert_eq!(other.spent_secs(), 0);
        assert!(!other.exceeded());

        // Rolling the day resets the spend and moves the anchor past the night.
        let mut day = resumed;
        day.start_new_day(1_000_030, long_break_secs(day.spent_secs()));
        assert_eq!(day.spent_secs(), 0);
        assert!(!day.exceeded());
        assert!(day.budget_secs() > 0);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_day_older_than_the_window_starts_over() {
        let path = temp_path("stale");
        let _ = std::fs::remove_file(&path);

        let mut day = StealthTracker::load_from(path.clone(), "Notch", 1_000_000, 600 * 60);
        let budget = day.budget_secs();
        day.add_spent(budget);
        assert!(day.exceeded());

        // 40h later the file is yesterday's business, not a day in progress.
        let later =
            StealthTracker::load_from(path.clone(), "Notch", 1_000_000 + 40 * 3600, 600 * 60);
        assert!(!later.exceeded());
        assert_eq!(later.spent_secs(), 0);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_zero_hint_disables_the_daily_cap() {
        let path = temp_path("nocap");
        let _ = std::fs::remove_file(&path);

        let mut day = StealthTracker::load_from(path, "Notch", 1_000_000, 0);
        assert_eq!(day.budget_secs(), 0);
        day.add_spent(50 * 3600);
        assert!(!day.exceeded());
    }
}
