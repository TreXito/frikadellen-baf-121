//! Per-session earnings envelope.
//!
//! The expensive ban on SkyBlock is not the cheat ban. It is the boosting ban,
//! which arrives with a profile wipe, and it is settled from the profile's
//! economy rather than from any packet: the value curve over time. A profile
//! that goes vertical in minutes is the evidence, and no amount of correct
//! protocol hides it. Auction flipping is the most legible income on the server
//! because Hypixel owns the auction house, so every coin of it is in their
//! ledger.
//!
//! This keeps the curve inside a band instead of letting it be "as fast as the
//! market allows".
//!
//! Gated by environment for now, because the threshold has to be calibrated
//! against your own worth data rather than invented:
//!
//! * `BAF_MAX_GAIN_PER_HOUR`  coins per hour to allow (0 or unset = no cap)
//! * `BAF_RAMP_HOURS`         hours over which the band ramps up (default 6)
//!
//! While capped, new purchases are refused but selling and listing continue, so
//! a capped session drains inventory instead of stalling.

use parking_lot::Mutex;
use std::collections::VecDeque;

/// How long a window of samples to keep. More than an hour so a one hour window
/// always has a bracketing sample.
const KEEP_MS: u64 = 90 * 60 * 1000;

fn now_ms() -> u64 {
    static START: once_cell::sync::Lazy<std::time::Instant> =
        once_cell::sync::Lazy::new(std::time::Instant::now);
    START.elapsed().as_millis() as u64
}

pub struct EarningsEnvelope {
    samples: Mutex<VecDeque<(u64, u64)>>,
    max_gain_per_hour: u64,
    ramp_hours: u64,
    started_ms: Mutex<u64>,
}

impl EarningsEnvelope {
    pub fn new(max_gain_per_hour: u64, ramp_hours: u64) -> Self {
        Self {
            samples: Mutex::new(VecDeque::new()),
            max_gain_per_hour,
            ramp_hours: ramp_hours.max(1),
            started_ms: Mutex::new(now_ms()),
        }
    }

    /// Build from the environment. Never fails; an unparseable value means off.
    pub fn from_env() -> Self {
        let cap = std::env::var("BAF_MAX_GAIN_PER_HOUR")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(0);
        let ramp = std::env::var("BAF_RAMP_HOURS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(6);
        Self::new(cap, ramp)
    }

    pub fn enabled(&self) -> bool {
        self.max_gain_per_hour > 0
    }

    pub fn cap_per_hour(&self) -> u64 {
        self.max_gain_per_hour
    }

    /// Record a worth sample (`purse + locked`). Cheap; call it every few
    /// seconds. A sample that cannot be read (scoreboard not parsed yet) should
    /// simply not be recorded.
    pub fn record(&self, worth: u64) {
        let now = now_ms();
        let mut samples = self.samples.lock();
        samples.push_back((now, worth));
        while samples
            .front()
            .is_some_and(|(t, _)| now.saturating_sub(*t) > KEEP_MS)
        {
            samples.pop_front();
        }
    }

    /// Gain over the last hour, from the oldest sample inside the window.
    /// `None` while there is no usable bracket yet (fewer than two samples, or
    /// a window shorter than the ramp wants).
    pub fn gain_last_hour(&self) -> Option<i64> {
        let samples = self.samples.lock();
        let (_, newest) = samples.back()?;
        let now = now_ms();
        // Oldest sample that is still within the hour.
        let base = samples
            .iter()
            .find(|(t, _)| now.saturating_sub(*t) <= 60 * 60 * 1000)
            .map(|(_, w)| *w)?;
        if samples.len() < 2 {
            return None;
        }
        Some(*newest as i64 - base as i64)
    }

    /// The band currently in force. Ramps linearly from 25% to 100% of the
    /// configured cap over `ramp_hours`, so a fresh profile does not earn at
    /// full rate in its first minutes, which is exactly the shape that gets
    /// flagged.
    pub fn allowed_gain_per_hour(&self) -> u64 {
        if self.max_gain_per_hour == 0 {
            return 0;
        }
        let age_ms = now_ms().saturating_sub(*self.started_ms.lock());
        let ramp_ms = self.ramp_hours.saturating_mul(60 * 60 * 1000).max(1);
        let frac = (age_ms as f64 / ramp_ms as f64).clamp(0.0, 1.0);
        let frac = 0.25 + 0.75 * frac;
        (self.max_gain_per_hour as f64 * frac) as u64
    }

    /// True when the profile has already gained more than the band allows in
    /// the last hour, i.e. new purchases must be refused.
    pub fn capped(&self) -> bool {
        if !self.enabled() {
            return false;
        }
        match self.gain_last_hour() {
            Some(gain) => gain > self.allowed_gain_per_hour() as i64,
            None => false,
        }
    }

    /// One line summary for the periodic log / the web panel.
    pub fn summary(&self) -> String {
        let gain = self
            .gain_last_hour()
            .map(|g| g.to_string())
            .unwrap_or_else(|| "?".to_string());
        format!(
            "gain/h {} of {}/h allowed (cap {}/h, ramp {}h, {} samples){}",
            gain,
            self.allowed_gain_per_hour(),
            self.max_gain_per_hour,
            self.ramp_hours,
            self.samples.lock().len(),
            if self.capped() { " CAPPED" } else { "" }
        )
    }
}

/// The process-wide envelope. `from_env()` so the cap can be set per instance
/// without a config schema change; move it into `Config` once the band is
/// calibrated.
pub static ENVELOPE: once_cell::sync::Lazy<EarningsEnvelope> =
    once_cell::sync::Lazy::new(EarningsEnvelope::from_env);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_when_cap_is_zero() {
        let e = EarningsEnvelope::new(0, 6);
        e.record(1_000);
        e.record(2_000_000);
        assert!(!e.enabled());
        assert!(!e.capped());
    }

    #[test]
    fn ramps_from_quarter_of_the_cap() {
        let e = EarningsEnvelope::new(1_000_000, 6);
        // Age zero: 25% of the cap, with a second of slack for scheduling noise.
        let band = e.allowed_gain_per_hour();
        assert!(
            (250_000..=251_000).contains(&band),
            "expected the band to start at a quarter of the cap, got {band}"
        );
    }

    #[test]
    fn gain_needs_two_samples() {
        let e = EarningsEnvelope::new(1_000_000, 6);
        assert!(e.gain_last_hour().is_none());
        e.record(100);
        assert!(e.gain_last_hour().is_none());
        e.record(250);
        assert_eq!(e.gain_last_hour(), Some(150));
    }

    #[test]
    fn caps_once_the_hourly_gain_exceeds_the_band() {
        let e = EarningsEnvelope::new(1_000_000, 6);
        e.record(0);
        e.record(400_000); // above the 250k starting band
        assert!(e.capped());
    }

    #[test]
    fn stays_open_below_the_band() {
        let e = EarningsEnvelope::new(1_000_000, 6);
        e.record(0);
        e.record(200_000); // below the 250k starting band
        assert!(!e.capped());
    }
}
