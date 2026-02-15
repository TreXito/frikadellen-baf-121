pub mod window_handler;
pub mod slot_manager;

pub use window_handler::{WindowConfig, WindowHandler, WindowSlot, wait_for_window_with_timeout};
pub use slot_manager::{SlotManager, StandardSlot, WindowKind};
