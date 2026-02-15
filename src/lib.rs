//! Frikadellen BAF (Bazaar Auction Flipper) for Hypixel Skyblock
//! 
//! A high-performance Minecraft bot for automated bazaar and auction house flipping.
//! Rust port of the original TypeScript implementation using the Azalea framework.

pub mod bot;
pub mod config;
pub mod gui;
pub mod handlers;
pub mod inventory;
pub mod logging;
pub mod state;
pub mod types;
pub mod utils;
pub mod websocket;

pub use bot::{BotClient, BotEvent, BotEventHandlers};
pub use types::{BotState, CommandPriority, CommandType, Flip, BazaarFlipRecommendation};
