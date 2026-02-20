use anyhow::{anyhow, Result};
use azalea::prelude::*;
use azalea_protocol::packets::game::{
    ClientboundGamePacket,
};
use azalea_inventory::operations::ClickType;
use azalea_client::chat::ChatPacket;
use bevy_app::AppExit;
use parking_lot::RwLock;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{info, error, debug, warn};

use crate::types::{BotState, QueuedCommand};
use crate::websocket::CoflWebSocket;
use super::handlers::BotEventHandlers;

/// Connection wait duration (seconds) - time to wait for bot connection to establish
const CONNECTION_WAIT_SECONDS: u64 = 2;

/// Delay after spawning in lobby before sending /play skyblock command
const LOBBY_COMMAND_DELAY_SECS: u64 = 1;

/// Delay after detecting SkyBlock join before teleporting to island
const ISLAND_TELEPORT_DELAY_SECS: u64 = 2;

/// Wait time for island teleport to complete
const TELEPORT_COMPLETION_WAIT_SECS: u64 = 3;

/// Timeout for waiting for SkyBlock join confirmation (seconds)
const SKYBLOCK_JOIN_TIMEOUT_SECS: u64 = 15;

/// Delay before clicking accept button in trade response window (milliseconds)
/// TypeScript waits to check for "Deal!" or "Warning!" messages before accepting
const TRADE_RESPONSE_DELAY_MS: u64 = 3400;

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
#[derive(Clone)]
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
    /// Event receiver channel (cloned for each listener)
    event_rx: Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<BotEvent>>>,
    /// Command sender channel (for sending commands to the bot)
    command_tx: mpsc::UnboundedSender<QueuedCommand>,
    /// Command receiver channel (for the event handler to receive commands)
    command_rx: Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<QueuedCommand>>>,
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
    /// Startup workflow completed - bot is ready to accept flips
    StartupComplete,
}

impl BotClient {
    /// Create a new bot client instance
    pub fn new() -> Self {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        
        Self {
            state: Arc::new(RwLock::new(BotState::GracePeriod)),
            action_counter: Arc::new(RwLock::new(1)),
            last_window_id: Arc::new(RwLock::new(0)),
            handlers: Arc::new(BotEventHandlers::new()),
            event_tx,
            event_rx: Arc::new(tokio::sync::Mutex::new(event_rx)),
            command_tx,
            command_rx: Arc::new(tokio::sync::Mutex::new(command_rx)),
        }
    }

    /// Connect to Hypixel with Microsoft authentication
    /// 
    /// Uses azalea 0.15 ClientBuilder API to:
    /// - Authenticate with Microsoft account
    /// - Connect to mc.hypixel.net
    /// - Set up event handlers for chat, window, and inventory events
    /// 
    /// # Arguments
    /// 
    /// * `username` - Ingame username for connection
    /// * `ws_client` - Optional WebSocket client for inventory uploads
    /// 
    /// # Example
    /// 
    /// ```no_run
    /// use frikadellen_baf::bot::BotClient;
    /// 
    /// #[tokio::main]
    /// async fn main() {
    ///     let mut bot = BotClient::new();
    ///     bot.connect("email@example.com".to_string(), None).await.unwrap();
    /// }
    /// ```
    pub async fn connect(&mut self, username: String, ws_client: Option<CoflWebSocket>) -> Result<()> {
        info!("Connecting to Hypixel as: {}", username);
        
        // Keep state at GracePeriod (matches TypeScript's initial `bot.state = 'gracePeriod'`).
        // GracePeriod allows commands – only the active startup-workflow state (Startup) blocks them.
        // State transitions:  GracePeriod -> Idle  (via Login timeout or chat detection)
        //                      -> Startup           (only if an active startup workflow runs)
        //                      -> Idle              (after startup workflow completes)
        
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
            command_rx: self.command_rx.clone(),
            joined_skyblock: Arc::new(RwLock::new(false)),
            teleported_to_island: Arc::new(RwLock::new(false)),
            skyblock_join_time: Arc::new(RwLock::new(None)),
            ws_client,
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
        self.event_rx.lock().await.recv().await
    }

    /// Send a command to the bot for execution
    /// 
    /// This queues a command to be executed by the bot event handler.
    /// Commands are processed in the context of the Azalea client where
    /// chat messages and window clicks can be sent.
    pub fn send_command(&self, command: QueuedCommand) -> Result<()> {
        self.command_tx.send(command)
            .map_err(|e| anyhow!("Failed to send command to bot: {}", e))
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
    /// # async fn example(bot: Client) {
    /// // Inside the event handler:
    /// bot.write_chat_packet("/bz");
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
    #[allow(dead_code)]
    pub action_counter: Arc<RwLock<i16>>,
    pub last_window_id: Arc<RwLock<u8>>,
    pub command_rx: Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<QueuedCommand>>>,
    /// Flag to track if we've joined SkyBlock
    pub joined_skyblock: Arc<RwLock<bool>>,
    /// Flag to track if we've teleported to island
    pub teleported_to_island: Arc<RwLock<bool>>,
    /// Time when we joined SkyBlock (for timeout detection)
    pub skyblock_join_time: Arc<RwLock<Option<tokio::time::Instant>>>,
    /// WebSocket client for sending messages (e.g., inventory uploads)
    pub ws_client: Option<CoflWebSocket>,
}

impl Default for BotClientState {
    fn default() -> Self {
        let (event_tx, _) = mpsc::unbounded_channel();
        let (_, command_rx) = mpsc::unbounded_channel();
        Self {
            bot_state: Arc::new(RwLock::new(BotState::GracePeriod)),
            handlers: Arc::new(BotEventHandlers::new()),
            event_tx,
            action_counter: Arc::new(RwLock::new(1)),
            last_window_id: Arc::new(RwLock::new(0)),
            command_rx: Arc::new(tokio::sync::Mutex::new(command_rx)),
            joined_skyblock: Arc::new(RwLock::new(false)),
            teleported_to_island: Arc::new(RwLock::new(false)),
            skyblock_join_time: Arc::new(RwLock::new(None)),
            ws_client: None,
        }
    }
}

/// Handle events from the Azalea client
async fn event_handler(
    bot: Client,
    event: Event,
    state: BotClientState,
) -> Result<()> {
    // Process any pending commands first
    // We use try_recv() to avoid blocking on command reception
    if let Ok(mut command_rx) = state.command_rx.try_lock() {
        if let Ok(command) = command_rx.try_recv() {
            // Execute the command
            execute_command(&bot, &command, &state).await;
        }
    }

    match event {
        Event::Login => {
            info!("Bot logged in successfully");
            if state.event_tx.send(BotEvent::Login).is_err() {
                debug!("Failed to send Login event - receiver dropped");
            }
            
            // Reset startup flags on (re)login so the startup sequence runs again.
            // Keep state at GracePeriod (allows commands), matching TypeScript where
            // 'gracePeriod' does NOT block flips – only 'startup' does.
            *state.joined_skyblock.write() = false;
            *state.teleported_to_island.write() = false;
            *state.skyblock_join_time.write() = None;
            
            // Keep GracePeriod state – allows commands/flips just like TypeScript.
            // Do NOT set to Startup here; Startup is reserved for an active startup workflow.
            *state.bot_state.write() = BotState::GracePeriod;

            // Spawn a 30-second startup-completion watchdog (matching TypeScript's ~5.5 s grace
            // period + runStartupWorkflow).  If the chat-based detection hasn't fired by then,
            // this guarantees the bot exits GracePeriod and becomes fully ready.
            {
                let bot_state_wd = state.bot_state.clone();
                let teleported_wd = state.teleported_to_island.clone();
                let joined_wd = state.joined_skyblock.clone();
                let bot_wd = bot.clone();
                let event_tx_wd = state.event_tx.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;
                    let already_done = *teleported_wd.read();
                    if !already_done {
                        warn!("[Startup] 30-second watchdog: forcing startup completion");
                        *joined_wd.write() = true;
                        *teleported_wd.write() = true;
                        tokio::time::sleep(tokio::time::Duration::from_secs(ISLAND_TELEPORT_DELAY_SECS)).await;
                        bot_wd.write_chat_packet("/is");
                        tokio::time::sleep(tokio::time::Duration::from_secs(TELEPORT_COMPLETION_WAIT_SECS)).await;
                        info!("[Startup] Watchdog: state → Idle, bot ready to flip");
                        *bot_state_wd.write() = BotState::Idle;
                        let _ = event_tx_wd.send(BotEvent::StartupComplete);
                    }
                });
            }
        }
        
        Event::Init => {
            info!("Bot initialized and spawned in world");
            if state.event_tx.send(BotEvent::Spawn).is_err() {
                debug!("Failed to send Spawn event - receiver dropped");
            }
            
            // Check if we've already joined SkyBlock
            let joined_skyblock = *state.joined_skyblock.read();
            
            if !joined_skyblock {
                // First spawn -- we're in the lobby, join SkyBlock
                info!("Joining Hypixel SkyBlock...");
                
                // Spawn a task to send the command after delay (non-blocking)
                let bot_clone = bot.clone();
                let skyblock_join_time = state.skyblock_join_time.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(tokio::time::Duration::from_secs(LOBBY_COMMAND_DELAY_SECS)).await;
                    bot_clone.write_chat_packet("/play skyblock");
                });
                
                // Set the join time for timeout tracking
                *skyblock_join_time.write() = Some(tokio::time::Instant::now());
            }
            // Note: startup-completion watchdog is spawned from Event::Login,
            // which fires reliably after the bot is authenticated and in the game.
        }
        
        Event::Chat(chat) => {
            // Filter out overlay messages (action bar - e.g., health/defense/mana stats)
            let is_overlay = matches!(chat, ChatPacket::System(ref packet) if packet.overlay);
            
            if is_overlay {
                // Skip overlay messages - they spam the logs with stats updates
                return Ok(());
            }
            
            let message = chat.message().to_string();
            state.handlers.handle_chat_message(&message).await;
            if state.event_tx.send(BotEvent::ChatMessage(message.clone())).is_err() {
                debug!("Failed to send ChatMessage event - receiver dropped");
            }
            
            // Check if we've teleported to island yet
            let teleported = *state.teleported_to_island.read();
            let join_time = *state.skyblock_join_time.read();
            
            // Look for messages indicating we're in SkyBlock and should go to island
            if let Some(join_time) = join_time {
                if !teleported {
                    // Check for timeout (if we've been waiting too long, try anyway)
                    let should_timeout = join_time.elapsed() > tokio::time::Duration::from_secs(SKYBLOCK_JOIN_TIMEOUT_SECS);
                    
                    // Strip Minecraft color codes before checking for SkyBlock join messages
                    // Hypixel uses §-prefixed color codes (e.g. §aWelcome to Hypixel SkyBlock!)
                    let clean_message = crate::bot::handlers::BotEventHandlers::remove_color_codes(&message);
                    
                    // Check if message is a SkyBlock join confirmation
                    let skyblock_detected = {
                        // Primary welcome message - use starts_with after stripping color codes
                        if clean_message.starts_with("Welcome to Hypixel SkyBlock") {
                            true
                        }
                        // Profile selection messages like "[Profile] You are currently on: Your Island"
                        else if clean_message.starts_with("[Profile]") && clean_message.contains("currently") {
                            true
                        }
                        // Catch any other system messages about SKYBLOCK profile
                        // Convert to uppercase once for case-insensitive comparison
                        else if clean_message.starts_with("[") {
                            let upper = clean_message.to_uppercase();
                            upper.contains("SKYBLOCK") && upper.contains("PROFILE")
                        } else {
                            false
                        }
                    };
                    
                    if skyblock_detected || should_timeout {
                        // Mark as joined now that we've confirmed
                        *state.joined_skyblock.write() = true;
                        *state.teleported_to_island.write() = true;
                        
                        if should_timeout {
                            info!("Timeout waiting for SkyBlock confirmation - attempting to teleport to island anyway...");
                        } else {
                            info!("Detected SkyBlock join - teleporting to island...");
                        }
                        
                        // Spawn a task to handle teleportation (non-blocking)
                        let bot_clone = bot.clone();
                        let bot_state = state.bot_state.clone();
                        let event_tx_startup = state.event_tx.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(tokio::time::Duration::from_secs(ISLAND_TELEPORT_DELAY_SECS)).await;
                            bot_clone.write_chat_packet("/is");
                            
                            // Wait for teleport to complete
                            tokio::time::sleep(tokio::time::Duration::from_secs(TELEPORT_COMPLETION_WAIT_SECS)).await;
                            
                            // Now ready to process commands
                            info!("Bot initialization complete - ready to flip!");
                            *bot_state.write() = BotState::Idle;
                            let _ = event_tx_startup.send(BotEvent::StartupComplete);
                        });
                    }
                }
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
                    if state.event_tx.send(BotEvent::WindowOpen(window_id as u8, window_type.clone(), parsed_title.clone())).is_err() {
                        debug!("Failed to send WindowOpen event - receiver dropped");
                    }

                    // Handle window interactions based on current state and window title
                    handle_window_interaction(&bot, &state, window_id as u8, &parsed_title).await;
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

/// Execute a command from the command queue
async fn execute_command(
    bot: &Client,
    command: &QueuedCommand,
    state: &BotClientState,
) {
    use crate::types::CommandType;

    info!("Executing command: {:?}", command.command_type);

    match &command.command_type {
        CommandType::SendChat { message } => {
            // Send chat message to Minecraft
            info!("Sending chat message: {}", message);
            bot.write_chat_packet(message);
        }
        CommandType::PurchaseAuction { flip } => {
            // Send /viewauction command
            let uuid = flip.uuid.as_ref().map(|s| s.as_str()).unwrap_or("");
            let chat_command = format!("/viewauction {}", uuid);
            
            info!("Sending chat command: {}", chat_command);
            bot.write_chat_packet(&chat_command);
            
            // Set state to purchasing
            *state.bot_state.write() = BotState::Purchasing;
        }
        CommandType::BazaarBuyOrder { item_name, item_tag, amount: _, price_per_unit: _ } => {
            // Send /bz command with item tag or name
            let search_term = item_tag.as_ref().unwrap_or(item_name);
            let chat_command = format!("/bz {}", search_term);
            
            info!("Sending bazaar buy order command: {}", chat_command);
            bot.write_chat_packet(&chat_command);
            
            // Set state to bazaar
            *state.bot_state.write() = BotState::Bazaar;
        }
        CommandType::BazaarSellOrder { item_name, item_tag, amount: _, price_per_unit: _ } => {
            // Send /bz command with item tag or name
            let search_term = item_tag.as_ref().unwrap_or(item_name);
            let chat_command = format!("/bz {}", search_term);
            
            info!("Sending bazaar sell order command: {}", chat_command);
            bot.write_chat_packet(&chat_command);
            
            // Set state to bazaar
            *state.bot_state.write() = BotState::Bazaar;
        }
        // Advanced command types (matching TypeScript BAF.ts)
        CommandType::ClickSlot { slot } => {
            info!("Clicking slot {}", slot);
            // TypeScript: clicks slot in current window after checking trade display
            // For tradeResponse, TypeScript checks if window contains "Deal!" or "Warning!"
            // and waits before clicking to ensure trade window is fully loaded
            tokio::time::sleep(tokio::time::Duration::from_millis(TRADE_RESPONSE_DELAY_MS)).await;
            let window_id = *state.last_window_id.read();
            if window_id > 0 {
                click_window_slot(bot, window_id, *slot).await;
            } else {
                warn!("No window open (window_id=0), cannot click slot {}", slot);
            }
        }
        CommandType::SwapProfile { profile_name } => {
            info!("Swapping to profile: {}", profile_name);
            // TypeScript: sends /profiles command and clicks on profile
            bot.write_chat_packet("/profiles");
            // TODO: Implement profile selection from menu when window opens
            warn!("SwapProfile implementation incomplete - needs window interaction");
        }
        CommandType::AcceptTrade { player_name } => {
            info!("Accepting trade with player: {}", player_name);
            // TypeScript: sends /trade <player> command
            bot.write_chat_packet(&format!("/trade {}", player_name));
            // TODO: Implement trade window handling
            warn!("AcceptTrade implementation incomplete - needs trade window handling");
        }
        CommandType::SellToAuction { item_name, starting_bid, duration_hours } => {
            info!("Creating auction: {} at {} coins for {} hours", item_name, starting_bid, duration_hours);
            // TypeScript: opens /ah and creates auction
            bot.write_chat_packet("/ah");
            // TODO: Implement auction creation flow
            warn!("SellToAuction implementation incomplete - needs auction house window handling");
        }
        CommandType::UploadInventory => {
            info!("Uploading inventory to COFL");
            
            // Get the bot's inventory menu
            let inventory = bot.menu();
            
            // Serialize all slots to match mineflayer's bot.inventory.slots structure
            // In mineflayer, bot.inventory.slots is an array where null = empty slot
            // We need to send the exact same structure to COFL
            let slots_array: Vec<serde_json::Value> = inventory.slots().iter().enumerate().map(|(slot_num, item)| {
                // Empty slots become null in the JSON
                if item.is_empty() {
                    serde_json::Value::Null
                } else {
                    // Get the numeric item type ID (protocol ID)
                    let item_type = item.kind() as u32;
                    
                    // Serialize ItemStack to get component data (replaces NBT in 1.21+)
                    // This includes ExtraAttributes, enchantments, and other SkyBlock data
                    let nbt_data = if let Some(item_data) = item.as_present() {
                        // Serialize the ItemStackData which includes component_patch
                        match serde_json::to_value(item_data) {
                            Ok(value) => {
                                // ItemStackData serialization includes count, id, and components fields
                                // For COFL inventory upload, we only want the components field
                                // This avoids duplicate count/id fields at the root level
                                value.as_object()
                                    .and_then(|obj| obj.get("components").cloned())
                                    .unwrap_or(value)
                            }
                            Err(e) => {
                                warn!("Failed to serialize item component data for slot {}: {}", slot_num, e);
                                serde_json::Value::Null
                            }
                        }
                    } else {
                        serde_json::Value::Null
                    };
                    
                    serde_json::json!({
                        "type": item_type,  // Numeric item ID for protocol
                        "count": item.count(),
                        "metadata": 0,
                        "nbt": nbt_data,  // Component data serialized as NBT-compatible JSON
                        "name": item.kind().to_string(),
                        "slot": slot_num
                    })
                }
            }).collect();
            
            // Build the inventory object matching mineflayer's Window structure
            // Must match the Window class from prismarine-windows
            // This must exactly match bot.inventory structure from mineflayer
            // For SkyBlock, use "SKYBLOCK_MENU" type to include item data
            // Note: This bot is specifically designed for Hypixel SkyBlock and auto-joins
            // SkyBlock servers, so SKYBLOCK_MENU is always appropriate
            let inventory_json = serde_json::json!({
                "id": 0,  // Player inventory always has window ID 0
                "type": "SKYBLOCK_MENU",  // SkyBlock-specific type (was "minecraft:inventory")
                "title": "Inventory",  // Must match mineflayer: "Inventory" not "container.inventory"
                "slots": slots_array,
                "inventoryStart": 9,  // First inventory slot (after crafting)
                "inventoryEnd": 45,  // Last inventory slot + 1
                "hotbarStart": 36,  // First hotbar slot
                "craftingResultSlot": 0,  // Crafting output slot
                "requiresConfirmation": true,  // Standard for inventory
                "selectedItem": serde_json::Value::Null  // No item being held by cursor
            });
            
            // Send to websocket
            if let Some(ws) = &state.ws_client {
                match serde_json::to_string(&inventory_json) {
                    Ok(data_json) => {
                        let message = serde_json::json!({
                            "type": "uploadInventory",
                            "data": data_json
                        }).to_string();
                        
                        let ws_clone = ws.clone();
                        tokio::spawn(async move {
                            if let Err(e) = ws_clone.send_message(&message).await {
                                error!("Failed to upload inventory to websocket: {}", e);
                            } else {
                                info!("Uploaded inventory to COFL successfully");
                            }
                        });
                    }
                    Err(e) => {
                        error!("Failed to serialize inventory to JSON: {}", e);
                    }
                }
            } else {
                warn!("WebSocket client not available, cannot upload inventory");
            }
        }
        CommandType::ClaimSoldItem | CommandType::CheckCookie | 
        CommandType::DiscoverOrders | CommandType::ExecuteOrders => {
            info!("Command type not yet fully implemented in execute_command: {:?}", command.command_type);
        }
    }
}

/// Handle window interactions based on bot state and window title
async fn handle_window_interaction(
    bot: &Client,
    state: &BotClientState,
    window_id: u8,
    window_title: &str,
) {
    let bot_state = *state.bot_state.read();
    
    match bot_state {
        BotState::Purchasing => {
            // Handle auction house windows
            if window_title.contains("BIN Auction View") {
                info!("BIN Auction View opened - clicking purchase button (slot 31)");
                // Click slot 31 (purchase button)
                click_window_slot(bot, window_id, 31).await;
                
                // Wait a bit for confirmation window to open
                tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
            } else if window_title.contains("Confirm Purchase") {
                info!("Confirm Purchase window opened - clicking confirm button (slot 11)");
                // Click slot 11 (confirm button)
                click_window_slot(bot, window_id, 11).await;
                
                // Wait a bit for purchase to complete
                tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                
                // Purchase complete, go back to idle
                *state.bot_state.write() = BotState::Idle;
            }
        }
        BotState::Bazaar => {
            // Handle bazaar windows
            if window_title.contains("Bazaar") {
                info!("Bazaar window opened: {}", window_title);
                // TODO: Implement bazaar order placement flow
                // This involves:
                // 1. Clicking the correct item in search results (if search)
                // 2. Clicking "Create Buy Order" or "Create Sell Offer"
                // 3. Filling in the amount sign
                // 4. Filling in the price sign
                // 5. Clicking confirm buttons
                
                // For now, just go back to idle after a delay
                tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
                *state.bot_state.write() = BotState::Idle;
            }
        }
        _ => {
            // Not in a state that requires window interaction
        }
    }
}

/// Click a window slot
async fn click_window_slot(bot: &Client, window_id: u8, slot: i16) {
    use azalea_protocol::packets::game::s_container_click::{
        ServerboundContainerClick,
        HashedStack,
    };
    
    let packet = ServerboundContainerClick {
        container_id: window_id as i32,
        state_id: 0,
        slot_num: slot,
        button_num: 0,
        click_type: ClickType::Pickup,
        changed_slots: Default::default(),
        carried_item: HashedStack(None),
    };
    
    bot.write_packet(packet);
    info!("Clicked slot {} in window {}", slot, window_id);
}
