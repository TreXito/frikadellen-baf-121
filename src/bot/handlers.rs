use parking_lot::RwLock;
use regex::Regex;
use serde_json::Value as JsonValue;
use std::sync::Arc;
use tracing::{debug, info, warn};

use crate::types::WindowType;

/// Event handlers for bot events
pub struct BotEventHandlers {
    /// Current window title
    current_window_title: Arc<RwLock<Option<String>>>,
    /// Current window type
    current_window_type: Arc<RwLock<Option<WindowType>>>,
    /// Current window ID
    current_window_id: Arc<RwLock<Option<u8>>>,
    /// Regex for Coflnet chat messages
    cofl_chat_regex: Regex,
}

impl BotEventHandlers {
    /// Create new event handlers
    pub fn new() -> Self {
        Self {
            current_window_title: Arc::new(RwLock::new(None)),
            current_window_type: Arc::new(RwLock::new(None)),
            current_window_id: Arc::new(RwLock::new(None)),
            cofl_chat_regex: Regex::new(r"\[Chat\]").unwrap(),
        }
    }

    /// Handle window open event
    pub async fn handle_window_open(&self, window_id: u8, window_type: &str, title: &str) {
        info!(
            "[Window] Opened: id={} type={} title=\"{}\"",
            window_id, window_type, title
        );

        *self.current_window_id.write() = Some(window_id);
        *self.current_window_title.write() = Some(title.to_string());
        *self.current_window_type.write() = Some(Self::classify_window(title));
    }

    /// Handle window close event
    pub async fn handle_window_close(&self) {
        if let Some(title) = self.current_window_title.read().as_ref() {
            info!("[Window] Closed: title=\"{}\"", title);
        }

        *self.current_window_id.write() = None;
        *self.current_window_title.write() = None;
        *self.current_window_type.write() = None;
    }

    /// Handle chat message
    pub async fn handle_chat_message(&self, message: &str) {
        // Check if it's a Coflnet message
        let is_cofl = self.is_cofl_chat_message(message);
        
        if !is_cofl {
            debug!("[Chat] {}", message);
        }

        // Filter specific message patterns
        if message.contains("[Bazaar]") {
            info!("[Bazaar] {}", message);
        }
    }

    /// Parse window title from JSON format
    /// 
    /// Window titles come in JSON format like:
    /// {"text":"","extra":[{"text":"Bazaar"}]}
    /// {"translate":"container.chest"}
    pub fn parse_window_title(&self, title_json: &str) -> String {
        // Try to parse as JSON
        match serde_json::from_str::<JsonValue>(title_json) {
            Ok(json) => {
                // Try to extract from "extra" array first
                if let Some(extra) = json.get("extra") {
                    if let Some(extra_array) = extra.as_array() {
                        if let Some(first) = extra_array.get(0) {
                            if let Some(text) = first.get("text") {
                                if let Some(text_str) = text.as_str() {
                                    return text_str.to_string();
                                }
                            }
                        }
                    }
                }

                // Try "translate" field
                if let Some(translate) = json.get("translate") {
                    if let Some(translate_str) = translate.as_str() {
                        return translate_str.to_string();
                    }
                }

                // Try direct "text" field
                if let Some(text) = json.get("text") {
                    if let Some(text_str) = text.as_str() {
                        if !text_str.is_empty() {
                            return text_str.to_string();
                        }
                    }
                }

                // Return raw JSON if we can't parse it
                title_json.to_string()
            }
            Err(_) => {
                // Not valid JSON, return as-is
                title_json.to_string()
            }
        }
    }

    /// Classify window type based on title
    fn classify_window(title: &str) -> WindowType {
        let title_lower = title.to_lowercase();

        if title_lower.contains("bazaar") && !title_lower.contains("order") {
            WindowType::BazaarSearch
        } else if title_lower.contains("buy order") 
            || title_lower.contains("sell offer")
            || title_lower.contains("order options") {
            WindowType::BazaarOrderCreation
        } else if title_lower.contains("manage orders") {
            WindowType::ManageOrders
        } else if title_lower.contains("bin auction view") 
            || title_lower.contains("auction view") {
            WindowType::BinAuctionView
        } else if title_lower.contains("confirm purchase") {
            WindowType::ConfirmPurchase
        } else if title_lower.contains("storage") 
            || title_lower.contains("chest")
            || title_lower.contains("backpack") {
            WindowType::Storage
        } else {
            WindowType::Other(title.to_string())
        }
    }

    /// Check if a message is from Coflnet chat
    pub fn is_cofl_chat_message(&self, message: &str) -> bool {
        // Remove Minecraft color codes first
        let clean = Self::remove_color_codes(message);
        
        // Check if it starts with [Chat]
        clean.starts_with("[Chat]")
    }

    /// Remove Minecraft color codes from text
    pub fn remove_color_codes(text: &str) -> String {
        // Minecraft color codes: §[0-9a-fk-or]
        let re = Regex::new(r"§[0-9a-fk-or]").unwrap();
        re.replace_all(text, "").to_string()
    }

    /// Get current window title
    pub fn current_window_title(&self) -> Option<String> {
        self.current_window_title.read().clone()
    }

    /// Get current window type
    pub fn current_window_type(&self) -> Option<WindowType> {
        self.current_window_type.read().clone()
    }

    /// Get current window ID
    pub fn current_window_id(&self) -> Option<u8> {
        *self.current_window_id.read()
    }

    /// Extract SkyBlock item ID from NBT data
    /// 
    /// SkyBlock items have a custom NBT tag structure:
    /// ExtraAttributes.id = "SKYBLOCK_ITEM_ID"
    pub fn extract_skyblock_id(nbt: &JsonValue) -> Option<String> {
        nbt.get("ExtraAttributes")
            .and_then(|ea| ea.get("id"))
            .and_then(|id| id.as_str())
            .map(|s| s.to_string())
    }

    /// Extract display name from item NBT
    /// 
    /// Priority:
    /// 1. NBT display name (display.Name) - Custom Hypixel name
    /// 2. Item name - Vanilla name
    pub fn extract_display_name(nbt: &JsonValue) -> Option<String> {
        // Try NBT display name first
        if let Some(display) = nbt.get("display") {
            if let Some(name) = display.get("Name") {
                if let Some(name_str) = name.as_str() {
                    // Name might be JSON formatted
                    return Some(Self::parse_display_name_json(name_str));
                }
            }
        }

        None
    }

    /// Parse display name from JSON or plain text
    /// 
    /// Display names can be:
    /// - JSON: {"text":"","extra":[{"text":"Dedication I"}]}
    /// - Plain text with color codes: §9Dedication I
    fn parse_display_name_json(name: &str) -> String {
        // Try to parse as JSON
        if let Ok(json) = serde_json::from_str::<JsonValue>(name) {
            let mut result = String::new();

            // Concatenate text from main and extra fields
            if let Some(text) = json.get("text") {
                if let Some(text_str) = text.as_str() {
                    result.push_str(text_str);
                }
            }

            if let Some(extra) = json.get("extra") {
                if let Some(extra_array) = extra.as_array() {
                    for item in extra_array {
                        if let Some(text) = item.get("text") {
                            if let Some(text_str) = text.as_str() {
                                result.push_str(text_str);
                            }
                        } else if let Some(text_str) = item.as_str() {
                            result.push_str(text_str);
                        }
                    }
                }
            }

            Self::remove_color_codes(&result).trim().to_string()
        } else {
            // Plain text with color codes
            Self::remove_color_codes(name).trim().to_string()
        }
    }

    /// Find slot with specific item name in window
    pub async fn find_slot_with_name(
        &self,
        slots: &[Option<crate::types::ItemStack>],
        name: &str,
    ) -> Option<usize> {
        for (idx, slot) in slots.iter().enumerate() {
            if let Some(item) = slot {
                if item.name.contains(name) {
                    return Some(idx);
                }
            }
        }
        None
    }

    /// Parse price from item lore
    /// 
    /// Looks for patterns like:
    /// "Price: 1,234,567 coins"
    /// "Cost: 1.2M coins"
    pub fn parse_price_from_lore(lore: &[String]) -> Option<f64> {
        let price_regex = Regex::new(r"(?:Price|Cost):\s*([0-9,\.]+)\s*([KMB])?\s*coins?").unwrap();

        for line in lore {
            let clean = Self::remove_color_codes(line);
            
            if let Some(captures) = price_regex.captures(&clean) {
                if let Some(number_str) = captures.get(1) {
                    // Remove commas
                    let number_clean = number_str.as_str().replace(",", "");
                    
                    if let Ok(mut value) = number_clean.parse::<f64>() {
                        // Apply multiplier if present
                        if let Some(multiplier) = captures.get(2) {
                            value *= match multiplier.as_str() {
                                "K" => 1_000.0,
                                "M" => 1_000_000.0,
                                "B" => 1_000_000_000.0,
                                _ => 1.0,
                            };
                        }
                        
                        return Some(value);
                    }
                }
            }
        }

        None
    }

    /// Parse bazaar price from sign text
    /// 
    /// Sign shows current instant-buy/sell prices like:
    /// "Instant-Buy: 1,234.5"
    /// "Instant-Sell: 5,678.9"
    pub fn parse_bazaar_sign_price(sign_lines: &[String]) -> Option<f64> {
        let price_regex = Regex::new(r"Instant-(?:Buy|Sell):\s*([0-9,\.]+)").unwrap();

        for line in sign_lines {
            let clean = Self::remove_color_codes(line);
            
            if let Some(captures) = price_regex.captures(&clean) {
                if let Some(number_str) = captures.get(1) {
                    let number_clean = number_str.as_str().replace(",", "");
                    
                    if let Ok(value) = number_clean.parse::<f64>() {
                        return Some(value);
                    }
                }
            }
        }

        None
    }
}

impl Default for BotEventHandlers {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_window_title_with_extra() {
        let handlers = BotEventHandlers::new();
        let json = r#"{"text":"","extra":[{"text":"Bazaar"}]}"#;
        let title = handlers.parse_window_title(json);
        assert_eq!(title, "Bazaar");
    }

    #[test]
    fn test_parse_window_title_translate() {
        let handlers = BotEventHandlers::new();
        let json = r#"{"translate":"container.chest"}"#;
        let title = handlers.parse_window_title(json);
        assert_eq!(title, "container.chest");
    }

    #[test]
    fn test_remove_color_codes() {
        let text = "§aGreen§r §bBlue§r Normal";
        let clean = BotEventHandlers::remove_color_codes(text);
        assert_eq!(clean, "Green Blue Normal");
    }

    #[test]
    fn test_is_cofl_chat_message() {
        let handlers = BotEventHandlers::new();
        assert!(handlers.is_cofl_chat_message("§7[Chat] Test message"));
        assert!(handlers.is_cofl_chat_message("[Chat] Test message"));
        assert!(!handlers.is_cofl_chat_message("Regular message"));
    }

    #[test]
    fn test_classify_window() {
        assert!(matches!(
            BotEventHandlers::classify_window("Bazaar"),
            WindowType::BazaarSearch
        ));
        
        assert!(matches!(
            BotEventHandlers::classify_window("Create Buy Order"),
            WindowType::BazaarOrderCreation
        ));
        
        assert!(matches!(
            BotEventHandlers::classify_window("BIN Auction View"),
            WindowType::BinAuctionView
        ));
        
        assert!(matches!(
            BotEventHandlers::classify_window("Confirm Purchase"),
            WindowType::ConfirmPurchase
        ));
    }

    #[test]
    fn test_parse_display_name_json() {
        let json = r#"{"text":"","extra":[{"text":"Dedication I"}]}"#;
        let name = BotEventHandlers::parse_display_name_json(json);
        assert_eq!(name, "Dedication I");
    }

    #[test]
    fn test_parse_display_name_with_colors() {
        let text = "§9Dedication I";
        let name = BotEventHandlers::parse_display_name_json(text);
        assert_eq!(name, "Dedication I");
    }

    #[test]
    fn test_parse_price_from_lore() {
        let lore = vec![
            "".to_string(),
            "§7Price: §61,234,567 coins".to_string(),
            "".to_string(),
        ];
        
        let price = BotEventHandlers::parse_price_from_lore(&lore);
        assert_eq!(price, Some(1_234_567.0));
    }

    #[test]
    fn test_parse_price_with_multiplier() {
        let lore = vec![
            "§7Cost: §61.2M coins".to_string(),
        ];
        
        let price = BotEventHandlers::parse_price_from_lore(&lore);
        assert_eq!(price, Some(1_200_000.0));
    }
}
