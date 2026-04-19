//! Tracks active bazaar orders for the web panel.
//!
//! Orders are added on [`BazaarOrderPlaced`] events and removed when
//! [`BazaarOrderCollected`] or [`BazaarOrderCancelled`] events fire.
//!
//! Orders and buy costs are persisted to disk so profit tracking survives
//! across bot restarts.

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, warn};

/// File name for persisted orders (stored next to the executable / in the logs dir).
const ORDERS_FILE: &str = "bazaar_orders.json";
/// File name for persisted buy costs.
const BUY_COSTS_FILE: &str = "bazaar_buy_costs.json";

/// A single tracked bazaar order visible on the web panel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackedBazaarOrder {
    pub item_name: String,
    pub amount: u64,
    pub price_per_unit: f64,
    pub is_buy_order: bool,
    /// `"open"` or `"filled"`.
    pub status: String,
    /// Unix timestamp (seconds) when the order was placed.
    pub placed_at: u64,
}

/// Thread-safe tracker for active bazaar orders.
#[derive(Clone)]
pub struct BazaarOrderTracker {
    orders: Arc<RwLock<Vec<TrackedBazaarOrder>>>,
    /// Stores (price_per_unit, amount) for all collected buy orders per item,
    /// so that profit can be computed when the corresponding sell offer is
    /// collected.  Multiple buy orders for the same item are accumulated
    /// (weighted-average PPU) instead of overwriting.
    last_buy_costs: Arc<RwLock<HashMap<String, (f64, u64)>>>,
    /// Per-item profit data from `/cofl bz l` output.
    /// Maps normalized item name → (total_profit, flip_count).
    /// Used as a fallback when local buy-cost tracking has no data for a sell.
    bz_list_profits: Arc<RwLock<HashMap<String, (i64, u32)>>>,
}

impl BazaarOrderTracker {
    pub fn new() -> Self {
        let tracker = Self {
            orders: Arc::new(RwLock::new(Vec::new())),
            last_buy_costs: Arc::new(RwLock::new(HashMap::new())),
            bz_list_profits: Arc::new(RwLock::new(HashMap::new())),
        };
        tracker.load_from_disk();
        tracker
    }

    /// Create a tracker that does NOT load from / save to disk.
    /// Used in unit tests to avoid cross-test interference.
    #[cfg(test)]
    pub fn new_in_memory() -> Self {
        Self {
            orders: Arc::new(RwLock::new(Vec::new())),
            last_buy_costs: Arc::new(RwLock::new(HashMap::new())),
            bz_list_profits: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Record a newly placed bazaar order.
    pub fn add_order(
        &self,
        item_name: String,
        amount: u64,
        price_per_unit: f64,
        is_buy_order: bool,
    ) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.orders.write().push(TrackedBazaarOrder {
            item_name,
            amount,
            price_per_unit,
            is_buy_order,
            status: "open".to_string(),
            placed_at: now,
        });
        self.save_orders_to_disk();
    }

    /// Mark the most recent matching open order as `"filled"`.
    pub fn mark_filled(&self, item_name: &str, is_buy_order: bool) {
        let mut orders = self.orders.write();
        if let Some(order) = orders.iter_mut().rev().find(|o| {
            o.status == "open"
                && o.is_buy_order == is_buy_order
                && normalize_for_match(&o.item_name) == normalize_for_match(item_name)
        }) {
            order.status = "filled".to_string();
        }
        drop(orders);
        self.save_orders_to_disk();
    }

    /// Remove a matching order (on collect or cancel) and return its data
    /// so the caller can use price/amount for profit calculation.
    pub fn remove_order(&self, item_name: &str, is_buy_order: bool) -> Option<TrackedBazaarOrder> {
        let mut orders = self.orders.write();
        let result = if let Some(pos) = orders.iter().rposition(|o| {
            (o.status == "open" || o.status == "filled")
                && o.is_buy_order == is_buy_order
                && normalize_for_match(&o.item_name) == normalize_for_match(item_name)
        }) {
            Some(orders.remove(pos))
        } else {
            None
        };
        drop(orders);
        self.save_orders_to_disk();
        result
    }

    /// Return a snapshot of all tracked orders.
    pub fn get_orders(&self) -> Vec<TrackedBazaarOrder> {
        self.orders.read().clone()
    }

    /// Remove all tracked orders and persist.  Used on startup to get a clean
    /// view since the in-game ManageOrders cycle will cancel everything.
    pub fn clear_all_orders(&self) -> usize {
        let mut orders = self.orders.write();
        let removed = orders.len();
        orders.clear();
        drop(orders);
        self.save_orders_to_disk();
        removed
    }

    /// Returns `true` if at least one tracked order has status `"filled"`.
    /// Used by the periodic ManageOrders timer to skip GUI cycles when there
    /// is nothing to collect.
    pub fn has_filled_orders(&self) -> bool {
        self.orders.read().iter().any(|o| o.status == "filled")
    }

    /// Remove orders older than `max_age_secs` seconds.
    /// Returns the number of stale orders removed.
    pub fn remove_stale_orders(&self, max_age_secs: u64) -> usize {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let mut orders = self.orders.write();
        let original_len = orders.len();
        orders.retain(|o| now.saturating_sub(o.placed_at) < max_age_secs);
        let removed = original_len - orders.len();
        drop(orders);
        if removed > 0 {
            self.save_orders_to_disk();
        }
        removed
    }

    /// Reconcile the tracker with the orders currently visible in-game.
    ///
    /// `ingame_orders` is the list of `(item_name, is_buy_order, amount, price_per_unit)`
    /// tuples taken from the Bazaar Orders window during a ManageOrders cycle.
    /// Any tracked order whose item+type does **not** appear in this list is
    /// removed so the web panel stays in sync with the actual in-game state.
    ///
    /// Orders visible in-game but NOT yet tracked (e.g. placed before the bot
    /// started, or placed manually) are added as new entries so the web panel
    /// shows all active orders from startup.
    ///
    /// Duplicate same-item orders are handled by counting occurrences: if the
    /// in-game window shows 2 "Coal" buy orders, at most 2 tracked "Coal" buy
    /// orders are kept.
    ///
    /// Returns the number of stale tracker entries removed.
    pub fn reconcile_with_ingame(&self, ingame_orders: &[(String, bool, u64, f64)]) -> usize {
        // Build a count map: (normalized_name, is_buy) → how many in-game.
        let mut ingame_counts: std::collections::HashMap<(String, bool), usize> =
            std::collections::HashMap::new();
        for (name, is_buy, _, _) in ingame_orders {
            *ingame_counts
                .entry((normalize_for_match(name), *is_buy))
                .or_insert(0) += 1;
        }
        let mut orders = self.orders.write();
        let original_len = orders.len();
        // Track how many of each (item, side) we have already kept so we
        // don't exceed the in-game count.
        let mut kept_counts: std::collections::HashMap<(String, bool), usize> =
            std::collections::HashMap::new();
        orders.retain(|o| {
            let key = (normalize_for_match(&o.item_name), o.is_buy_order);
            let allowed = ingame_counts.get(&key).copied().unwrap_or(0);
            let kept = kept_counts.entry(key).or_insert(0);
            if *kept < allowed {
                *kept += 1;
                true
            } else {
                false
            }
        });
        let removed = original_len - orders.len();

        // Add in-game orders that aren't already tracked.
        // Iterate over unique keys to avoid duplicate additions.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let mut added = 0usize;

        // Build a map from (normalized_name, is_buy) → Vec<(amount, price)>
        // so we can pick the correct data for each missing order.
        let mut ingame_data: std::collections::HashMap<(String, bool), Vec<(u64, f64)>> =
            std::collections::HashMap::new();
        for (name, is_buy, amount, price) in ingame_orders {
            ingame_data
                .entry((normalize_for_match(name), *is_buy))
                .or_default()
                .push((*amount, *price));
        }

        for (key, data_entries) in &ingame_data {
            let tracked = kept_counts.get(key).copied().unwrap_or(0);
            let needed = data_entries.len();
            for idx in tracked..needed {
                let (amount, price) = data_entries[idx];
                // Use title case for the item name from the first matching ingame order
                let display_name = ingame_orders.iter()
                    .find(|(n, b, _, _)| normalize_for_match(n) == key.0 && *b == key.1)
                    .map(|(n, _, _, _)| n.clone())
                    .unwrap_or_else(|| key.0.clone());
                orders.push(TrackedBazaarOrder {
                    item_name: display_name,
                    amount,
                    price_per_unit: price,
                    is_buy_order: key.1,
                    status: "open".to_string(),
                    placed_at: now,
                });
                added += 1;
            }
            *kept_counts.entry(key.clone()).or_insert(0) = needed;
        }

        drop(orders);
        if removed > 0 || added > 0 {
            if added > 0 {
                debug!("[BazaarTracker] Added {} in-game orders not previously tracked", added);
            }
            self.save_orders_to_disk();
        }
        removed
    }

    /// Record a collected buy order's cost so that profit can be computed
    /// when the corresponding sell offer for the same item is collected.
    ///
    /// If a buy cost already exists for this item (e.g. two buy orders filled
    /// before a single sell offer is collected), the amounts are accumulated
    /// and the price-per-unit is recomputed as a weighted average so that the
    /// profit calculation accounts for **all** purchased units, not just the
    /// last batch.
    pub fn record_buy_cost(&self, item_name: &str, price_per_unit: f64, amount: u64) {
        let key = normalize_for_match(item_name);
        let mut costs = self.last_buy_costs.write();
        let entry = costs.entry(key).or_insert((0.0, 0));
        let old_total_cost = entry.0 * entry.1 as f64;
        let new_total_cost = price_per_unit * amount as f64;
        let combined_amount = entry.1 + amount;
        entry.0 = if combined_amount > 0 {
            (old_total_cost + new_total_cost) / combined_amount as f64
        } else {
            0.0
        };
        entry.1 = combined_amount;
        drop(costs);
        self.save_buy_costs_to_disk();
    }

    /// Consume and return the stored buy cost for an item (if any).
    /// Used when a sell offer is collected to compute profit/loss.
    pub fn take_buy_cost(&self, item_name: &str) -> Option<(f64, u64)> {
        let result = self.last_buy_costs
            .write()
            .remove(&normalize_for_match(item_name));
        self.save_buy_costs_to_disk();
        result
    }

    /// Replace the per-item profit map with data parsed from `/cofl bz l`.
    /// Called after collecting all flip lines from a single `/cofl bz l` response.
    pub fn set_bz_list_profits(&self, items: HashMap<String, (i64, u32)>) {
        let normalized: HashMap<String, (i64, u32)> = items
            .into_iter()
            .map(|(k, v)| (normalize_for_match(&k), v))
            .collect();
        *self.bz_list_profits.write() = normalized;
    }

    /// Return the total profit for an item from the latest `/cofl bz l` data.
    /// Used as a fallback when local buy-cost tracking has no data for a sell.
    /// Returns the profit exactly as shown in the `/cofl bz l` list for that item.
    pub fn get_bz_list_profit(&self, item_name: &str) -> Option<i64> {
        let key = normalize_for_match(item_name);
        let data = self.bz_list_profits.read();
        data.get(&key).map(|(total, _count)| *total)
    }

    // ── Persistence helpers ──

    fn persistence_dir() -> std::path::PathBuf {
        crate::logging::get_logs_dir()
    }

    fn save_orders_to_disk(&self) {
        #[cfg(test)]
        return;
        #[cfg(not(test))]
        {
        let orders = self.orders.read().clone();
        let path = Self::persistence_dir().join(ORDERS_FILE);
        if let Err(e) = std::fs::create_dir_all(Self::persistence_dir()) {
            warn!("[BazaarTracker] Failed to create persistence dir: {}", e);
            return;
        }
        match serde_json::to_string(&orders) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&path, json) {
                    warn!("[BazaarTracker] Failed to write {}: {}", path.display(), e);
                }
            }
            Err(e) => warn!("[BazaarTracker] Failed to serialize orders: {}", e),
        }
        }
    }

    fn save_buy_costs_to_disk(&self) {
        #[cfg(test)]
        return;
        #[cfg(not(test))]
        {
        let costs = self.last_buy_costs.read().clone();
        let path = Self::persistence_dir().join(BUY_COSTS_FILE);
        if let Err(e) = std::fs::create_dir_all(Self::persistence_dir()) {
            warn!("[BazaarTracker] Failed to create persistence dir: {}", e);
            return;
        }
        match serde_json::to_string(&costs) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&path, json) {
                    warn!("[BazaarTracker] Failed to write {}: {}", path.display(), e);
                }
            }
            Err(e) => warn!("[BazaarTracker] Failed to serialize buy costs: {}", e),
        }
        }
    }

    fn load_from_disk(&self) {
        let orders_path = Self::persistence_dir().join(ORDERS_FILE);
        if orders_path.exists() {
            match std::fs::read_to_string(&orders_path) {
                Ok(json) => match serde_json::from_str::<Vec<TrackedBazaarOrder>>(&json) {
                    Ok(orders) => {
                        debug!("[BazaarTracker] Loaded {} orders from disk", orders.len());
                        *self.orders.write() = orders;
                    }
                    Err(e) => warn!("[BazaarTracker] Failed to parse {}: {}", orders_path.display(), e),
                },
                Err(e) => warn!("[BazaarTracker] Failed to read {}: {}", orders_path.display(), e),
            }
        }
        let costs_path = Self::persistence_dir().join(BUY_COSTS_FILE);
        if costs_path.exists() {
            match std::fs::read_to_string(&costs_path) {
                Ok(json) => match serde_json::from_str::<HashMap<String, (f64, u64)>>(&json) {
                    Ok(costs) => {
                        debug!("[BazaarTracker] Loaded {} buy costs from disk", costs.len());
                        *self.last_buy_costs.write() = costs;
                    }
                    Err(e) => warn!("[BazaarTracker] Failed to parse {}: {}", costs_path.display(), e),
                },
                Err(e) => warn!("[BazaarTracker] Failed to read {}: {}", costs_path.display(), e),
            }
        }
    }
}

fn normalize_for_match(name: &str) -> String {
    name.to_lowercase().trim().to_string()
}

/// Public wrapper for `normalize_for_match` — used by `ManageOrders` targeted cancel.
pub fn normalize_for_match_pub(name: &str) -> String {
    normalize_for_match(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_and_remove_order() {
        let tracker = BazaarOrderTracker::new_in_memory();
        tracker.add_order("Enchanted Coal Block".into(), 4, 30100.0, false);
        assert_eq!(tracker.get_orders().len(), 1);

        let removed = tracker.remove_order("Enchanted Coal Block", false);
        assert!(removed.is_some());
        assert_eq!(removed.unwrap().amount, 4);
        assert_eq!(tracker.get_orders().len(), 0);
    }

    #[test]
    fn mark_filled() {
        let tracker = BazaarOrderTracker::new_in_memory();
        tracker.add_order("Diamond".into(), 64, 100.0, true);
        assert!(!tracker.has_filled_orders());
        tracker.mark_filled("Diamond", true);
        assert_eq!(tracker.get_orders()[0].status, "filled");
        assert!(tracker.has_filled_orders());
    }

    #[test]
    fn has_filled_orders_empty() {
        let tracker = BazaarOrderTracker::new_in_memory();
        assert!(!tracker.has_filled_orders());
    }

    #[test]
    fn has_filled_orders_cleared_on_remove() {
        let tracker = BazaarOrderTracker::new_in_memory();
        tracker.add_order("Coal".into(), 10, 500.0, true);
        tracker.mark_filled("Coal", true);
        assert!(tracker.has_filled_orders());
        tracker.remove_order("Coal", true);
        assert!(!tracker.has_filled_orders());
    }

    #[test]
    fn remove_filled_order() {
        let tracker = BazaarOrderTracker::new_in_memory();
        tracker.add_order("Diamond".into(), 64, 100.0, true);
        tracker.mark_filled("Diamond", true);
        let removed = tracker.remove_order("Diamond", true);
        assert!(removed.is_some());
        assert_eq!(tracker.get_orders().len(), 0);
    }

    #[test]
    fn case_insensitive_match() {
        let tracker = BazaarOrderTracker::new_in_memory();
        tracker.add_order("Enchanted Coal Block".into(), 4, 30100.0, false);
        let removed = tracker.remove_order("enchanted coal block", false);
        assert!(removed.is_some());
        assert_eq!(tracker.get_orders().len(), 0);
    }

    #[test]
    fn remove_returns_none_for_missing() {
        let tracker = BazaarOrderTracker::new_in_memory();
        assert!(tracker.remove_order("Nonexistent", true).is_none());
    }

    #[test]
    fn remove_stale_orders() {
        let tracker = BazaarOrderTracker::new_in_memory();
        // Manually insert an order with a very old timestamp
        {
            let mut orders = tracker.orders.write();
            orders.push(TrackedBazaarOrder {
                item_name: "Old Item".into(),
                amount: 10,
                price_per_unit: 100.0,
                is_buy_order: true,
                status: "open".into(),
                placed_at: 1000, // ancient timestamp
            });
        }
        // Also add a fresh order normally
        tracker.add_order("Fresh Item".into(), 5, 200.0, false);

        let removed = tracker.remove_stale_orders(3600); // 1 hour max age
        assert_eq!(removed, 1);
        let remaining = tracker.get_orders();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].item_name, "Fresh Item");
    }

    #[test]
    fn profit_calculation_from_removed_orders() {
        let tracker = BazaarOrderTracker::new_in_memory();
        tracker.add_order("Coal".into(), 10, 500.0, true);
        tracker.add_order("Coal".into(), 10, 600.0, false);

        let buy = tracker.remove_order("Coal", true).unwrap();
        let sell = tracker.remove_order("Coal", false).unwrap();
        let profit = (sell.price_per_unit * sell.amount as f64)
            - (buy.price_per_unit * buy.amount as f64);
        assert_eq!(profit, 1000.0);
    }

    #[test]
    fn record_and_take_buy_cost() {
        let tracker = BazaarOrderTracker::new_in_memory();
        tracker.record_buy_cost("Enchanted Coal Block", 500.0, 10);

        let cost = tracker.take_buy_cost("Enchanted Coal Block");
        assert!(cost.is_some());
        let (ppu, amt) = cost.unwrap();
        assert_eq!(ppu, 500.0);
        assert_eq!(amt, 10);

        // Second take returns None (consumed).
        assert!(tracker.take_buy_cost("Enchanted Coal Block").is_none());
    }

    #[test]
    fn take_buy_cost_case_insensitive() {
        let tracker = BazaarOrderTracker::new_in_memory();
        tracker.record_buy_cost("Enchanted Coal Block", 500.0, 10);
        assert!(tracker.take_buy_cost("enchanted coal block").is_some());
    }

    #[test]
    fn take_buy_cost_returns_none_when_missing() {
        let tracker = BazaarOrderTracker::new_in_memory();
        assert!(tracker.take_buy_cost("Nonexistent").is_none());
    }

    #[test]
    fn sell_profit_from_recorded_buy_cost() {
        let tracker = BazaarOrderTracker::new_in_memory();
        // Simulate buy order collected: 10x Coal @ 500 coins/unit
        tracker.record_buy_cost("Coal", 500.0, 10);
        // Simulate sell offer collected: 10x Coal @ 600 coins/unit
        let sell_ppu = 600.0;
        let sell_amount = 10u64;
        let (buy_ppu, buy_amount) = tracker.take_buy_cost("Coal").unwrap();
        let profit = (sell_ppu * sell_amount as f64) - (buy_ppu * buy_amount as f64);
        assert_eq!(profit, 1000.0);
    }

    #[test]
    fn multiple_buy_orders_accumulate_cost() {
        let tracker = BazaarOrderTracker::new_in_memory();
        // Two buy orders for the same item collected before the sell
        tracker.record_buy_cost("Coal", 500.0, 10);
        tracker.record_buy_cost("Coal", 500.0, 10);
        // Sell 20x Coal @ 600 coins/unit
        let (buy_ppu, buy_amount) = tracker.take_buy_cost("Coal").unwrap();
        assert_eq!(buy_amount, 20);
        assert!((buy_ppu - 500.0).abs() < 0.01);
        let sell_total = 600.0 * 20.0;
        let buy_total = buy_ppu * buy_amount as f64;
        let profit = sell_total - buy_total;
        // Expected: (600*20) - (500*20) = 12000 - 10000 = 2000
        assert_eq!(profit, 2000.0);
    }

    #[test]
    fn multiple_buy_orders_weighted_average() {
        let tracker = BazaarOrderTracker::new_in_memory();
        // Buy 10 @ 500/unit, then 10 @ 510/unit
        tracker.record_buy_cost("Diamond", 500.0, 10);
        tracker.record_buy_cost("Diamond", 510.0, 10);
        let (buy_ppu, buy_amount) = tracker.take_buy_cost("Diamond").unwrap();
        assert_eq!(buy_amount, 20);
        // Weighted avg = (500*10 + 510*10) / 20 = 10100 / 20 = 505
        assert!((buy_ppu - 505.0).abs() < 0.01);
    }

    #[test]
    fn reconcile_removes_stale_orders() {
        let tracker = BazaarOrderTracker::new_in_memory();
        tracker.add_order("Coal".into(), 10, 500.0, true);
        tracker.add_order("Diamond".into(), 5, 1000.0, false);
        tracker.add_order("Iron Ingot".into(), 64, 50.0, true);
        assert_eq!(tracker.get_orders().len(), 3);

        // In-game only has Coal BUY and Diamond SELL — Iron Ingot is stale
        let ingame = vec![
            ("Coal".to_string(), true, 10, 500.0),
            ("Diamond".to_string(), false, 5, 1000.0),
        ];
        let removed = tracker.reconcile_with_ingame(&ingame);
        assert_eq!(removed, 1);
        let remaining = tracker.get_orders();
        assert_eq!(remaining.len(), 2);
        assert!(remaining.iter().any(|o| o.item_name == "Coal" && o.is_buy_order));
        assert!(remaining.iter().any(|o| o.item_name == "Diamond" && !o.is_buy_order));
    }

    #[test]
    fn reconcile_case_insensitive() {
        let tracker = BazaarOrderTracker::new_in_memory();
        tracker.add_order("Enchanted Coal Block".into(), 4, 30100.0, false);
        let ingame = vec![("enchanted coal block".to_string(), false, 4, 30100.0)];
        let removed = tracker.reconcile_with_ingame(&ingame);
        assert_eq!(removed, 0);
        assert_eq!(tracker.get_orders().len(), 1);
    }

    #[test]
    fn reconcile_duplicate_same_item_orders() {
        let tracker = BazaarOrderTracker::new_in_memory();
        // Tracker has 3 "Coal" buy orders
        tracker.add_order("Coal".into(), 10, 500.0, true);
        tracker.add_order("Coal".into(), 20, 510.0, true);
        tracker.add_order("Coal".into(), 30, 520.0, true);
        assert_eq!(tracker.get_orders().len(), 3);

        // In-game only has 1 "Coal" buy order (2 were cancelled externally)
        let ingame = vec![("Coal".to_string(), true, 10, 500.0)];
        let removed = tracker.reconcile_with_ingame(&ingame);
        assert_eq!(removed, 2);
        assert_eq!(tracker.get_orders().len(), 1);
    }

    #[test]
    fn reconcile_keeps_correct_count_of_duplicates() {
        let tracker = BazaarOrderTracker::new_in_memory();
        // Tracker has 2 "Coal" buy orders and 1 "Diamond" sell order
        tracker.add_order("Coal".into(), 10, 500.0, true);
        tracker.add_order("Coal".into(), 20, 510.0, true);
        tracker.add_order("Diamond".into(), 5, 1000.0, false);
        assert_eq!(tracker.get_orders().len(), 3);

        // In-game has 2 "Coal" buy orders and 1 "Diamond" sell order
        let ingame = vec![
            ("Coal".to_string(), true, 10, 500.0),
            ("Coal".to_string(), true, 20, 510.0),
            ("Diamond".to_string(), false, 5, 1000.0),
        ];
        let removed = tracker.reconcile_with_ingame(&ingame);
        assert_eq!(removed, 0);
        assert_eq!(tracker.get_orders().len(), 3);
    }

    #[test]
    fn reconcile_adds_new_orders_with_correct_data() {
        let tracker = BazaarOrderTracker::new_in_memory();
        // Empty tracker, in-game has 2 orders
        let ingame = vec![
            ("Coal".to_string(), true, 64, 500.0),
            ("Diamond".to_string(), false, 10, 1200.5),
        ];
        let removed = tracker.reconcile_with_ingame(&ingame);
        assert_eq!(removed, 0);
        let orders = tracker.get_orders();
        assert_eq!(orders.len(), 2);

        let coal = orders.iter().find(|o| o.item_name == "Coal").unwrap();
        assert_eq!(coal.amount, 64);
        assert!((coal.price_per_unit - 500.0).abs() < 0.01);
        assert!(coal.is_buy_order);

        let diamond = orders.iter().find(|o| o.item_name == "Diamond").unwrap();
        assert_eq!(diamond.amount, 10);
        assert!((diamond.price_per_unit - 1200.5).abs() < 0.01);
        assert!(!diamond.is_buy_order);
    }

    #[test]
    fn bz_list_profit_single_flip() {
        let tracker = BazaarOrderTracker::new_in_memory();
        let mut items = HashMap::new();
        items.insert("Worm Membrane".to_string(), (100_000i64, 1u32));
        tracker.set_bz_list_profits(items);
        assert_eq!(tracker.get_bz_list_profit("Worm Membrane"), Some(100_000));
    }

    #[test]
    fn bz_list_profit_multiple_flips_returns_total() {
        let tracker = BazaarOrderTracker::new_in_memory();
        let mut items = HashMap::new();
        items.insert("Worm Membrane".to_string(), (741_000i64, 7u32));
        tracker.set_bz_list_profits(items);
        // Returns total profit, not per-flip average
        assert_eq!(tracker.get_bz_list_profit("Worm Membrane"), Some(741_000));
    }

    #[test]
    fn bz_list_profit_case_insensitive() {
        let tracker = BazaarOrderTracker::new_in_memory();
        let mut items = HashMap::new();
        items.insert("Enchanted Coal Block".to_string(), (50_000i64, 2u32));
        tracker.set_bz_list_profits(items);
        assert_eq!(tracker.get_bz_list_profit("enchanted coal block"), Some(50_000));
    }

    #[test]
    fn bz_list_profit_missing_item() {
        let tracker = BazaarOrderTracker::new_in_memory();
        assert!(tracker.get_bz_list_profit("Nonexistent").is_none());
    }

    #[test]
    fn bz_list_profit_zero_count_still_returns_total() {
        let tracker = BazaarOrderTracker::new_in_memory();
        let mut items = HashMap::new();
        items.insert("Coal".to_string(), (50_000i64, 0u32));
        tracker.set_bz_list_profits(items);
        assert_eq!(tracker.get_bz_list_profit("Coal"), Some(50_000));
    }

    #[test]
    fn bz_list_profits_replaced_on_new_set() {
        let tracker = BazaarOrderTracker::new_in_memory();
        let mut items1 = HashMap::new();
        items1.insert("Coal".to_string(), (10_000i64, 1u32));
        tracker.set_bz_list_profits(items1);
        assert!(tracker.get_bz_list_profit("Coal").is_some());

        // Second set replaces all data
        let mut items2 = HashMap::new();
        items2.insert("Diamond".to_string(), (20_000i64, 2u32));
        tracker.set_bz_list_profits(items2);
        assert!(tracker.get_bz_list_profit("Coal").is_none());
        assert_eq!(tracker.get_bz_list_profit("Diamond"), Some(20_000));
    }
}
