use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// A single profit data point: (unix_timestamp_secs, cumulative_profit_coins)
pub type ProfitPoint = (u64, i64);

/// Maximum number of points kept per series.  `set_ah_total`/`set_bz_total` are
/// called on every periodic `/cofl profit` and `/cofl bz l` poll, so without a
/// cap these vectors grow unbounded for the lifetime of the process (and are
/// cloned into every `/api/profit` response and OG image).  When the cap is hit
/// the series is downsampled, which bounds memory while preserving chart shape.
const MAX_POINTS: usize = 5000;

/// Append `point` to `points`, downsampling older points when the series grows
/// past [`MAX_POINTS`].  Always keeps the first (baseline) and last (latest)
/// points; drops every other point in between.
fn push_point(points: &mut Vec<ProfitPoint>, point: ProfitPoint) {
    points.push(point);
    if points.len() <= MAX_POINTS {
        return;
    }
    let first = points[0];
    let last = *points.last().expect("points is non-empty after push");
    let mut decimated: Vec<ProfitPoint> = Vec::with_capacity(points.len() / 2 + 2);
    decimated.push(first);
    let mut i = 1;
    while i + 1 < points.len() {
        decimated.push(points[i]);
        i += 2;
    }
    decimated.push(last);
    *points = decimated;
}

/// Thread-safe profit tracker for AH and Bazaar realized profits.
pub struct ProfitTracker {
    inner: Mutex<ProfitTrackerInner>,
}

struct ProfitTrackerInner {
    ah_points: Vec<ProfitPoint>,
    bz_points: Vec<ProfitPoint>,
    ah_total: i64,
    bz_total: i64,
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

impl ProfitTracker {
    pub fn new() -> Self {
        let now = now_unix();
        Self {
            inner: Mutex::new(ProfitTrackerInner {
                ah_points: vec![(now, 0)],
                bz_points: vec![(now, 0)],
                ah_total: 0,
                bz_total: 0,
            }),
        }
    }

    /// Record a realized AH profit (positive or negative).
    pub fn record_ah_profit(&self, profit: i64) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.ah_total += profit;
            let total = inner.ah_total;
            push_point(&mut inner.ah_points, (now_unix(), total));
        }
    }

    /// Replace the AH total with an authoritative value (e.g. from Coflnet
    /// `/cofl profit`) and record a new data-point so the chart updates.
    pub fn set_ah_total(&self, total: i64) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.ah_total = total;
            push_point(&mut inner.ah_points, (now_unix(), total));
        }
    }

    /// Record a realized Bazaar profit (positive or negative).
    pub fn record_bz_profit(&self, profit: i64) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.bz_total += profit;
            let total = inner.bz_total;
            push_point(&mut inner.bz_points, (now_unix(), total));
        }
    }

    /// Replace the BZ total with an authoritative value (e.g. from `/cofl bz l`
    /// accumulated profit) and record a new data-point so the chart updates.
    pub fn set_bz_total(&self, total: i64) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.bz_total = total;
            push_point(&mut inner.bz_points, (now_unix(), total));
        }
    }

    /// Get all AH profit data points.
    pub fn ah_points(&self) -> Vec<ProfitPoint> {
        self.inner
            .lock()
            .map(|i| i.ah_points.clone())
            .unwrap_or_default()
    }

    /// Get all Bazaar profit data points.
    pub fn bz_points(&self) -> Vec<ProfitPoint> {
        self.inner
            .lock()
            .map(|i| i.bz_points.clone())
            .unwrap_or_default()
    }

    /// Get totals: (ah_total, bz_total)
    pub fn totals(&self) -> (i64, i64) {
        self.inner
            .lock()
            .map(|i| (i.ah_total, i.bz_total))
            .unwrap_or((0, 0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn points_are_bounded() {
        let tracker = ProfitTracker::new();
        // Far exceed the cap to force multiple downsampling passes.
        for _ in 0..(MAX_POINTS * 3) {
            tracker.record_ah_profit(1);
        }
        let points = tracker.ah_points();
        assert!(points.len() <= MAX_POINTS, "points should stay bounded, got {}", points.len());
        // The running total must remain correct despite downsampling.
        assert_eq!(tracker.totals().0, (MAX_POINTS * 3) as i64);
    }

    #[test]
    fn push_point_keeps_first_and_last() {
        let mut pts: Vec<ProfitPoint> = Vec::new();
        for i in 0..(MAX_POINTS + 10) {
            push_point(&mut pts, (i as u64, i as i64));
        }
        assert!(pts.len() <= MAX_POINTS);
        assert_eq!(pts.first().unwrap().0, 0);
        assert_eq!(pts.last().unwrap().0, (MAX_POINTS + 9) as u64);
    }
}
