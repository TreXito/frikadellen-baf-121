/// Auction flip handler
/// 
/// Handles auction house flip recommendations from Coflnet:
/// - Processes flip recommendations with bed spam support
/// - Navigates to BIN Auction View window
/// - Clicks "Buy Item Right Now" button (slot 31)
/// - Clicks confirm button (slot 11)
/// - Tracks purchase timing
/// - Implements skip logic (pre-click optimization)
/// 
/// Preserves exact slot numbers and timing from TypeScript implementation.

use anyhow::{anyhow, Result};
use parking_lot::RwLock;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

use crate::gui::{StandardSlot, WindowConfig, WindowHandler, WindowSlot};
use crate::types::{BotState, Flip};
use crate::utils::format_number_with_separators;

/// Maximum failed clicks before stopping bed spam
const BED_SPAM_MAX_FAILED_CLICKS: usize = 5;

/// Configuration for flip handler
#[derive(Debug, Clone)]
pub struct FlipConfig {
    /// Enable auction house flips
    pub enabled: bool,
    /// Skip configuration
    pub skip: Option<SkipConfig>,
    /// Window handler configuration
    pub window_config: WindowConfig,
}

impl Default for FlipConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            skip: None,
            window_config: WindowConfig::default(),
        }
    }
}

/// Skip (pre-click) optimization configuration
#[derive(Debug, Clone)]
pub struct SkipConfig {
    /// Always use skip optimization
    pub always: bool,
    /// Minimum profit to trigger skip
    pub min_profit: Option<u64>,
    /// Skip for USER finder flips
    pub user_finder: bool,
    /// Skip for skin items
    pub skins: bool,
    /// Minimum profit percentage to trigger skip
    pub profit_percentage: Option<f64>,
    /// Minimum price to trigger skip
    pub min_price: Option<u64>,
}

/// Flip handler state
pub struct FlipHandler {
    /// Current flip being processed
    current_flip: Arc<RwLock<Option<Flip>>>,
    /// Action counter for window clicks (anti-cheat)
    action_counter: Arc<RwLock<i16>>,
    /// Whether the last flip used skip optimization
    recently_skipped: Arc<RwLock<bool>>,
    /// When the BIN Auction View opened (for timing)
    purchase_start_time: Arc<RwLock<Option<Instant>>>,
    /// Configuration
    config: Arc<RwLock<FlipConfig>>,
    /// Window handler
    window_handler: WindowHandler,
}

impl FlipHandler {
    /// Create a new flip handler
    pub fn new() -> Self {
        Self {
            current_flip: Arc::new(RwLock::new(None)),
            action_counter: Arc::new(RwLock::new(1)),
            recently_skipped: Arc::new(RwLock::new(false)),
            purchase_start_time: Arc::new(RwLock::new(None)),
            config: Arc::new(RwLock::new(FlipConfig::default())),
            window_handler: WindowHandler::new(),
        }
    }

    /// Create a new flip handler with custom configuration
    pub fn with_config(config: FlipConfig) -> Self {
        let window_handler = WindowHandler::with_config(config.window_config.clone());
        Self {
            current_flip: Arc::new(RwLock::new(None)),
            action_counter: Arc::new(RwLock::new(1)),
            recently_skipped: Arc::new(RwLock::new(false)),
            purchase_start_time: Arc::new(RwLock::new(None)),
            config: Arc::new(RwLock::new(config)),
            window_handler,
        }
    }

    /// Update configuration
    pub fn update_config(&self, config: FlipConfig) {
        *self.config.write() = config;
    }

    /// Check if flips are enabled
    pub fn is_enabled(&self) -> bool {
        self.config.read().enabled
    }

    /// Get current flip
    pub fn get_current_flip(&self) -> Option<Flip> {
        self.current_flip.read().clone()
    }

    /// Clear current flip
    pub fn clear_current_flip(&self) {
        *self.current_flip.write() = None;
    }

    /// Get purchase start time
    pub fn get_purchase_start_time(&self) -> Option<Instant> {
        *self.purchase_start_time.read()
    }

    /// Clear purchase start time
    pub fn clear_purchase_start_time(&self) {
        *self.purchase_start_time.write() = None;
    }

    /// Determine whether a flip should use the skip (pre-click) optimization
    fn should_skip_flip(&self, flip: &Flip, profit: u64) -> bool {
        let config = self.config.read();
        let skip_config = match &config.skip {
            Some(cfg) => cfg,
            None => return false,
        };

        if skip_config.always {
            return true;
        }

        if let Some(min_profit) = skip_config.min_profit {
            if profit >= min_profit {
                return true;
            }
        }

        if skip_config.user_finder {
            if let Some(finder) = &flip.finder {
                if finder == "USER" {
                    return true;
                }
            }
        }

        if skip_config.skins && Self::is_skin(&flip.item_name) {
            return true;
        }

        if let Some(profit_perc_threshold) = skip_config.profit_percentage {
            if let Some(profit_perc) = flip.profit_perc {
                if profit_perc >= profit_perc_threshold {
                    return true;
                }
            }
        }

        if let Some(min_price) = skip_config.min_price {
            if flip.starting_bid >= min_price {
                return true;
            }
        }

        false
    }

    /// Check if an item is a skin
    fn is_skin(item_name: &str) -> bool {
        item_name.to_lowercase().contains("skin")
    }

    /// Log the reason a flip was skipped
    fn log_skip_reason(&self, flip: &Flip, profit: u64) {
        let config = self.config.read();
        let skip_config = match &config.skip {
            Some(cfg) => cfg,
            None => return,
        };

        let reason = if skip_config.always {
            "ALWAYS".to_string()
        } else if let Some(min_profit) = skip_config.min_profit {
            if profit >= min_profit {
                format!("MIN_PROFIT ({} >= {})", profit, min_profit)
            } else {
                "Unknown".to_string()
            }
        } else if skip_config.user_finder {
            if let Some(finder) = &flip.finder {
                if finder == "USER" {
                    "USER_FINDER".to_string()
                } else {
                    "Unknown".to_string()
                }
            } else {
                "Unknown".to_string()
            }
        } else if skip_config.skins && Self::is_skin(&flip.item_name) {
            "SKINS".to_string()
        } else if let Some(profit_perc_threshold) = skip_config.profit_percentage {
            if let Some(profit_perc) = flip.profit_perc {
                if profit_perc >= profit_perc_threshold {
                    format!(
                        "PROFIT_PERCENTAGE ({} >= {})",
                        profit_perc, profit_perc_threshold
                    )
                } else {
                    "Unknown".to_string()
                }
            } else {
                "Unknown".to_string()
            }
        } else if let Some(min_price) = skip_config.min_price {
            if flip.starting_bid >= min_price {
                format!("MIN_PRICE ({} >= {})", flip.starting_bid, min_price)
            } else {
                "Unknown".to_string()
            }
        } else {
            "Unknown".to_string()
        };

        info!("Skip reason for {}: {}", flip.item_name, reason);
    }

    /// Send a confirm click packet for faster window confirmation
    /// 
    /// This sends a transaction packet to acknowledge the window operation.
    /// Must be sent BEFORE any slot clicking for proper anti-cheat behavior.
    pub fn confirm_click(&self, window_id: u8) -> i16 {
        let mut counter = self.action_counter.write();
        let action = *counter;
        *counter += 1;
        
        info!("Sending confirm click for window {} (action {})", window_id, action);
        
        // NOTE: Actual packet sending would happen here through the bot client
        // In the TypeScript version, this is:
        // bot._client.write('transaction', {
        //     windowId: windowId,
        //     action: actionCounter,
        //     accepted: true
        // })
        
        action
    }

    /// Click a slot using low-level packets for faster response
    /// 
    /// Mouse button 2 = middle click, mode 3 = special inventory interaction
    /// 
    /// # Arguments
    /// * `slot` - Slot number to click
    /// * `window_id` - Window ID
    /// * `item_id` - Item block ID (e.g., 371 for gold_nugget)
    pub fn click_slot(&self, slot: usize, window_id: u8, item_id: u32) -> i16 {
        let mut counter = self.action_counter.write();
        let action = *counter;
        *counter += 1;
        
        info!(
            "Clicking slot {} in window {} with item ID {} (action {})",
            slot, window_id, item_id, action
        );
        
        // NOTE: Actual packet sending would happen here through the bot client
        // In the TypeScript version, this is:
        // bot._client.write('window_click', {
        //     windowId: windowId,
        //     slot: slot,
        //     mouseButton: 2, // Middle click
        //     mode: 3, // Special inventory interaction mode
        //     item: { "blockId": itemId },
        //     action: actionCounter
        // })
        
        action
    }

    /// Handle a flip recommendation
    /// 
    /// This is the main entry point for processing auction flips.
    /// 
    /// # Arguments
    /// * `flip` - The flip recommendation
    /// * `bot_state` - Current bot state (checked for interruption)
    /// * `send_command` - Callback to send chat commands
    /// * `window_opened` - Callback for when a window opens
    pub async fn handle_flip<F, G>(
        &self,
        flip: Flip,
        bot_state: Arc<RwLock<BotState>>,
        send_command: F,
        window_opened: G,
    ) -> Result<()>
    where
        F: Fn(&str) -> Result<()>,
        G: Fn(u8, &str) -> Result<()>,
    {
        // Check if flips are enabled
        if !self.is_enabled() {
            debug!("AH flips are disabled in config");
            return Ok(());
        }

        // Check if bot is in startup state
        if *bot_state.read() == BotState::Startup {
            info!("Ignoring AH flip during startup");
            return Ok(());
        }

        // Store current flip
        *self.current_flip.write() = Some(flip.clone());

        // Check if bot is busy
        loop {
            let state = *bot_state.read();
            if state == BotState::Idle {
                break;
            }
            
            // TODO: Check if we can interrupt the current operation
            warn!("Bot busy with {:?}, retrying in 1.1s", state);
            sleep(Duration::from_millis(1100)).await;
        }

        // Set bot state to purchasing
        *bot_state.write() = BotState::Purchasing;

        let profit = flip.target - flip.starting_bid;
        
        info!(
            "Trying to purchase flip: {} for {} coins (Target: {})",
            flip.item_name,
            format_number_with_separators(flip.starting_bid),
            format_number_with_separators(flip.target)
        );

        // Send the /viewauction command
        let command = format!("/viewauction {}", flip.uuid.as_ref().unwrap_or(&String::new()));
        send_command(&command)?;

        // Wait for window to open (implemented in caller)
        // The window_opened callback will handle the window logic

        *bot_state.write() = BotState::Idle;
        Ok(())
    }

    /// Handle BIN Auction View window
    /// 
    /// This is called when the "BIN Auction View" window opens.
    /// 
    /// # Arguments
    /// * `window_id` - The window ID
    /// * `next_window_id` - The next expected window ID (for skip optimization)
    /// * `slots` - Window slots
    pub async fn handle_bin_auction_view(
        &self,
        window_id: u8,
        next_window_id: u8,
        slots: &[WindowSlot],
    ) -> Result<()> {
        // Calculate skip conditions BEFORE clicking anything
        let (should_skip, profit) = {
            let flip_lock = self.current_flip.read();
            if let Some(flip) = flip_lock.as_ref() {
                let profit = flip.target - flip.starting_bid;
                let should_skip = self.should_skip_flip(flip, profit);
                (should_skip, profit)
            } else {
                (false, 0)
            }
        };

        // Record start time
        *self.purchase_start_time.write() = Some(Instant::now());

        // Wait for item to load in slot 31
        let slot_31 = slots.iter().find(|s| s.index == 31);
        
        let item_name = slot_31.map(|s| s.name.as_str()).unwrap_or("");

        match item_name {
            "gold_nugget" => {
                info!("Found gold nugget, clicking purchase button (slot 31)");
                
                // Send low-level click packet for slot 31
                self.click_slot(31, window_id, 371);
                
                // Redundant click in case first packet was lost
                // (In actual implementation, this would be clickWindow from mineflayer)
                
                if should_skip {
                    // SKIP: pre-click Confirm (slot 11) on the NEXT window in the same tick
                    info!("Using skip optimization - pre-clicking confirm button");
                    self.click_slot(11, next_window_id, 159);
                    *self.recently_skipped.write() = true;
                    
                    if let Some(flip) = self.current_flip.read().as_ref() {
                        self.log_skip_reason(flip, profit);
                    }
                    
                    // RETURN here — the window listener will handle Confirm Purchase
                    return Ok(());
                }
                
                // Only reached if skip was NOT used
                *self.recently_skipped.write() = false;
                Ok(())
            }
            "bed" => {
                info!("Found a bed! Starting bed spam");
                // Bed spam will be handled by a separate loop
                // (Implemented in caller)
                Ok(())
            }
            "potato" | "" => {
                warn!("Potatoed! Item no longer available");
                Err(anyhow!("Potatoed"))
            }
            "feather" => {
                info!("Found feather, waiting for slot to change");
                // Need to wait for slot to change (implemented in caller)
                Ok(())
            }
            "gold_block" => {
                info!("Sold auction - claiming it");
                self.click_slot(31, window_id, 41);
                Ok(())
            }
            "poisonous_potato" => {
                warn!("Too poor to buy it");
                Err(anyhow!("Insufficient funds"))
            }
            "stained_glass_pane" => {
                warn!("Stained glass pane - edge case");
                Err(anyhow!("Edge case - stained glass pane"))
            }
            _ => {
                warn!("Unexpected item in slot 31: {}", item_name);
                Err(anyhow!("Unexpected item: {}", item_name))
            }
        }
    }

    /// Handle Confirm Purchase window
    /// 
    /// This is called when the "Confirm Purchase" window opens.
    /// 
    /// # Arguments
    /// * `window_id` - The window ID
    /// * `first_gui_time` - Time when BIN Auction View opened
    pub async fn handle_confirm_purchase(
        &self,
        window_id: u8,
        first_gui_time: Instant,
    ) -> Result<()> {
        let confirm_at = first_gui_time.elapsed().as_millis();
        info!("Confirm at {}ms", confirm_at);

        let recently_skipped = *self.recently_skipped.read();

        // First click: only if we didn't pre-click via skip
        if !recently_skipped {
            info!("Clicking confirm button (slot 11)");
            self.click_slot(11, window_id, 159);
        } else {
            info!("Skip was used - confirm already pre-clicked");
        }

        // Wait ~150ms (3 ticks)
        sleep(Duration::from_millis(150)).await;

        // Safety retry loop: runs REGARDLESS of recently_skipped
        // If skip pre-click worked, window is already gone and loop doesn't execute
        // If skip pre-click failed (packet lost), this catches it
        
        // NOTE: In actual implementation, this would check if window is still open
        // For now, we just do a few retries
        for _ in 0..3 {
            info!("Safety retry - clicking confirm button");
            self.click_slot(11, window_id, 159);
            sleep(Duration::from_millis(250)).await;
        }

        Ok(())
    }

    /// Initialize bed spam prevention
    /// 
    /// Continuously clicks slot 31 until gold_nugget appears or max failures reached.
    pub async fn init_bed_spam<F>(
        &self,
        window_id: u8,
        check_slot: F,
    ) -> Result<()>
    where
        F: Fn() -> Option<String>,
    {
        info!("Starting bed spam prevention...");
        
        let click_delay = self.window_handler.bed_spam_click_delay();
        let max_failed = self.window_handler.bed_spam_max_failed_clicks();
        let mut failed_clicks = 0;

        loop {
            if failed_clicks >= max_failed {
                warn!("Stopped bed spam after {} failed clicks", failed_clicks);
                break;
            }

            let slot_name = check_slot();
            
            if let Some(name) = slot_name {
                if name == "gold_nugget" {
                    info!("Bed spam: gold_nugget appeared, clicking");
                    self.click_slot(31, window_id, 371);
                    break;
                } else {
                    failed_clicks += 1;
                }
            } else {
                // Window closed
                info!("Stopped bed spam - window closed");
                break;
            }

            sleep(click_delay).await;
        }

        Ok(())
    }
}

impl Default for FlipHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_skin() {
        assert!(FlipHandler::is_skin("Dragon Skin"));
        assert!(FlipHandler::is_skin("skin of something"));
        assert!(!FlipHandler::is_skin("Gold Nugget"));
    }

    #[test]
    fn test_should_skip_flip() {
        let handler = FlipHandler::new();
        
        // No skip config
        let flip = Flip {
            item_name: "Test Item".to_string(),
            starting_bid: 1000,
            target: 2000,
            finder: None,
            profit_perc: None,
            uuid: None,
        };
        assert!(!handler.should_skip_flip(&flip, 1000));
        
        // With always skip
        handler.update_config(FlipConfig {
            enabled: true,
            skip: Some(SkipConfig {
                always: true,
                min_profit: None,
                user_finder: false,
                skins: false,
                profit_percentage: None,
                min_price: None,
            }),
            window_config: WindowConfig::default(),
        });
        assert!(handler.should_skip_flip(&flip, 1000));
    }
}
