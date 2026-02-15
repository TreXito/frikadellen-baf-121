/// Slot manager for handling GUI slot positions and translations
/// 
/// This module defines standard slot positions used across different window types
/// in the Hypixel SkyBlock GUI system. Slot numbers are preserved exactly from
/// the TypeScript implementation to ensure packet compatibility.

use std::collections::HashMap;
use tracing::{debug, warn};

/// Standard slot positions used in various GUIs
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StandardSlot {
    /// Slot 31 - "Buy Item Right Now" button in BIN Auction View
    PurchaseButton = 31,
    /// Slot 11 - Confirm button in "Confirm Purchase" window
    ConfirmButton = 11,
    /// Slot 13 - Center slot, used for bazaar order confirmation
    CenterSlot = 13,
    /// Slot 50 - Close button in many GUIs
    CloseButton = 50,
    /// Slot 49 - Alternative close button position
    AltCloseButton = 49,
    /// Slot 22 - Common item display position
    ItemDisplay = 22,
}

impl StandardSlot {
    /// Get the raw slot number
    pub fn slot(&self) -> usize {
        *self as usize
    }
}

/// Represents different window types in Hypixel SkyBlock
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowKind {
    /// "BIN Auction View" - the window that opens when viewing an auction
    BinAuctionView,
    /// "Confirm Purchase" - confirmation dialog for auction purchases
    ConfirmPurchase,
    /// "Auction View" - non-BIN auction (should be disabled)
    AuctionView,
    /// "Bazaar ➜ ..." - bazaar search results or item detail
    Bazaar,
    /// "Manage Orders" - order management screen
    ManageOrders,
    /// Generic chest window
    Chest,
    /// Player inventory
    Inventory,
    /// Unknown window type
    Unknown(String),
}

impl WindowKind {
    /// Parse window kind from title
    pub fn from_title(title: &str) -> Self {
        // Remove JSON formatting if present
        let clean_title = if title.contains("\"text\":") {
            // Extract text from JSON formatted title
            if let Some(text_start) = title.find("\"text\":\"") {
                let start = text_start + 8;
                if let Some(end) = title[start..].find('"') {
                    &title[start..start + end]
                } else {
                    title
                }
            } else {
                title
            }
        } else {
            title
        };

        match clean_title {
            t if t == "BIN Auction View" => WindowKind::BinAuctionView,
            t if t == "Confirm Purchase" => WindowKind::ConfirmPurchase,
            t if t == "Auction View" => WindowKind::AuctionView,
            t if t.starts_with("Bazaar") => WindowKind::Bazaar,
            t if t.contains("Manage") && t.contains("Orders") => WindowKind::ManageOrders,
            _ => WindowKind::Unknown(clean_title.to_string()),
        }
    }
}

/// Slot manager for translating logical slots to physical window slots
pub struct SlotManager {
    /// Cached slot mappings for different window types
    mappings: HashMap<String, HashMap<String, usize>>,
}

impl SlotManager {
    /// Create a new slot manager
    pub fn new() -> Self {
        Self {
            mappings: HashMap::new(),
        }
    }

    /// Get the physical slot number for a logical slot name in a given window
    /// 
    /// # Arguments
    /// * `window_kind` - The type of window
    /// * `slot_name` - Logical name of the slot (e.g., "purchase", "confirm")
    /// 
    /// # Returns
    /// Physical slot number, or None if mapping doesn't exist
    pub fn get_slot(&self, window_kind: &WindowKind, slot_name: &str) -> Option<usize> {
        let window_key = format!("{:?}", window_kind);
        
        // Check if we have a cached mapping
        if let Some(window_map) = self.mappings.get(&window_key) {
            return window_map.get(slot_name).copied();
        }

        // Return standard mappings
        match (window_kind, slot_name) {
            (WindowKind::BinAuctionView, "purchase") => Some(StandardSlot::PurchaseButton.slot()),
            (WindowKind::ConfirmPurchase, "confirm") => Some(StandardSlot::ConfirmButton.slot()),
            (WindowKind::Bazaar, "confirm") => Some(StandardSlot::CenterSlot.slot()),
            (_, "close") => Some(StandardSlot::CloseButton.slot()),
            _ => {
                warn!("No slot mapping for window {:?}, slot name: {}", window_kind, slot_name);
                None
            }
        }
    }

    /// Register a custom slot mapping for a window type
    pub fn register_slot(
        &mut self,
        window_kind: WindowKind,
        slot_name: String,
        physical_slot: usize,
    ) {
        let window_key = format!("{:?}", window_kind);
        self.mappings
            .entry(window_key)
            .or_insert_with(HashMap::new)
            .insert(slot_name, physical_slot);
    }
}

impl Default for SlotManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_standard_slot_values() {
        assert_eq!(StandardSlot::PurchaseButton.slot(), 31);
        assert_eq!(StandardSlot::ConfirmButton.slot(), 11);
        assert_eq!(StandardSlot::CenterSlot.slot(), 13);
    }

    #[test]
    fn test_window_kind_from_title() {
        assert_eq!(
            WindowKind::from_title("BIN Auction View"),
            WindowKind::BinAuctionView
        );
        assert_eq!(
            WindowKind::from_title("{\"italic\":false,\"extra\":[{\"text\":\"BIN Auction View\"}],\"text\":\"\"}"),
            WindowKind::BinAuctionView
        );
        assert_eq!(
            WindowKind::from_title("Confirm Purchase"),
            WindowKind::ConfirmPurchase
        );
    }

    #[test]
    fn test_slot_manager_mappings() {
        let manager = SlotManager::new();
        
        assert_eq!(
            manager.get_slot(&WindowKind::BinAuctionView, "purchase"),
            Some(31)
        );
        assert_eq!(
            manager.get_slot(&WindowKind::ConfirmPurchase, "confirm"),
            Some(11)
        );
        assert_eq!(
            manager.get_slot(&WindowKind::Bazaar, "confirm"),
            Some(13)
        );
    }
}
