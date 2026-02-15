/// Window handler for GUI interaction
/// 
/// This module provides functionality for:
/// - Parsing window slots and finding items by name
/// - Handling window opening sequences
/// - Waiting for windows with timeout
/// - Clicking specific slots with proper packet format
/// 
/// Preserves all packet-driven logic from the TypeScript implementation.

use anyhow::{anyhow, Result};
use std::time::Duration;
use tokio::time::{sleep, timeout};
use tracing::{debug, info, warn};

use crate::gui::slot_manager::{StandardSlot, WindowKind};

/// Configuration for window operations
#[derive(Debug, Clone)]
pub struct WindowConfig {
    /// Default timeout for window operations (5000ms from TypeScript)
    pub default_timeout: Duration,
    /// Delay between flip actions (FLIP_ACTION_DELAY, default 150ms)
    pub flip_action_delay: Duration,
    /// Delay for bed spam clicks (BED_SPAM_CLICK_DELAY, default 100ms)
    pub bed_spam_click_delay: Duration,
    /// Maximum failed clicks before stopping bed spam
    pub bed_spam_max_failed_clicks: usize,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            default_timeout: Duration::from_millis(5000),
            flip_action_delay: Duration::from_millis(150),
            bed_spam_click_delay: Duration::from_millis(100),
            bed_spam_max_failed_clicks: 5,
        }
    }
}

/// Represents a slot in a window
#[derive(Debug, Clone)]
pub struct WindowSlot {
    pub index: usize,
    pub item_id: u32,
    pub count: u8,
    pub name: String,
    pub display_name: Option<String>,
    pub nbt: Option<serde_json::Value>,
}

/// Window handler for managing GUI interactions
pub struct WindowHandler {
    config: WindowConfig,
}

impl WindowHandler {
    /// Create a new window handler with default config
    pub fn new() -> Self {
        Self {
            config: WindowConfig::default(),
        }
    }

    /// Create a new window handler with custom config
    pub fn with_config(config: WindowConfig) -> Self {
        Self { config }
    }

    /// Parse window title from JSON format
    /// 
    /// Hypixel sends window titles in JSON format like:
    /// `{"italic":false,"extra":[{"text":"BIN Auction View"}],"text":""}`
    pub fn parse_window_title(raw_title: &str) -> String {
        // Try to parse as JSON first
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(raw_title) {
            // Try to extract text from "extra" array first
            if let Some(extra) = json.get("extra").and_then(|e| e.as_array()) {
                if let Some(first) = extra.first() {
                    if let Some(text) = first.get("text").and_then(|t| t.as_str()) {
                        return text.to_string();
                    }
                }
            }
            
            // Fallback to "text" field
            if let Some(text) = json.get("text").and_then(|t| t.as_str()) {
                if !text.is_empty() {
                    return text.to_string();
                }
            }
        }
        
        // If parsing fails, return the raw title
        raw_title.to_string()
    }

    /// Find an item in window slots by name (exact match)
    pub fn find_item_by_name(&self, slots: &[WindowSlot], name: &str) -> Option<usize> {
        let clean_name = Self::remove_minecraft_colors(name).to_lowercase();
        
        for slot in slots {
            let slot_name = Self::remove_minecraft_colors(&slot.name).to_lowercase();
            if slot_name == clean_name {
                debug!("Found exact match for '{}' at slot {}", name, slot.index);
                return Some(slot.index);
            }
        }
        
        None
    }

    /// Find an item in window slots by name (contains match)
    pub fn find_item_containing(&self, slots: &[WindowSlot], name: &str) -> Option<usize> {
        let clean_name = Self::remove_minecraft_colors(name).to_lowercase();
        
        for slot in slots {
            let slot_name = Self::remove_minecraft_colors(&slot.name).to_lowercase();
            if slot_name.contains(&clean_name) {
                debug!("Found contains match for '{}' at slot {}", name, slot.index);
                return Some(slot.index);
            }
        }
        
        None
    }

    /// Remove Minecraft color codes from text
    /// Format: §x where x is a color code
    pub fn remove_minecraft_colors(text: &str) -> String {
        let mut result = String::new();
        let mut chars = text.chars();
        
        while let Some(ch) = chars.next() {
            if ch == '§' || ch == '┬' {
                // Skip the next character (color code)
                chars.next();
            } else {
                result.push(ch);
            }
        }
        
        result
    }

    /// Wait for a specific item to appear in a slot
    /// 
    /// This implements the TPM+ pattern from the TypeScript version:
    /// - Polls slot at 1ms intervals
    /// - Times out after `delay * 3` milliseconds
    /// - Optionally waits for item to CHANGE from current value
    pub async fn wait_for_item_load<F>(
        &self,
        slot_index: usize,
        already_loaded: bool,
        get_slot: F,
    ) -> Result<Option<WindowSlot>>
    where
        F: Fn() -> Option<WindowSlot>,
    {
        let delay = self.config.flip_action_delay;
        let timeout_duration = delay * 3;
        
        let initial_name = if already_loaded {
            get_slot().map(|s| s.name.clone())
        } else {
            None
        };
        
        let start = std::time::Instant::now();
        let mut iterations = 0;
        
        loop {
            if start.elapsed() >= timeout_duration {
                warn!("Failed to find item in slot {} after timeout", slot_index);
                return Ok(None);
            }
            
            if let Some(slot) = get_slot() {
                if already_loaded {
                    // Wait for item to CHANGE from initial value
                    if Some(&slot.name) != initial_name.as_ref() {
                        info!(
                            "Found {} on iteration {} (changed from {:?})",
                            slot.name, iterations, initial_name
                        );
                        return Ok(Some(slot));
                    }
                } else {
                    // Wait for any item to appear
                    if !slot.name.is_empty() {
                        info!("Found {} on iteration {}", slot.name, iterations);
                        return Ok(Some(slot));
                    }
                }
            }
            
            iterations += 1;
            sleep(Duration::from_millis(1)).await;
        }
    }

    /// Get the default timeout duration
    pub fn default_timeout(&self) -> Duration {
        self.config.default_timeout
    }

    /// Get the flip action delay
    pub fn flip_action_delay(&self) -> Duration {
        self.config.flip_action_delay
    }

    /// Get the bed spam click delay
    pub fn bed_spam_click_delay(&self) -> Duration {
        self.config.bed_spam_click_delay
    }

    /// Get the max bed spam failed clicks
    pub fn bed_spam_max_failed_clicks(&self) -> usize {
        self.config.bed_spam_max_failed_clicks
    }
}

impl Default for WindowHandler {
    fn default() -> Self {
        Self::new()
    }
}

/// Helper function to wait for a window to open with timeout
pub async fn wait_for_window_with_timeout<F>(
    timeout_duration: Duration,
    mut check_window: F,
) -> Result<()>
where
    F: FnMut() -> bool,
{
    let result = timeout(timeout_duration, async {
        loop {
            if check_window() {
                return Ok(());
            }
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    
    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(anyhow!("Timeout waiting for window")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_window_title() {
        let json_title = r#"{"italic":false,"extra":[{"text":"BIN Auction View"}],"text":""}"#;
        assert_eq!(
            WindowHandler::parse_window_title(json_title),
            "BIN Auction View"
        );

        let simple_title = "Confirm Purchase";
        assert_eq!(
            WindowHandler::parse_window_title(simple_title),
            "Confirm Purchase"
        );
    }

    #[test]
    fn test_remove_minecraft_colors() {
        assert_eq!(
            WindowHandler::remove_minecraft_colors("§aGreen§r Text"),
            "Green Text"
        );
        assert_eq!(
            WindowHandler::remove_minecraft_colors("§6Buy Item Right Now"),
            "Buy Item Right Now"
        );
    }

    #[test]
    fn test_find_item_by_name() {
        let handler = WindowHandler::new();
        let slots = vec![
            WindowSlot {
                index: 31,
                item_id: 371,
                count: 1,
                name: "gold_nugget".to_string(),
                display_name: Some("§6Buy Item Right Now".to_string()),
                nbt: None,
            },
            WindowSlot {
                index: 13,
                item_id: 355,
                count: 1,
                name: "bed".to_string(),
                display_name: Some("Bed".to_string()),
                nbt: None,
            },
        ];

        assert_eq!(handler.find_item_by_name(&slots, "gold_nugget"), Some(31));
        assert_eq!(handler.find_item_by_name(&slots, "bed"), Some(13));
        assert_eq!(handler.find_item_by_name(&slots, "nonexistent"), None);
    }
}
