use anyhow::{anyhow, Result};
use azalea::prelude::*;
use azalea_protocol::packets::game::{
    ClientboundGamePacket,
    c_set_display_objective::DisplaySlot,
    c_set_player_team::Method as TeamMethod,
    s_sign_update::ServerboundSignUpdate,
    s_container_close::ServerboundContainerClose,
};
use std::sync::atomic::{AtomicBool, Ordering};
use azalea_inventory::operations::ClickType;
use azalea_client::chat::ChatPacket;
use bevy_app::AppExit;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{info, error, debug, warn};

use crate::types::{BotState, QueuedCommand};
use crate::websocket::CoflWebSocket;
use super::handlers::BotEventHandlers;

/// Connection wait duration (seconds) - time to wait for bot connection to establish
const CONNECTION_WAIT_SECONDS: u64 = 2;

/// Delay after spawning in lobby before sending /play sb command
const LOBBY_COMMAND_DELAY_SECS: u64 = 3;

/// Delay after detecting SkyBlock join before teleporting to island
const ISLAND_TELEPORT_DELAY_SECS: u64 = 2;

/// Wait time for island teleport to complete
const TELEPORT_COMPLETION_WAIT_SECS: u64 = 3;

/// Timeout for waiting for SkyBlock join confirmation (seconds)
const SKYBLOCK_JOIN_TIMEOUT_SECS: u64 = 15;

/// Delay before clicking accept button in trade response window (milliseconds)
/// TypeScript waits to check for "Deal!" or "Warning!" messages before accepting
const TRADE_RESPONSE_DELAY_MS: u64 = 3400;
const FASTBUY_PRECLICK_DELAY_MS: u64 = 35;
const STARTUP_ENTRY_TIMEOUT_SECS: u64 = 60;
const BIN_PURCHASE_ITEM_KIND: &str = "gold_nugget";

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
    /// Scoreboard scores shared with BotClientState: objective_name -> (owner -> (display_text, score))
    scoreboard_scores: Arc<RwLock<HashMap<String, HashMap<String, (String, u32)>>>>,
    /// Which objective is displayed in the sidebar slot (shared with BotClientState)
    sidebar_objective: Arc<RwLock<Option<String>>>,
    /// Team data for scoreboard rendering: team_name -> (prefix, suffix, members)
    scoreboard_teams: Arc<RwLock<HashMap<String, (String, String, Vec<String>)>>>,
    /// Whether to use fastbuy (window-skip) when purchasing BIN auctions
    pub fastbuy: bool,
    /// Count of bazaar orders cancelled during startup order management
    manage_orders_cancelled: Arc<RwLock<u64>>,
    /// Set when "You reached your maximum of XY Bazaar orders!" is received.
    /// Cleared when an order fills (Claimed message detected).
    bazaar_at_limit: Arc<AtomicBool>,
    /// Cached player-inventory JSON (serialised Window object).
    /// Updated on every ContainerSetContent / ContainerSetSlot for the player
    /// inventory window (id 0) so that getInventory can be answered instantly,
    /// in parallel with any ongoing Hypixel interaction — matching TypeScript
    /// BAF.ts which calls `JSON.stringify(bot.inventory)` directly without
    /// waiting for a command-queue slot.
    cached_inventory_json: Arc<RwLock<Option<String>>>,
    /// AUTO_COOKIE config value passed through to BotClientState.
    auto_cookie_hours: Arc<RwLock<u64>>,
    /// Hidden config gate for purchaseAt bed timing mode.
    pub freemoney: bool,
    /// Item name to sell via bazaar "Sell Instantly" when inventory is full
    insta_sell_item: Arc<RwLock<Option<String>>>,
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
    StartupComplete {
        /// Number of bazaar orders cancelled during startup
        orders_cancelled: u64,
    },
    /// Item purchased from AH
    ItemPurchased { item_name: String, price: u64, buy_speed_ms: Option<u64> },
    /// Item sold on AH
    ItemSold { item_name: String, price: u64, buyer: String },
    /// Bazaar order placed successfully
    BazaarOrderPlaced {
        item_name: String,
        amount: u64,
        price_per_unit: f64,
        is_buy_order: bool,
    },
    /// AH BIN auction listed successfully
    AuctionListed {
        item_name: String,
        starting_bid: u64,
        duration_hours: u64,
    },
    /// A bazaar buy/sell order was fully filled and is ready to collect
    BazaarOrderFilled,
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
            scoreboard_scores: Arc::new(RwLock::new(HashMap::new())),
            sidebar_objective: Arc::new(RwLock::new(None)),
            scoreboard_teams: Arc::new(RwLock::new(HashMap::new())),
            fastbuy: false,
            manage_orders_cancelled: Arc::new(RwLock::new(0)),
            bazaar_at_limit: Arc::new(AtomicBool::new(false)),
            cached_inventory_json: Arc::new(RwLock::new(None)),
            auto_cookie_hours: Arc::new(RwLock::new(0)),
            freemoney: false,
            insta_sell_item: Arc::new(RwLock::new(None)),
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
            claiming_purchased: Arc::new(RwLock::new(false)),
            claim_sold_uuid: Arc::new(RwLock::new(None)),
            bazaar_item_name: Arc::new(RwLock::new(String::new())),
            bazaar_amount: Arc::new(RwLock::new(0)),
            bazaar_price_per_unit: Arc::new(RwLock::new(0.0)),
            bazaar_is_buy_order: Arc::new(RwLock::new(true)),
            bazaar_step: Arc::new(RwLock::new(BazaarStep::Initial)),
            auction_item_name: Arc::new(RwLock::new(String::new())),
            auction_starting_bid: Arc::new(RwLock::new(0)),
            auction_duration_hours: Arc::new(RwLock::new(24)),
            auction_item_slot: Arc::new(RwLock::new(None)),
            auction_item_id: Arc::new(RwLock::new(None)),
            auction_step: Arc::new(RwLock::new(AuctionStep::Initial)),
            scoreboard_scores: self.scoreboard_scores.clone(),
            sidebar_objective: self.sidebar_objective.clone(),
            scoreboard_teams: self.scoreboard_teams.clone(),
            fastbuy: self.fastbuy,
            manage_orders_cancelled: self.manage_orders_cancelled.clone(),
            bazaar_at_limit: self.bazaar_at_limit.clone(),
            purchase_start_time: Arc::new(RwLock::new(None)),
            last_buy_speed_ms: Arc::new(RwLock::new(None)),
            grace_period_spam_active: Arc::new(AtomicBool::new(false)),
            purchase_at_instant: Arc::new(RwLock::new(None)),
            bed_timing_active: Arc::new(AtomicBool::new(false)),
            cached_inventory_json: self.cached_inventory_json.clone(),
            auto_cookie_hours: self.auto_cookie_hours.clone(),
            freemoney: self.freemoney,
            cookie_time_secs: Arc::new(RwLock::new(0)),
            cookie_step: Arc::new(RwLock::new(CookieStep::Initial)),
            command_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            inventory_full: Arc::new(AtomicBool::new(false)),
            insta_sell_item: self.insta_sell_item.clone(),
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

    /// Set the AUTO_COOKIE hours threshold. Pass `config.auto_cookie` before calling `connect()`.
    pub fn set_auto_cookie_hours(&self, hours: u64) {
        *self.auto_cookie_hours.write() = hours;
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

    /// Get the current SkyBlock scoreboard sidebar lines as a JSON-serializable array.
    ///
    /// Returns the lines sorted by score (descending), matching the TypeScript
    /// `bot.scoreboard.sidebar.items.map(item => item.displayName.getText(null).replace(item.name, ''))`.
    ///
    /// Returns an empty Vec if the sidebar objective is not yet known.
    pub fn get_scoreboard_lines(&self) -> Vec<String> {
        let sidebar = self.sidebar_objective.read();
        let sidebar_name = match sidebar.as_ref() {
            Some(name) => name.clone(),
            None => return Vec::new(),
        };
        drop(sidebar);
        let scores = self.scoreboard_scores.read();
        let objective = match scores.get(&sidebar_name) {
            Some(obj) => obj,
            None => return Vec::new(),
        };
        // Sort entries by score descending (matches mineflayer sidebar order)
        let mut entries: Vec<(&String, &(String, u32))> = objective.iter().collect();
        entries.sort_by(|a, b| b.1.1.cmp(&a.1.1));
        // Build a member -> (prefix+suffix) lookup from team data for proper display
        let teams = self.scoreboard_teams.read();
        let mut member_display: HashMap<String, String> = HashMap::new();
        for (_, (prefix, suffix, members)) in teams.iter() {
            let text = format!("{}{}", prefix, suffix);
            for member in members {
                member_display.insert(member.clone(), text.clone());
            }
        }
        drop(teams);
        entries.iter().map(|(owner, (display, _))| {
            member_display.get(owner.as_str())
                .cloned()
                .unwrap_or_else(|| display.clone())
        }).collect()
    }

    /// Parse the player's current purse from the SkyBlock scoreboard sidebar.
    ///
    /// Looks for a line matching "Purse: X" or "Piggy: X" (Hypixel uses "Piggy" in
    /// certain areas). Strips color codes and commas before parsing.
    /// Matches TypeScript `getCurrentPurse()` in BAF.ts.
    pub fn get_purse(&self) -> Option<u64> {
        for line in self.get_scoreboard_lines() {
            let clean = remove_mc_colors(&line);
            let trimmed = clean.trim();
            for prefix in &["Purse: ", "Piggy: "] {
                if let Some(rest) = trimmed.strip_prefix(prefix) {
                    let num_str = rest
                        .split_whitespace()
                        .next()
                        .unwrap_or("")
                        .replace(',', "");
                    if let Ok(n) = num_str.parse::<u64>() {
                        return Some(n);
                    }
                }
            }
        }
        None
    }

    /// Return the last cached player-inventory JSON, if one has been built yet.
    ///
    /// The cache is updated on every `ContainerSetContent` and `ContainerSetSlot`
    /// packet for the player-inventory window so that it is always available without
    /// requiring a command-queue slot.  Matching TypeScript BAF.ts `getInventory`
    /// handler which calls `JSON.stringify(bot.inventory)` directly.
    pub fn get_cached_inventory_json(&self) -> Option<String> {
        self.cached_inventory_json.read().clone()
    }

    /// Returns true if the bazaar order limit has been hit and not yet cleared.
    pub fn is_bazaar_at_limit(&self) -> bool {
        self.bazaar_at_limit.load(Ordering::Relaxed)
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

/// Which step of the auction creation flow the bot is in.
/// Matches TypeScript's setPrice/durationSet flags in sellHandler.ts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AuctionStep {
    #[default]
    Initial,       // Just sent /ah, waiting for "Auction House"
    OpenManage,    // Clicked slot 15 in AH, waiting for "Manage Auctions"
    ClickCreate,   // Clicked "Create Auction" in Manage Auctions, waiting for "Create Auction"
    SelectBIN,     // Clicked slot 48 in "Create Auction", waiting for "Create BIN Auction"
    PriceSign,     // Clicked item + slot 31, sign expected (setPrice=false in TS)
    SetDuration,   // Price sign done; "Create BIN Auction" second visit → click slot 33
    DurationSign,  // "Auction Duration" opened + slot 16 clicked; sign expected for duration
    ConfirmSell,   // Duration sign done; "Create BIN Auction" third visit → click slot 29
    FinalConfirm,  // In "Confirm BIN Auction" → click slot 11
}


#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BazaarStep {
    #[default]
    Initial,
    SearchResults,
    SelectOrderType,
    SetAmount,
    SetPrice,
    Confirm,
}

/// Sub-steps within the BuyingCookie state.
/// Matches TypeScript cookieHandler.ts buyCookie() flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CookieStep {
    #[default]
    Initial,        // Sent /bz booster cookie, waiting for Bazaar window
    ItemDetail,     // Clicked cookie item (slot 11), waiting for detail window
    BuyConfirm,     // Clicked Buy Instantly (slot 10), waiting for confirm window
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
    /// true = claiming purchased item, false = claiming sold item
    pub claiming_purchased: Arc<RwLock<bool>>,
    /// UUID for direct ClaimSoldItem flow
    pub claim_sold_uuid: Arc<RwLock<Option<String>>>,
    // ---- Bazaar order context (set in execute_command, read in window/sign handlers) ----
    /// Item name for current bazaar order
    pub bazaar_item_name: Arc<RwLock<String>>,
    /// Amount for current bazaar order
    pub bazaar_amount: Arc<RwLock<u64>>,
    /// Price per unit for current bazaar order
    pub bazaar_price_per_unit: Arc<RwLock<f64>>,
    /// true = buy order, false = sell offer
    pub bazaar_is_buy_order: Arc<RwLock<bool>>,
    /// Which step of the bazaar flow we're in
    pub bazaar_step: Arc<RwLock<BazaarStep>>,
    // ---- Auction creation context (set in execute_command, read in window/sign handlers) ----
    /// Item name for current auction listing
    pub auction_item_name: Arc<RwLock<String>>,
    /// Starting bid for current auction
    pub auction_starting_bid: Arc<RwLock<u64>>,
    /// Duration in hours for current auction
    pub auction_duration_hours: Arc<RwLock<u64>>,
    /// Mineflayer inventory slot (9-44) for item to auction
    pub auction_item_slot: Arc<RwLock<Option<u64>>>,
    /// ExtraAttributes.id of item to auction (for identity verification)
    pub auction_item_id: Arc<RwLock<Option<String>>>,
    /// Which step of the auction creation flow we're in
    pub auction_step: Arc<RwLock<AuctionStep>>,
    /// Scoreboard scores: objective_name -> (owner -> (display_text, score))
    pub scoreboard_scores: Arc<RwLock<HashMap<String, HashMap<String, (String, u32)>>>>,
    /// Which objective is currently displayed in the sidebar slot
    pub sidebar_objective: Arc<RwLock<Option<String>>>,
    /// Team data for scoreboard rendering: team_name -> (prefix, suffix, members)
    pub scoreboard_teams: Arc<RwLock<HashMap<String, (String, String, Vec<String>)>>>,
    /// Whether to use fastbuy (window-skip) when purchasing BIN auctions
    pub fastbuy: bool,
    /// Count of bazaar orders cancelled during startup order management (shared with run_startup_workflow)
    pub manage_orders_cancelled: Arc<RwLock<u64>>,
    /// Set when Hypixel sends "You reached your maximum of XY Bazaar orders!".
    /// Cleared when an order fills. Prevents placing new orders while at the cap.
    pub bazaar_at_limit: Arc<AtomicBool>,
    /// Time when BIN Auction View opened — start of buy-speed measurement
    pub purchase_start_time: Arc<RwLock<Option<std::time::Instant>>>,
    /// Buy speed in ms: from BIN Auction View open to "Putting coins in escrow..."
    pub last_buy_speed_ms: Arc<RwLock<Option<u64>>>,
    /// Set to true while a grace-period spam-click loop is running so a second
    /// chat message does not start a duplicate loop.
    pub grace_period_spam_active: Arc<AtomicBool>,
    /// COFL-provided bed end timing (`purchaseAt`) converted to a local instant.
    pub purchase_at_instant: Arc<RwLock<Option<tokio::time::Instant>>>,
    /// Set to true while the bot is waiting for a bed (grace-period) to expire so
    /// the 5-second GUI watchdog does not incorrectly auto-close the BIN Auction View.
    pub bed_timing_active: Arc<AtomicBool>,
    /// Cached player-inventory JSON shared with BotClient for instant getInventory replies.
    pub cached_inventory_json: Arc<RwLock<Option<String>>>,
    /// AUTO_COOKIE config value (hours threshold to trigger a cookie buy). 0 = disabled.
    pub auto_cookie_hours: Arc<RwLock<u64>>,
    /// Hidden config gate for purchaseAt bed timing mode.
    pub freemoney: bool,
    /// Measured remaining cookie time in seconds (set during CheckingCookie).
    pub cookie_time_secs: Arc<RwLock<u64>>,
    /// Sub-step within the BuyingCookie flow.
    pub cookie_step: Arc<RwLock<CookieStep>>,
    /// Incremented at the start of every execute_command call so the 5-second GUI
    /// watchdog can detect whether a new command has started since the watched
    /// window was opened and skip the auto-close if so.
    pub command_generation: Arc<std::sync::atomic::AtomicU64>,
    /// Set when "[Bazaar] You don't have the space required to claim that!" is
    /// received.  The ManageOrders loop reads this flag to stop trying to collect
    /// and log the remaining orders to pending_claims.log.
    pub inventory_full: Arc<AtomicBool>,
    /// Item name to instasell via bazaar "Sell Instantly" when inventory is dominated
    /// by one stackable item type. Set by ManageOrders, consumed by InstaSelling handler.
    pub insta_sell_item: Arc<RwLock<Option<String>>>,
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
            claiming_purchased: Arc::new(RwLock::new(false)),
            claim_sold_uuid: Arc::new(RwLock::new(None)),
            bazaar_item_name: Arc::new(RwLock::new(String::new())),
            bazaar_amount: Arc::new(RwLock::new(0)),
            bazaar_price_per_unit: Arc::new(RwLock::new(0.0)),
            bazaar_is_buy_order: Arc::new(RwLock::new(true)),
            bazaar_step: Arc::new(RwLock::new(BazaarStep::Initial)),
            auction_item_name: Arc::new(RwLock::new(String::new())),
            auction_starting_bid: Arc::new(RwLock::new(0)),
            auction_duration_hours: Arc::new(RwLock::new(24)),
            auction_item_slot: Arc::new(RwLock::new(None)),
            auction_item_id: Arc::new(RwLock::new(None)),
            auction_step: Arc::new(RwLock::new(AuctionStep::Initial)),
            scoreboard_scores: Arc::new(RwLock::new(HashMap::new())),
            sidebar_objective: Arc::new(RwLock::new(None)),
            scoreboard_teams: Arc::new(RwLock::new(HashMap::new())),
            fastbuy: false,
            manage_orders_cancelled: Arc::new(RwLock::new(0)),
            bazaar_at_limit: Arc::new(AtomicBool::new(false)),
            purchase_start_time: Arc::new(RwLock::new(None)),
            last_buy_speed_ms: Arc::new(RwLock::new(None)),
            grace_period_spam_active: Arc::new(AtomicBool::new(false)),
            purchase_at_instant: Arc::new(RwLock::new(None)),
            bed_timing_active: Arc::new(AtomicBool::new(false)),
            cached_inventory_json: Arc::new(RwLock::new(None)),
            auto_cookie_hours: Arc::new(RwLock::new(0)),
            freemoney: false,
            cookie_time_secs: Arc::new(RwLock::new(0)),
            cookie_step: Arc::new(RwLock::new(CookieStep::Initial)),
            command_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            inventory_full: Arc::new(AtomicBool::new(false)),
            insta_sell_item: Arc::new(RwLock::new(None)),
        }
    }
}

impl BotClientState {
    /// Read the player's current purse from the scoreboard sidebar.
    /// Mirrors BotClient::get_purse() for use within window/sign handlers.
    /// Uses team prefix+suffix display text (same as get_scoreboard_lines()) because
    /// Hypixel SkyBlock stores sidebar text in teams, not in the score display field.
    fn get_purse(&self) -> Option<u64> {
        let sidebar = self.sidebar_objective.read().clone()?;
        let scores = self.scoreboard_scores.read();
        let objective = scores.get(&sidebar)?;
        // Build member → display text map from teams (prefix+suffix), identical to
        // get_scoreboard_lines() so that the purse line is found the same way.
        let teams = self.scoreboard_teams.read();
        let mut member_display: HashMap<String, String> = HashMap::with_capacity(teams.len());
        for (_, (prefix, suffix, members)) in teams.iter() {
            let text = format!("{}{}", prefix, suffix);
            for member in members {
                member_display.insert(member.clone(), text.clone());
            }
        }
        drop(teams);
        for (owner, (display, _)) in objective.iter() {
            let text = member_display.get(owner.as_str())
                .cloned()
                .unwrap_or_else(|| display.clone());
            let clean = remove_mc_colors(&text);
            for prefix in &["Purse: ", "Piggy: "] {
                if let Some(rest) = clean.trim().strip_prefix(prefix) {
                    let num = rest.split_whitespace().next().unwrap_or("").replace(',', "");
                    if let Ok(n) = num.parse::<u64>() {
                        return Some(n);
                    }
                }
            }
        }
        None
    }
}

/// Remove Minecraft §-prefixed color/format codes from a string
fn remove_mc_colors(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '§' {
            chars.next(); // skip the code character
        } else {
            result.push(c);
        }
    }
    result
}

/// Get the display name of an item slot as a plain string (no color codes).
/// Checks `minecraft:custom_name` first (custom-named items), then falls back
/// to `minecraft:item_name` (base item name override used by some Hypixel GUI items).
fn get_item_display_name_from_slot(item: &azalea_inventory::ItemStack) -> Option<String> {
    if let Some(item_data) = item.as_present() {
        if let Ok(value) = serde_json::to_value(item_data) {
            let components = value.get("components");
            // Try minecraft:custom_name first, then minecraft:item_name as fallback
            let name_val = components
                .and_then(|c| c.get("minecraft:custom_name"))
                .or_else(|| components.and_then(|c| c.get("minecraft:item_name")));
            if let Some(name_val) = name_val {
                let raw = if name_val.is_string() {
                    name_val.as_str().unwrap_or("").to_string()
                } else {
                    name_val.to_string()
                };
                // The name may be a JSON chat component string like {"text":"..."}
                let plain = if let Ok(json_val) = serde_json::from_str::<serde_json::Value>(&raw) {
                    extract_text_from_chat_component(&json_val)
                } else {
                    remove_mc_colors(&raw)
                };
                return Some(plain);
            }
        }
    }
    None
}

/// Recursively extract plain text from an Azalea/Minecraft chat component
fn extract_text_from_chat_component(val: &serde_json::Value) -> String {
    let mut result = String::new();
    if let Some(text) = val.get("text").and_then(|v| v.as_str()) {
        result.push_str(text);
    }
    if let Some(extra) = val.get("extra").and_then(|v| v.as_array()) {
        for part in extra {
            result.push_str(&extract_text_from_chat_component(part));
        }
    }
    remove_mc_colors(&result)
}

/// Get lore lines from an item slot as plain strings (no color codes)
fn get_item_lore_from_slot(item: &azalea_inventory::ItemStack) -> Vec<String> {
    let mut lore_lines = Vec::new();
    if let Some(item_data) = item.as_present() {
        if let Ok(value) = serde_json::to_value(item_data) {
            if let Some(lore_arr) = value
                .get("components")
                .and_then(|c| c.get("minecraft:lore"))
                .and_then(|l| l.as_array())
            {
                for entry in lore_arr {
                    let raw = if entry.is_string() {
                        entry.as_str().unwrap_or("").to_string()
                    } else {
                        entry.to_string()
                    };
                    let plain = if let Ok(json_val) = serde_json::from_str::<serde_json::Value>(&raw) {
                        extract_text_from_chat_component(&json_val)
                    } else {
                        remove_mc_colors(&raw)
                    };
                    lore_lines.push(plain);
                }
            }
        }
    }
    lore_lines
}

/// Find the first slot index matching the given name (case-insensitive)
/// Format a f64 price with comma-separated thousands for sign input.
/// e.g. 60000000.2 → "60,000,000.2", 8.0 → "8.0", 1234567.89 → "1,234,567.9"
fn format_price_for_sign(price: f64) -> String {
    let rounded = (price * 10.0).round() / 10.0;
    let int_part = rounded.floor() as i64;
    let frac = (rounded - int_part as f64).abs();
    let int_str = int_part.to_string();
    // Insert commas every 3 digits from the right
    let digits: Vec<char> = int_str.chars().collect();
    let mut with_commas = String::new();
    let len = digits.len();
    for (i, &c) in digits.iter().enumerate() {
        if i > 0 && (len - i) % 3 == 0 {
            with_commas.push(',');
        }
        with_commas.push(c);
    }
    // Always show one decimal place
    let frac_digit = (frac * 10.0).round() as u32;
    format!("{}.{}", with_commas, frac_digit)
}

fn find_slot_by_name(slots: &[azalea_inventory::ItemStack], name: &str) -> Option<usize> {
    let name_lower = name.to_lowercase();
    for (i, item) in slots.iter().enumerate() {
        if let Some(display) = get_item_display_name_from_slot(item) {
            if display.to_lowercase().contains(&name_lower) {
                return Some(i);
            }
        }
    }
    None
}

/// Parse the remaining grace-period time in seconds from the bed item displayed in
/// slot 31 of the BIN Auction View.  Hypixel typically shows the time in the item's
/// lore as a "M:SS" or "MM:SS" pattern (e.g. "0:45", "1:00").
/// Returns `None` if no time can be extracted.
fn parse_bed_remaining_secs(item: &azalea_inventory::ItemStack) -> Option<u64> {
    let name = get_item_display_name_from_slot(item).unwrap_or_default();
    let lore = get_item_lore_from_slot(item);
    let all_text = std::iter::once(name).chain(lore).collect::<Vec<_>>().join(" ");
    parse_bed_remaining_secs_from_text(&all_text)
}

fn parse_bed_remaining_secs_from_text(all_text: &str) -> Option<u64> {
    // Match "M:SS" or "MM:SS" — the first such pattern is the time remaining
    let mut chars = all_text.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() {
            let mut minutes = String::from(c);
            while chars.peek().map(|x| x.is_ascii_digit()).unwrap_or(false) {
                minutes.push(chars.next().unwrap());
            }
            if chars.next() == Some(':') {
                let mut secs = String::new();
                for _ in 0..2 {
                    if let Some(d) = chars.next() {
                        if d.is_ascii_digit() { secs.push(d); } else { break; }
                    }
                }
                if secs.len() == 2 {
                    if let (Ok(m), Ok(s)) = (minutes.parse::<u64>(), secs.parse::<u64>()) {
                        if s < 60 {
                            return Some(m * 60 + s);
                        }
                    }
                }
            }
        }
    }
    // Match textual variants like "1m 5s"
    if let Ok(minute_second_re) =
        regex::Regex::new(r"(?i)\b(\d+)\s*m(?:in(?:ute)?s?)?\s*(\d+)\s*s(?:ec(?:ond)?s?)?\b")
    {
        if let Some(caps) = minute_second_re.captures(all_text) {
            if let (Some(m), Some(s)) = (caps.get(1), caps.get(2)) {
                if let (Ok(m), Ok(s)) = (m.as_str().parse::<u64>(), s.as_str().parse::<u64>()) {
                    if s < 60 {
                        return Some(m * 60 + s);
                    }
                }
            }
        }
    }
    // Match seconds-only variants like "59s" / "59 sec"
    if let Ok(second_only_re) = regex::Regex::new(r"(?i)\b(\d+)\s*s(?:ec(?:ond)?s?)?\b") {
        if let Some(caps) = second_only_re.captures(all_text) {
            if let Some(s) = caps.get(1) {
                if let Ok(s) = s.as_str().parse::<u64>() {
                    return Some(s);
                }
            }
        }
    }
    None
}

/// Returns true if the item is a claimable (sold/ended/expired) auction slot.
/// Matches TypeScript ingameMessageHandler claimableIndicators / activeIndicators.
fn is_claimable_auction_slot(item: &azalea_inventory::ItemStack) -> bool {
    let lore = get_item_lore_from_slot(item);
    if lore.is_empty() {
        return false;
    }
    let combined = lore.join("\n").to_lowercase();
    // Must have at least one claimable indicator (from TypeScript claimableIndicators)
    let has_claimable = combined.contains("sold!")
        || combined.contains("ended")
        || combined.contains("expired")
        || combined.contains("click to claim")
        || combined.contains("claim your");
    // Must NOT have active-auction indicators (from TypeScript activeIndicators)
    let is_active = combined.contains("ends in")
        || combined.contains("buy it now")
        || combined.contains("starting bid");
    has_claimable && !is_active
}

/// Returns true when Hypixel chat indicates the purchase flow is terminally invalid
/// and should be aborted immediately instead of waiting for the GUI watchdog timeout.
fn is_terminal_purchase_failure_message(message: &str) -> bool {
    message.contains("You didn't participate in this auction!")
        || message.contains("This auction wasn't found!")
        || message.contains("The auction wasn't found!")
        || message.contains("You cannot view this auction!")
        || message.contains("You cannot afford this auction!")
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
                let manage_orders_cancelled_wd = state.manage_orders_cancelled.clone();
                let auto_cookie_wd = state.auto_cookie_hours.clone();
                let command_generation_wd = state.command_generation.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;
                    let already_done = *teleported_wd.read();
                    if !already_done {
                        warn!("[Startup] 30-second watchdog: forcing startup completion");
                        *joined_wd.write() = true;
                        *teleported_wd.write() = true;
                        // Retry /play sb in case the initial attempt failed (lobby not ready)
                        bot_wd.write_chat_packet("/play sb");
                        // Wait for SkyBlock to load (5s) + island teleport delay combined
                        tokio::time::sleep(tokio::time::Duration::from_secs(5 + ISLAND_TELEPORT_DELAY_SECS)).await;
                        bot_wd.write_chat_packet("/is");
                        tokio::time::sleep(tokio::time::Duration::from_secs(TELEPORT_COMPLETION_WAIT_SECS)).await;
                        run_startup_workflow(bot_wd, bot_state_wd, event_tx_wd, manage_orders_cancelled_wd, auto_cookie_wd, command_generation_wd).await;
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
                    bot_clone.write_chat_packet("/play sb");
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

            // Detect purchase/sold messages and emit events
            let clean_message = crate::bot::handlers::BotEventHandlers::remove_color_codes(&message);

            if clean_message.contains("You purchased") && clean_message.contains("coins!") {
                // "You purchased <item> for <price> coins!"
                if let Some((item_name, price)) = parse_purchased_message(&clean_message) {
                    // Include the buy speed measured from BIN Auction View open to escrow message
                    let buy_speed_ms = state.last_buy_speed_ms.write().take();
                    let _ = state.event_tx.send(BotEvent::ItemPurchased { item_name, price, buy_speed_ms });
                }
            } else if clean_message.contains("Putting coins in escrow") {
                // "Putting coins in escrow..." — purchase accepted by server.
                // Calculate buy speed from when BIN Auction View opened (matching TypeScript).
                if let Some(start) = state.purchase_start_time.write().take() {
                    let speed_ms = start.elapsed().as_millis() as u64;
                    *state.last_buy_speed_ms.write() = Some(speed_ms);
                    let _ = state.event_tx.send(BotEvent::ChatMessage(
                        format!("§f[§4BAF§f]: §aAuction bought in {}ms", speed_ms)
                    ));
                    info!("[AH] Buy speed: {}ms", speed_ms);
                }
            } else if *state.bot_state.read() == BotState::Purchasing
                && is_terminal_purchase_failure_message(&clean_message)
            {
                // Abort immediately on terminal purchase failure messages so we don't keep a
                // stale purchasing window open for 5s and overlap the next queued command.
                let window_id = *state.last_window_id.read();
                warn!(
                    "[AH] Terminal purchase failure detected: \"{}\" — closing window {}",
                    clean_message, window_id
                );
                if window_id > 0 {
                    bot.write_packet(ServerboundContainerClose {
                        container_id: window_id as i32,
                    });
                }
                *state.bot_state.write() = BotState::Idle;
                state.grace_period_spam_active.store(false, Ordering::Relaxed);
                *state.purchase_start_time.write() = None;
                *state.purchase_at_instant.write() = None;
                state.bed_timing_active.store(false, Ordering::Relaxed);
            } else if clean_message.contains("[Auction]") && clean_message.contains("bought") && clean_message.contains("for") && clean_message.contains("coins") {
                // "[Auction] <buyer> bought <item> for <price> coins"
                if let Some((buyer, item_name, price)) = parse_sold_message(&clean_message) {
                    // Extract UUID if present
                    let uuid = extract_viewauction_uuid(&clean_message);
                    *state.claim_sold_uuid.write() = uuid;
                    let _ = state.event_tx.send(BotEvent::ItemSold { item_name, price, buyer });
                }
            } else if clean_message.contains("BIN Auction started for") {
                // "BIN Auction started for <item>!" — Hypixel's confirmation that our listing
                // was accepted.  Emit AuctionListed using the context stored in state.
                // This matches TypeScript sellHandler.ts messageListener pattern.
                let item = state.auction_item_name.read().clone();
                let bid  = *state.auction_starting_bid.read();
                let dur  = *state.auction_duration_hours.read();
                if !item.is_empty() {
                    info!("[Auction] Chat confirmed listing of \"{}\" @ {} coins ({}h)", item, bid, dur);
                    let _ = state.event_tx.send(BotEvent::AuctionListed {
                        item_name: item,
                        starting_bid: bid,
                        duration_hours: dur,
                    });
                }
            } else if clean_message.contains("This BIN sale is still in its grace period!") {
                // Hypixel rejected the buy click because the BIN is in its grace period,
                // but slot 31 already shows gold_nugget (not a bed).  Keep clicking every
                // 100 ms until the Confirm Purchase window opens — matches
                // AutoBuy.initBedSpam() which clicks whenever slotName === "gold_nugget".
                if *state.bot_state.read() == BotState::Purchasing {
                    let already_active = state.grace_period_spam_active.swap(true, Ordering::Relaxed);
                    if !already_active {
                        let bot_clone = bot.clone();
                        let window_id = *state.last_window_id.read();
                        let shared_window_id = state.last_window_id.clone();
                        let bot_state = state.bot_state.clone();
                        let spam_flag = state.grace_period_spam_active.clone();
                        info!("[AH] Grace period detected — starting bed spam ({} ms interval)", 100);
                        tokio::spawn(async move {
                            const CLICK_INTERVAL_MS: u64 = 100;
                            const MAX_FAILED_CLICKS: usize = 5;
                            let mut failed_clicks: usize = 0;
                            loop {
                                tokio::time::sleep(tokio::time::Duration::from_millis(CLICK_INTERVAL_MS)).await;
                                let current_window_id = *shared_window_id.read();
                                if current_window_id != window_id {
                                    info!(
                                        "[AH] Grace period spam: window changed ({} -> {}), stopping",
                                        window_id, current_window_id
                                    );
                                    break;
                                }
                                let current_kind = {
                                    let menu = bot_clone.menu();
                                    let slots = menu.slots();
                                    slots.get(31).map(|s| {
                                        if s.is_empty() { "air".to_string() }
                                        else { s.kind().to_string().to_lowercase() }
                                    }).unwrap_or_else(|| "air".to_string())
                                };
                                if current_kind.contains("air") {
                                    info!("[AH] Grace period spam: window closed");
                                    *bot_state.write() = BotState::Idle;
                                    break;
                                } else if current_kind.contains("gold_nugget") {
                                    // Grace period may still be active — keep clicking.
                                    // Reset failed counter: slot is correct, just waiting.
                                    failed_clicks = 0;
                                    click_window_slot(&bot_clone, window_id, 31).await;
                                } else {
                                    failed_clicks += 1;
                                    debug!("[AH] Grace period spam: slot 31 = {} (failed {}/{})", current_kind, failed_clicks, MAX_FAILED_CLICKS);
                                    if failed_clicks >= MAX_FAILED_CLICKS {
                                        warn!("[AH] Grace period spam stopped after {} failed clicks", failed_clicks);
                                        *bot_state.write() = BotState::Idle;
                                        break;
                                    }
                                }
                            }
                            spam_flag.store(false, Ordering::Relaxed);
                        });
                    }
                }
            }

            // Detect bazaar order limit ("You reached your maximum of XY Bazaar orders!")
            // and clear it when an order fills ("Claimed ... coins from ...").
            if clean_message.contains("You reached your maximum of") && clean_message.contains("Bazaar orders") {
                warn!("[Bazaar] Order limit reached — pausing bazaar flips until a slot frees up");
                state.bazaar_at_limit.store(true, Ordering::Relaxed);
            } else if clean_message.contains("[Bazaar]") && (clean_message.contains("coins from selling") || clean_message.contains("coins from buying")) {
                // An order filled — a slot is now free
                if state.bazaar_at_limit.load(Ordering::Relaxed) {
                    info!("[Bazaar] Order filled, clearing order-limit flag");
                    state.bazaar_at_limit.store(false, Ordering::Relaxed);
                }
            }

            // Detect "[Bazaar] Your Buy Order/Sell Offer for X was filled!" — trigger a
            // ManageOrders run so the filled items are collected promptly.
            if clean_message.contains("[Bazaar]")
                && clean_message.contains("was filled")
                && (clean_message.contains("Buy Order") || clean_message.contains("Sell Offer"))
            {
                info!("[BazaarOrders] Order fill notification — emitting BazaarOrderFilled");
                let _ = state.event_tx.send(BotEvent::BazaarOrderFilled);
            }

            // Detect "You don't have the space required to claim that!" and set the
            // inventory_full flag so ManageOrders can stop and log remaining orders.
            if clean_message.contains("don't have the space required to claim") {
                warn!("[ManageOrders] Inventory full — logging unclaimed orders");
                state.inventory_full.store(true, Ordering::Relaxed);
            }

            // Check if we've teleported to island yet
            let teleported = *state.teleported_to_island.read();
            let join_time = *state.skyblock_join_time.read();
            
            // Look for messages indicating we're in SkyBlock and should go to island
            if let Some(join_time) = join_time {
                if !teleported {
                    // Check for timeout (if we've been waiting too long, try anyway)
                    let should_timeout = join_time.elapsed() > tokio::time::Duration::from_secs(SKYBLOCK_JOIN_TIMEOUT_SECS);
                    
                    // Check if message is a SkyBlock join confirmation
                    let skyblock_detected = {
                        if clean_message.starts_with("Welcome to Hypixel SkyBlock") {
                            true
                        }
                        else if clean_message.starts_with("[Profile]") && clean_message.contains("currently") {
                            true
                        }
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
                        
                        // Spawn a task to handle teleportation and startup workflow (non-blocking)
                        let bot_clone = bot.clone();
                        let bot_state = state.bot_state.clone();
                        let event_tx_startup = state.event_tx.clone();
                        let manage_orders_cancelled_startup = state.manage_orders_cancelled.clone();
                        let auto_cookie_startup = state.auto_cookie_hours.clone();
                        let command_generation_startup = state.command_generation.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(tokio::time::Duration::from_secs(ISLAND_TELEPORT_DELAY_SECS)).await;
                            bot_clone.write_chat_packet("/is");
                            
                            // Wait for teleport to complete
                            tokio::time::sleep(tokio::time::Duration::from_secs(TELEPORT_COMPLETION_WAIT_SECS)).await;

                            run_startup_workflow(bot_clone, bot_state, event_tx_startup, manage_orders_cancelled_startup, auto_cookie_startup, command_generation_startup).await;
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

                    // Spawn a 5-second watchdog: if this window is still open in an
                    // interactive bot state after 5 s it is considered stuck and is
                    // closed automatically.  Matches user requirement "guis should
                    // autoclose if not used for over 5 seconds".
                    // Exception: bed (grace-period) timing — the BIN Auction View must
                    // stay open for up to 60 s while waiting for the grace period to end.
                    // Also skips if a newer command started since this window was opened
                    // (prevents a stale watchdog from interrupting a new command).
                    {
                        let wdog_bot   = bot.clone();
                        let wdog_wid   = window_id as u8;
                        let wdog_state = state.bot_state.clone();
                        let wdog_last  = state.last_window_id.clone();
                        let wdog_spam  = state.grace_period_spam_active.clone();
                        let wdog_bed   = state.bed_timing_active.clone();
                        let wdog_gen   = state.command_generation.clone();
                        let wdog_gen_at_open = state.command_generation.load(Ordering::SeqCst);
                        tokio::spawn(async move {
                            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                            let still_open  = *wdog_last.read() == wdog_wid;
                            let cur_state   = *wdog_state.read();
                            let is_bed      = wdog_bed.load(Ordering::Relaxed);
                            let is_interactive = matches!(cur_state,
                                BotState::Purchasing | BotState::Bazaar | BotState::Selling
                                | BotState::ClaimingPurchased | BotState::ClaimingSold
                            );
                            // Only fire if no new command started since this window was opened.
                            let gen_unchanged = wdog_gen.load(Ordering::SeqCst) == wdog_gen_at_open;
                            if still_open && is_interactive && !is_bed && gen_unchanged {
                                warn!("[GUI] Window {} open for >5 s in state {:?} — auto-closing", wdog_wid, cur_state);
                                wdog_bot.write_packet(ServerboundContainerClose {
                                    container_id: wdog_wid as i32,
                                });
                                *wdog_state.write() = BotState::Idle;
                                wdog_spam.store(false, Ordering::Relaxed);
                            }
                        });
                    }

                    // Handle window interactions based on current state and window title
                    handle_window_interaction(&bot, &state, window_id as u8, &parsed_title).await;
                }
                
                ClientboundGamePacket::ContainerClose(_) => {
                    // Clear grace-period spam and bed-timing flags so a new BIN Auction View
                    // can start fresh.
                    state.grace_period_spam_active.store(false, Ordering::Relaxed);
                    *state.purchase_at_instant.write() = None;
                    state.bed_timing_active.store(false, Ordering::Relaxed);
                    state.handlers.handle_window_close().await;
                    if state.event_tx.send(BotEvent::WindowClose).is_err() {
                        debug!("Failed to send WindowClose event - receiver dropped");
                    }
                }
                
                ClientboundGamePacket::ContainerSetSlot(_slot_update) => {
                    // Rebuild the cached player-inventory JSON whenever a slot changes.
                    // This keeps the cache up-to-date for instant getInventory replies
                    // (matching TypeScript: `bot.inventory` is always fresh mineflayer state).
                    rebuild_cached_inventory_json(&bot, &state);
                }
                
                ClientboundGamePacket::ContainerSetContent(_content) => {
                    // Rebuild the cached player-inventory JSON on full content updates.
                    rebuild_cached_inventory_json(&bot, &state);
                }

                ClientboundGamePacket::OpenSignEditor(pkt) => {
                    // Hypixel sends this when the bot clicks "Custom Amount", "Custom Price"
                    // (bazaar), slot 31 (auction price), or slot 16 in "Auction Duration".
                    // We respond immediately with ServerboundSignUpdate to write the value
                    // (matching TypeScript's bot._client.once('open_sign_entity')).
                    let bot_state = *state.bot_state.read();
                    if bot_state == BotState::Bazaar {
                        let step = *state.bazaar_step.read();
                        let pos = pkt.pos;
                        let is_front = pkt.is_front_text;

                        let text_to_write = match step {
                            BazaarStep::SetAmount => {
                                let amount = *state.bazaar_amount.read();
                                info!("[Bazaar] Sign opened for amount — writing: {}", amount);
                                amount.to_string()
                            }
                            BazaarStep::SetPrice => {
                                let price = *state.bazaar_price_per_unit.read();
                                let s = format_price_for_sign(price);
                                info!("[Bazaar] Sign opened for price — writing: {}", s);
                                s
                            }
                            BazaarStep::SelectOrderType => {
                                // Hypixel opened a sign directly after clicking "Create Sell/Buy Order"
                                // (direct-sign flow — no intermediate "Custom Price" GUI button).
                                // Treat this as the price sign (matching TypeScript behaviour where
                                // sell offers go straight to the price sign).
                                let price = *state.bazaar_price_per_unit.read();
                                let s = format_price_for_sign(price);
                                info!("[Bazaar] Sign opened at SelectOrderType (direct sign) — writing price: {}", s);
                                *state.bazaar_step.write() = BazaarStep::SetPrice;
                                s
                            }
                            _ => {
                                warn!("[Bazaar] Unexpected sign opened at step {:?}", step);
                                return Ok(());
                            }
                        };

                        // Sign format exactly matching TypeScript bazaarFlipHandler.ts:
                        // text1: the value (price or amount as plain string)
                        // text2: "^^^^^^^^^^^^^^^" hint arrows (from JSON extra["^^^^^^^^^^^^^^^"])
                        // text3, text4: empty
                        let packet = ServerboundSignUpdate {
                            pos,
                            is_front_text: is_front,
                            lines: [
                                text_to_write,
                                "^^^^^^^^^^^^^^^".to_string(),
                                String::new(),
                                String::new(),
                            ],
                        };
                        bot.write_packet(packet);
                    } else if bot_state == BotState::Selling {
                        // Auction sign handler — matches TypeScript's setAuctionDuration and
                        // bot._client.once('open_sign_entity') for price in sellHandler.ts
                        let step = *state.auction_step.read();
                        let pos = pkt.pos;
                        let is_front = pkt.is_front_text;

                        let (text_to_write, next_step) = match step {
                            AuctionStep::PriceSign => {
                                let price = *state.auction_starting_bid.read();
                                info!("[Auction] Sign opened for price — writing: {}", price);
                                (price.to_string(), AuctionStep::SetDuration)
                            }
                            AuctionStep::DurationSign => {
                                let hours = *state.auction_duration_hours.read();
                                info!("[Auction] Sign opened for duration — writing: {} hours", hours);
                                (hours.to_string(), AuctionStep::ConfirmSell)
                            }
                            _ => {
                                warn!("[Auction] Unexpected sign opened at step {:?}", step);
                                return Ok(());
                            }
                        };

                        *state.auction_step.write() = next_step;
                        let packet = ServerboundSignUpdate {
                            pos,
                            is_front_text: is_front,
                            lines: [
                                text_to_write,
                                String::new(),
                                String::new(),
                                String::new(),
                            ],
                        };
                        bot.write_packet(packet);
                    }
                }

                // ---- Scoreboard packets ----
                // Track scoreboard data from Hypixel SkyBlock sidebar.
                // The sidebar contains player purse, stats, etc. which COFL uses
                // to validate flip eligibility (e.g. purse check before buying).

                ClientboundGamePacket::SetDisplayObjective(pkt) => {
                    // Slot 1 = sidebar
                    if matches!(pkt.slot, DisplaySlot::Sidebar) {
                        *state.sidebar_objective.write() = Some(pkt.objective_name.clone());
                        debug!("[Scoreboard] Sidebar objective set to: {}", pkt.objective_name);
                    }
                }

                ClientboundGamePacket::SetScore(pkt) => {
                    // Store score entry: objective -> owner -> (display, score)
                    // Hypixel SkyBlock encodes sidebar text in the owner field;
                    // the optional display override is absent for most entries.
                    let display_text = pkt.display
                        .as_ref()
                        .and_then(|d| { let s = d.to_string(); if s.is_empty() { None } else { Some(s) } })
                        .unwrap_or_else(|| pkt.owner.clone());
                    state.scoreboard_scores
                        .write()
                        .entry(pkt.objective_name.clone())
                        .or_default()
                        .insert(pkt.owner.clone(), (display_text, pkt.score));
                }

                ClientboundGamePacket::ResetScore(pkt) => {
                    // Remove a score entry
                    let mut scores = state.scoreboard_scores.write();
                    if let Some(obj_name) = &pkt.objective_name {
                        if let Some(objective) = scores.get_mut(obj_name.as_str()) {
                            objective.remove(&pkt.owner);
                        }
                    } else {
                        // Remove from all objectives
                        for objective in scores.values_mut() {
                            objective.remove(&pkt.owner);
                        }
                    }
                }

                ClientboundGamePacket::SetPlayerTeam(pkt) => {
                    // Track team prefix/suffix for scoreboard display.
                    // Hypixel SkyBlock uses team-based encoding: the team prefix
                    // contains the actual visible text; the score entry owner (e.g. §y)
                    // is used only as a unique identifier.
                    let mut teams = state.scoreboard_teams.write();
                    match &pkt.method {
                        TeamMethod::Add((params, members)) => {
                            let prefix = params.player_prefix.to_string();
                            let suffix = params.player_suffix.to_string();
                            teams.insert(pkt.name.clone(), (prefix, suffix, members.clone()));
                        }
                        TeamMethod::Remove => {
                            teams.remove(&pkt.name);
                        }
                        TeamMethod::Change(params) => {
                            let prefix = params.player_prefix.to_string();
                            let suffix = params.player_suffix.to_string();
                            let (entry_prefix, entry_suffix, _) = teams
                                .entry(pkt.name.clone())
                                .or_insert_with(|| (String::new(), String::new(), Vec::new()));
                            *entry_prefix = prefix;
                            *entry_suffix = suffix;
                        }
                        TeamMethod::Join(members) => {
                            let (_, _, entry_members) = teams
                                .entry(pkt.name.clone())
                                .or_insert_with(|| (String::new(), String::new(), Vec::new()));
                            entry_members.extend(members.clone());
                        }
                        TeamMethod::Leave(members) => {
                            if let Some((_, _, entry_members)) = teams.get_mut(&pkt.name) {
                                let leaving: std::collections::HashSet<&String> = members.iter().collect();
                                entry_members.retain(|m| !leaving.contains(m));
                            }
                        }
                    }
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

    // Increment the command generation counter so the GUI watchdog knows a new
    // command has started and should not auto-close windows from a previous command.
    state.command_generation.fetch_add(1, Ordering::SeqCst);

    info!("Executing command: {:?}", command.command_type);

    match &command.command_type {
        CommandType::SendChat { message } => {
            // Send chat message to Minecraft
            info!("Sending chat message: {}", message);
            bot.write_chat_packet(message);
        }
        CommandType::PurchaseAuction { flip } => {
            // Send /viewauction command
            let uuid = match flip.uuid.as_deref().filter(|s| !s.is_empty()) {
                Some(u) => u,
                None => {
                    warn!("Cannot purchase auction for '{}': missing UUID", flip.item_name);
                    return;
                }
            };
            let chat_command = format!("/viewauction {}", uuid);
            
            info!("Sending chat command: {}", chat_command);
            bot.write_chat_packet(&chat_command);

            // Convert COFL purchaseAt to a local instant so bed timing can wait
            // for the exact grace-period end sent by COFL.
            let purchase_at_instant = flip.purchase_at_ms.and_then(|purchase_at_ms| {
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()
                    .map(|d| d.as_millis() as i64)?;
                if purchase_at_ms <= now_ms {
                    return Some(tokio::time::Instant::now());
                }
                let wait_ms = (purchase_at_ms - now_ms) as u64;
                Some(tokio::time::Instant::now() + tokio::time::Duration::from_millis(wait_ms))
            });
            *state.purchase_at_instant.write() = purchase_at_instant;
            
            // Set state to purchasing
            *state.bot_state.write() = BotState::Purchasing;
        }
        CommandType::BazaarBuyOrder { item_name, item_tag, amount, price_per_unit } => {
            // Store order context so window/sign handlers can use it
            *state.bazaar_item_name.write() = item_name.clone();
            *state.bazaar_amount.write() = *amount;
            *state.bazaar_price_per_unit.write() = *price_per_unit;
            *state.bazaar_is_buy_order.write() = true;
            *state.bazaar_step.write() = BazaarStep::Initial;

            // Use itemTag when available (skips search results page), else title-case itemName
            let search_term = item_tag.as_ref().map(|s| s.as_str())
                .unwrap_or_else(|| item_name.as_str());
            let cmd = if item_tag.is_some() {
                format!("/bz {}", search_term)
            } else {
                format!("/bz {}", crate::utils::to_title_case(search_term))
            };
            info!("Sending bazaar buy order command: {}", cmd);
            bot.write_chat_packet(&cmd);
            *state.bot_state.write() = BotState::Bazaar;
        }
        CommandType::BazaarSellOrder { item_name, item_tag, amount, price_per_unit } => {
            // Store order context so window/sign handlers can use it
            *state.bazaar_item_name.write() = item_name.clone();
            *state.bazaar_amount.write() = *amount;
            *state.bazaar_price_per_unit.write() = *price_per_unit;
            *state.bazaar_is_buy_order.write() = false;
            *state.bazaar_step.write() = BazaarStep::Initial;

            // Use itemTag when available, else title-case itemName
            let search_term = item_tag.as_ref().map(|s| s.as_str())
                .unwrap_or_else(|| item_name.as_str());
            let cmd = if item_tag.is_some() {
                format!("/bz {}", search_term)
            } else {
                format!("/bz {}", crate::utils::to_title_case(search_term))
            };
            info!("Sending bazaar sell order command: {}", cmd);
            bot.write_chat_packet(&cmd);
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
        CommandType::SellToAuction { item_name, starting_bid, duration_hours, item_slot, item_id } => {
            info!("Creating auction: {} at {} coins for {} hours", item_name, starting_bid, duration_hours);
            // Store context for window/sign handlers (matches TypeScript sellHandler.ts)
            *state.auction_item_name.write() = item_name.clone();
            *state.auction_starting_bid.write() = *starting_bid;
            *state.auction_duration_hours.write() = *duration_hours;
            *state.auction_item_slot.write() = *item_slot;
            *state.auction_item_id.write() = item_id.clone();
            *state.auction_step.write() = AuctionStep::Initial;
            // Open auction house — window handler takes over from here
            bot.write_chat_packet("/ah");
            *state.bot_state.write() = BotState::Selling;
        }
        CommandType::ClaimSoldItem => {
            *state.claiming_purchased.write() = false;
            let uuid = state.claim_sold_uuid.read().clone();
            if let Some(uuid) = uuid {
                info!("Claiming sold item via direct /viewauction {}", uuid);
                bot.write_chat_packet(&format!("/viewauction {}", uuid));
            } else {
                info!("Claiming sold items via /ah");
                bot.write_chat_packet("/ah");
            }
            *state.bot_state.write() = BotState::ClaimingSold;
        }
        CommandType::ClaimPurchasedItem => {
            *state.claiming_purchased.write() = true;
            *state.claim_sold_uuid.write() = None;
            info!("Claiming purchased item via /ah");
            bot.write_chat_packet("/ah");
            *state.bot_state.write() = BotState::ClaimingPurchased;
        }
        CommandType::CheckCookie => {
            let auto_cookie_hours = *state.auto_cookie_hours.read();
            if auto_cookie_hours == 0 {
                debug!("[Cookie] AUTO_COOKIE disabled — skipping check");
                return;
            }
            info!("[Cookie] Checking booster cookie status via /sbmenu...");
            *state.cookie_time_secs.write() = 0;
            *state.cookie_step.write() = CookieStep::Initial;
            bot.write_chat_packet("/sbmenu");
            *state.bot_state.write() = BotState::CheckingCookie;
        }
        CommandType::ManageOrders => {
            info!("[ManageOrders] Triggered by periodic check or order fill — opening /bz");
            *state.manage_orders_cancelled.write() = 0;
            state.inventory_full.store(false, Ordering::Relaxed);
            bot.write_chat_packet("/bz");
            *state.bot_state.write() = BotState::ManagingOrders;
        }
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
            if window_title.contains("BIN Auction View") {
                // Record buy-speed start time (matches TypeScript: purchaseStartTime = Date.now())
                *state.purchase_start_time.write() = Some(std::time::Instant::now());

                // Wait up to 500ms for slot 31 to be populated by ContainerSetContent.
                // Without this wait the click fires ~0.3ms after OpenScreen before the server
                // has sent the container contents, causing the click to land on an empty slot
                // and be silently ignored by Hypixel.  TypeScript uses itemLoad() which polls
                // every 1ms for up to FLIP_ACTION_DELAY*3 ms (default 450ms).
                let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(500);
                loop {
                    let slot_populated = {
                        let menu = bot.menu();
                        let slots = menu.slots();
                        slots.get(31).map(|s| !s.is_empty()).unwrap_or(false)
                    };
                    if slot_populated || tokio::time::Instant::now() >= deadline {
                        break;
                    }
                    tokio::time::sleep(tokio::time::Duration::from_millis(25)).await;
                }

                // Check item in slot 31 to decide on purchase strategy.
                // Matches AutoBuy.initBedSpam() + flipHandler.ts item-switch logic.
                let slot_31_kind = {
                    let menu = bot.menu();
                    let slots = menu.slots();
                    slots.get(31).map(|s| s.kind().to_string().to_lowercase()).unwrap_or_default()
                };

                if slot_31_kind.contains("bed") {
                    // Bed = auction is still in grace period.
                    // Signal the 5-second GUI watchdog to leave this window open.
                    state.bed_timing_active.store(true, Ordering::Relaxed);

                    const PRE_CLICK_LEAD_MS: u64 = 100; // start clicking this many ms before expiry
                    const BED_SPAM_INTERVAL_MS: u64 = 100;
                    const BED_TIMING_INTERVAL_MS: u64 = 20;
                    const MAX_FAILED_CLICKS: usize = 5;
                    let click_interval_ms = if state.freemoney {
                        BED_TIMING_INTERVAL_MS
                    } else {
                        BED_SPAM_INTERVAL_MS
                    };

                    if state.freemoney {
                        // Prefer COFL purchaseAt timing (grace-period end) when available.
                        let remaining_ms_from_purchase_at = state.purchase_at_instant.read()
                            .as_ref()
                            .and_then(|deadline| deadline.checked_duration_since(tokio::time::Instant::now()))
                            .map(|d| d.as_millis() as u64);

                        // Fallback: parse remaining seconds from the bed item in slot 31.
                        let remaining_secs = {
                            let menu = bot.menu();
                            let slots = menu.slots();
                            slots.get(31).and_then(|s| parse_bed_remaining_secs(s))
                        };

                        if let Some(remaining_ms) = remaining_ms_from_purchase_at {
                            let wait_ms = remaining_ms.saturating_sub(PRE_CLICK_LEAD_MS);
                            if wait_ms > 0 {
                                info!("[AH] Bed timing: using COFL purchaseAt — waiting {}ms before clicking", wait_ms);
                                let wait_deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(wait_ms);
                                loop {
                                    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                                    if tokio::time::Instant::now() >= wait_deadline {
                                        break;
                                    }
                                    let kind_now = {
                                        let menu = bot.menu();
                                        let slots = menu.slots();
                                        slots.get(31).map(|s| {
                                            if s.is_empty() { "air".to_string() }
                                            else { s.kind().to_string().to_lowercase() }
                                        }).unwrap_or_else(|| "air".to_string())
                                    };
                                    if !kind_now.contains("bed") {
                                        break;
                                    }
                                }
                            }
                            info!("[AH] Bed timing: entering rapid-click phase (~{}ms before purchaseAt)", PRE_CLICK_LEAD_MS);
                        } else if let Some(secs) = remaining_secs {
                            // Wait until PRE_CLICK_LEAD_MS before the grace period ends
                            let wait_ms = (secs * 1000).saturating_sub(PRE_CLICK_LEAD_MS);
                            if wait_ms > 0 {
                                info!("[AH] Bed timing: {}s remaining — waiting {}ms before clicking", secs, wait_ms);
                                // While waiting, poll every 200ms to bail early if bed disappears
                                let wait_deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(wait_ms);
                                loop {
                                    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                                    if tokio::time::Instant::now() >= wait_deadline {
                                        break;
                                    }
                                    // If the window changed state already, break out early
                                    let kind_now = {
                                        let menu = bot.menu();
                                        let slots = menu.slots();
                                        slots.get(31).map(|s| {
                                            if s.is_empty() { "air".to_string() }
                                            else { s.kind().to_string().to_lowercase() }
                                        }).unwrap_or_else(|| "air".to_string())
                                    };
                                    if !kind_now.contains("bed") {
                                        break;
                                    }
                                }
                            }
                            info!("[AH] Bed timing: entering rapid-click phase (~{}ms before expiry)", PRE_CLICK_LEAD_MS);
                        } else {
                            info!("[AH] Bed detected in slot 31 — time unknown, starting clicks ({}ms interval)", click_interval_ms);
                        }
                    } else {
                        info!("[AH] Bed detected in slot 31 — starting bed spam ({}ms interval)", click_interval_ms);
                    }

                    let bed_deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(70);
                    let mut failed_clicks: usize = 0;
                    loop {
                        tokio::time::sleep(tokio::time::Duration::from_millis(click_interval_ms)).await;

                        if tokio::time::Instant::now() >= bed_deadline {
                            warn!("[AH] Bed timing: grace period did not end — giving up");
                            state.bed_timing_active.store(false, Ordering::Relaxed);
                            *state.bot_state.write() = BotState::Idle;
                            return;
                        }

                        let current_kind = {
                            let menu = bot.menu();
                            let slots = menu.slots();
                            slots.get(31).map(|s| {
                                if s.is_empty() { "air".to_string() }
                                else { s.kind().to_string().to_lowercase() }
                            }).unwrap_or_else(|| "air".to_string())
                        };

                        if current_kind == "air" || current_kind.contains("air") {
                            // Window closed — stop
                            info!("[AH] Bed timing: window closed");
                            state.bed_timing_active.store(false, Ordering::Relaxed);
                            *state.bot_state.write() = BotState::Idle;
                            return;
                        } else if current_kind.contains("gold_nugget") {
                            // Grace period ended — click to purchase.
                            info!("[AH] Bed timing: gold_nugget appeared, clicking slot 31");
                            state.bed_timing_active.store(false, Ordering::Relaxed);
                            if *state.last_window_id.read() == window_id {
                                click_window_slot(bot, window_id, 31).await;
                            }
                            // Stay in Purchasing so Confirm Purchase handler fires
                            break;
                        } else if current_kind.contains("bed") {
                            // Still in grace period — pre-click so a packet arrives at the
                            // server right as the transition happens.
                            debug!("[AH] Bed timing: grace period active, pre-clicking slot 31");
                            if *state.last_window_id.read() == window_id {
                                click_window_slot(bot, window_id, 31).await;
                            }
                        } else {
                            // Unexpected slot state
                            failed_clicks += 1;
                            debug!("[AH] Bed timing: slot 31 = {} (failed {}/{})", current_kind, failed_clicks, MAX_FAILED_CLICKS);
                            if failed_clicks >= MAX_FAILED_CLICKS {
                                warn!("[AH] Bed timing: stopped after {} unexpected slot states", failed_clicks);
                                state.bed_timing_active.store(false, Ordering::Relaxed);
                                *state.bot_state.write() = BotState::Idle;
                                return;
                            }
                        }
                    }
                } else {
                    // Click slot 31 once (buy-now button).
                    // Single click only — double-clicking sends a second packet while Hypixel
                    // is already preparing the Confirm Purchase window, which confuses the server
                    // and adds ~300ms of unnecessary latency.
                    click_window_slot(bot, window_id, 31).await;

                    // Optional fastbuy (window-skip): pre-click confirm in the next window.
                    // If this packet is ignored/lost, the Confirm Purchase handler below still
                    // performs normal confirm clicks with retries.
                    if state.fastbuy && slot_31_kind.contains(BIN_PURCHASE_ITEM_KIND) {
                        // Small pre-click delay to let the slot-31 buy packet reach the server
                        // before we send the next-window confirm packet.
                        tokio::time::sleep(tokio::time::Duration::from_millis(FASTBUY_PRECLICK_DELAY_MS)).await;
                        let observed_window_id = *state.last_window_id.read();
                        if observed_window_id == window_id {
                            // Hypixel's confirm GUI for this click is the next container id.
                            // wrapping_add handles the u8 id rollover safely.
                            let next_window_id = observed_window_id.wrapping_add(1);
                            click_window_slot(bot, next_window_id, 11).await;
                        }
                    }
                }
            } else if window_title.contains("Confirm Purchase") {
                // Click slot 11 immediately — speed is everything on a low-latency VPS.
                // NO pre-delay (matches TypeScript: "NO delay here — speed is everything").
                click_window_slot(bot, window_id, 11).await;

                // Wait 100ms. On a VPS near Hypixel this is enough for the
                // server to process the click and close the window.
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

                // Safety retry loop: if the window is still open (click was lost or
                // the server needs more time), keep retrying every 250ms.
                // Matches TypeScript's while-loop retry pattern in flipHandler.ts.
                while state.handlers.current_window_title()
                    .as_deref()
                    .map(|t| t.contains("Confirm Purchase"))
                    .unwrap_or(false)
                {
                    click_window_slot(bot, window_id, 11).await;
                    tokio::time::sleep(tokio::time::Duration::from_millis(250)).await;
                }

                *state.bot_state.write() = BotState::Idle;
            }
        }
        BotState::Bazaar => {
            // Full bazaar order-placement flow matching TypeScript placeBazaarOrder().
            // Context (item_name, amount, price_per_unit, is_buy_order) was stored in
            // execute_command when the BazaarBuyOrder / BazaarSellOrder command ran.
            //
            // Steps:
            //  1. Search-results page  ("Bazaar" in title, step == Initial)
            //     → find the item by name, click it.
            //  2. Item-detail page  (has "Create Buy Order" / "Create Sell Offer" slot)
            //     → click the right button.
            //  3. Amount screen  (has "Custom Amount" slot, buy orders only)
            //     → click Custom Amount, then write sign.
            //  4. Price screen   (has "Custom Price" slot)
            //     → click Custom Price, then write sign.
            //  5. Confirm screen  (step == SetPrice, no other matching slot)
            //     → click slot 13.
            //
            // Sign writing is handled separately in the OpenSignEditor packet handler below.

            let item_name = state.bazaar_item_name.read().clone();
            let is_buy_order = *state.bazaar_is_buy_order.read();
            let current_step = *state.bazaar_step.read();

            info!("[Bazaar] Window: \"{}\" | step: {:?}", window_title, current_step);

            // Poll every 50ms for up to 1500ms for slots to be populated by ContainerSetContent.
            // Matching TypeScript's findAndClick() poll pattern (checks every 50ms, up to ~600ms).
            // This is more reliable than a fixed sleep because ContainerSetContent may arrive
            // at any time after OpenScreen.
            let poll_deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(1500);

            // Helper: read the current slots from the menu
            let read_slots = || {
                let menu = bot.menu();
                menu.slots()
            };

            // Determine which button name to look for on the item-detail page
            let order_btn_name = if is_buy_order { "Create Buy Order" } else { "Create Sell Offer" };

            // Step 2: Item-detail page — poll for the order-creation button.
            // Only relevant when we haven't clicked an order button yet (Initial or SearchResults).
            // Skipped for SelectOrderType and beyond because order buttons only appear on the
            // item-detail page, not on price/amount/confirm screens.
            if current_step == BazaarStep::Initial || current_step == BazaarStep::SearchResults {
                // Poll until we find either "Create Buy Order" or "Create Sell Offer"
                let order_button_slot = loop {
                    // Guard: if a newer window has opened this handler is stale — bail out.
                    if *state.last_window_id.read() != window_id {
                        debug!("[Bazaar] Window {} superseded during order-button poll, aborting", window_id);
                        return;
                    }
                    let slots = read_slots();
                    let buy_s  = find_slot_by_name(&slots, "Create Buy Order");
                    let sell_s = find_slot_by_name(&slots, "Create Sell Offer");
                    let found = if is_buy_order { buy_s } else { sell_s };
                    if found.is_some() {
                        break found;
                    }
                    // Also break early if we're on a search-results or amount/price screen
                    // (those don't have order buttons, no point waiting)
                    let has_custom_amount = find_slot_by_name(&slots, "Custom Amount").is_some();
                    let has_custom_price  = find_slot_by_name(&slots, "Custom Price").is_some();
                    if has_custom_amount || has_custom_price {
                        break None;
                    }
                    // Any window with "Bazaar" in the title is a search-results / category page —
                    // those never contain order buttons, so break immediately.
                    if window_title.contains("Bazaar") {
                        break None;
                    }
                    if tokio::time::Instant::now() >= poll_deadline {
                        // Log all non-empty slots for debugging
                        warn!("[Bazaar] Polling timed out waiting for \"{}\" in \"{}\"", order_btn_name, window_title);
                        for (i, item) in slots.iter().enumerate() {
                            if let Some(name) = get_item_display_name_from_slot(item) {
                                warn!("[Bazaar]   slot {}: {}", i, name);
                            }
                        }
                        break None;
                    }
                    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
                };

                if let Some(i) = order_button_slot {
                    // Final guard before clicking — reject if a newer window has taken over.
                    if *state.last_window_id.read() != window_id {
                        debug!("[Bazaar] Window {} superseded before order-button click, aborting", window_id);
                        return;
                    }
                    info!("[Bazaar] Item detail: clicking \"{}\" at slot {}", order_btn_name, i);
                    *state.bazaar_step.write() = BazaarStep::SelectOrderType;
                    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                    if *state.last_window_id.read() != window_id { return; }
                    click_window_slot(bot, window_id, i as i16).await;
                    return;
                }
            }

            // Step 1: Search-results page — "Bazaar" in title, step == Initial.
            // Handles both "Bazaar" (plain search) and "Bazaar ➜ "ItemName"" (filtered results)
            // where the item appears in the grid but order buttons are not yet visible.
            if window_title.contains("Bazaar") && current_step == BazaarStep::Initial {
                info!("[Bazaar] Search results: looking for \"{}\"", item_name);
                *state.bazaar_step.write() = BazaarStep::SearchResults;

                // Poll briefly for the item to appear in search results
                let found = loop {
                    if *state.last_window_id.read() != window_id { return; }
                    let slots = read_slots();
                    let f = find_slot_by_name(&slots, &item_name);
                    if f.is_some() || tokio::time::Instant::now() >= poll_deadline {
                        break f;
                    }
                    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
                };

                match found {
                    Some(i) => {
                        if *state.last_window_id.read() != window_id { return; }
                        info!("[Bazaar] Found item at slot {}", i);
                        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                        if *state.last_window_id.read() != window_id { return; }
                        click_window_slot(bot, window_id, i as i16).await;
                    }
                    None => {
                        warn!("[Bazaar] Item \"{}\" not found in search results; going idle", item_name);
                        *state.bot_state.write() = BotState::Idle;
                    }
                }
                return;
            }

            // For steps 3-5: poll for the relevant button (Custom Amount / Custom Price) for up
            // to 1500ms matching the order-button poll above.  A single fixed sleep is unreliable
            // because ContainerSetContent may arrive at any time after OpenScreen.
            let poll_deadline2 = tokio::time::Instant::now() + tokio::time::Duration::from_millis(1500);
            let (amount_slot, price_slot) = loop {
                if *state.last_window_id.read() != window_id { return; }
                let slots = read_slots();
                let ca = if is_buy_order { find_slot_by_name(&slots, "Custom Amount") } else { None };
                let cp = find_slot_by_name(&slots, "Custom Price");
                if ca.is_some() || cp.is_some() || tokio::time::Instant::now() >= poll_deadline2 {
                    break (ca, cp);
                }
                tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
            };

            // Step 3: Amount screen (buy orders only)
            if let (Some(i), true) = (amount_slot,
                is_buy_order && current_step == BazaarStep::SelectOrderType)
            {
                if *state.last_window_id.read() != window_id { return; }
                info!("[Bazaar] Amount screen: clicking Custom Amount at slot {}", i);
                *state.bazaar_step.write() = BazaarStep::SetAmount;
                click_window_slot(bot, window_id, i as i16).await;
                // Sign response is sent in the OpenSignEditor packet handler
            }
            // Step 4: Price screen
            else if let (Some(i), true) = (price_slot,
                current_step == BazaarStep::SelectOrderType || current_step == BazaarStep::SetAmount)
            {
                if *state.last_window_id.read() != window_id { return; }
                info!("[Bazaar] Price screen: clicking Custom Price at slot {}", i);
                *state.bazaar_step.write() = BazaarStep::SetPrice;
                click_window_slot(bot, window_id, i as i16).await;
                // Sign response is sent in the OpenSignEditor packet handler
            }
            // Step 5: Confirm screen — anything that opens after SetPrice
            else if current_step == BazaarStep::SetPrice {
                if *state.last_window_id.read() != window_id { return; }
                info!("[Bazaar] Confirm screen: clicking slot 13");
                *state.bazaar_step.write() = BazaarStep::Confirm;
                click_window_slot(bot, window_id, 13).await;

                // Wait briefly for the server to respond (limit message arrives asynchronously)
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

                if state.bazaar_at_limit.load(Ordering::Relaxed) {
                    warn!("[Bazaar] Order rejected (at limit) — not emitting BazaarOrderPlaced");
                } else {
                    let item = item_name.clone();
                    let amount = *state.bazaar_amount.read();
                    let price_per_unit = *state.bazaar_price_per_unit.read();
                    let _ = state.event_tx.send(BotEvent::BazaarOrderPlaced {
                        item_name: item,
                        amount,
                        price_per_unit,
                        is_buy_order,
                    });
                    info!("[Bazaar] ===== ORDER COMPLETE =====");
                }
                *state.bot_state.write() = BotState::Idle;
            }
        }
        BotState::InstaSelling => {
            // Sell a dominant inventory item via /bz → Sell Instantly to free space.
            // Triggered by ManageOrders when inventory is full and one item type occupies
            // more than half the player inventory slots.
            //
            // Flow (reuses bazaar_step for sub-state):
            //   Initial       — bazaar search page: find item by name, click it
            //   SearchResults — item detail page: find "Sell Instantly", click it
            //   SelectOrderType — confirmation/warning page: wait ≤5 s, confirm
            //
            // After confirmation the bot opens /bz and returns to ManagingOrders so the
            // collect loop can retry now that there is inventory space.
            let item_name = match state.insta_sell_item.read().clone() {
                Some(name) => name,
                None => {
                    warn!("[InstaSell] No item name stored, going idle");
                    *state.bot_state.write() = BotState::Idle;
                    return;
                }
            };

            // Abort if this window has already been superseded
            if *state.last_window_id.read() != window_id {
                return;
            }

            let step = *state.bazaar_step.read();
            info!("[InstaSell] Window: \"{}\" | step: {:?} | item: \"{}\"", window_title, step, item_name);

            tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
            if *state.last_window_id.read() != window_id { return; }

            if step == BazaarStep::Initial && window_title.contains("Bazaar") {
                // Search results: find the item by name and click it
                let poll_deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(1500);
                let item_slot = loop {
                    if *state.last_window_id.read() != window_id { return; }
                    let slots = bot.menu().slots();
                    if let Some(i) = find_slot_by_name(&slots, &item_name) {
                        break Some(i);
                    }
                    if tokio::time::Instant::now() >= poll_deadline { break None; }
                    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
                };
                match item_slot {
                    Some(i) => {
                        if *state.last_window_id.read() != window_id { return; }
                        info!("[InstaSell] Found \"{}\" at slot {}, clicking", item_name, i);
                        *state.bazaar_step.write() = BazaarStep::SearchResults;
                        click_window_slot(bot, window_id, i as i16).await;
                    }
                    None => {
                        warn!("[InstaSell] Item \"{}\" not found in bazaar search, going idle", item_name);
                        *state.insta_sell_item.write() = None;
                        *state.bot_state.write() = BotState::Idle;
                    }
                }
            } else if step == BazaarStep::SearchResults {
                // Item detail page: find "Sell Instantly" and click it
                let poll_deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(1500);
                let sell_slot = loop {
                    if *state.last_window_id.read() != window_id { return; }
                    let slots = bot.menu().slots();
                    if let Some(i) = find_slot_by_name(&slots, "Sell Instantly") {
                        break Some(i);
                    }
                    if tokio::time::Instant::now() >= poll_deadline { break None; }
                    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
                };
                match sell_slot {
                    Some(i) => {
                        if *state.last_window_id.read() != window_id { return; }
                        info!("[InstaSell] Clicking \"Sell Instantly\" at slot {}", i);
                        *state.bazaar_step.write() = BazaarStep::SelectOrderType;
                        click_window_slot(bot, window_id, i as i16).await;
                    }
                    None => {
                        warn!("[InstaSell] \"Sell Instantly\" not found, going idle");
                        *state.insta_sell_item.write() = None;
                        *state.bot_state.write() = BotState::Idle;
                    }
                }
            } else if step == BazaarStep::SelectOrderType {
                // Confirmation page (warning may be present for up to 5 seconds).
                // Wait up to 5 s for a "Confirm" button, then click it.
                info!("[InstaSell] Waiting up to 5s for confirm button...");
                let confirm_deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(5);
                let confirm_slot = loop {
                    if *state.last_window_id.read() != window_id { return; }
                    let slots = bot.menu().slots();
                    if let Some(i) = find_slot_by_name(&slots, "Confirm") {
                        break Some(i);
                    }
                    if tokio::time::Instant::now() >= confirm_deadline { break None; }
                    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                };
                if *state.last_window_id.read() != window_id { return; }
                match confirm_slot {
                    Some(i) => {
                        info!("[InstaSell] Clicking Confirm at slot {}", i);
                        click_window_slot(bot, window_id, i as i16).await;
                    }
                    None => {
                        // Confirm button did not appear within 5 s — the sell may have already
                        // completed silently (no warning shown) or failed.  Log and continue so
                        // ManageOrders can retry; avoid clicking a random slot.
                        warn!("[InstaSell] Confirm button not found after 5s — sell may have completed or failed");
                    }
                }
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                // Reset and return to ManagingOrders so collect can retry
                info!("[InstaSell] Complete — returning to ManageOrders");
                *state.insta_sell_item.write() = None;
                *state.bazaar_step.write() = BazaarStep::Initial;
                state.inventory_full.store(false, Ordering::Relaxed);
                *state.bot_state.write() = BotState::ManagingOrders;
                bot.write_chat_packet("/bz");
            }
        }
        BotState::ClaimingPurchased => {
            if window_title.contains("Auction House") {
                // Hardcoded slot 13 for "Your Bids" navigation — matches TypeScript clickWindow(bot, 13)
                tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
                info!("[ClaimPurchased] Auction House opened - clicking slot 13 (Your Bids)");
                click_window_slot(bot, window_id, 13).await;
            } else if window_title.contains("Your Bids") {
                info!("[ClaimPurchased] Your Bids opened - looking for Claim All or Sold item");
                // Wait for ContainerSetContent to arrive and populate slots
                tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
                let menu = bot.menu();
                let slots = menu.slots();
                let mut found = false;
                // First look for Claim All by name (most reliable, matches TypeScript pattern)
                if let Some(i) = find_slot_by_name(&slots, "Claim All") {
                    info!("[ClaimPurchased] Found Claim All at slot {}", i);
                    click_window_slot(bot, window_id, i as i16).await;
                    *state.bot_state.write() = BotState::Idle;
                    found = true;
                }
                if !found {
                    // Look for purchased item with "Status: Sold!" in lore (TypeScript pattern)
                    for (i, item) in slots.iter().enumerate() {
                        let lore = get_item_lore_from_slot(item);
                        let lore_lower = lore.join("\n").to_lowercase();
                        if lore_lower.contains("status:") && lore_lower.contains("sold") {
                            info!("[ClaimPurchased] Found purchased item with Sold status at slot {}", i);
                            click_window_slot(bot, window_id, i as i16).await;
                            // Stay in ClaimingPurchased — next window should be BIN Auction View
                            found = true;
                            break;
                        }
                    }
                    if !found {
                        info!("[ClaimPurchased] Nothing to claim, going idle");
                        *state.bot_state.write() = BotState::Idle;
                    }
                }
            } else if window_title.contains("BIN Auction View") || window_title.contains("Auction View") {
                info!("[ClaimPurchased] Auction View opened - clicking slot 31 to collect");
                tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                click_window_slot(bot, window_id, 31).await;
                *state.bot_state.write() = BotState::Idle;
            }
        }
        BotState::ClaimingSold => {
            if window_title.contains("Auction House") {
                info!("[ClaimSold] Auction House opened - looking for Manage Auctions");
                // Wait for ContainerSetContent to arrive and populate slots
                tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
                let menu = bot.menu();
                let slots = menu.slots();
                if let Some(i) = find_slot_by_name(&slots, "Manage Auctions") {
                    info!("[ClaimSold] Clicking Manage Auctions at slot {}", i);
                    click_window_slot(bot, window_id, i as i16).await;
                } else {
                    warn!("[ClaimSold] Manage Auctions not found, going idle");
                    *state.bot_state.write() = BotState::Idle;
                }
            } else if window_title.contains("Manage Auctions") {
                info!("[ClaimSold] Manage Auctions opened - looking for claimable items");
                // Wait for ContainerSetContent to arrive and populate slots
                tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
                let menu = bot.menu();
                let slots = menu.slots();
                // Look for Claim All first
                if let Some(i) = find_slot_by_name(&slots, "Claim All") {
                    info!("[ClaimSold] Clicking Claim All at slot {}", i);
                    click_window_slot(bot, window_id, i as i16).await;
                    // Claim All finishes everything — go idle
                    *state.bot_state.write() = BotState::Idle;
                } else {
                    // Look for first claimable item
                    let mut found = false;
                    for (i, item) in slots.iter().enumerate() {
                        if is_claimable_auction_slot(item) {
                            info!("[ClaimSold] Clicking claimable item at slot {}", i);
                            click_window_slot(bot, window_id, i as i16).await;
                            // Stay in ClaimingSold — Hypixel re-opens Manage Auctions after the detail
                            found = true;
                            break;
                        }
                    }
                    if !found {
                        info!("[ClaimSold] Nothing to claim, going idle");
                        *state.bot_state.write() = BotState::Idle;
                    }
                }
            } else if window_title.contains("BIN Auction View") || window_title.contains("Auction View") {
                info!("[ClaimSold] Auction detail opened - looking for Claim button");
                tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                let menu = bot.menu();
                let slots = menu.slots();
                if let Some(i) = find_slot_by_name(&slots, "Claim") {
                    info!("[ClaimSold] Clicking Claim at slot {}", i);
                    click_window_slot(bot, window_id, i as i16).await;
                } else {
                    info!("[ClaimSold] Clicking slot 31");
                    click_window_slot(bot, window_id, 31).await;
                }
                // Spawn a short watchdog: if Hypixel doesn't re-open Manage Auctions within
                // 1.5 s, transition to Idle so the command queue can proceed.
                let claim_state_ref = state.bot_state.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(tokio::time::Duration::from_millis(1500)).await;
                    if *claim_state_ref.read() == BotState::ClaimingSold {
                        info!("[ClaimSold] No follow-up window after 1.5s, going idle");
                        *claim_state_ref.write() = BotState::Idle;
                    }
                });
            }
        }
        BotState::Selling => {
            // Full auction creation flow matching TypeScript sellHandler.ts
            // Exact slot numbers from TypeScript: slot 15 (AH nav), slot 48 (BIN type),
            // slot 31 (price setter), slot 33 (duration), slot 29 (confirm), slot 11 (final confirm)

            // Wait for ContainerSetContent to populate slots
            tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

            let step = *state.auction_step.read();
            let item_name = state.auction_item_name.read().clone();
            let item_slot_opt = *state.auction_item_slot.read();
            let menu = bot.menu();
            let slots = menu.slots();

            info!("[Auction] Window: \"{}\" | step: {:?}", window_title, step);

            match step {
                AuctionStep::Initial => {
                    // "Auction House" opened — click slot 15 (nav to Manage Auctions)
                    if window_title.contains("Auction House") {
                        info!("[Auction] AH opened, clicking slot 15 (Manage Auctions nav)");
                        *state.auction_step.write() = AuctionStep::OpenManage;
                        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                        click_window_slot(bot, window_id, 15).await;
                    }
                }
                AuctionStep::OpenManage => {
                    // "Manage Auctions" opened — find "Create Auction" button by name
                    if window_title.contains("Manage Auctions") {
                        if let Some(i) = find_slot_by_name(&slots, "Create Auction") {
                            // Check if auction limit reached (TypeScript: check lore for "maximum number")
                            let lore = get_item_lore_from_slot(&slots[i]);
                            let lore_text = lore.join(" ").to_lowercase();
                            if lore_text.contains("maximum") || lore_text.contains("limit") {
                                warn!("[Auction] Maximum auction count reached, going idle");
                                *state.bot_state.write() = BotState::Idle;
                                return;
                            }
                            info!("[Auction] Clicking Create Auction at slot {}", i);
                            *state.auction_step.write() = AuctionStep::ClickCreate;
                            tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                            click_window_slot(bot, window_id, i as i16).await;
                        } else {
                            warn!("[Auction] Create Auction not found in Manage Auctions, going idle");
                            *state.bot_state.write() = BotState::Idle;
                        }
                    } else if window_title.contains("Create Auction") && !window_title.contains("BIN") {
                        // Co-op AH or similar: jumped directly to "Create Auction" — click slot 48 (BIN)
                        info!("[Auction] Skipped Manage Auctions, in Create Auction — clicking slot 48 (BIN)");
                        *state.auction_step.write() = AuctionStep::SelectBIN;
                        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                        click_window_slot(bot, window_id, 48).await;
                    } else if window_title.contains("Create BIN Auction") {
                        // Co-op AH opened "Create BIN Auction" directly (skipping Manage Auctions).
                        // Run the SelectBIN logic inline.
                        info!("[Auction] Co-op AH: jumped straight to Create BIN Auction, handling as SelectBIN");
                        let player_start = *menu.player_slots_range().start();
                        let target_slot = if let Some(mj_slot) = item_slot_opt {
                            if mj_slot >= 9 && mj_slot <= 44 {
                                let offset = (mj_slot as usize) - 9;
                                let ws = player_start + offset;
                                if ws < slots.len() && !slots[ws].is_empty() {
                                    Some(ws)
                                } else {
                                    find_slot_by_name(&slots, &item_name)
                                }
                            } else {
                                find_slot_by_name(&slots, &item_name)
                            }
                        } else {
                            find_slot_by_name(&slots, &item_name)
                        };
                        if let Some(i) = target_slot {
                            info!("[Auction] Co-op AH: clicking item at slot {}", i);
                            let item_to_carry = slots[i].clone();
                            tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                            click_window_slot(bot, window_id, i as i16).await;
                            tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
                            info!("[Auction] Co-op AH: clicking slot 31 (price setter)");
                            *state.auction_step.write() = AuctionStep::PriceSign;
                            click_window_slot_carrying(bot, window_id, 31, &item_to_carry).await;
                        } else {
                            warn!("[Auction] Co-op AH: item \"{}\" not found, going idle", item_name);
                            *state.bot_state.write() = BotState::Idle;
                        }
                    }
                }
                AuctionStep::ClickCreate => {
                    // "Create Auction" opened — click slot 48 (BIN auction type)
                    if window_title.contains("Create Auction") && !window_title.contains("BIN") {
                        info!("[Auction] Create Auction window opened, clicking slot 48 (BIN)");
                        *state.auction_step.write() = AuctionStep::SelectBIN;
                        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                        click_window_slot(bot, window_id, 48).await;
                    } else if window_title.contains("Create BIN Auction") {
                        // Hypixel sometimes opens "Create BIN Auction" directly after clicking
                        // "Create Auction" in Manage Auctions (skipping the type-select step).
                        // Run SelectBIN logic inline so the flow continues without getting stuck.
                        info!("[Auction] ClickCreate: jumped straight to Create BIN Auction, handling as SelectBIN");
                        let player_start = *menu.player_slots_range().start();
                        let target_slot = if let Some(mj_slot) = item_slot_opt {
                            if mj_slot >= 9 && mj_slot <= 44 {
                                let offset = (mj_slot as usize) - 9;
                                let ws = player_start + offset;
                                if ws < slots.len() && !slots[ws].is_empty() {
                                    Some(ws)
                                } else {
                                    find_slot_by_name(&slots, &item_name)
                                }
                            } else {
                                find_slot_by_name(&slots, &item_name)
                            }
                        } else {
                            find_slot_by_name(&slots, &item_name)
                        };
                        if let Some(i) = target_slot {
                            info!("[Auction] ClickCreate→SelectBIN: clicking item at slot {}", i);
                            let item_to_carry = slots[i].clone();
                            tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                            click_window_slot(bot, window_id, i as i16).await;
                            tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
                            info!("[Auction] ClickCreate→SelectBIN: clicking slot 31 (price setter)");
                            *state.auction_step.write() = AuctionStep::PriceSign;
                            click_window_slot_carrying(bot, window_id, 31, &item_to_carry).await;
                        } else {
                            warn!("[Auction] ClickCreate→SelectBIN: item \"{}\" not found, going idle", item_name);
                            *state.bot_state.write() = BotState::Idle;
                        }
                    }
                }
                AuctionStep::SelectBIN => {
                    // "Create BIN Auction" opened first time (setPrice=false in TS)
                    // Find item by slot or by name, click it, then click slot 31 for price sign
                    if window_title.contains("Create BIN Auction") {
                        // Calculate inventory slot: mineflayer_slot - 9 + window_player_start
                        let player_start = *menu.player_slots_range().start();
                        let target_slot = if let Some(mj_slot) = item_slot_opt {
                            // TypeScript: itemSlot = data.slot - bot.inventory.inventoryStart + sellWindow.inventoryStart
                            // mineflayer inventoryStart = 9; slots 9-44 are player inventory (36 slots)
                            if mj_slot >= 9 && mj_slot <= 44 {
                                let offset = (mj_slot as usize) - 9;
                                let ws = player_start + offset;
                                if ws < slots.len() && !slots[ws].is_empty() {
                                    info!("[Auction] Using computed slot {} for item (mj_slot={})", ws, mj_slot);
                                    Some(ws)
                                } else {
                                    info!("[Auction] Computed slot {} empty/invalid, falling back to name search", ws);
                                    find_slot_by_name(&slots, &item_name)
                                }
                            } else {
                                info!("[Auction] mj_slot {} out of expected range 9-44, falling back to name search", mj_slot);
                                find_slot_by_name(&slots, &item_name)
                            }
                        } else {
                            find_slot_by_name(&slots, &item_name)
                        };

                        if let Some(i) = target_slot {
                            info!("[Auction] Clicking item at slot {}", i);
                            let item_to_carry = slots[i].clone();
                            tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                            click_window_slot(bot, window_id, i as i16).await;
                            // Click slot 31 (price setter) — sign will open, handled in OpenSignEditor
                            tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
                            info!("[Auction] Clicking slot 31 (price setter)");
                            *state.auction_step.write() = AuctionStep::PriceSign;
                            click_window_slot_carrying(bot, window_id, 31, &item_to_carry).await;
                        } else {
                            warn!("[Auction] Item \"{}\" not found in Create BIN Auction window, going idle", item_name);
                            *state.bot_state.write() = BotState::Idle;
                        }
                    }
                }
                AuctionStep::SetDuration => {
                    // "Create BIN Auction" opened second time (setPrice=true, durationSet=false in TS)
                    // Click slot 33 to open "Auction Duration" window
                    if window_title.contains("Create BIN Auction") {
                        info!("[Auction] Price set, clicking slot 33 (duration)");
                        *state.auction_step.write() = AuctionStep::DurationSign;
                        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                        click_window_slot(bot, window_id, 33).await;
                    }
                }
                AuctionStep::DurationSign => {
                    // "Auction Duration" window opened — click slot 16 to open sign for duration
                    if window_title.contains("Auction Duration") {
                        info!("[Auction] Auction Duration window opened, clicking slot 16 (sign trigger)");
                        // Sign handler (OpenSignEditor) will fire and advance step to ConfirmSell
                        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                        click_window_slot(bot, window_id, 16).await;
                    }
                }
                AuctionStep::ConfirmSell => {
                    // "Create BIN Auction" opened third time (setPrice=true, durationSet=true in TS)
                    // Click slot 29 to proceed to "Confirm BIN Auction"
                    if window_title.contains("Create BIN Auction") {
                        info!("[Auction] Both price and duration set, clicking slot 29 (confirm item)");
                        *state.auction_step.write() = AuctionStep::FinalConfirm;
                        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                        click_window_slot(bot, window_id, 29).await;
                    }
                }
                AuctionStep::FinalConfirm => {
                    // "Confirm BIN Auction" window — click slot 11 to finalize.
                    // AuctionListed event is emitted from the chat handler when Hypixel sends
                    // "BIN Auction started for ..." (matches TypeScript sellHandler.ts).
                    if window_title.contains("Confirm BIN Auction") || window_title.contains("Confirm") {
                        info!("[Auction] Confirm BIN Auction window, clicking slot 11 (final confirm)");
                        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                        click_window_slot(bot, window_id, 11).await;
                        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                        info!("[Auction] ===== AUCTION CREATED =====");
                        *state.bot_state.write() = BotState::Idle;
                    }
                }
                // PriceSign step: no window interaction needed; sign handler does the work
                AuctionStep::PriceSign => {}
            }
        }
        BotState::ManagingOrders => {
            // Step 2/4 of startup: cancel all existing bazaar orders.
            // Mirrors TypeScript bazaarOrderManager.ts manageStartupOrders().
            // Flow: Bazaar window → click slot 50 (Manage Orders) → iterate orders → cancel each.
            if window_title.contains("Bazaar") && !window_title.contains("Manage Orders") && !window_title.contains("Bazaar Orders") {
                // Main bazaar page — click "Manage Orders" at slot 50
                info!("[ManageOrders] Bazaar window open, clicking Manage Orders (slot 50)");
                tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
                click_window_slot(bot, window_id, 50).await;
            } else if window_title.contains("Manage Orders") || window_title.contains("Your Orders") || window_title.contains("Bazaar Orders") {
                // Manage Orders window — cancel all existing orders one by one
                info!("[ManageOrders] Processing existing orders...");
                let mut cancelled: u64 = 0;
                // processed_items tracks items already cancelled so we don't loop forever
                let mut processed_items: std::collections::HashSet<String> = std::collections::HashSet::new();

                loop {
                    // Wait for ContainerSetContent to reflect latest state
                    tokio::time::sleep(tokio::time::Duration::from_millis(250)).await;

                    // If a newer window has opened (e.g. Hypixel auto-opened the next
                    // "Your Bazaar Orders" screen), stop this loop — the new handler will
                    // take over.  Without this check every concurrent ManageOrders loop
                    // would see the Cancel button in the newly-opened window and spam
                    // clicks into stale/closed container IDs.
                    if *state.last_window_id.read() != window_id {
                        debug!("[ManageOrders] Window {} superseded by window {}, stopping", window_id, *state.last_window_id.read());
                        break;
                    }

                    let slots = bot.menu().slots();

                    // Find the first order slot (BUY xxx / SELL xxx) not yet processed
                    let order_slot = slots.iter().enumerate().find_map(|(i, item)| {
                        if let Some(name) = get_item_display_name_from_slot(item) {
                            if (name.starts_with("BUY ") || name.starts_with("SELL "))
                                && !processed_items.contains(&name)
                            {
                                return Some((i, name));
                            }
                        }
                        None
                    });

                    match order_slot {
                        None => {
                            // No more unprocessed orders — done
                            info!("[ManageOrders] Done — cancelled {} order(s)", cancelled);
                            *state.manage_orders_cancelled.write() = cancelled;
                            *state.bot_state.write() = BotState::Idle;
                            break;
                        }
                        Some((i, order_name)) => {
                            // Mark as processed before clicking (prevents re-processing after cancel)
                            processed_items.insert(order_name.clone());
                            info!("[ManageOrders] Found order at slot {}: \"{}\"", i, order_name);

                            // Click the order to view its detail page
                            click_window_slot(bot, window_id, i as i16).await;

                            // Poll for a "Cancel" or "Collect" button (up to 3 seconds).
                            // In Hypixel, clicking an order slot updates the SAME window in-place
                            // (no new OpenScreen event) to show the order detail.
                            // Filled orders show a "Collect" button; open orders show "Cancel".
                            // Matches TypeScript bazaarOrderManager.ts manageStartupOrders().
                            let action_deadline =
                                tokio::time::Instant::now() + tokio::time::Duration::from_secs(3);
                            let mut cancel_slot: Option<usize> = None;
                            let mut collect_slot: Option<usize> = None;
                            loop {
                                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                                // Stop if a newer window superseded this one
                                if *state.last_window_id.read() != window_id {
                                    break;
                                }
                                let slots2 = bot.menu().slots();
                                // Check for Collect first (filled order) then Cancel (open order)
                                if let Some(cs) = find_slot_by_name(&slots2, "Collect") {
                                    collect_slot = Some(cs);
                                    break;
                                }
                                // Match "Cancel Order", "Cancel Buy Order", "Cancel Sell Offer", etc.
                                if let Some(cs) = find_slot_by_name(&slots2, "Cancel") {
                                    cancel_slot = Some(cs);
                                    break;
                                }
                                // If inventory just became full from this click (filled BUY order
                                // rejected by server) the detail view won't appear — break early
                                // instead of burning the full 3-second timeout.
                                if state.inventory_full.load(Ordering::Relaxed) && order_name.starts_with("BUY ") {
                                    break;
                                }
                                if tokio::time::Instant::now() >= action_deadline {
                                    warn!("[ManageOrders] No Collect/Cancel button found for \"{}\", skipping", order_name);
                                    break;
                                }
                            }

                            if let Some(cs) = collect_slot {
                                if *state.last_window_id.read() == window_id {
                                    // inventory_full only blocks BUY order collection (items need space).
                                    // SELL offers deliver coins which always fit — always collect those.
                                    let is_buy = order_name.starts_with("BUY ");
                                    if is_buy && state.inventory_full.load(Ordering::Relaxed) {
                                        warn!("[ManageOrders] Inventory full — cannot collect BUY items for \"{}\", checking for instasell", order_name);
                                        // Check if a single dominant item is taking up >half of inventory —
                                        // if so, switch to InstaSelling to free space, then retry.
                                        if let Some(dominant) = find_dominant_inventory_item(bot) {
                                            info!("[ManageOrders] Dominant item '{}' found (>50% of inventory) — instaselling to free space", dominant);
                                            *state.insta_sell_item.write() = Some(dominant.clone());
                                            *state.bazaar_step.write() = BazaarStep::Initial;
                                            *state.bot_state.write() = BotState::InstaSelling;
                                            bot.write_chat_packet(&format!("/bz {}", dominant));
                                            break; // break outer order loop; InstaSelling handler takes over
                                        }
                                        log_pending_claim(&order_name);
                                        // Still try remaining orders (cancel open ones) but skip collects
                                    } else {
                                        info!("[ManageOrders] Clicking Collect at slot {} (filled order: \"{}\")", cs, order_name);
                                        click_window_slot(bot, window_id, cs as i16).await;
                                        // Wait briefly, then check if inventory became full
                                        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                                        if is_buy && state.inventory_full.load(Ordering::Relaxed) {
                                            // Collect failed: check for instasell opportunity
                                            warn!("[ManageOrders] Inventory full after collect attempt for \"{}\"", order_name);
                                            if let Some(dominant) = find_dominant_inventory_item(bot) {
                                                info!("[ManageOrders] Dominant item '{}' found — instaselling to free space", dominant);
                                                *state.insta_sell_item.write() = Some(dominant.clone());
                                                *state.bazaar_step.write() = BazaarStep::Initial;
                                                *state.bot_state.write() = BotState::InstaSelling;
                                                bot.write_chat_packet(&format!("/bz {}", dominant));
                                                break;
                                            }
                                            log_pending_claim(&order_name);
                                        }
                                    }
                                }
                            } else if let Some(cs) = cancel_slot {
                                if *state.last_window_id.read() == window_id {
                                    info!("[ManageOrders] Clicking Cancel at slot {}", cs);
                                    click_window_slot(bot, window_id, cs as i16).await;
                                    cancelled += 1;
                                    // Wait for the window content to revert to the order list
                                    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                                }
                            } else if collect_slot.is_none() && cancel_slot.is_none()
                                && state.inventory_full.load(Ordering::Relaxed)
                                && order_name.starts_with("BUY ")
                            {
                                // Clicking the order triggered "inventory full" and no detail view
                                // opened (server rejected the collect). Try instasell if dominant item.
                                if let Some(dominant) = find_dominant_inventory_item(bot) {
                                    info!("[ManageOrders] No button + inventory full + dominant item '{}' — instaselling", dominant);
                                    *state.insta_sell_item.write() = Some(dominant.clone());
                                    *state.bazaar_step.write() = BazaarStep::Initial;
                                    *state.bot_state.write() = BotState::InstaSelling;
                                    bot.write_chat_packet(&format!("/bz {}", dominant));
                                    break;
                                }
                                log_pending_claim(&order_name);
                            }
                            // Loop continues to find remaining orders
                        }
                    }
                }
            }
        }
        BotState::CheckingCookie => {
            // Opened by /sbmenu — search every slot for a Booster Cookie buff indicator
            // (lore contains "Duration:"). Matches TypeScript cookieHandler.ts slot 51 check.
            info!("[Cookie] /sbmenu window opened — scanning for cookie buff...");
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
            let menu = bot.menu();
            let slots = menu.slots();
            let auto_cookie_hours = *state.auto_cookie_hours.read();

            let mut cookie_time_secs: Option<u64> = None;
            for item in slots.iter() {
                let lore = get_item_lore_from_slot(item);
                let lore_text = lore.join(" ");
                if lore_text.to_lowercase().contains("duration") {
                    let secs = parse_cookie_duration_secs(&lore_text);
                    cookie_time_secs = Some(secs);
                    break;
                }
            }

            // Close the SkyBlock menu before proceeding
            bot.write_packet(ServerboundContainerClose {
                container_id: window_id as i32,
            });
            tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

            match cookie_time_secs {
                None => {
                    // No duration found — either no cookie is active or menu didn't load
                    info!("[Cookie] Cookie duration not found in /sbmenu — skipping buy");
                    *state.bot_state.write() = BotState::Idle;
                }
                Some(secs) => {
                    let hours = secs / 3600;
                    let color = if hours >= auto_cookie_hours { "§a" } else { "§c" };
                    let _ = state.event_tx.send(BotEvent::ChatMessage(format!(
                        "§f[§4BAF§f]: §3Cookie time remaining: {}{}h§3 (threshold: {}h)",
                        color, hours, auto_cookie_hours
                    )));
                    info!("[Cookie] Cookie time: {}h, threshold: {}h", hours, auto_cookie_hours);
                    *state.cookie_time_secs.write() = secs;

                    if hours >= auto_cookie_hours {
                        info!("[Cookie] Cookie time sufficient — skipping buy");
                        *state.bot_state.write() = BotState::Idle;
                    } else {
                        // Need to buy a cookie — use get_purse() via scoreboard
                        let purse = state.get_purse().unwrap_or(0);
                        // Require at least 7.5M coins (1.5× 5M default price) before buying
                        const MIN_PURSE_FOR_COOKIE: u64 = 7_500_000;
                        if purse < MIN_PURSE_FOR_COOKIE {
                            let _ = state.event_tx.send(BotEvent::ChatMessage(format!(
                                "§f[§4BAF§f]: §c[AutoCookie] Not enough coins to buy cookie (need 7.5M, have {}M)",
                                purse / 1_000_000
                            )));
                            warn!("[Cookie] Insufficient coins ({}) — skipping cookie buy", purse);
                            *state.bot_state.write() = BotState::Idle;
                        } else {
                            info!("[Cookie] Buying cookie ({}h remaining < {}h threshold)...", hours, auto_cookie_hours);
                            let _ = state.event_tx.send(BotEvent::ChatMessage(
                                "§f[§4BAF§f]: §6[AutoCookie] Buying booster cookie...".to_string()
                            ));
                            *state.cookie_step.write() = CookieStep::Initial;
                            bot.write_chat_packet("/bz Booster Cookie");
                            *state.bot_state.write() = BotState::BuyingCookie;
                        }
                    }
                }
            }
        }
        BotState::BuyingCookie => {
            // Multi-step cookie buy flow matching TypeScript cookieHandler.ts buyCookie().
            let step = *state.cookie_step.read();
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

            if step == CookieStep::Initial && window_title.contains("Bazaar") {
                // Bazaar search results: click slot 11 (the cookie item)
                info!("[Cookie] Bazaar opened — clicking cookie item (slot 11)");
                click_window_slot(bot, window_id, 11).await;
                *state.cookie_step.write() = CookieStep::ItemDetail;
            } else if step == CookieStep::ItemDetail {
                // Cookie item detail: click slot 10 (Buy Instantly)
                info!("[Cookie] Cookie detail — clicking Buy Instantly (slot 10)");
                click_window_slot(bot, window_id, 10).await;
                *state.cookie_step.write() = CookieStep::BuyConfirm;
            } else if step == CookieStep::BuyConfirm {
                // Purchase confirmation: click slot 10 again to confirm
                info!("[Cookie] Buy confirmation — clicking Confirm (slot 10)");
                click_window_slot(bot, window_id, 10).await;
                // Purchase accepted — close window and find cookie in inventory to consume
                tokio::time::sleep(tokio::time::Duration::from_millis(2000)).await;
                // Close any open window
                bot.write_packet(ServerboundContainerClose {
                    container_id: window_id as i32,
                });
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

                // Find cookie in inventory and consume via right-click + slot 11
                let menu = bot.menu();
                let all_slots = menu.slots();
                let player_range = menu.player_slots_range();
                let range_start = *player_range.start();
                let cookie_slot = all_slots[player_range].iter().enumerate().find_map(|(i, item)| {
                    let name = get_item_display_name_from_slot(item).unwrap_or_default().to_lowercase();
                    if name.contains("booster cookie") || name.contains("cookie") {
                        Some(i)
                    } else {
                        None
                    }
                });

                match cookie_slot {
                    Some(idx) => {
                        let current_time = *state.cookie_time_secs.read();
                        let new_hours = (current_time + 4 * 86400) / 3600;
                        let old_hours = current_time / 3600;
                        info!("[Cookie] Found cookie at inventory slot {} — consuming", idx);
                        // Right-click the cookie to open its GUI, then click slot 11 to consume
                        let win_slot = range_start + idx;
                        click_window_slot(bot, window_id, win_slot as i16).await;
                        tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
                        // After right-clicking, the cookie GUI opens — handled in next window event
                        // For now mark as Idle; the cookie GUI window will be a new OpenScreen event.
                        // This is handled below if the cookie GUI opens as a new window.
                        let _ = state.event_tx.send(BotEvent::ChatMessage(format!(
                            "§f[§4BAF§f]: §aBought booster cookie! Time: {}h → {}h",
                            old_hours, new_hours
                        )));
                    }
                    None => {
                        warn!("[Cookie] Cookie not found in inventory after purchase");
                        let _ = state.event_tx.send(BotEvent::ChatMessage(
                            "§f[§4BAF§f]: §c[AutoCookie] Cookie purchased but not found in inventory".to_string()
                        ));
                    }
                }
                *state.bot_state.write() = BotState::Idle;
            }
        }
        _ => {
            // Not in a state that requires window interaction
        }
    }
}

/// Parse a cookie duration string from lore text and return seconds.
/// Handles "Duration: Xd Xh Xm" format (matching TypeScript parseCookieDuration).
fn parse_cookie_duration_secs(lore_text: &str) -> u64 {
    let clean = remove_mc_colors(lore_text);
    let mut total: u64 = 0;
    if let Some(m) = regex_first_u64(&clean, r"(\d+)d") { total += m * 86400; }
    if let Some(m) = regex_first_u64(&clean, r"(\d+)h") { total += m * 3600; }
    if let Some(m) = regex_first_u64(&clean, r"(\d+)m") { total += m * 60; }
    total
}

/// Helper: extract first captured u64 from a simple regex pattern (no deps on regex crate).
fn regex_first_u64(text: &str, pattern: &str) -> Option<u64> {
    // Simple manual parser for patterns like r"(\d+)d"
    // We only need to handle "Nd", "Nh", "Nm" patterns.
    let suffix = pattern.trim_start_matches(r"(\d+)");
    let suffix = suffix.trim_end_matches(')');
    for word in text.split_whitespace() {
        if word.ends_with(suffix) {
            let num_part = word.trim_end_matches(suffix);
            if let Ok(n) = num_part.trim_matches(',').parse::<u64>() {
                return Some(n);
            }
        }
    }
    None
}

fn extract_item_nbt_components(item_data: &azalea_inventory::ItemStackData) -> serde_json::Value {
    match serde_json::to_value(&item_data.component_patch) {
        Ok(value) => {
            if value.as_object().map_or(false, |o| o.is_empty()) {
                serde_json::Value::Null
            } else {
                value
            }
        }
        Err(e) => {
            if should_suppress_component_patch_serialization_warning(&e) {
                debug!(
                    "[Inventory] Skipping component patch NBT extraction due to expected serialization limitation"
                );
            } else {
                warn!(
                    "[Inventory] Failed to serialize component patch for NBT extraction: {}",
                    e
                );
            }
            serde_json::Value::Null
        }
    }
}

fn should_suppress_component_patch_serialization_warning(error: &serde_json::Error) -> bool {
    error.to_string().contains("key must be a string")
}

/// Rebuild and cache the player-inventory JSON from the bot's current menu.
///
/// Called after every ContainerSetContent / ContainerSetSlot so that
/// `BotClient::get_cached_inventory_json()` always returns fresh data.
/// The serialised format matches TypeScript `JSON.stringify(bot.inventory)`.
fn rebuild_cached_inventory_json(bot: &Client, state: &BotClientState) {
    let menu = bot.menu();
    let all_slots = menu.slots();
    let player_range = menu.player_slots_range();

    let mut slots_array: Vec<serde_json::Value> = vec![serde_json::Value::Null; 46];
    let mut slot_descriptions: Vec<String> = Vec::new();

    for (i, item) in all_slots[player_range].iter().enumerate() {
        let mineflayer_slot = 9 + i;
        if mineflayer_slot > 44 {
            break;
        }
        if item.is_empty() {
            slots_array[mineflayer_slot] = serde_json::Value::Null;
        } else {
            let item_type = item.kind() as u32;
            let nbt_data = if let Some(item_data) = item.as_present() {
                extract_item_nbt_components(item_data)
            } else {
                serde_json::Value::Null
            };
            let item_name = item.kind().to_string();
            slot_descriptions.push(format!("slot {}: {}x {}", mineflayer_slot, item.count(), item_name));
            slots_array[mineflayer_slot] = serde_json::json!({
                "type": item_type,
                "count": item.count(),
                "metadata": 0,
                "nbt": nbt_data,
                "name": item_name,
                "slot": mineflayer_slot
            });
        }
    }

    debug!("[Inventory] Rebuilt cache: {} non-empty slots — {}", slot_descriptions.len(), slot_descriptions.join(", "));

    let inventory_json = serde_json::json!({
        "id": 0,
        "slots": slots_array,
        "inventoryStart": 9,
        "inventoryEnd": 45,
        "hotbarStart": 36,
        "craftingResultSlot": 0,
        "requiresConfirmation": true,
        "selectedItem": serde_json::Value::Null
    });

    if let Ok(json_str) = serde_json::to_string(&inventory_json) {
        *state.cached_inventory_json.write() = Some(json_str);
    }
}

/// Append an unclaimed bazaar order to `pending_claims.log` with an RFC 3339 timestamp.
/// Called when inventory is full and a filled order cannot be collected.
/// The log can be reviewed later to know which orders need manual collection.
fn log_pending_claim(order_name: &str) {
    use std::io::Write;
    let timestamp = chrono::Utc::now().to_rfc3339();
    let line = format!("{} {}\n", timestamp, order_name);
    let log_path = match std::env::current_exe() {
        Ok(exe) => exe.parent().map(|p| p.join("pending_claims.log"))
            .unwrap_or_else(|| std::path::PathBuf::from("pending_claims.log")),
        Err(_) => std::path::PathBuf::from("pending_claims.log"),
    };
    match std::fs::OpenOptions::new().create(true).append(true).open(&log_path) {
        Ok(mut f) => { let _ = f.write_all(line.as_bytes()); }
        Err(e) => warn!("[ManageOrders] Failed to write pending_claims.log: {}", e),
    }
    warn!("[ManageOrders] Logged unclaimed order \"{}\" to {:?}", order_name, log_path);
}

/// Returns the display name of the item that occupies more than half of the player's
/// inventory slots (> half of 36 = 18 slots). Used to detect a dominant stackable item
/// that should be instasold to free space when inventory is full.
/// Returns None if no single item type dominates the inventory.
fn find_dominant_inventory_item(bot: &Client) -> Option<String> {
    let menu = bot.menu();
    let all_slots = menu.slots();
    let player_range = menu.player_slots_range();
    let player_slots = &all_slots[player_range];

    let total = player_slots.len(); // 36 for a standard player inventory
    let half = total / 2;

    let mut counts: HashMap<String, usize> = HashMap::new();
    for slot in player_slots.iter() {
        if !slot.is_empty() {
            if let Some(name) = get_item_display_name_from_slot(slot) {
                *counts.entry(name).or_insert(0) += 1;
            }
        }
    }

    counts.into_iter()
        .find(|(_, count)| *count > half)
        .map(|(name, _)| name)
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

/// Click a window slot while reporting the item currently on the cursor.
/// Used when the cursor already holds an item from a previous pick-up click,
/// so the server receives the correct `carried_item` and processes the interaction
/// (e.g. placing the item in the auction item-slot to trigger the price sign).
async fn click_window_slot_carrying(
    bot: &Client,
    window_id: u8,
    slot: i16,
    carried: &azalea_inventory::ItemStack,
) {
    use azalea_protocol::packets::game::s_container_click::{
        ServerboundContainerClick,
        HashedStack,
    };

    let carried_item = bot.with_registry_holder(|reg| HashedStack::from_item_stack(carried, reg));

    let packet = ServerboundContainerClick {
        container_id: window_id as i32,
        state_id: 0,
        slot_num: slot,
        button_num: 0,
        click_type: ClickType::Pickup,
        changed_slots: Default::default(),
        carried_item,
    };

    bot.write_packet(packet);
    info!("Clicked slot {} in window {} (carrying item)", slot, window_id);
}

/// Shared startup workflow: cancel old orders, claim sold items, then emit StartupComplete.
/// Called from both the chat-based detection path and the 30-second watchdog.
/// Matches TypeScript BAF.ts `runStartupWorkflow` all 4 steps.
async fn run_startup_workflow(
    bot: Client,
    bot_state: Arc<RwLock<BotState>>,
    event_tx: tokio::sync::mpsc::UnboundedSender<BotEvent>,
    manage_orders_cancelled: Arc<RwLock<u64>>,
    auto_cookie_hours: Arc<RwLock<u64>>,
    command_generation: Arc<std::sync::atomic::AtomicU64>,
) {
    // Do not run startup steps while another interactive flow is active.
    // Wait briefly for idle/grace period; abort if the bot stays busy.
    let entry_deadline = tokio::time::Instant::now()
        + tokio::time::Duration::from_secs(STARTUP_ENTRY_TIMEOUT_SECS);
    loop {
        let current_state = *bot_state.read();
        if matches!(current_state, BotState::GracePeriod | BotState::Idle) {
            break;
        }
        if current_state == BotState::Startup {
            debug!("[Startup] Workflow already running, skipping duplicate start");
            return;
        }
        if tokio::time::Instant::now() >= entry_deadline {
            warn!("[Startup] Skipping startup workflow: bot stayed busy in state {:?}", current_state);
            return;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(250)).await;
    }

    // Snapshot command generation at workflow entry. If this value changes, execute_command
    // has started a new queued command and startup must abort to avoid overlapping GUI flows.
    let startup_generation = command_generation.load(Ordering::SeqCst);

    info!("╔══════════════════════════════════════╗");
    info!("║        BAF Startup Workflow          ║");
    info!("╚══════════════════════════════════════╝");

    // Set state = Startup to block flips/bazaar during the workflow
    *bot_state.write() = BotState::Startup;

    // Step 1/4: Cookie check — send /sbmenu, let window handler parse cookie duration.
    // Matches TypeScript cookieHandler.ts checkAndBuyCookie().
    let cookie_hours = *auto_cookie_hours.read();
    if cookie_hours > 0 {
        info!("[Startup] Step 1/4: Checking cookie status...");
        let _ = event_tx.send(BotEvent::ChatMessage(
            "§f[§4BAF§f]: §7[Startup] §bStep 1/4: §fChecking cookie status...".to_string()
        ));
        bot.write_chat_packet("/sbmenu");
        *bot_state.write() = BotState::CheckingCookie;

        // Wait up to 15 seconds for cookie check to finish (state → Idle when done)
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(15);
        loop {
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
            if command_generation.load(Ordering::SeqCst) != startup_generation {
                warn!("[Startup] Aborting workflow: another command started during cookie check");
                return;
            }
            let cur = *bot_state.read();
            if matches!(cur, BotState::Idle | BotState::Startup) || tokio::time::Instant::now() >= deadline {
                break;
            }
        }
        if command_generation.load(Ordering::SeqCst) != startup_generation {
            warn!("[Startup] Aborting workflow: another command started during cookie check");
            return;
        }
        // Ensure we're not stuck in cookie states
        *bot_state.write() = BotState::Startup;
        info!("[Startup] Step 1/4: Cookie check complete");
        let _ = event_tx.send(BotEvent::ChatMessage(
            "§f[§4BAF§f]: §a[Startup] Cookie check complete".to_string()
        ));
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    } else {
        info!("[Startup] Step 1/4: Cookie check skipped (AUTO_COOKIE=0)");
    }

    // Step 2/4: Bazaar order management — open /bz, navigate to Manage Orders, cancel all old orders.
    // Mirrors TypeScript bazaarOrderManager.ts manageStartupOrders().
    info!("[Startup] Step 2/4: Managing bazaar orders...");
    *manage_orders_cancelled.write() = 0;
    bot.write_chat_packet("/bz");
    *bot_state.write() = BotState::ManagingOrders;

    // Wait up to 30 seconds for order management to finish (state → Idle when done)
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(30);
    loop {
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        if command_generation.load(Ordering::SeqCst) != startup_generation {
            warn!("[Startup] Aborting workflow: another command started during bazaar order management");
            return;
        }
        let cur = *bot_state.read();
        if cur == BotState::Idle || tokio::time::Instant::now() >= deadline {
            break;
        }
    }
    if command_generation.load(Ordering::SeqCst) != startup_generation {
        warn!("[Startup] Aborting workflow: another command started during bazaar order management");
        return;
    }
    // Ensure Idle before proceeding (in case of timeout)
    *bot_state.write() = BotState::Idle;
    let orders_cancelled = *manage_orders_cancelled.read();
    info!("[Startup] Step 2/4: Order management complete — {} order(s) cancelled", orders_cancelled);

    // Step 3/4: Claim sold items and purchased items
    info!("[Startup] Step 3/4: Claiming sold items...");
    bot.write_chat_packet("/ah");
    *bot_state.write() = BotState::ClaimingSold;

    // Wait up to 30 seconds for claiming to finish (state → Idle when done)
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(30);
    loop {
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        if command_generation.load(Ordering::SeqCst) != startup_generation {
            warn!("[Startup] Aborting workflow: another command started during claim-sold step");
            return;
        }
        let cur = *bot_state.read();
        if cur == BotState::Idle || tokio::time::Instant::now() >= deadline {
            break;
        }
    }
    if command_generation.load(Ordering::SeqCst) != startup_generation {
        warn!("[Startup] Aborting workflow: another command started during claim-sold step");
        return;
    }
    // Ensure Idle before proceeding
    *bot_state.write() = BotState::Idle;

    // Also claim any purchased items waiting in "Your Bids" (e.g. from a previous session)
    // Matching TypeScript runStartupWorkflow which claims both sold and purchased items.
    info!("[Startup] Step 3b/4: Claiming purchased items (Your Bids)...");
    bot.write_chat_packet("/ah");
    *bot_state.write() = BotState::ClaimingPurchased;

    // Wait up to 30 seconds for claiming to finish (state → Idle when done)
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(30);
    loop {
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        if command_generation.load(Ordering::SeqCst) != startup_generation {
            warn!("[Startup] Aborting workflow: another command started during claim-purchased step");
            return;
        }
        let cur = *bot_state.read();
        if cur == BotState::Idle || tokio::time::Instant::now() >= deadline {
            break;
        }
    }
    if command_generation.load(Ordering::SeqCst) != startup_generation {
        warn!("[Startup] Aborting workflow: another command started during claim-purchased step");
        return;
    }
    // Ensure Idle before proceeding
    *bot_state.write() = BotState::Idle;

    // Step 4/4: Emit StartupComplete — main.rs requests bazaar flips and sends webhook
    info!("[Startup] Step 4/4: Startup complete - bot is ready to flip!");
    let _ = event_tx.send(BotEvent::StartupComplete { orders_cancelled });
}

/// Parse "You purchased <item> for <price> coins!" → (item_name, price)
fn parse_purchased_message(msg: &str) -> Option<(String, u64)> {
    // "You purchased <item> for <price> coins!"
    let after = msg.strip_prefix("You purchased ")?;
    let for_idx = after.rfind(" for ")?;
    let item_name = after[..for_idx].to_string();
    let rest = &after[for_idx + 5..];
    let coins_idx = rest.find(" coins")?;
    let price_str = rest[..coins_idx].replace(',', "");
    let price: u64 = price_str.trim().parse().ok()?;
    Some((item_name, price))
}

/// Parse "[Auction] <buyer> bought <item> for <price> coins" → (buyer, item_name, price)
fn parse_sold_message(msg: &str) -> Option<(String, String, u64)> {
    // "[Auction] <buyer> bought <item> for <price> coins"
    let after = msg.strip_prefix("[Auction] ")?;
    let bought_idx = after.find(" bought ")?;
    let buyer = after[..bought_idx].to_string();
    let rest = &after[bought_idx + 8..];
    let for_idx = rest.rfind(" for ")?;
    let item_name = rest[..for_idx].to_string();
    let rest2 = &rest[for_idx + 5..];
    let coins_idx = rest2.find(" coins")?;
    let price_str = rest2[..coins_idx].replace(',', "");
    let price: u64 = price_str.trim().parse().ok()?;
    Some((buyer, item_name, price))
}

/// Extract UUID from a message that might contain "/viewauction <UUID>"
fn extract_viewauction_uuid(msg: &str) -> Option<String> {
    let idx = msg.find("/viewauction ")?;
    let rest = &msg[idx + 13..];
    let end = rest.find(|c: char| c.is_whitespace()).unwrap_or(rest.len());
    let uuid = rest[..end].trim().to_string();
    if uuid.is_empty() { None } else { Some(uuid) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use azalea::registry::builtin::ItemKind;
    use azalea_inventory::components::MapId;
    use azalea_inventory::ItemStack;
    use azalea_inventory::ItemStackData;

    #[test]
    fn test_parse_purchased_message() {
        let msg = "You purchased Gemstone Fuel Tank for 40,000,000 coins!";
        let result = parse_purchased_message(msg);
        assert_eq!(result, Some(("Gemstone Fuel Tank".to_string(), 40_000_000)));
    }

    #[test]
    fn test_parse_purchased_message_simple_price() {
        let msg = "You purchased Dirt for 100 coins!";
        let result = parse_purchased_message(msg);
        assert_eq!(result, Some(("Dirt".to_string(), 100)));
    }

    #[test]
    fn test_parse_sold_message() {
        let msg = "[Auction] SomePlayer bought Gemstone Fuel Tank for 45,000,000 coins!";
        let result = parse_sold_message(msg);
        assert_eq!(result, Some(("SomePlayer".to_string(), "Gemstone Fuel Tank".to_string(), 45_000_000)));
    }

    #[test]
    fn test_extract_viewauction_uuid() {
        let msg = "click /viewauction 26e353e9556a4b9791f5e03710ddc505 to view";
        let result = extract_viewauction_uuid(msg);
        assert_eq!(result, Some("26e353e9556a4b9791f5e03710ddc505".to_string()));
    }

    #[test]
    fn test_remove_mc_colors() {
        assert_eq!(remove_mc_colors("§aHello §r§bWorld"), "Hello World");
        assert_eq!(remove_mc_colors("No colors"), "No colors");
    }

    #[test]
    fn test_parse_bed_remaining_secs_from_text() {
        assert_eq!(parse_bed_remaining_secs_from_text("Ends in 0:45"), Some(45));
        assert_eq!(parse_bed_remaining_secs_from_text("Purchase in 1m 05s"), Some(65));
        assert_eq!(parse_bed_remaining_secs_from_text("Grace period: 59s"), Some(59));
        assert_eq!(parse_bed_remaining_secs_from_text("No time here"), None);
    }

    #[test]
    fn test_is_terminal_purchase_failure_message() {
        assert!(is_terminal_purchase_failure_message("You didn't participate in this auction!"));
        assert!(is_terminal_purchase_failure_message("This auction wasn't found!"));
        assert!(!is_terminal_purchase_failure_message("Putting coins in escrow..."));
    }

    #[test]
    fn test_extract_item_nbt_components_with_map_component() {
        let item = ItemStack::from(ItemKind::Map).with_component(MapId { id: 123 });
        let item_data = item.as_present().expect("map item should be present");

        let nbt = extract_item_nbt_components(item_data);
        assert!(nbt.get("minecraft:map_id").is_some());
    }

    #[test]
    fn test_extract_item_nbt_components_with_empty_patch() {
        let item = ItemStackData::from(ItemKind::Stone);
        let nbt = extract_item_nbt_components(&item);
        assert!(nbt.is_null());
    }

    #[test]
    fn test_extract_item_nbt_components_suppresses_expected_serialization_warning() {
        let mut tuple_keys = std::collections::HashMap::new();
        tuple_keys.insert((1_i32, 2_i32), "value");
        let err = serde_json::to_value(tuple_keys).expect_err("tuple map keys should fail in JSON");
        assert!(
            should_suppress_component_patch_serialization_warning(&err),
            "unexpected error: {err}"
        );
    }
}
