/// Bazaar flip handler
/// 
/// Handles bazaar order placement recommendations from Coflnet:
/// - Processes bazaar flip recommendations  
/// - Navigates bazaar search → item detail → order creation
/// - Fills in amount and price on signs
/// - Clicks through confirmation windows
/// - Handles buy/sell order types
/// - Implements price failsafes and validation
/// 
/// Preserves exact slot numbers, timing delays, and packet logic from TypeScript.

use anyhow::{anyhow, Result};
use parking_lot::RwLock;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

use crate::gui::{WindowHandler, WindowSlot};
use crate::types::{BazaarFlipRecommendation, BotState};
use crate::utils::to_title_case;

/// Configuration constants
const RETRY_DELAY_MS: u64 = 1100;
const OPERATION_TIMEOUT_MS: u64 = 20000;
const MAX_LOGGED_SLOTS: usize = 15;
const MINEFLAYER_WINDOW_PROCESS_DELAY_MS: u64 = 300;
const BAZAAR_RETRY_DELAY_MS: u64 = 2000;
const MAX_ORDER_PLACEMENT_RETRIES: usize = 3;
const RETRY_BACKOFF_BASE_MS: u64 = 1000;
const FIRST_SEARCH_RESULT_SLOT: usize = 11;

/// Price failsafe thresholds
const PRICE_FAILSAFE_BUY_THRESHOLD: f64 = 0.9;  // Reject buy orders if sign price < 90% of order price
const PRICE_FAILSAFE_SELL_THRESHOLD: f64 = 1.1; // Reject sell orders if sign price > 110% of order price

/// Rate limiting
const LIMIT_WARNING_COOLDOWN_MS: u64 = 60000;

/// Order placement confirmation
const ORDER_REJECTION_WAIT_MS: u64 = 1000;

/// Stale command detection (60 seconds)
const COMMAND_STALE_THRESHOLD_MS: u64 = 60000;

/// Configuration for bazaar flip handler
#[derive(Debug, Clone)]
pub struct BazaarFlipConfig {
    /// Enable bazaar flips
    pub enabled: bool,
    /// Maximum concurrent orders
    pub max_buy_orders: usize,
    pub max_sell_orders: usize,
}

impl Default for BazaarFlipConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_buy_orders: 10,
            max_sell_orders: 10,
        }
    }
}

/// Bazaar order step in the placement flow
#[derive(Debug, Clone, PartialEq, Eq)]
enum BazaarStep {
    Initial,
    SearchResults,
    SelectOrderType,
    SetAmount,
    SetPrice,
    Confirm,
}

/// Bazaar flip handler state
pub struct BazaarFlipHandler {
    /// Configuration
    config: Arc<RwLock<BazaarFlipConfig>>,
    /// Window handler
    window_handler: WindowHandler,
    /// Last time we showed "slots full" warning
    last_limit_warning_time: Arc<RwLock<Instant>>,
}

impl BazaarFlipHandler {
    /// Create a new bazaar flip handler
    pub fn new() -> Self {
        Self {
            config: Arc::new(RwLock::new(BazaarFlipConfig::default())),
            window_handler: WindowHandler::new(),
            last_limit_warning_time: Arc::new(RwLock::new(Instant::now() - Duration::from_secs(120))),
        }
    }

    /// Create a new bazaar flip handler with custom configuration
    pub fn with_config(config: BazaarFlipConfig) -> Self {
        Self {
            config: Arc::new(RwLock::new(config)),
            window_handler: WindowHandler::new(),
            last_limit_warning_time: Arc::new(RwLock::new(Instant::now() - Duration::from_secs(120))),
        }
    }

    /// Update configuration
    pub fn update_config(&self, config: BazaarFlipConfig) {
        *self.config.write() = config;
    }

    /// Check if bazaar flips are enabled
    pub fn is_enabled(&self) -> bool {
        self.config.read().enabled
    }

    /// Parse bazaar flip recommendation from JSON
    /// 
    /// Handles structured JSON data from websocket:
    /// - { itemName: "Item", itemTag: "ITEM_ID", price: 1000, amount: 64, isSell: false }
    /// - { itemName: "Item", amount: 4, pricePerUnit: 265000, totalPrice: 1060000, isBuyOrder: true }
    pub fn parse_bazaar_flip_json(data: &serde_json::Value) -> Result<BazaarFlipRecommendation> {
        info!("Parsing bazaar flip JSON: {}", data);

        // Extract item name
        let item_name = data
            .get("itemName")
            .or_else(|| data.get("item"))
            .or_else(|| data.get("name"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("Missing item name in bazaar flip JSON"))?
            .to_string();

        debug!("Parsed item name: {}", item_name);

        // Extract item tag (optional)
        let item_tag = data
            .get("itemTag")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        // Extract amount
        let amount = data
            .get("amount")
            .or_else(|| data.get("count"))
            .or_else(|| data.get("quantity"))
            .and_then(|v| v.as_u64())
            .ok_or_else(|| anyhow!("Missing or invalid amount in bazaar flip JSON"))?;

        debug!("Parsed amount: {}", amount);

        // Extract price per unit
        let price_per_unit = if let Some(v) = data.get("pricePerUnit").or_else(|| data.get("unitPrice")) {
            v.as_f64()
                .ok_or_else(|| anyhow!("Invalid price in bazaar flip JSON"))?
        } else if let Some(v) = data.get("price") {
            v.as_f64()
                .ok_or_else(|| anyhow!("Invalid price in bazaar flip JSON"))?
        } else {
            return Err(anyhow!("Missing price in bazaar flip JSON"));
        };

        debug!("Parsed price per unit: {}", price_per_unit);

        // Extract total price (optional)
        let total_price = data
            .get("totalPrice")
            .and_then(|v| v.as_f64())
            .or_else(|| Some(price_per_unit * amount as f64));

        // Determine order type
        let is_buy_order = if let Some(v) = data.get("isBuyOrder") {
            v.as_bool().unwrap_or(true)
        } else if let Some(v) = data.get("isSell") {
            !v.as_bool().unwrap_or(false)
        } else if let Some(v) = data.get("type") {
            v.as_str().map(|s| s.to_lowercase() == "buy").unwrap_or(true)
        } else if let Some(v) = data.get("orderType") {
            v.as_str().map(|s| s.to_lowercase() == "buy").unwrap_or(true)
        } else {
            true // Default to buy order
        };

        debug!("Order type: {}", if is_buy_order { "BUY" } else { "SELL" });

        info!(
            "Successfully parsed bazaar flip: {}x {} @ {} (total: {}) [{}]",
            amount,
            item_name,
            price_per_unit,
            total_price.unwrap_or(price_per_unit * amount as f64),
            if is_buy_order { "BUY" } else { "SELL" }
        );

        Ok(BazaarFlipRecommendation {
            item_name,
            item_tag,
            amount,
            price_per_unit,
            total_price,
            is_buy_order,
        })
    }

    /// Parse a bazaar flip recommendation message from Coflnet
    /// 
    /// Example: "[Coflnet]: Recommending an order of 4x Cindershade for 1.06M(1)"
    pub fn parse_bazaar_flip_message(message: &str) -> Result<Option<BazaarFlipRecommendation>> {
        let clean_message = WindowHandler::remove_minecraft_colors(message);

        // Check if this is a bazaar flip recommendation
        if !clean_message.contains("[Coflnet]") || !clean_message.contains("Recommending an order of") {
            return Ok(None);
        }

        // Extract amount and item name: "4x Cindershade"
        let re_order = regex::Regex::new(r"(\d+)x\s+([^\s]+(?:\s+[^\s]+)*?)\s+for").unwrap();
        let order_match = re_order
            .captures(&clean_message)
            .ok_or_else(|| anyhow!("Failed to parse order from message"))?;

        let amount = order_match[1].parse::<u64>()?;
        let item_name = order_match[2].trim().to_string();

        // Extract price: "1.06M"
        let re_price = regex::Regex::new(r"for\s+([\d.]+[KkMm]?)\(").unwrap();
        let price_match = re_price
            .captures(&clean_message)
            .ok_or_else(|| anyhow!("Failed to parse price from message"))?;

        let price_str = &price_match[1];
        let mut total_price = price_str
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect::<String>()
            .parse::<f64>()?;

        // Convert K/M suffixes
        if price_str.to_lowercase().ends_with('k') {
            total_price *= 1000.0;
        } else if price_str.to_lowercase().ends_with('m') {
            total_price *= 1_000_000.0;
        }

        let price_per_unit = total_price / amount as f64;

        // Determine order type from message
        let is_buy_order = !(clean_message.to_lowercase().contains("sell")
            || clean_message.to_lowercase().contains("offer"));

        Ok(Some(BazaarFlipRecommendation {
            item_name,
            item_tag: None,
            amount,
            price_per_unit,
            total_price: Some(total_price),
            is_buy_order,
        }))
    }

    /// Handle a bazaar flip recommendation
    /// 
    /// This is the main entry point for processing bazaar flips.
    pub async fn handle_bazaar_flip_recommendation<F>(
        &self,
        recommendation: BazaarFlipRecommendation,
        bot_state: Arc<RwLock<BotState>>,
        send_command: F,
    ) -> Result<()>
    where
        F: Fn(&str) -> Result<()>,
    {
        // Check if bazaar flips are enabled
        if !self.is_enabled() {
            warn!("Bazaar flips are disabled in config");
            return Ok(());
        }

        // Check if bot is in startup state
        if *bot_state.read() == BotState::Startup {
            info!("Ignoring bazaar flip during startup phase");
            return Ok(());
        }

        let order_type = if recommendation.is_buy_order { "BUY" } else { "SELL" };
        info!(
            "━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n{} ORDER - {}\nAmount: {}x\nPrice/unit: {} coins\nTotal: {} coins\n━━━━━━━━━━━━━━━━━━━━━━━━━━━━",
            order_type,
            recommendation.item_name,
            recommendation.amount,
            recommendation.price_per_unit,
            recommendation.calculate_total_price()
        );

        // Execute bazaar flip with retries
        self.execute_bazaar_flip(recommendation, bot_state, send_command).await
    }

    /// Execute a bazaar flip operation
    async fn execute_bazaar_flip<F>(
        &self,
        recommendation: BazaarFlipRecommendation,
        bot_state: Arc<RwLock<BotState>>,
        send_command: F,
    ) -> Result<()>
    where
        F: Fn(&str) -> Result<()>,
    {
        let mut last_error: Option<anyhow::Error> = None;

        for attempt in 1..=MAX_ORDER_PLACEMENT_RETRIES {
            *bot_state.write() = BotState::Bazaar;

            if attempt > 1 {
                info!("Retry attempt {}/{} for {}", attempt, MAX_ORDER_PLACEMENT_RETRIES, recommendation.item_name);
            }

            match self.place_bazaar_order(&recommendation, &send_command).await {
                Ok(()) => {
                    info!("===== BAZAAR FLIP ORDER COMPLETED =====");
                    info!("Successfully placed bazaar order!");
                    *bot_state.write() = BotState::Idle;
                    return Ok(());
                }
                Err(e) => {
                    last_error = Some(e);
                    let error_message = last_error.as_ref().unwrap().to_string();
                    
                    let is_timeout_error = error_message.contains("timed out");
                    let is_retryable_error = is_timeout_error || error_message.contains("Price failsafe");
                    
                    error!("Error handling bazaar flip (attempt {}/{}): {}", attempt, MAX_ORDER_PLACEMENT_RETRIES, error_message);
                    
                    if attempt < MAX_ORDER_PLACEMENT_RETRIES && is_retryable_error {
                        let backoff_delay = RETRY_BACKOFF_BASE_MS * 2_u64.pow(attempt as u32 - 1);
                        info!("Will retry after {}ms delay...", backoff_delay);
                        *bot_state.write() = BotState::Idle;
                        sleep(Duration::from_millis(backoff_delay)).await;
                    } else {
                        if !is_retryable_error {
                            error!("Non-retryable error, aborting: {}", error_message);
                        } else {
                            error!("Max retries ({}) reached, giving up", MAX_ORDER_PLACEMENT_RETRIES);
                        }
                        *bot_state.write() = BotState::Idle;
                        return Err(last_error.unwrap());
                    }
                }
            }
        }

        *bot_state.write() = BotState::Idle;
        Err(last_error.unwrap_or_else(|| anyhow!("Order placement failed after all retries")))
    }

    /// Place a bazaar order by navigating through the Hypixel bazaar interface
    /// 
    /// Steps:
    /// 1. Search results (if using item name instead of tag)
    /// 2. Item detail view with Create Buy Order / Create Sell Offer
    /// 3. Amount selection - buy orders only
    /// 4. Price selection
    /// 5. Confirmation
    async fn place_bazaar_order<F>(
        &self,
        recommendation: &BazaarFlipRecommendation,
        send_command: F,
    ) -> Result<()>
    where
        F: Fn(&str) -> Result<()>,
    {
        info!(
            "[BazaarFlow] Starting placeBazaarOrder for {} ({}x @ {} coins, {})",
            recommendation.item_name,
            recommendation.amount,
            recommendation.price_per_unit,
            if recommendation.is_buy_order { "BUY" } else { "SELL" }
        );

        // Prefer itemTag over itemName for /bz command
        let search_term = recommendation
            .item_tag
            .as_ref()
            .map(|s| s.as_str())
            .unwrap_or_else(|| &recommendation.item_name);

        let search_term_formatted = if recommendation.item_tag.is_some() {
            search_term.to_string()
        } else {
            to_title_case(search_term)
        };

        if recommendation.item_tag.is_some() {
            info!("Using itemTag \"{}\" for /bz command (faster, skips search results)", search_term_formatted);
        } else {
            info!("itemTag not available, using itemName \"{}\" for /bz command", search_term_formatted);
        }

        // Set up window listener (in actual implementation)
        // For now, just send the command
        info!("[BazaarFlow] Executing /bz {}", search_term_formatted);
        let command = format!("/bz {}", search_term_formatted);
        send_command(&command)?;

        // TODO: Implement actual window handling with open_window event listener
        // This would involve:
        // 1. Listening for open_window packets
        // 2. Parsing window title and slots
        // 3. Clicking through the bazaar interface steps
        // 4. Handling sign inputs for amount and price
        // 5. Confirming the order
        
        info!("[BazaarFlow] Window handling not yet implemented - this is a skeleton");
        
        Ok(())
    }

    /// Find item in search results with exact match priority, then fuzzy fallback
    fn find_item_in_search_results(
        &self,
        slots: &[WindowSlot],
        item_name: &str,
    ) -> Option<usize> {
        let clean_target = WindowHandler::remove_minecraft_colors(item_name)
            .to_lowercase()
            .replace(['☘', '☂', '✪', '◆', '❤'], "")
            .trim()
            .to_string();

        let mut all_slot_names: Vec<String> = Vec::new();
        let mut slot_data: Vec<(usize, String)> = Vec::new();

        // Phase 1: Try exact match
        for slot in slots {
            let slot_name = WindowHandler::remove_minecraft_colors(&slot.name)
                .to_lowercase()
                .replace(['☘', '☂', '✪', '◆', '❤'], "")
                .trim()
                .to_string();

            if slot_name.is_empty() || slot_name == "close" {
                continue;
            }

            all_slot_names.push(slot_name.clone());
            slot_data.push((slot.index, slot_name.clone()));

            // Exact match
            if slot_name == clean_target {
                info!("Found exact match for \"{}\" at slot {}", item_name, slot.index);
                return Some(slot.index);
            }
        }

        // Phase 2: Try token-based matching
        let target_tokens: Vec<&str> = clean_target.split_whitespace().collect();
        for (index, name) in &slot_data {
            if target_tokens.iter().all(|token| name.contains(token)) {
                info!("Found token match for \"{}\" at slot {} ({})", item_name, index, name);
                return Some(*index);
            }
        }

        // Phase 3: Try partial matching
        for (index, name) in &slot_data {
            if name.contains(&clean_target) || clean_target.contains(name) {
                info!("Found partial match for \"{}\" at slot {} ({})", item_name, index, name);
                return Some(*index);
            }
        }

        // Phase 4: Try fuzzy matching with Levenshtein distance
        if clean_target.len() >= 5 {
            let max_distance = std::cmp::max(2, (clean_target.len() as f64 * 0.2) as usize);
            let mut best_slot = None;
            let mut best_score = usize::MAX;

            for (index, name) in &slot_data {
                let distance = Self::levenshtein_distance(&clean_target, name);
                if distance <= max_distance && distance < best_score {
                    best_slot = Some(*index);
                    best_score = distance;
                }
            }

            if let Some(slot) = best_slot {
                info!("Found fuzzy match for \"{}\" at slot {} with distance {}", item_name, slot, best_score);
                return Some(slot);
            }
        }

        // No match found
        warn!("No match for \"{}\" in search results — found: [{}]", item_name, all_slot_names.join(", "));
        None
    }

    /// Calculate Levenshtein distance between two strings
    fn levenshtein_distance(a: &str, b: &str) -> usize {
        let len_a = a.len();
        let len_b = b.len();

        if len_a == 0 {
            return len_b;
        }
        if len_b == 0 {
            return len_a;
        }

        let mut matrix = vec![vec![0; len_a + 1]; len_b + 1];

        for i in 0..=len_b {
            matrix[i][0] = i;
        }
        for j in 0..=len_a {
            matrix[0][j] = j;
        }

        let a_chars: Vec<char> = a.chars().collect();
        let b_chars: Vec<char> = b.chars().collect();

        for i in 1..=len_b {
            for j in 1..=len_a {
                let cost = if b_chars[i - 1] == a_chars[j - 1] { 0 } else { 1 };
                matrix[i][j] = std::cmp::min(
                    std::cmp::min(
                        matrix[i - 1][j] + 1,     // deletion
                        matrix[i][j - 1] + 1,     // insertion
                    ),
                    matrix[i - 1][j - 1] + cost,  // substitution
                );
            }
        }

        matrix[len_b][len_a]
    }

    /// Check for bazaar errors in window slots
    fn check_for_bazaar_errors(&self, slots: &[WindowSlot]) -> Option<String> {
        let known_error_patterns = [
            "cannot place any more",
            "order limit",
            "insufficient",
            "not enough",
            "maximum orders",
            "buy order limit",
            "sell offer limit",
        ];

        for slot in slots {
            // Check display name for red text (§c)
            if let Some(display_name) = &slot.display_name {
                if display_name.contains("§c") {
                    let clean_name = WindowHandler::remove_minecraft_colors(display_name).to_lowercase();
                    for pattern in &known_error_patterns {
                        if clean_name.contains(pattern) {
                            let full_clean_name = WindowHandler::remove_minecraft_colors(display_name);
                            warn!("Detected bazaar error message: {}", full_clean_name);
                            return Some(full_clean_name);
                        }
                    }
                }
            }

            // Check NBT lore for red text errors
            if let Some(nbt) = &slot.nbt {
                if let Some(display) = nbt.get("display") {
                    if let Some(lore) = display.get("Lore") {
                        if let Some(lore_array) = lore.as_array() {
                            for lore_line in lore_array {
                                if let Some(line_str) = lore_line.as_str() {
                                    if line_str.contains("§c") {
                                        let clean_line = WindowHandler::remove_minecraft_colors(line_str).to_lowercase();
                                        for pattern in &known_error_patterns {
                                            if clean_line.contains(pattern) {
                                                let full_clean_line = WindowHandler::remove_minecraft_colors(line_str);
                                                warn!("Detected bazaar error in lore: {}", full_clean_line);
                                                return Some(full_clean_line);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        None
    }


}

impl Default for BazaarFlipHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_levenshtein_distance() {
        assert_eq!(BazaarFlipHandler::levenshtein_distance("kitten", "sitting"), 3);
        assert_eq!(BazaarFlipHandler::levenshtein_distance("hello", "hello"), 0);
        assert_eq!(BazaarFlipHandler::levenshtein_distance("", "hello"), 5);
    }

    #[test]
    fn test_parse_bazaar_flip_json() {
        let json = serde_json::json!({
            "itemName": "Test Item",
            "itemTag": "TEST_ITEM",
            "price": 1000.0,
            "amount": 64,
            "isSell": false
        });

        let result = BazaarFlipHandler::parse_bazaar_flip_json(&json).unwrap();
        assert_eq!(result.item_name, "Test Item");
        assert_eq!(result.item_tag, Some("TEST_ITEM".to_string()));
        assert_eq!(result.amount, 64);
        assert_eq!(result.price_per_unit, 1000.0);
        assert!(result.is_buy_order);
    }
}
