/// Inventory manager
/// 
/// Manages bot inventory state and operations.

use crate::types::ItemStack;

/// Inventory manager
pub struct InventoryManager {
    /// Current inventory slots
    slots: Vec<Option<ItemStack>>,
}

impl InventoryManager {
    /// Create a new inventory manager
    pub fn new() -> Self {
        Self {
            slots: vec![None; 45], // Standard Minecraft inventory size
        }
    }

    /// Get item at slot
    pub fn get_slot(&self, slot: usize) -> Option<&ItemStack> {
        self.slots.get(slot)?.as_ref()
    }

    /// Set item at slot
    pub fn set_slot(&mut self, slot: usize, item: Option<ItemStack>) {
        if slot < self.slots.len() {
            self.slots[slot] = item;
        }
    }

    /// Clear all slots
    pub fn clear(&mut self) {
        self.slots = vec![None; 45];
    }

    /// Find item by SkyBlock ID
    pub fn find_by_skyblock_id(&self, skyblock_id: &str) -> Option<usize> {
        for (i, slot) in self.slots.iter().enumerate() {
            if let Some(item) = slot {
                if let Some(id) = item.skyblock_id() {
                    if id == skyblock_id {
                        return Some(i);
                    }
                }
            }
        }
        None
    }
}

impl Default for InventoryManager {
    fn default() -> Self {
        Self::new()
    }
}
