use anyhow::{anyhow, Result};
use azalea::prelude::*;
use azalea::chat::SendChatEvent;
use azalea_protocol::packets::game::{
    s_container_click::ServerboundContainerClick,
    ClientboundGamePacket,
};
use azalea_inventory::operations::ClickType;
use bevy_app::AppExit;
use parking_lot::RwLock;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{info, error, debug};

use crate::types::BotState;
use super::handlers::BotEventHandlers;

/// Connection wait duration (seconds) - time to wait for bot connection to establish
const CONNECTION_WAIT_SECONDS: u64 = 2;

/// Main bot client wrapper for Azalea
/// 
/// Provides integration with azalea 0.15 for Minecraft bot functionality on Hypixel.
/// 
/// ## Key Features
/// 
/// - Microsoft authentication (azalea::Account::microsoft)
/// - Connection to Hypixel (mc.hypixel.net)
/// - Window packet handling (open_window, container_close)
/// - Chat message filtering (Coflnet messages)
/// - Window clicking with action counter (anti-cheat)
/// - NBT parsing for SkyBlock item IDs
/// 
/// ## References
/// 
/// - Original TypeScript: `/tmp/frikadellen-baf/src/BAF.ts`
/// - Azalea examples: https://github.com/azalea-rs/azalea/tree/main/azalea/examples
pub struct BotClient {
    /// Current bot state
    state: Arc<RwLock<BotState>>,
    /// Action counter for window clicks (anti-cheat)
    action_counter: Arc<RwLock<i16>>,
    /// Last window ID seen
    last_window_id: Arc<RwLock<u8>>,
    /// Event handlers
    handlers: Arc<BotEventHandlers>,
    /// Event sender channel
    event_tx: mpsc::UnboundedSender<BotEvent>,
    /// Event receiver channel
    event_rx: Arc<RwLock<mpsc::UnboundedReceiver<BotEvent>>>,
}

/// Events that can be emitted by the bot
#[derive(Debug, Clone)]
pub enum BotEvent {
    /// Bot logged in successfully
    Login,
    /// Bot spawned in world
    Spawn,
    /// Chat message received
    ChatMessage(String),
    /// Window opened (window_id, window_type, title)
    WindowOpen(u8, String, String),
    /// Window closed
    WindowClose,
    /// Bot disconnected (reason)
    Disconnected(String),
    /// Bot kicked (reason)
    Kicked(String),
}

impl BotClient {
    /// Create a new bot client instance
    pub fn new() -> Self {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        
        Self {
            state: Arc::new(RwLock::new(BotState::GracePeriod)),
            action_counter: Arc::new(RwLock::new(1)),
            last_window_id: Arc::new(RwLock::new(0)),
            handlers: Arc::new(BotEventHandlers::new()),
            event_tx,
            event_rx: Arc::new(RwLock::new(event_rx)),
        }
    }

    /// Connect to Hypixel with Microsoft authentication
    /// 
    /// Uses azalea 0.15 ClientBuilder API to:
    /// - Authenticate with Microsoft account
    /// - Connect to mc.hypixel.net
    /// - Set up event handlers for chat, window, and inventory events
    /// 
    /// # Example
    /// 
    /// ```no_run
    /// use frikadellen_baf::bot::client::BotClient;
    /// 
    /// #[tokio::main]
    /// async fn main() {
    ///     let mut bot = BotClient::new();
    ///     bot.connect("email@example.com".to_string()).await.unwrap();
    /// }
    /// ```
    pub async fn connect(&mut self, username: String) -> Result<()> {
        info!("Connecting to Hypixel as: {}", username);
        
        // Set state to startup
        self.set_state(BotState::Startup);
        
        // Authenticate with Microsoft
        let account = Account::microsoft(&username)
            .await
            .map_err(|e| anyhow!("Failed to authenticate with Microsoft: {}", e))?;
        
        info!("Microsoft authentication successful");
        
        // Create the handler state
        let handler_state = BotClientState {
            bot_state: self.state.clone(),
            handlers: self.handlers.clone(),
            event_tx: self.event_tx.clone(),
            action_counter: self.action_counter.clone(),
            last_window_id: self.last_window_id.clone(),
        };
        
        // Build and start the client (this blocks until disconnection)
        let handler_state_clone = handler_state.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new()
                .expect("Failed to create tokio runtime for bot - this should never happen unless system resources are exhausted");
            rt.block_on(async move {
                let exit_result = ClientBuilder::new()
                    .set_handler(event_handler)
                    .set_state(handler_state_clone)
                    .start(account, "mc.hypixel.net")
                    .await;
                    
                match exit_result {
                    AppExit::Success => {
                        info!("Bot disconnected successfully");
                    }
                    AppExit::Error(code) => {
                        error!("Bot exited with error code: {:?}", code);
                    }
                }
            });
        });
        
        // Wait for connection to establish
        tokio::time::sleep(tokio::time::Duration::from_secs(CONNECTION_WAIT_SECONDS)).await;
        
        info!("Bot connection initiated");
        
        Ok(())
    }

    /// Get current bot state
    pub fn state(&self) -> BotState {
        *self.state.read()
    }

    /// Set bot state
    pub fn set_state(&self, new_state: BotState) {
        let old_state = *self.state.read();
        *self.state.write() = new_state;
        info!("Bot state changed: {:?} -> {:?}", old_state, new_state);
    }

    /// Get the event handlers
    pub fn handlers(&self) -> Arc<BotEventHandlers> {
        self.handlers.clone()
    }

    /// Wait for next event
    pub async fn next_event(&self) -> Option<BotEvent> {
        self.event_rx.write().recv().await
    }

    /// Get the current action counter value
    /// 
    /// The action counter is incremented with each window click to prevent
    /// server-side bot detection. This matches the TypeScript implementation's
    /// anti-cheat behavior.
    pub fn action_counter(&self) -> i16 {
        *self.action_counter.read()
    }

    /// Increment the action counter (for window clicks)
    pub fn increment_action_counter(&self) {
        *self.action_counter.write() += 1;
    }

    /// Get the last window ID
    pub fn last_window_id(&self) -> u8 {
        *self.last_window_id.read()
    }

    /// Set the last window ID
    pub fn set_last_window_id(&self, id: u8) {
        *self.last_window_id.write() = id;
    }

    /// Documentation for sending chat messages
    /// 
    /// **Important**: This method cannot be called directly because the azalea Client
    /// is not accessible from outside event handlers. Chat messages must be sent from
    /// within the event_handler where the Client is available.
    /// 
    /// # Example (within event_handler)
    /// 
    /// ```no_run
    /// # use azalea::prelude::*;
    /// # use azalea::chat::SendChatEvent;
    /// # async fn example(bot: Client) {
    /// // Inside the event handler:
    /// bot.ecs.lock().write_message(SendChatEvent {
    ///     entity: bot.entity,
    ///     content: "/bz".to_string(),
    /// });
    /// # }
    /// ```
    #[deprecated(note = "Cannot be called from outside event handlers. Use the Client directly within event_handler. See method documentation for example.")]
    pub async fn chat(&self, _message: &str) -> Result<()> {
        Err(anyhow!(
            "chat() cannot be called from outside event handlers. \
             The azalea Client is only accessible within event_handler. \
             See the method documentation for how to send chat messages."
        ))
    }

    /// Documentation for clicking window slots
    /// 
    /// **Important**: This method cannot be called directly because the azalea Client
    /// is not accessible from outside event handlers. Window clicks must be sent from
    /// within the event_handler where the Client is available.
    /// 
    /// # Arguments
    /// 
    /// * `slot` - The slot number to click (0-indexed)
    /// * `button` - Mouse button (0 = left, 1 = right, 2 = middle)
    /// * `click_type` - Click operation type (Pickup, ShiftClick, etc.)
    /// 
    /// # Example (within event_handler)
    /// 
    /// ```no_run
    /// # use azalea::prelude::*;
    /// # use azalea_protocol::packets::game::s_container_click::ServerboundContainerClick;
    /// # use azalea_inventory::operations::ClickType;
    /// # async fn example(bot: Client, window_id: i32, slot: i16) {
    /// // Inside the event handler:
    /// let packet = ServerboundContainerClick {
    ///     container_id: window_id,
    ///     state_id: 0,
    ///     slot_num: slot,
    ///     button_num: 0,
    ///     click_type: ClickType::Pickup,
    ///     changed_slots: Default::default(),
    ///     carried_item: azalea_protocol::packets::game::s_container_click::HashedStack(None),
    /// };
    /// bot.write_packet(packet);
    /// # }
    /// ```
    #[deprecated(note = "Cannot be called from outside event handlers. Use the Client directly within event_handler. See method documentation for example.")]
    pub async fn click_window(&self, _slot: i16, _button: u8, _click_type: ClickType) -> Result<()> {
        Err(anyhow!(
            "click_window() cannot be called from outside event handlers. \
             The azalea Client is only accessible within event_handler. \
             See the method documentation for how to send window click packets."
        ))
    }

    /// Click the purchase button (slot 31) in BIN Auction View
    /// 
    /// **Important**: See `click_window()` documentation. This method cannot be called
    /// from outside event handlers. Use the pattern shown there within event_handler.
    /// 
    /// The purchase button is at slot 31 (gold ingot) in Hypixel's BIN Auction View.
    #[deprecated(note = "Cannot be called from outside event handlers. See click_window() documentation.")]
    pub async fn click_purchase(&self, _price: u64) -> Result<()> {
        Err(anyhow!(
            "click_purchase() cannot be called from outside event handlers. \
             See click_window() documentation for how to send window click packets."
        ))
    }

    /// Click the confirm button (slot 11) in Confirm Purchase window
    /// 
    /// **Important**: See `click_window()` documentation. This method cannot be called
    /// from outside event handlers. Use the pattern shown there within event_handler.
    /// 
    /// The confirm button is at slot 11 (green stained clay) in Hypixel's Confirm Purchase window.
    #[deprecated(note = "Cannot be called from outside event handlers. See click_window() documentation.")]
    pub async fn click_confirm(&self, _price: u64, _item_name: &str) -> Result<()> {
        Err(anyhow!(
            "click_confirm() cannot be called from outside event handlers. \
             See click_window() documentation for how to send window click packets."
        ))
    }
}

impl Default for BotClient {
    fn default() -> Self {
        Self::new()
    }
}

/// State type for bot client event handler
#[derive(Clone, Component)]
pub struct BotClientState {
    pub bot_state: Arc<RwLock<BotState>>,
    pub handlers: Arc<BotEventHandlers>,
    pub event_tx: mpsc::UnboundedSender<BotEvent>,
    pub action_counter: Arc<RwLock<i16>>,
    pub last_window_id: Arc<RwLock<u8>>,
}

impl Default for BotClientState {
    fn default() -> Self {
        let (event_tx, _) = mpsc::unbounded_channel();
        Self {
            bot_state: Arc::new(RwLock::new(BotState::GracePeriod)),
            handlers: Arc::new(BotEventHandlers::new()),
            event_tx,
            action_counter: Arc::new(RwLock::new(1)),
            last_window_id: Arc::new(RwLock::new(0)),
        }
    }
}

/// Handle events from the Azalea client
async fn event_handler(
    bot: Client,
    event: Event,
    state: BotClientState,
) -> Result<()> {
    match event {
        Event::Login => {
            info!("Bot logged in successfully");
            if state.event_tx.send(BotEvent::Login).is_err() {
                debug!("Failed to send Login event - receiver dropped");
            }
            *state.bot_state.write() = BotState::Idle;
        }
        
        Event::Init => {
            info!("Bot initialized and spawned in world");
            if state.event_tx.send(BotEvent::Spawn).is_err() {
                debug!("Failed to send Spawn event - receiver dropped");
            }
        }
        
        Event::Chat(chat) => {
            let message = chat.message().to_string();
            state.handlers.handle_chat_message(&message).await;
            if state.event_tx.send(BotEvent::ChatMessage(message)).is_err() {
                debug!("Failed to send ChatMessage event - receiver dropped");
            }
        }
        
        Event::Packet(packet) => {
            // Handle specific packets for window open/close and inventory updates
            match packet.as_ref() {
                ClientboundGamePacket::OpenScreen(open_screen) => {
                    let window_id = open_screen.container_id;
                    let window_type = format!("{:?}", open_screen.menu_type);
                    let title = open_screen.title.to_string();
                    
                    // Parse the title from JSON format
                    let parsed_title = state.handlers.parse_window_title(&title);
                    
                    // Store window ID
                    *state.last_window_id.write() = window_id as u8;
                    
                    state.handlers.handle_window_open(window_id as u8, &window_type, &parsed_title).await;
                    if state.event_tx.send(BotEvent::WindowOpen(window_id as u8, window_type, parsed_title)).is_err() {
                        debug!("Failed to send WindowOpen event - receiver dropped");
                    }
                }
                
                ClientboundGamePacket::ContainerClose(_) => {
                    state.handlers.handle_window_close().await;
                    if state.event_tx.send(BotEvent::WindowClose).is_err() {
                        debug!("Failed to send WindowClose event - receiver dropped");
                    }
                }
                
                ClientboundGamePacket::ContainerSetSlot(_slot_update) => {
                    // Track inventory slot updates
                    debug!("Inventory slot updated");
                }
                
                ClientboundGamePacket::ContainerSetContent(_content) => {
                    // Track full inventory updates
                    debug!("Inventory content updated");
                }
                
                _ => {}
            }
        }
        
        Event::Disconnect(reason) => {
            info!("Bot disconnected: {:?}", reason);
            let reason_str = format!("{:?}", reason);
            if state.event_tx.send(BotEvent::Disconnected(reason_str)).is_err() {
                debug!("Failed to send Disconnected event - receiver dropped");
            }
        }
        
        _ => {}
    }
    
    Ok(())
}
