use anyhow::{anyhow, Result};
use azalea::prelude::*;
use azalea_protocol::packets::game::{
    ClientboundGamePacket,
    c_set_display_objective::DisplaySlot,
    c_set_player_team::Method as TeamMethod,
    s_sign_update::ServerboundSignUpdate,
    s_container_close::ServerboundContainerClose,
    s_use_item::ServerboundUseItem,
    s_set_carried_item::ServerboundSetCarriedItem,
    s_interact::InteractionHand,
    s_chat_command::ServerboundChatCommand,
};
use std::sync::atomic::{AtomicBool, Ordering};
use azalea_inventory::operations::ClickType;
use azalea_client::chat::ChatPacket;
use azalea_client::inventory::{MenuOpenedEvent, SetContainerContentEvent};
use azalea_client::auto_reconnect::AutoReconnectDelay;
use bevy_app::AppExit;
use once_cell::sync::Lazy;
use parking_lot::RwLock;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{info, error, debug, warn};

use crate::types::{BotState, QueuedCommand};
use crate::state::CommandQueue;
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
const STARTUP_ENTRY_TIMEOUT_SECS: u64 = 60;
/// Interval for safety retry clicks in the Confirm Purchase window (milliseconds).
const CONFIRM_PURCHASE_RETRY_MS: u64 = 50;
/// Brief delay after closing a stale window so Hypixel processes the
/// container-close packet before the next command is sent.
const WINDOW_CLOSE_DELAY_MS: u64 = 150;
const MAX_CLAIM_SOLD_UUID_QUEUE: usize = 64;
/// Delay before retrying the auction flow after closing a window to remove a
/// stuck item from the auction slot.  Gives Hypixel time to process the
/// container-close packet and return the item to inventory.
const AUCTION_RETRY_AFTER_STUCK_ITEM_MS: u64 = 2000;
/// Maximum number of retry attempts when "You already have an item in the auction
/// slot!" keeps recurring.  After this many retries the bot gives up and goes Idle
/// instead of looping indefinitely and risking a "Sending packets too fast!" kick.
const MAX_AUCTION_STUCK_ITEM_RETRIES: u8 = 3;
/// When `auction_slot_blocked` is set after exhausting stuck-item retries,
/// this flag prevents further SellToAuction attempts until it is cleared
/// (e.g. by a successful ClaimSold cycle that frees the slot).
/// Cleared by the ClaimingSold → Idle transition or ManageOrders completion.
/// Fallback slot index for "Manage Orders" in the Bazaar GUI when dynamic name
/// lookup fails.  Hypixel's default layout places it at slot 50.
const MANAGE_ORDERS_FALLBACK_SLOT: usize = 50;
/// Fallback slot index for "Sell Inventory Now" in the Bazaar GUI when dynamic
/// name lookup fails.  Hypixel's default layout places it at slot 47.
const SELL_INVENTORY_NOW_FALLBACK_SLOT: usize = 47;
/// Debounce interval for `rebuild_cached_window_json` on `ContainerSetSlot` events.
/// Individual slot updates are coalesced within this window to avoid excessive CPU
/// from repeated NBT extraction + JSON serialisation during rapid GUI interactions.
const WINDOW_CACHE_REBUILD_DEBOUNCE_MS: u64 = 100;
/// Debounce interval for `rebuild_cached_inventory_json` on `ContainerSetSlot` events.
/// Same rationale as `WINDOW_CACHE_REBUILD_DEBOUNCE_MS`: coalesces rapid per-slot
/// updates into a single rebuild to keep the ECS World lock acquisition frequency low
/// and reduce CPU spent on repeated JSON serialisation.
const INVENTORY_CACHE_REBUILD_DEBOUNCE_MS: u64 = 100;
/// Timeout (seconds) for `wait_for_collect_confirmation` and
/// `wait_for_cancel_confirmation` to consider an action unprocessed.
/// Raised from 5 → 8 to accommodate Hypixel server lag that caused
/// frequent false "not confirmed" warnings and skipped events.
const ORDER_ACTION_CONFIRMATION_TIMEOUT_SECS: u64 = 8;
/// Maximum number of cancel attempts per order before giving up.
/// After this many failed cancel clicks in Order options, the order is skipped
/// so the bot doesn't get stuck retrying indefinitely.
const MAX_CANCEL_RETRIES: u32 = 5;
/// Minimum number of empty player-inventory slots required to consider the
/// inventory "not full".  Used when verifying the `inventory_full` flag
/// against actual slot counts so stale flags are auto-cleared after a manual
/// instasell or any other action that frees space.
const MIN_FREE_SLOTS_FOR_BUY: u8 = 2;
/// Threshold for "near full" inventory — when the number of empty slots is
/// at or below this value, the bot stops claiming purchased items (bids) to
/// keep space available for selling.
const NEAR_FULL_SLOT_THRESHOLD: u8 = 4;
#[cfg(test)]
static SOLD_FOR_PRICE_RE: Lazy<regex::Regex> =
    Lazy::new(|| regex::Regex::new(r"(?i)sold\s*for[: ]+\s*([0-9,]+)\s*coins").expect("valid sold-for regex"));
#[cfg(test)]
static SOLD_BUYER_RE: Lazy<regex::Regex> =
    Lazy::new(|| regex::Regex::new(r"(?i)buyer[: ]+\s*([^\n]+)").expect("valid sold-buyer regex"));

// ---------------------------------------------------------------------------
// Bevy Plugin: PacketAcceleratorPlugin
//
// Registers ECS observers for MenuOpenedEvent and SetContainerContentEvent
// that fire DURING the ECS frame (in apply_deferred, between PreUpdate and
// Update).  This is significantly earlier than the Event::Packet pipeline
// which only delivers events AFTER the frame completes:
//
//   Event::Packet path:
//     PreUpdate (read_packets) → Update (packet_listener) → channel →
//     event_copying_task → dispatch loop → spawn handler → handler runs
//
//   Observer path (this plugin):
//     PreUpdate (read_packets → process_packet → commands.trigger) →
//     apply_deferred → observer fires → Notify set
//     → After frame: waiting task resumes immediately
//
// On busy servers (many entities/chunks/events), the Event::Packet path
// can add 20-70 ms of pipeline delay on top of network RTT.  The observer
// path eliminates this overhead for time-critical purchase operations.
//
// ## Anti-Cheat Safety Analysis
//
// This plugin is safe and will NOT trigger Hypixel anti-cheat because:
//
// 1. **No new packets sent** — The plugin only fires Notify signals earlier.
//    It does NOT send any packets to the server.  The actual buy-click path
//    (click_window_slot) is completely unchanged.
//
// 2. **gold_nugget gate unchanged** — The buy-click is still gated on
//    slot 31 containing gold_nugget (see handle_window_interaction).
//    Clicking before the item loads ("impossible action") is still
//    impossible because the slot check runs before any click.
//
// 3. **Standard Minecraft protocol** — All clicks use normal
//    ServerboundContainerClick packets with correct window IDs, slot
//    numbers, and state IDs.  No packet modification or forging.
//
// 4. **TCP ordering guarantees safety** — The server sends OpenScreen →
//    client detects it → client clicks.  Because TCP preserves order,
//    the server will always have finished sending the window before it
//    receives the client's click response.  Faster client-side detection
//    cannot produce out-of-order packets.
//
// 5. **Timing within normal variance** — The 20-70ms improvement is well
//    within normal network latency variance.  A player with 30ms ping
//    naturally has 70ms lower latency than one with 100ms ping.
//    The accelerator just removes internal pipeline overhead so the bot
//    performs as its network latency allows, same as any other client.
//
// 6. **Pre-existing behavior** — The fast account (same server, different
//    bot instance) already operates at the accelerated speed (~58ms).
//    This plugin brings the slow path (~128ms) to parity by eliminating
//    Azalea's internal Event::Packet channel overhead.
// ---------------------------------------------------------------------------

/// Information about the most recently opened window, set by the ECS observer.
#[derive(Clone, Debug)]
pub struct WindowOpenInfo {
    pub window_id: i32,
    pub title: String,
    pub timestamp: std::time::Instant,
}

/// Shared bridge between the Bevy ECS observers and async tokio tasks.
/// The ECS observers write data here; the purchase handler reads it.
#[derive(Clone, bevy_ecs::prelude::Resource)]
struct PacketAcceleratorBridge {
    /// Fired when any window opens (MenuOpenedEvent observer).
    window_open_notify: Arc<tokio::sync::Notify>,
    /// Most recent window open info (set by MenuOpenedEvent observer).
    window_open_info: Arc<RwLock<Option<WindowOpenInfo>>>,
    /// Fired when container content is set (SetContainerContentEvent observer).
    slot_data_notify: Arc<tokio::sync::Notify>,
}

/// Bevy Plugin that registers ECS-level observers for instant packet detection.
pub struct PacketAcceleratorPlugin {
    bridge: PacketAcceleratorBridge,
}

impl PacketAcceleratorPlugin {
    fn new(
        window_open_notify: Arc<tokio::sync::Notify>,
        window_open_info: Arc<RwLock<Option<WindowOpenInfo>>>,
        slot_data_notify: Arc<tokio::sync::Notify>,
    ) -> Self {
        Self {
            bridge: PacketAcceleratorBridge {
                window_open_notify,
                window_open_info,
                slot_data_notify,
            },
        }
    }
}

impl bevy_app::Plugin for PacketAcceleratorPlugin {
    fn build(&self, app: &mut bevy_app::App) {
        app.insert_resource(self.bridge.clone());
        app.add_observer(on_menu_opened);
        app.add_observer(on_container_set_content);
    }
}

/// ECS Observer: fires during apply_deferred when the server sends OpenScreen.
/// Sets the window info and notifies any waiting purchase handler task.
fn on_menu_opened(
    event: bevy_ecs::observer::On<MenuOpenedEvent>,
    bridge: bevy_ecs::system::Res<PacketAcceleratorBridge>,
) {
    let ev = event.event();
    let title = ev.title.to_string();
    let window_id = ev.window_id;
    *bridge.window_open_info.write() = Some(WindowOpenInfo {
        window_id,
        title,
        timestamp: std::time::Instant::now(),
    });
    bridge.window_open_notify.notify_waiters();
}

/// ECS Observer: fires during apply_deferred when the server sends
/// ContainerSetContent.  This is the same data that populates slot 31 in
/// the BIN Auction View.  Notifying here lets the purchase handler react
/// to slot data without waiting for the Event::Packet pipeline.
fn on_container_set_content(
    _event: bevy_ecs::observer::On<SetContainerContentEvent>,
    bridge: bevy_ecs::system::Res<PacketAcceleratorBridge>,
) {
    bridge.slot_data_notify.notify_waiters();
}

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
    /// Count of bazaar orders cancelled during startup order management
    manage_orders_cancelled: Arc<RwLock<u64>>,
    /// Set when "You reached your maximum of XY Bazaar orders!" is received.
    /// Cleared when an order fills (Claimed message detected).
    bazaar_at_limit: Arc<AtomicBool>,
    /// Set when "[Bazaar] You reached the daily limit" is detected.
    /// Cleared on account switch or at 0:00 UTC daily reset.
    bazaar_daily_limit: Arc<AtomicBool>,
    /// Set when "Maximum auction count reached" is detected in the Manage Auctions GUI.
    /// Cleared when an auction is sold/claimed (slot freed). Prevents repeated
    /// SellToAuction → /ah → limit-detected → idle loops.
    auction_at_limit: Arc<AtomicBool>,
    /// Set after exhausting `MAX_AUCTION_STUCK_ITEM_RETRIES` while trying to
    /// list an item — "You already have an item in the auction slot!" kept
    /// recurring.  Prevents further SellToAuction commands from being processed
    /// until a successful ClaimSold/ManageOrders cycle clears it.
    auction_slot_blocked: Arc<AtomicBool>,
    /// Retry counter for "You already have an item in the auction slot!" errors.
    /// Shared with BotClientState so clearing the blocked flag also resets retries.
    auction_stuck_item_retries: Arc<std::sync::atomic::AtomicU8>,
    /// Set when the server rejects an order placement (e.g. "Your price isn't
    /// competitive enough").  Cleared before each confirm-click so only the
    /// response to the *current* placement attempt is captured.
    bazaar_order_rejected: Arc<AtomicBool>,
    /// Cached player-inventory JSON (serialised Window object).
    /// Updated on every ContainerSetContent / ContainerSetSlot for the player
    /// inventory window (id 0) so that getInventory can be answered instantly,
    /// in parallel with any ongoing Hypixel interaction — matching TypeScript
    /// BAF.ts which calls `JSON.stringify(bot.inventory)` directly without
    /// waiting for a command-queue slot.
    cached_inventory_json: Arc<RwLock<Option<String>>>,
    /// AUTO_COOKIE config value passed through to BotClientState.
    auto_cookie_hours: Arc<RwLock<u64>>,
    /// Config gate for purchaseAt-based bed (grace-period) timing mode.
    pub bedtiming: bool,
    /// Enable skip-click: pre-click confirm on predicted Confirm Purchase window.
    pub skip: bool,
    /// Interval in milliseconds for grace-period bed/gold_nugget click loops.
    pub bed_spam_click_delay: u64,
    /// Item name to sell via bazaar "Sell Instantly" when inventory is full
    insta_sell_item: Arc<RwLock<Option<String>>>,
    /// How many ms before bed timer expiry to start pre-clicking (default: 100).
    pub bed_pre_click_ms: u64,
    /// Items the bot has listed on the AH (by lowercase item name).
    /// Used to filter out coop member sales from our own sales.
    active_auction_listings: Arc<RwLock<std::collections::HashSet<String>>>,
    /// The bot's in-game name, used for coop sale filtering.
    pub ingame_name: Arc<RwLock<String>>,
    /// When true, the ManagingOrders handler also cancels open orders (startup mode).
    manage_orders_cancel_open: Arc<AtomicBool>,
    /// When set, the ManageOrders handler targets a specific order for cancellation.
    manage_orders_target_item: Arc<RwLock<Option<(String, bool)>>>,
    /// Cancel open bazaar orders when they are older than this many minutes per million coins.
    /// 0 disables age-based cancellation in periodic ManageOrders runs.
    pub bazaar_order_cancel_minutes_per_million: u64,
    /// Updated whenever the bot opens the Manage/My Auctions GUI.
    cached_my_auctions_json: Arc<RwLock<Option<String>>>,
    /// Time when /viewauction was sent. Used for internal timing diagnostics.
    /// Shared with `BotClientState` so the event handler can compute elapsed time
    /// when "Putting coins in escrow" arrives.
    purchase_start_time: Arc<RwLock<Option<std::time::Instant>>>,
    /// Time when the BIN Auction View window opened for the current purchase.
    /// Buy speed is measured from here (view-open → coins-in-escrow) so it
    /// reflects the bot's act-on-open speed rather than the /viewauction RTT.
    bin_view_open_time: Arc<RwLock<Option<std::time::Instant>>>,
    /// Shared AH-pause flag so ManageOrders can self-abort when AH flips are incoming.
    pub bazaar_flips_paused: Arc<AtomicBool>,
    /// Buffer of Hypixel chat messages to send as a chatBatch to Coflnet WebSocket.
    /// Drained periodically and sent as `{"type":"chatBatch","data":"[...]"}`.
    pub chat_batch_buffer: Arc<RwLock<Vec<String>>>,
    /// Cached GUI window JSON for the web panel game view.
    cached_window_json: Arc<RwLock<Option<String>>>,
    /// Set when inventory is full (stashed items / no space to claim).
    /// Shared with `BotClientState` so `main.rs` can check before enqueuing ManageOrders.
    inventory_full: Arc<AtomicBool>,
    /// Cached count of empty player inventory slots (shared with BotClientState).
    /// Updated on every inventory rebuild.
    cached_empty_player_slots: Arc<std::sync::atomic::AtomicU8>,
    /// Shared reference to the command queue so the startup workflow can enqueue
    /// commands (CheckCookie, ManageOrders, ClaimSoldItem, ClaimPurchasedItem)
    /// through the proper queue instead of directly driving bot state.
    command_queue: Arc<RwLock<Option<CommandQueue>>>,
    /// Set to true while the startup workflow is running. Checked by flip handlers
    /// to block bazaar flips during startup (since bot state cycles through
    /// Idle between queued startup commands).
    startup_in_progress: Arc<AtomicBool>,
    /// Whether bazaar flips are enabled in the config.  Used by the startup
    /// workflow to decide whether to cancel all open bazaar orders.
    pub enable_bazaar_flips: Arc<AtomicBool>,
    /// Ensures the dedicated command processor is only spawned once, using the
    /// first live Azalea `Client` handle delivered to `event_handler`.
    command_processor_started: Arc<AtomicBool>,
    /// Set to true to request a clean disconnect from Hypixel.
    /// The command processor polls this flag and calls `bot.disconnect()` when set.
    disconnect_requested: Arc<AtomicBool>,
}
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
    /// A bazaar buy/sell order was fully filled and is ready to collect.
    /// Carries the parsed item name and order type so the tracker can mark
    /// the order as filled without opening the GUI.
    BazaarOrderFilled {
        item_name: String,
        is_buy_order: bool,
    },
    /// A bazaar order was collected (filled items/coins claimed) during ManageOrders.
    BazaarOrderCollected {
        item_name: String,
        is_buy_order: bool,
        /// Actual quantity claimed from this collection.  `None` when the
        /// filled amount could not be determined (falls back to the tracker's
        /// original order amount).
        claimed_amount: Option<u64>,
    },
    /// A bazaar order was cancelled during ManageOrders.
    BazaarOrderCancelled {
        item_name: String,
        is_buy_order: bool,
        /// When `true`, a `BazaarOrderCollected` event was already emitted for
        /// this same order (partial collect followed by cancel).  The handler
        /// should NOT call `remove_order` again because the collected event
        /// already removed the tracker entry.
        already_collected: bool,
    },
    /// An auction was cancelled via the web GUI
    AuctionCancelled {
        item_name: String,
        starting_bid: u64,
    },
    /// Reconciliation snapshot: the set of orders visible in the in-game
    /// Bazaar Orders window.  Each entry is (item_name, is_buy_order, amount, price_per_unit).
    /// The tracker should remove any orders not in this list.
    BazaarOrdersSnapshot {
        ingame_orders: Vec<(String, bool, u64, f64)>,
    },
    /// "You cannot view this auction!" was received — no active booster cookie.
    /// The bot cannot function; the user must manually buy a cookie.
    NoCookieDetected,
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
            manage_orders_cancelled: Arc::new(RwLock::new(0)),
            bazaar_at_limit: Arc::new(AtomicBool::new(false)),
            bazaar_daily_limit: Arc::new(AtomicBool::new(false)),
            auction_at_limit: Arc::new(AtomicBool::new(false)),
            auction_slot_blocked: Arc::new(AtomicBool::new(false)),
            auction_stuck_item_retries: Arc::new(std::sync::atomic::AtomicU8::new(0)),
            bazaar_order_rejected: Arc::new(AtomicBool::new(false)),
            cached_inventory_json: Arc::new(RwLock::new(None)),
            auto_cookie_hours: Arc::new(RwLock::new(0)),
            bedtiming: false,
            skip: false,
            bed_spam_click_delay: 100,
            insta_sell_item: Arc::new(RwLock::new(None)),
            bed_pre_click_ms: 100,
            active_auction_listings: Arc::new(RwLock::new(std::collections::HashSet::new())),
            ingame_name: Arc::new(RwLock::new(String::new())),
            manage_orders_cancel_open: Arc::new(AtomicBool::new(false)),
            manage_orders_target_item: Arc::new(RwLock::new(None)),
            bazaar_order_cancel_minutes_per_million: 5,
            cached_my_auctions_json: Arc::new(RwLock::new(None)),
            purchase_start_time: Arc::new(RwLock::new(None)),
            bin_view_open_time: Arc::new(RwLock::new(None)),
            bazaar_flips_paused: Arc::new(AtomicBool::new(false)),
            chat_batch_buffer: Arc::new(RwLock::new(Vec::new())),
            cached_window_json: Arc::new(RwLock::new(None)),
            inventory_full: Arc::new(AtomicBool::new(false)),
            cached_empty_player_slots: Arc::new(std::sync::atomic::AtomicU8::new(36)),
            command_queue: Arc::new(RwLock::new(None)),
            startup_in_progress: Arc::new(AtomicBool::new(false)),
            enable_bazaar_flips: Arc::new(AtomicBool::new(true)),
            command_processor_started: Arc::new(AtomicBool::new(false)),
            disconnect_requested: Arc::new(AtomicBool::new(false)),
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
            claim_sold_uuid_queue: Arc::new(RwLock::new(VecDeque::new())),
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
            auction_sell_aborted: Arc::new(AtomicBool::new(false)),
            auction_stuck_item_retries: self.auction_stuck_item_retries.clone(),
            scoreboard_scores: self.scoreboard_scores.clone(),
            sidebar_objective: self.sidebar_objective.clone(),
            scoreboard_teams: self.scoreboard_teams.clone(),
            manage_orders_cancelled: self.manage_orders_cancelled.clone(),
            bazaar_at_limit: self.bazaar_at_limit.clone(),
            bazaar_daily_limit: self.bazaar_daily_limit.clone(),
            auction_at_limit: self.auction_at_limit.clone(),
            auction_slot_blocked: self.auction_slot_blocked.clone(),
            bazaar_order_rejected: self.bazaar_order_rejected.clone(),
            purchase_start_time: self.purchase_start_time.clone(),
            bin_view_open_time: self.bin_view_open_time.clone(),
            last_buy_speed_ms: Arc::new(RwLock::new(None)),
            grace_period_spam_active: Arc::new(AtomicBool::new(false)),
            pending_purchase_at_ms: Arc::new(RwLock::new(None)),
            bed_timing_active: Arc::new(AtomicBool::new(false)),
            skip_click_sent: Arc::new(AtomicBool::new(false)),
            fast_buy_clicked: Arc::new(AtomicBool::new(false)),
            slot_data_notify: Arc::new(tokio::sync::Notify::new()),
            window_open_notify: Arc::new(tokio::sync::Notify::new()),
            window_open_info: Arc::new(RwLock::new(None)),
            cached_inventory_json: self.cached_inventory_json.clone(),
            auto_cookie_hours: self.auto_cookie_hours.clone(),
            bedtiming: self.bedtiming,
            skip: self.skip,
            bed_spam_click_delay: self.bed_spam_click_delay,
            cookie_time_secs: Arc::new(RwLock::new(0)),
            cookie_step: Arc::new(RwLock::new(CookieStep::Initial)),
            command_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            inventory_full: self.inventory_full.clone(),
            cached_empty_player_slots: self.cached_empty_player_slots.clone(),
            insta_sell_item: self.insta_sell_item.clone(),
            bed_pre_click_ms: self.bed_pre_click_ms,
            active_auction_listings: self.active_auction_listings.clone(),
            ingame_name: self.ingame_name.clone(),
            manage_orders_cancel_open: self.manage_orders_cancel_open.clone(),
            manage_orders_target_item: self.manage_orders_target_item.clone(),
            bazaar_order_cancel_minutes_per_million: self.bazaar_order_cancel_minutes_per_million,
            managing_order_context: Arc::new(RwLock::new(None)),
            cached_my_auctions_json: self.cached_my_auctions_json.clone(),
            manage_orders_processed: Arc::new(RwLock::new(std::collections::HashSet::new())),
            cancel_auction_item_name: Arc::new(RwLock::new(String::new())),
            cancel_auction_starting_bid: Arc::new(RwLock::new(0)),
            manage_orders_deadline: Arc::new(RwLock::new(None)),
            bazaar_flips_paused: self.bazaar_flips_paused.clone(),
            chat_batch_buffer: self.chat_batch_buffer.clone(),
            cached_window_json: self.cached_window_json.clone(),
            window_cache_rebuild_scheduled: Arc::new(AtomicBool::new(false)),
            inventory_cache_rebuild_scheduled: Arc::new(AtomicBool::new(false)),
            command_queue: self.command_queue.clone(),
            startup_in_progress: self.startup_in_progress.clone(),
            enable_bazaar_flips: self.enable_bazaar_flips.clone(),
            order_cancel_failures: Arc::new(RwLock::new(HashMap::new())),
            command_processor_started: self.command_processor_started.clone(),
            disconnect_requested: self.disconnect_requested.clone(),
        };
        
        // Build and start the client (this blocks until disconnection)
        let handler_state_clone = handler_state.clone();
        // Create the PacketAcceleratorPlugin sharing the same Notify/info as BotClientState.
        // The plugin registers ECS-level observers that fire DURING the frame
        // (in apply_deferred), bypassing the multi-hop Event::Packet channel pipeline.
        let accelerator_plugin = PacketAcceleratorPlugin::new(
            handler_state.window_open_notify.clone(),
            handler_state.window_open_info.clone(),
            handler_state.slot_data_notify.clone(),
        );
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new()
                .expect("Failed to create tokio runtime for bot - this should never happen unless system resources are exhausted");
            rt.block_on(async move {
                let exit_result = ClientBuilder::new()
                    .add_plugins(accelerator_plugin)
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

    /// Get the cached GUI window JSON for the web panel "Game View" tab.
    pub fn get_cached_window_json(&self) -> Option<String> {
        self.cached_window_json.read().clone()
    }

    /// Return the last cached "My Auctions" JSON, extracted from the in-game
    /// Manage Auctions GUI. Returns `None` if the bot has not opened the
    /// My Auctions window yet.
    pub fn get_cached_my_auctions_json(&self) -> Option<String> {
        self.cached_my_auctions_json.read().clone()
    }

    /// Returns the number of items currently tracked as active auction listings.
    pub fn active_auction_count(&self) -> usize {
        self.active_auction_listings.read().len()
    }

    /// Returns true if the bazaar order limit has been hit and not yet cleared.
    pub fn is_bazaar_at_limit(&self) -> bool {
        self.bazaar_at_limit.load(Ordering::Relaxed)
    }

    /// Returns true if the bazaar daily sell value limit has been hit.
    pub fn is_bazaar_daily_limit(&self) -> bool {
        self.bazaar_daily_limit.load(Ordering::Relaxed)
    }

    /// Clears the bazaar daily sell value limit flag (e.g. at 0:00 UTC reset).
    pub fn clear_bazaar_daily_limit(&self) {
        self.bazaar_daily_limit.store(false, Ordering::Relaxed);
    }

    /// Clears the bazaar order-limit flag.  Used by the idle-inventory
    /// failsafe to unstick accounts whose limit flag became stale (e.g.
    /// the collection chat message was missed).
    pub fn clear_bazaar_at_limit(&self) {
        self.bazaar_at_limit.store(false, Ordering::Relaxed);
    }

    /// Returns true if the auction house limit has been hit and not yet cleared.
    /// Used by `main.rs` to skip SellToAuction commands when at the cap.
    pub fn is_auction_at_limit(&self) -> bool {
        self.auction_at_limit.load(Ordering::Relaxed)
    }

    /// Returns true if the auction slot is blocked due to a stuck item.
    /// Prevents SellToAuction commands until cleared by a ClaimSold or
    /// ManageOrders cycle.
    pub fn is_auction_slot_blocked(&self) -> bool {
        self.auction_slot_blocked.load(Ordering::Relaxed)
    }

    /// Clear the auction-slot-blocked flag after a successful claim or manage cycle.
    /// Also resets the stuck-item retry counter so subsequent SellToAuction commands
    /// get a fresh set of retries.
    pub fn clear_auction_slot_blocked(&self) {
        self.auction_slot_blocked.store(false, Ordering::Relaxed);
        self.auction_stuck_item_retries.store(0, Ordering::Relaxed);
    }

    /// Clear the auction-at-limit flag so the bot retries listing.
    /// Called by the idle-inventory failsafe — AH slots may have freed up
    /// from expired auctions without triggering an ItemSold event.
    pub fn clear_auction_at_limit(&self) {
        self.auction_at_limit.store(false, Ordering::Relaxed);
    }

    /// Request the bot to disconnect from Hypixel.
    /// Sets state to Idle and logs the disconnect — actual TCP teardown happens
    /// in the bot runtime thread.
    pub fn disconnect(&self) {
        info!("[BotClient] Disconnect requested via web GUI");
        self.set_state(BotState::Idle);
    }

    /// Request a clean TCP-level disconnect from the Hypixel server.
    /// Sets the `disconnect_requested` flag that the command-processor loop
    /// watches; the loop then calls `bot.disconnect()` on the live Azalea
    /// `Client`, which tears down the TCP connection.
    pub fn request_disconnect(&self) {
        info!("[BotClient] Requesting Azalea disconnect");
        self.disconnect_requested.store(true, Ordering::Release);
    }

    /// Returns true if inventory is full (items stashed / no space to claim).
    /// Also checks the cached empty-slot count: if the inventory has free
    /// slots the flag is auto-cleared so stale "stashed away" reminders
    /// don't block BUY orders indefinitely.
    pub fn is_inventory_full(&self) -> bool {
        if !self.inventory_full.load(Ordering::Relaxed) {
            return false;
        }
        // Reality-check: the flag might be stale after a manual instasell.
        let empty = self.cached_empty_player_slots.load(Ordering::Relaxed);
        if empty >= MIN_FREE_SLOTS_FOR_BUY {
            info!("[Inventory] Clearing stale inventory_full flag — cached {} empty slots", empty);
            self.inventory_full.store(false, Ordering::Relaxed);
            return false;
        }
        true
    }

    /// Clear the inventory-full flag.  Called by the periodic order-check
    /// timer after a cooldown so that ManageOrders can retry BUY collection.
    pub fn clear_inventory_full(&self) {
        self.inventory_full.store(false, Ordering::Relaxed);
    }

    /// Returns true if inventory is near full (≤ NEAR_FULL_SLOT_THRESHOLD
    /// empty slots).  Used to stop claiming purchased items (bids) proactively
    /// so the bot always has room to sell.
    pub fn is_inventory_near_full(&self) -> bool {
        let empty = self.cached_empty_player_slots.load(Ordering::Relaxed);
        empty <= NEAR_FULL_SLOT_THRESHOLD
    }

    /// Returns the number of empty player inventory slots (cached).
    /// Used to cap BUY order amounts for unstackable items so the bot
    /// doesn't try to buy more items than it can hold.
    pub fn empty_slot_count(&self) -> u8 {
        self.cached_empty_player_slots.load(Ordering::Relaxed)
    }

    /// Clear window tracking state so the bot is no longer associated with
    /// any open window.  Used during the AH flip countdown to ensure the
    /// bot is free to process incoming flips.  The server-side window will
    /// time out automatically.
    pub fn close_current_window(&self) {
        self.handlers.clear_window_tracking();
    }

    /// Returns true if the startup workflow is currently running.
    /// Used by flip handlers to block bazaar flips during startup.
    pub fn is_startup_in_progress(&self) -> bool {
        self.startup_in_progress.load(Ordering::Relaxed)
    }

    /// Set the shared command queue so the startup workflow can enqueue commands.
    /// Must be called before `connect()`.
    pub fn set_command_queue(&self, queue: CommandQueue) {
        *self.command_queue.write() = Some(queue);
    }

    /// Drain the buffered chat messages for a chatBatch upload.
    /// Returns the messages collected since the last drain (may be empty).
    pub fn drain_chat_batch(&self) -> Vec<String> {
        let mut buf = self.chat_batch_buffer.write();
        std::mem::take(&mut *buf)
    }

    /// Record the current instant as the buy-speed start time.
    /// Called from execute_command right before /viewauction is sent,
    /// so buy speed measures **command-send → coins-in-escrow**.
    pub fn mark_purchase_start(&self) {
        *self.purchase_start_time.write() = Some(std::time::Instant::now());
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
    Initial,         // Sent /bz booster cookie, waiting for Bazaar window
    ItemDetail,      // Clicked cookie item (slot 11), waiting for detail window
    BuyConfirm,      // Clicked Buy Instantly (slot 10), waiting for confirm window
    WaitingForCookie, // Clicked Confirm, waiting for cookie to appear in inventory
    ConsumingCookie, // Right-clicked cookie, waiting for cookie GUI window
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
    /// UUID for direct ClaimSoldItem flow (legacy single-value fallback)
    pub claim_sold_uuid: Arc<RwLock<Option<String>>>,
    /// Queue of sold-auction UUIDs extracted from chat clickEvent (/viewauction <uuid>).
    /// Keeps claim order stable when multiple auctions sell close together.
    pub claim_sold_uuid_queue: Arc<RwLock<VecDeque<String>>>,
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
    /// Set when Hypixel rejects auction item placement ("You already have an item in the
    /// auction slot!"). Checked before ConfirmSell/FinalConfirm to abort the flow and
    /// prevent listing the wrong item.
    pub auction_sell_aborted: Arc<AtomicBool>,
    /// Number of times the auction sell flow has been retried due to a stuck item in the
    /// auction slot.  Reset at the start of each SellToAuction command.  When this reaches
    /// MAX_AUCTION_STUCK_ITEM_RETRIES the bot gives up and goes Idle.
    pub auction_stuck_item_retries: Arc<std::sync::atomic::AtomicU8>,
    /// Scoreboard scores: objective_name -> (owner -> (display_text, score))
    pub scoreboard_scores: Arc<RwLock<HashMap<String, HashMap<String, (String, u32)>>>>,
    /// Which objective is currently displayed in the sidebar slot
    pub sidebar_objective: Arc<RwLock<Option<String>>>,
    /// Team data for scoreboard rendering: team_name -> (prefix, suffix, members)
    pub scoreboard_teams: Arc<RwLock<HashMap<String, (String, String, Vec<String>)>>>,
    /// Count of bazaar orders cancelled during startup order management (shared with run_startup_workflow)
    pub manage_orders_cancelled: Arc<RwLock<u64>>,
    /// Set when Hypixel sends "You reached your maximum of XY Bazaar orders!".
    /// Cleared when an order fills. Prevents placing new orders while at the cap.
    pub bazaar_at_limit: Arc<AtomicBool>,
    /// Set when "[Bazaar] You reached the daily limit" is detected.
    /// Cleared on account switch or at 0:00 UTC daily reset.
    pub bazaar_daily_limit: Arc<AtomicBool>,
    /// Set when "Maximum auction count reached" is detected in the Manage Auctions GUI.
    /// Cleared when an auction is sold/claimed (slot freed). Prevents repeated
    /// SellToAuction → /ah → limit-detected → idle loops.
    pub auction_at_limit: Arc<AtomicBool>,
    /// Set after exhausting `MAX_AUCTION_STUCK_ITEM_RETRIES` — "You already have
    /// an item in the auction slot!" kept recurring.  Blocks SellToAuction commands
    /// until cleared by a ClaimSold / ManageOrders cycle.
    pub auction_slot_blocked: Arc<AtomicBool>,
    /// Set when the server rejects an order placement (e.g. "Your price isn't
    /// competitive enough").  Cleared before each confirm-click so only the
    /// response to the *current* placement attempt is captured.
    pub bazaar_order_rejected: Arc<AtomicBool>,
    /// Time when /viewauction was sent. Used for internal timing diagnostics.
    /// Shared with `BotClient` so main.rs can set it when the flip arrives.
    pub purchase_start_time: Arc<RwLock<Option<std::time::Instant>>>,
    /// Time when the BIN Auction View window opened for the current purchase —
    /// the start of the (new) buy-speed measurement.
    pub bin_view_open_time: Arc<RwLock<Option<std::time::Instant>>>,
    /// Buy speed in ms: from /viewauction sent to "Putting coins in escrow..."
    pub last_buy_speed_ms: Arc<RwLock<Option<u64>>>,
    /// Set to true while a grace-period spam-click loop is running so a second
    /// chat message does not start a duplicate loop.
    pub grace_period_spam_active: Arc<AtomicBool>,
    /// Raw COFL `purchaseAt` epoch-ms timestamp from the flip.  Converted to a
    /// local `Instant` only when a bed (grace-period) is detected in the BIN
    /// Auction View **and** bedtiming mode is enabled.
    pub pending_purchase_at_ms: Arc<RwLock<Option<i64>>>,
    /// Set to true while the bot is waiting for a bed (grace-period) to expire so
    /// the 5-second GUI watchdog does not incorrectly auto-close the BIN Auction View.
    pub bed_timing_active: Arc<AtomicBool>,
    /// Set when a skip-click (pre-click on predicted Confirm Purchase window) is sent.
    /// The Confirm Purchase handler checks this to avoid sending a duplicate reactive click.
    pub skip_click_sent: Arc<AtomicBool>,
    /// Set when the fast-path purchase task (spawned in execute_command, triggered
    /// by the ECS observer's window_open_notify) has already sent the buy-click
    /// and skip-click.  The Event::Packet handle_window_interaction checks this
    /// to avoid sending duplicate clicks.  Reset at the start of each purchase.
    pub fast_buy_clicked: Arc<AtomicBool>,
    /// Notified when ContainerSetSlot / ContainerSetContent arrives so the purchase
    /// handler can react instantly instead of polling every 10ms.
    pub slot_data_notify: Arc<tokio::sync::Notify>,
    /// Notified by the ECS-level MenuOpenedEvent observer (PacketAcceleratorPlugin).
    /// Fires during the ECS frame, bypassing the Event::Packet channel pipeline.
    pub window_open_notify: Arc<tokio::sync::Notify>,
    /// Most recent window open info, set by the ECS-level MenuOpenedEvent observer.
    /// Contains window_id, title, and timestamp.
    pub window_open_info: Arc<RwLock<Option<WindowOpenInfo>>>,
    /// Cached player-inventory JSON shared with BotClient for instant getInventory replies.
    pub cached_inventory_json: Arc<RwLock<Option<String>>>,
    /// AUTO_COOKIE config value (hours threshold to trigger a cookie buy). 0 = disabled.
    pub auto_cookie_hours: Arc<RwLock<u64>>,
    /// Config gate for purchaseAt-based bed (grace-period) timing mode.
    pub bedtiming: bool,
    /// Enable skip-click: pre-click confirm on predicted Confirm Purchase window.
    pub skip: bool,
    /// Interval in milliseconds for grace-period bed/gold_nugget click loops.
    pub bed_spam_click_delay: u64,
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
    /// Cached count of empty player inventory slots, updated on every inventory
    /// rebuild (ContainerSetContent / ContainerSetSlot).  Used to verify the
    /// inventory_full flag is not stale (e.g. after a manual instasell).
    pub cached_empty_player_slots: Arc<std::sync::atomic::AtomicU8>,
    /// Item name to instasell via bazaar "Sell Instantly" when inventory is dominated
    /// by one stackable item type. Set by ManageOrders, consumed by InstaSelling handler.
    pub insta_sell_item: Arc<RwLock<Option<String>>>,
    /// How many ms before bed timer expiry to start pre-clicking (default: 100).
    pub bed_pre_click_ms: u64,
    /// Items the bot has listed on the AH (by lowercase item name).
    /// Used to filter out coop member sales from our own sales.
    pub active_auction_listings: Arc<RwLock<std::collections::HashSet<String>>>,
    /// The bot's in-game name, used for coop sale filtering.
    pub ingame_name: Arc<RwLock<String>>,
    /// When true, the ManagingOrders handler also cancels open orders (startup mode).
    /// When false, it only collects filled orders and leaves open orders untouched.
    pub manage_orders_cancel_open: Arc<AtomicBool>,
    /// When set, the ManageOrders handler targets a specific order for cancellation
    /// instead of processing all orders.  Format: `(item_name, is_buy_order)`.
    /// Cleared at the start of each ManageOrders command.
    pub manage_orders_target_item: Arc<RwLock<Option<(String, bool)>>>,
    /// Cancel open bazaar orders when they are older than this many minutes per million coins.
    /// 0 disables age-based cancellation in periodic ManageOrders runs.
    pub bazaar_order_cancel_minutes_per_million: u64,
    /// Context of the order currently being processed in the ManageOrders iteration.
    /// Stored when clicking an order in Branch B so that Branch C (separate "Order options"
    /// window) can apply the same `cancel_due_to_age` logic.
    /// Fields: `(is_buy, order_display_name, order_identity, filled_amount)` where
    /// `order_identity` is the `(is_buy, item_tag)` tuple used by
    /// `should_cancel_open_order_due_to_age()`, and `filled_amount` is the actual
    /// filled quantity parsed from the "Filled: X/Y" lore line.
    pub managing_order_context: Arc<RwLock<Option<(bool, String, Option<(bool, String)>, Option<u64>)>>>,
    /// Cached "My Auctions" JSON shared with BotClient for instant replies.
    pub cached_my_auctions_json: Arc<RwLock<Option<String>>>,
    /// Persistent set of processed order names (normalized, slot-index-free) across
    /// ManageOrders window re-navigations.  Prevents re-clicking/re-emitting events
    /// for orders that were already handled in an earlier `/bz` cycle.
    /// Cleared at the start of each ManageOrders command.
    pub manage_orders_processed: Arc<RwLock<std::collections::HashSet<String>>>,
    /// Item name of the auction to cancel (set by CancelAuction command).
    pub cancel_auction_item_name: Arc<RwLock<String>>,
    /// Starting bid of the auction to cancel (for accurate identification).
    pub cancel_auction_starting_bid: Arc<RwLock<i64>>,
    /// Deadline for the current ManageOrders run. Set at the start of each
    /// ManageOrders command so that all window handlers (Branch A/B/C) can
    /// bail out early instead of waiting for the external 60-second timeout.
    pub manage_orders_deadline: Arc<RwLock<Option<tokio::time::Instant>>>,
    /// Shared AH-pause flag — when true, ManageOrders aborts early so AH
    /// flips can proceed without interference.
    pub bazaar_flips_paused: Arc<AtomicBool>,
    /// Buffer of Hypixel chat messages to send as a chatBatch to Coflnet WebSocket.
    pub chat_batch_buffer: Arc<RwLock<Vec<String>>>,
    /// Cached JSON of the currently-open GUI window (title, bot state, slot data).
    /// Updated on every OpenScreen / ContainerSetContent / ContainerClose event.
    /// The web panel polls this to show what the bot is currently "seeing".
    pub cached_window_json: Arc<RwLock<Option<String>>>,
    /// Debounce flag for `rebuild_cached_window_json` on `ContainerSetSlot` events.
    /// When true, a rebuild task is already scheduled; additional slot updates are
    /// coalesced into that single rebuild to avoid 100 % CPU during rapid updates.
    pub window_cache_rebuild_scheduled: Arc<AtomicBool>,
    /// Debounce flag for `rebuild_cached_inventory_json` on `ContainerSetSlot` events.
    /// Same pattern as `window_cache_rebuild_scheduled`: coalesces rapid per-slot
    /// updates into a single rebuild.
    pub inventory_cache_rebuild_scheduled: Arc<AtomicBool>,
    /// Shared reference to the command queue for the startup workflow.
    pub command_queue: Arc<RwLock<Option<CommandQueue>>>,
    /// Set while the startup workflow is running so flip handlers can block
    /// bazaar flips even when bot state briefly cycles through Idle between
    /// queued startup commands.
    pub startup_in_progress: Arc<AtomicBool>,
    /// Whether bazaar flips are enabled.  Shared with `BotClient` so the
    /// startup workflow can decide whether to cancel all open bazaar orders.
    pub enable_bazaar_flips: Arc<AtomicBool>,
    /// Persistent counter of cancel failures per order (normalized name → count).
    /// When a cancel attempt fails in Order options, the counter is incremented.
    /// After `MAX_CANCEL_RETRIES` failures for the same order, the order is skipped.
    /// Cleared on successful cancel or at the start of a `cancel_open` ManageOrders run.
    pub order_cancel_failures: Arc<RwLock<HashMap<String, u32>>>,
    /// Ensures the dedicated command processor is only spawned once.
    pub command_processor_started: Arc<AtomicBool>,
    /// Set to true to request a clean disconnect from Hypixel.
    /// The command processor polls this flag and calls `bot.disconnect()` when set.
    pub disconnect_requested: Arc<AtomicBool>,
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
            claim_sold_uuid_queue: Arc::new(RwLock::new(VecDeque::new())),
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
            auction_sell_aborted: Arc::new(AtomicBool::new(false)),
            auction_stuck_item_retries: Arc::new(std::sync::atomic::AtomicU8::new(0)),
            scoreboard_scores: Arc::new(RwLock::new(HashMap::new())),
            sidebar_objective: Arc::new(RwLock::new(None)),
            scoreboard_teams: Arc::new(RwLock::new(HashMap::new())),
            manage_orders_cancelled: Arc::new(RwLock::new(0)),
            bazaar_at_limit: Arc::new(AtomicBool::new(false)),
            bazaar_daily_limit: Arc::new(AtomicBool::new(false)),
            auction_at_limit: Arc::new(AtomicBool::new(false)),
            auction_slot_blocked: Arc::new(AtomicBool::new(false)),
            bazaar_order_rejected: Arc::new(AtomicBool::new(false)),
            purchase_start_time: Arc::new(RwLock::new(None)),
            bin_view_open_time: Arc::new(RwLock::new(None)),
            last_buy_speed_ms: Arc::new(RwLock::new(None)),
            grace_period_spam_active: Arc::new(AtomicBool::new(false)),
            pending_purchase_at_ms: Arc::new(RwLock::new(None)),
            bed_timing_active: Arc::new(AtomicBool::new(false)),
            skip_click_sent: Arc::new(AtomicBool::new(false)),
            fast_buy_clicked: Arc::new(AtomicBool::new(false)),
            slot_data_notify: Arc::new(tokio::sync::Notify::new()),
            window_open_notify: Arc::new(tokio::sync::Notify::new()),
            window_open_info: Arc::new(RwLock::new(None)),
            cached_inventory_json: Arc::new(RwLock::new(None)),
            auto_cookie_hours: Arc::new(RwLock::new(0)),
            bedtiming: false,
            skip: false,
            bed_spam_click_delay: 100,
            cookie_time_secs: Arc::new(RwLock::new(0)),
            cookie_step: Arc::new(RwLock::new(CookieStep::Initial)),
            command_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            inventory_full: Arc::new(AtomicBool::new(false)),
            cached_empty_player_slots: Arc::new(std::sync::atomic::AtomicU8::new(36)),
            insta_sell_item: Arc::new(RwLock::new(None)),
            bed_pre_click_ms: 100,
            active_auction_listings: Arc::new(RwLock::new(std::collections::HashSet::new())),
            ingame_name: Arc::new(RwLock::new(String::new())),
            manage_orders_cancel_open: Arc::new(AtomicBool::new(false)),
            manage_orders_target_item: Arc::new(RwLock::new(None)),
            bazaar_order_cancel_minutes_per_million: 5,
            managing_order_context: Arc::new(RwLock::new(None)),
            cached_my_auctions_json: Arc::new(RwLock::new(None)),
            manage_orders_processed: Arc::new(RwLock::new(std::collections::HashSet::new())),
            cancel_auction_item_name: Arc::new(RwLock::new(String::new())),
            cancel_auction_starting_bid: Arc::new(RwLock::new(0)),
            manage_orders_deadline: Arc::new(RwLock::new(None)),
            bazaar_flips_paused: Arc::new(AtomicBool::new(false)),
            chat_batch_buffer: Arc::new(RwLock::new(Vec::new())),
            cached_window_json: Arc::new(RwLock::new(None)),
            window_cache_rebuild_scheduled: Arc::new(AtomicBool::new(false)),
            inventory_cache_rebuild_scheduled: Arc::new(AtomicBool::new(false)),
            command_queue: Arc::new(RwLock::new(None)),
            startup_in_progress: Arc::new(AtomicBool::new(false)),
            enable_bazaar_flips: Arc::new(AtomicBool::new(true)),
            order_cancel_failures: Arc::new(RwLock::new(HashMap::new())),
            command_processor_started: Arc::new(AtomicBool::new(false)),
            disconnect_requested: Arc::new(AtomicBool::new(false)),
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
        } else {
            // Full item_data serialization failed (e.g., enchantment HashMap keys).
            // Fall back to direct component access for the display name.
            use azalea_inventory::components::CustomName;
            if let Some(cn) = item_data.component_patch.get::<CustomName>() {
                if let Ok(cn_val) = serde_json::to_value(cn) {
                    let plain = extract_text_from_chat_component(&cn_val);
                    if !plain.is_empty() {
                        return Some(plain);
                    }
                }
            }
        }
    }
    None
}

/// Get the display name of an item slot preserving §-color codes for rarity display.
/// Same logic as `get_item_display_name_from_slot` but uses `extract_text_with_colors`
/// so the web panel can render the name in the item's rarity color.
fn get_item_display_name_with_colors_from_slot(item: &azalea_inventory::ItemStack) -> Option<String> {
    if let Some(item_data) = item.as_present() {
        if let Ok(value) = serde_json::to_value(item_data) {
            let components = value.get("components");
            let name_val = components
                .and_then(|c| c.get("minecraft:custom_name"))
                .or_else(|| components.and_then(|c| c.get("minecraft:item_name")));
            if let Some(name_val) = name_val {
                let raw = if name_val.is_string() {
                    name_val.as_str().unwrap_or("").to_string()
                } else {
                    name_val.to_string()
                };
                let colored = if let Ok(json_val) = serde_json::from_str::<serde_json::Value>(&raw) {
                    extract_text_with_colors(&json_val)
                } else {
                    raw
                };
                return Some(colored);
            }
        } else {
            use azalea_inventory::components::CustomName;
            if let Some(cn) = item_data.component_patch.get::<CustomName>() {
                if let Ok(cn_val) = serde_json::to_value(cn) {
                    let colored = extract_text_with_colors(&cn_val);
                    if !colored.is_empty() {
                        return Some(colored);
                    }
                }
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

/// Recursively extract text from a Minecraft chat component, preserving §-color codes.
/// Converts JSON `"color"` fields back to § codes so the web UI can render them.
fn extract_text_with_colors(val: &serde_json::Value) -> String {
    let mut result = String::new();
    // Emit color/format codes from the component's properties
    if let Some(color) = val.get("color").and_then(|v| v.as_str()) {
        let code = match color {
            "black" => Some('0'), "dark_blue" => Some('1'), "dark_green" => Some('2'),
            "dark_aqua" => Some('3'), "dark_red" => Some('4'), "dark_purple" => Some('5'),
            "gold" => Some('6'), "gray" => Some('7'), "dark_gray" => Some('8'),
            "blue" => Some('9'), "green" => Some('a'), "aqua" => Some('b'),
            "red" => Some('c'), "light_purple" => Some('d'), "yellow" => Some('e'),
            "white" => Some('f'),
            _ => None,
        };
        if let Some(c) = code {
            result.push('§');
            result.push(c);
        }
    }
    if val.get("bold").and_then(|v| v.as_bool()) == Some(true) { result.push_str("§l"); }
    if val.get("italic").and_then(|v| v.as_bool()) == Some(true) { result.push_str("§o"); }
    if val.get("underlined").and_then(|v| v.as_bool()) == Some(true) { result.push_str("§n"); }
    if val.get("strikethrough").and_then(|v| v.as_bool()) == Some(true) { result.push_str("§m"); }
    if val.get("obfuscated").and_then(|v| v.as_bool()) == Some(true) { result.push_str("§k"); }
    if let Some(text) = val.get("text").and_then(|v| v.as_str()) {
        result.push_str(text);
    }
    if let Some(extra) = val.get("extra").and_then(|v| v.as_array()) {
        for part in extra {
            result.push_str(&extract_text_with_colors(part));
        }
    }
    result
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

/// Get lore lines from an item slot preserving Minecraft §-color codes.
/// Used by the web panel to render colorful lore tooltips.
fn get_item_lore_with_colors_from_slot(item: &azalea_inventory::ItemStack) -> Vec<String> {
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
                    let colored = if let Ok(json_val) = serde_json::from_str::<serde_json::Value>(&raw) {
                        extract_text_with_colors(&json_val)
                    } else {
                        raw
                    };
                    lore_lines.push(colored);
                }
            }
        }
    }
    lore_lines
}

/// Format a f64 price with commas for bazaar sign input.
/// Uses integer arithmetic (tenths) to avoid floating-point subtraction issues.
/// Commas are inserted into the integer part: 7500000.0 → "7,500,000",
/// 2040950.5 → "2,040,950.5".
fn format_price_for_sign(price: f64) -> String {
    // Work in tenths-of-a-coin as an integer to avoid floating-point precision
    // issues when splitting integer and fractional parts of large prices.
    let tenths = (price * 10.0).round() as i64;
    let int_part = tenths / 10;
    let frac_digit = (tenths % 10).unsigned_abs();

    let int_str = format_with_commas(int_part);
    if frac_digit == 0 {
        int_str
    } else {
        format!("{}.{}", int_str, frac_digit)
    }
}

/// Insert commas as thousands separators into an integer.
/// Handles negative numbers correctly: -1234567 → "-1,234,567".
fn format_with_commas(n: i64) -> String {
    let is_negative = n < 0;
    let s = n.unsigned_abs().to_string();
    let mut result = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            result.push(',');
        }
        result.push(ch);
    }
    if is_negative {
        result.insert(0, '-');
    }
    result
}

/// Normalize a string for fuzzy item-name matching: lowercase, strip Minecraft
/// color codes, replace hyphens/underscores with spaces, remove common decorative
/// Unicode symbols, and collapse consecutive whitespace.
fn normalize_for_matching(s: &str) -> String {
    remove_mc_colors(s)
        .to_lowercase()
        .replace(['-', '_'], " ")
        .replace(['☘', '☂', '✪', '◆', '❤'], "")
        .split_whitespace()
        .collect::<Vec<&str>>()
        .join(" ")
}

fn find_slot_by_name(slots: &[azalea_inventory::ItemStack], name: &str) -> Option<usize> {
    let name_lower = name.to_lowercase();

    // Phase 1: fast-path — original exact `contains` check (case-insensitive).
    for (i, item) in slots.iter().enumerate() {
        if let Some(display) = get_item_display_name_from_slot(item) {
            if display.to_lowercase().contains(&name_lower) {
                return Some(i);
            }
        }
    }

    // Phase 2: normalized `contains` — strips hyphens, underscores, decorative
    // symbols and MC color codes so e.g. "turbo wheat v" matches "Turbo-Wheat V".
    let name_norm = normalize_for_matching(name);
    for (i, item) in slots.iter().enumerate() {
        if let Some(display) = get_item_display_name_from_slot(item) {
            if normalize_for_matching(&display).contains(&name_norm) {
                return Some(i);
            }
        }
    }

    // Phase 3: token-based — every whitespace-delimited token of the search term
    // must appear somewhere in the (normalized) slot display name.  This handles
    // cases where the slot name has extra words, e.g. "Turbo-Wheat Hoe V" still
    // matches a search for "turbo wheat v".
    let tokens: Vec<&str> = name_norm.split_whitespace().collect();
    if tokens.len() > 1 {
        for (i, item) in slots.iter().enumerate() {
            if let Some(display) = get_item_display_name_from_slot(item) {
                let display_norm = normalize_for_matching(&display);
                if tokens.iter().all(|tok| display_norm.contains(tok)) {
                    return Some(i);
                }
            }
        }
    }

    None
}

/// Like [`find_slot_by_name`] but restricted to the player-inventory portion of
/// an open container (`inv_start`..end) and to non-empty slots.
///
/// The auction "select item" flow uses this for its name fallback so it can only
/// ever resolve to a real item the player is holding — never a GUI/display slot
/// (clicking one to "sell" a non-item is an impossible action Watchdog flags),
/// and never an empty slot. Returns the absolute slot index.
fn find_inventory_slot_by_name(
    slots: &[azalea_inventory::ItemStack],
    inv_start: usize,
    name: &str,
) -> Option<usize> {
    if inv_start >= slots.len() {
        return None;
    }
    let name_lower = name.to_lowercase();
    let name_norm = normalize_for_matching(name);
    let tokens: Vec<&str> = name_norm.split_whitespace().collect();
    // Phase 0: exact contains; 1: normalized contains; 2: all-tokens-present.
    for phase in 0..3u8 {
        for i in inv_start..slots.len() {
            if slots[i].is_empty() {
                continue;
            }
            let display = match get_item_display_name_from_slot(&slots[i]) {
                Some(d) => d,
                None => continue,
            };
            let hit = match phase {
                0 => display.to_lowercase().contains(&name_lower),
                1 => normalize_for_matching(&display).contains(&name_norm),
                _ => {
                    tokens.len() > 1 && {
                        let dn = normalize_for_matching(&display);
                        tokens.iter().all(|t| dn.contains(t))
                    }
                }
            };
            if hit {
                return Some(i);
            }
        }
    }
    None
}

fn lore_contains_phrase(lore: &[String], needle: &str) -> bool {
    let needle_lower = needle.to_lowercase();
    lore.iter()
        .any(|line| remove_mc_colors(line).to_lowercase().contains(&needle_lower))
}

fn find_slot_by_lore_contains(slots: &[azalea_inventory::ItemStack], needle: &str) -> Option<usize> {
    slots.iter().enumerate().find_map(|(i, item)| {
        let lore = get_item_lore_from_slot(item);
        lore_contains_phrase(&lore, needle).then_some(i)
    })
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

/// Check if a lowercased string contains "ended" as a standalone word
/// (e.g. "auction ended") but NOT as a suffix of another word (e.g.
/// "recommended", "mended", "befriended").  Uses a regex word boundary.
fn contains_word_ended(text: &str) -> bool {
    static RE: Lazy<regex::Regex> = Lazy::new(|| regex::Regex::new(r"\bended\b").unwrap());
    RE.is_match(text)
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
        || contains_word_ended(&combined)
        || combined.contains("expired")
        || combined.contains("click to claim")
        || combined.contains("claim your");
    // Must NOT have active-auction indicators (from TypeScript activeIndicators)
    let is_active = combined.contains("ends in")
        || combined.contains("buy it now")
        || combined.contains("starting bid");
    has_claimable && !is_active
}

fn is_my_auctions_window_title(window_title: &str) -> bool {
    window_title.contains("Manage Auctions") || window_title.contains("My Auctions")
}

/// Calculate the number of "window-content" slots (excluding the 36 player
/// inventory slots appended at the bottom).  Capped at 54 (max chest size).
fn window_content_slot_count(total_slots: usize) -> usize {
    total_slots.saturating_sub(36).min(54)
}

fn is_bazaar_orders_window_title(window_title: &str) -> bool {
    let lower = window_title.to_lowercase();
    lower.contains("manage orders")
        || lower.contains("your orders")
        || lower.contains("bazaar orders")
}

fn starts_with_phrase_delimited(text: &str, phrase: &str) -> bool {
    if !text.starts_with(phrase) {
        return false;
    }
    match text[phrase.len()..].chars().next() {
        None => true,
        Some(c) => !c.is_ascii_alphanumeric(),
    }
}

fn is_bazaar_order_entry_name(name: &str) -> bool {
    let lower = name.trim_start().to_lowercase();
    if starts_with_phrase_delimited(&lower, "buy order") {
        return true;
    }
    if starts_with_phrase_delimited(&lower, "sell offer") {
        return true;
    }
    // Legacy order-list entries are usually "BUY <item>" / "SELL <item>".
    // Avoid matching malformed "buy orderX"/"sell offerY" labels.
    if lower.starts_with("buy ") && !lower.starts_with("buy order") {
        return true;
    }
    if lower.starts_with("sell ") && !lower.starts_with("sell offer") {
        return true;
    }
    false
}

fn normalize_bazaar_order_text(text: &str) -> String {
    remove_mc_colors(text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Parse a "[Bazaar] Your Buy Order/Sell Offer for X was filled!" notification.
/// Returns `(item_name, is_buy_order)` or `None` if the message doesn't match.
fn parse_bazaar_filled_notification(message: &str) -> Option<(String, bool)> {
    if !message.contains("[Bazaar]") || !message.contains("was filled") {
        return None;
    }
    let is_buy = message.contains("Buy Order");
    let is_sell = message.contains("Sell Offer");
    if !is_buy && !is_sell {
        return None;
    }
    let prefix = if is_buy { "Buy Order for " } else { "Sell Offer for " };
    let item_name = message.split(prefix).nth(1)
        .and_then(|s| s.split(" was filled").next())
        .unwrap_or("")
        .trim()
        .to_string();
    if item_name.is_empty() {
        return None;
    }
    Some((item_name, is_buy))
}

/// Extract the clean item name from a ManageOrders order slot display name.
/// For enchanted books (and other items where the display name is generic),
/// prefers the lore-based identity which contains the specific enchantment
/// name (e.g. "Blast Protection VII" instead of "Enchanted Book").
/// Falls back to stripping the BUY/SELL prefix from the display name.
fn clean_order_item_name(order_name: &str, order_identity: &Option<(bool, String)>) -> String {
    let stripped = remove_mc_colors(order_name).trim().to_string();

    // Check if the display name is generic (e.g. "SELL Enchanted Book" or "BUY Enchanted Book").
    // When the lore-based identity has a *different* item name, prefer it.
    if let Some((_, ref lore_item)) = order_identity {
        // Strip the order prefix from display name for comparison.
        // These prefixes are lowercase because we compare against a lowercased
        // copy of the display name (different from the case-preserved prefixes
        // used below for the display name extraction path).
        let display_item = {
            let lower = stripped.to_lowercase();
            let mut found = String::new();
            for prefix in ["buy order: ", "sell offer: ", "buy ", "sell "] {
                if let Some(rest) = lower.strip_prefix(prefix) {
                    found = rest.trim().to_string();
                    break;
                }
            }
            found
        };
        let lore_lower = lore_item.to_lowercase();
        // If lore identity differs from display name, use lore (it's more specific).
        // e.g. display="enchanted book", lore="blast protection vii"
        if !display_item.is_empty() && !lore_lower.is_empty() && display_item != lore_lower {
            return crate::utils::to_title_case(lore_item);
        }
    }

    // First try stripping prefix from the ORIGINAL display name (preserves case)
    for prefix in ["BUY ", "SELL ", "Buy Order: ", "Sell Offer: "] {
        if let Some(rest) = stripped.strip_prefix(prefix) {
            let rest = rest.trim();
            if !rest.is_empty() {
                return rest.to_string();
            }
        }
    }
    // Case-insensitive fallback for unexpected prefix casing
    let lower = stripped.to_lowercase();
    for prefix in ["buy order: ", "sell offer: ", "buy ", "sell "] {
        if let Some(rest) = lower.strip_prefix(prefix) {
            if !rest.is_empty() {
                // Apply title case to recover proper display name
                return crate::utils::to_title_case(rest.trim());
            }
        }
    }
    // Last resort: use identity (lowercased) with title case
    if let Some((_, item)) = order_identity {
        return crate::utils::to_title_case(item);
    }
    stripped
}

fn parse_bazaar_order_identity_from_name(name: &str) -> Option<(bool, String)> {
    let normalized = normalize_bazaar_order_text(name);
    if let Some(item) = normalized.strip_prefix("buy order: ") {
        return Some((true, item.trim().to_string()));
    }
    if let Some(item) = normalized.strip_prefix("sell offer: ") {
        return Some((false, item.trim().to_string()));
    }
    if let Some(item) = normalized.strip_prefix("buy ") {
        if !item.starts_with("order") {
            return Some((true, item.trim().to_string()));
        }
    }
    if let Some(item) = normalized.strip_prefix("sell ") {
        if !item.starts_with("offer") {
            return Some((false, item.trim().to_string()));
        }
    }
    None
}

fn parse_bazaar_order_identity_from_lore(lore: &[String]) -> Option<(bool, String)> {
    let mut side: Option<bool> = None;
    let mut item_name: Option<String> = None;

    for line in lore {
        let clean = normalize_bazaar_order_text(line);
        if side.is_none() {
            if clean.contains("buy order") {
                side = Some(true);
            } else if clean.contains("sell offer") {
                side = Some(false);
            }
        }
        if item_name.is_none() {
            for prefix in ["item:", "product:", "commodity:"] {
                if let Some(rest) = clean.strip_prefix(prefix) {
                    let candidate = rest.trim();
                    if !candidate.is_empty() {
                        item_name = Some(candidate.to_string());
                        break;
                    }
                }
            }
        }
    }

    match (side, item_name) {
        (Some(is_buy), Some(item)) => Some((is_buy, item)),
        _ => None,
    }
}

fn is_buy_bazaar_order_name(name: &str) -> bool {
    let lower = name.trim_start().to_lowercase();
    starts_with_phrase_delimited(&lower, "buy order")
        || lower.starts_with("buy ")
}

fn parse_bazaar_order_identity(name: &str, lore: &[String]) -> Option<(bool, String)> {
    // Prefer lore-based identity when available — it contains the *specific*
    // item name (e.g. "Blast Protection VII") while the display name may be
    // generic (e.g. "Enchanted Book").  Fall back to name-based parsing when
    // the lore has no identity data.
    parse_bazaar_order_identity_from_lore(lore)
        .or_else(|| parse_bazaar_order_identity_from_name(name))
}

fn should_treat_as_bazaar_order_slot(name: &str, identity: Option<&(bool, String)>) -> bool {
    is_bazaar_order_entry_name(name) || identity.is_some()
}

/// Returns `true` when the order lore indicates items/coins are ready to collect.
///
/// Hypixel shows "Filled: X/Y 100%!" for fully filled orders and includes
/// "Click to claim!" or "items to claim" in the tooltip.  Partially filled
/// orders also show "items to claim" even though they are not 100%.
///
/// We intentionally check for *any* claimable indicator so that both fully
/// and partially filled orders are considered for collection.
fn is_order_claimable_from_lore(lore: &[String]) -> bool {
    for line in lore {
        let clean = remove_mc_colors(line).to_lowercase();
        // "Filled: 64/64 100%!" — fully filled
        if clean.contains("100%") {
            return true;
        }
        // "Click to claim!" — present on fully filled orders
        if clean.contains("click to claim") {
            return true;
        }
        // "You have X items to claim!" — present on filled buy orders
        if clean.contains("to claim") {
            return true;
        }
    }
    false
}

/// Parse the filled amount from Hypixel's order lore.
///
/// Lore lines contain `"Filled: X/Y Z%"` (e.g. `"Filled: 32/64 50%"`).
/// Returns `Some((filled, total))` when found.  Used to determine the actual
/// quantity claimed (not the original order amount) so buy-cost recording and
/// profit calculations are accurate for partial fills.
fn parse_filled_amount_from_lore(lore: &[String]) -> Option<(u64, u64)> {
    for line in lore {
        let clean = remove_mc_colors(line);
        // Expected format: "Filled: 32/64 50%"
        // After lowercasing: "filled: 32/64 50%"
        let lower = clean.to_lowercase();
        if let Some(idx) = lower.find("filled:") {
            // Extract the part after "filled:" — e.g. " 32/64 50%"
            let after = &clean[idx + "filled:".len()..].trim_start();
            // Split at '/' to get filled and total
            if let Some(slash_pos) = after.find('/') {
                let filled_str = after[..slash_pos].replace(',', "");
                let rest = &after[slash_pos + 1..];
                // Total ends at the next space or end of string
                let total_str = rest.split_whitespace().next().unwrap_or("").replace(',', "");
                if let (Ok(filled), Ok(total)) = (filled_str.parse::<u64>(), total_str.parse::<u64>()) {
                    return Some((filled, total));
                }
            }
        }
    }
    None
}

/// Parse the total order amount from Hypixel's order lore.
///
/// Lore lines contain `"Order amount: 64x"` (e.g. `"Order amount: 2,560x"`).
/// Returns the total amount when found.
fn parse_order_amount_from_lore(lore: &[String]) -> Option<u64> {
    for line in lore {
        let clean = remove_mc_colors(line);
        let lower = clean.to_lowercase();
        if let Some(idx) = lower.find("order amount:") {
            let after = &clean[idx + "order amount:".len()..].trim_start();
            // Strip trailing 'x' and commas, e.g. "2,560x" → "2560"
            let num_str: String = after.chars()
                .take_while(|c| c.is_ascii_digit() || *c == ',')
                .collect::<String>()
                .replace(',', "");
            if let Ok(amount) = num_str.parse::<u64>() {
                return Some(amount);
            }
        }
    }
    // Fallback: try to extract the total from the Filled line (X/Y → Y)
    parse_filled_amount_from_lore(lore).map(|(_, total)| total)
}

/// Parse the price per unit from Hypixel's order lore.
///
/// Lore lines contain `"Price per unit: 1,958.0 coins"`.
/// Returns the price when found.
fn parse_unit_price_from_lore(lore: &[String]) -> Option<f64> {
    for line in lore {
        let clean = remove_mc_colors(line);
        let lower = clean.to_lowercase();
        if let Some(idx) = lower.find("price per unit:") {
            let after = &clean[idx + "price per unit:".len()..].trim_start();
            let num_str: String = after.chars()
                .take_while(|c| c.is_ascii_digit() || *c == ',' || *c == '.')
                .collect::<String>()
                .replace(',', "");
            if let Ok(price) = num_str.parse::<f64>() {
                return Some(price);
            }
        }
    }
    None
}

/// Returns true when Hypixel chat indicates the purchase flow is terminally invalid
/// and should be aborted immediately instead of waiting for the GUI watchdog timeout.
fn is_terminal_purchase_failure_message(message: &str) -> bool {
    message.contains("You didn't participate in this auction!")
        || message.contains("This auction wasn't found!")
        || message.contains("The auction wasn't found!")
        || message.contains("You cannot view this auction!")
        || message.contains("You cannot afford this auction!")
        || message.contains("don't have the space required to claim")
        || message.contains("Your inventory is full!")
}

/// Handle events from the Azalea client
async fn event_handler(
    bot: Client,
    event: Event,
    state: BotClientState,
) -> Result<()> {
    // Spawn the dedicated command processor on the first event_handler call.
    // This replaces the opportunistic try_recv() drain: commands are now
    // processed as soon as they arrive, eliminating jitter from waiting for
    // the next Azalea event.
    if !state.command_processor_started.swap(true, Ordering::AcqRel) {
        let command_bot = bot.clone();
        let command_state = state.clone();
        tokio::spawn(async move {
            command_processor(command_bot, command_state).await;
        });
    }

    // While an intentional disconnect (humanization rest break) is in effect,
    // stay offline: re-disable auto-reconnect and drop the connection on every
    // event — including the Login fired by any reconnect attempt — so the bot
    // cannot silently rejoin and resume flipping mid-break. Reset by the
    // post-break process restart.
    if state.disconnect_requested.load(Ordering::Acquire) {
        enforce_offline(&bot);
        return Ok(());
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

            // IMPORTANT: reset startup_in_progress so that if the bot disconnected
            // mid-startup (while the flag was true), the new session's startup
            // workflow is not silently skipped.  Without this reset,
            // run_startup_workflow() would see the stale `true` value, bail out
            // immediately, and leave the flag permanently set — causing every
            // non-startup command (BazaarBuyOrder, ManageOrders, …) to be dropped
            // by the main.rs command-processor guard indefinitely.
            state.startup_in_progress.store(false, Ordering::Relaxed);
            
            // Reset the cancel-failure tracker on login so previously-stuck
            // orders get a fresh set of retry attempts after reconnect.
            state.order_cancel_failures.write().clear();

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
                let command_queue_wd = state.command_queue.clone();
                let startup_in_progress_wd = state.startup_in_progress.clone();
                let enable_bazaar_flips_wd = state.enable_bazaar_flips.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;
                    let already_done = *teleported_wd.read();
                    if !already_done {
                        warn!("[Startup] 30-second watchdog: forcing startup completion");
                        *joined_wd.write() = true;
                        *teleported_wd.write() = true;
                        // Retry /play sb in case the initial attempt failed (lobby not ready)
                        send_chat_command(&bot_wd, "/play sb");
                        // Wait for SkyBlock to load (5s) + island teleport delay combined
                        tokio::time::sleep(tokio::time::Duration::from_secs(5 + ISLAND_TELEPORT_DELAY_SECS)).await;
                        send_chat_command(&bot_wd, "/is");
                        tokio::time::sleep(tokio::time::Duration::from_secs(TELEPORT_COMPLETION_WAIT_SECS)).await;
                        run_startup_workflow(bot_wd, bot_state_wd, event_tx_wd, manage_orders_cancelled_wd, auto_cookie_wd, command_queue_wd, startup_in_progress_wd, enable_bazaar_flips_wd).await;
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
                    send_chat_command(&bot_clone, "/play sb");
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

            // Buffer the message for periodic chatBatch upload to Coflnet.
            // Uses clean text (color codes stripped) matching the Coflnet mod protocol.
            let clean_for_batch = crate::bot::handlers::BotEventHandlers::remove_color_codes(&message);
            if !clean_for_batch.trim().is_empty() {
                state.chat_batch_buffer.write().push(clean_for_batch);
            }

            // Detect purchase/sold messages and emit events
            let clean_message = crate::bot::handlers::BotEventHandlers::remove_color_codes(&message);

            if clean_message.contains("You purchased") && clean_message.contains("coins!") {
                // "You purchased <item> for <price> coins!"
                if let Some((item_name, price)) = parse_purchased_message(&clean_message) {
                    // Include the buy speed measured from flip received to escrow message
                    let buy_speed_ms = state.last_buy_speed_ms.write().take();
                    let _ = state.event_tx.send(BotEvent::ItemPurchased { item_name, price, buy_speed_ms });
                }
            } else if clean_message.contains("Putting coins in escrow") {
                // "Putting coins in escrow..." — purchase accepted by server.
                // Buy speed is measured from when the BIN Auction View opened
                // (act-on-open speed), falling back to the /viewauction send time
                // if the view-open marker is somehow missing.
                let start = state.bin_view_open_time.write().take()
                    .or_else(|| *state.purchase_start_time.read());
                // Always clear the /viewauction marker too so it doesn't leak.
                *state.purchase_start_time.write() = None;
                if let Some(start) = start {
                    let speed_ms = start.elapsed().as_millis() as u64;
                    *state.last_buy_speed_ms.write() = Some(speed_ms);
                    let _ = state.event_tx.send(BotEvent::ChatMessage(
                        format!("§f[§4BAF§f]: §aAuction bought in {}ms", speed_ms)
                    ));
                    info!("[AH] Buy speed (view-open → escrow): {}ms", speed_ms);
                }
            } else if *state.bot_state.read() == BotState::Purchasing
                && is_terminal_purchase_failure_message(&clean_message)
            {
                // Abort immediately on terminal purchase failure messages so we don't keep a
                // stale purchasing window open for 5s and overlap the next queued command.
                // Use write lock for atomic check-and-set to prevent a double-close race
                // with the slot-31 non-buyable handler (both can fire concurrently when
                // e.g. a potato + "You didn't participate" arrive at the same time).
                let should_close = {
                    let mut bs = state.bot_state.write();
                    if *bs == BotState::Purchasing {
                        *bs = BotState::Idle;
                        true
                    } else {
                        false
                    }
                };
                if should_close {
                    let window_id = *state.last_window_id.read();
                    warn!(
                        "[AH] Terminal purchase failure detected: \"{}\" — closing window {}",
                        clean_message, window_id
                    );
                    if window_id > 0 {
                        send_raw_close(&bot, window_id, &state.handlers);
                    }
                    state.grace_period_spam_active.store(false, Ordering::Relaxed);
                    state.skip_click_sent.store(false, Ordering::Relaxed);
                    *state.purchase_start_time.write() = None;
                    *state.pending_purchase_at_ms.write() = None;
                    state.bed_timing_active.store(false, Ordering::Relaxed);
                    // "You cannot view this auction!" means no booster cookie — alert user
                    if clean_message.contains("You cannot view this auction!") {
                        let _ = state.event_tx.send(BotEvent::NoCookieDetected);
                    }
                }
            } else if clean_message.contains("[Auction]") && clean_message.contains("bought") && clean_message.contains("for") && clean_message.contains("coins") {
                // "[Auction] <buyer> bought <item> for <price> coins"
                // Always claim sold auctions. The active_auction_listings filter was
                // previously used for coop filtering but it is an in-memory set that
                // is lost on restart and does not track items listed manually or via
                // /cofl sell — causing sold auctions like the Hyperion to be silently
                // skipped. Attempting to claim a coop member's sale is harmless
                // (the AH UI simply won't show a claim button).
                if let Some((buyer, item_name, price)) = parse_sold_message(&clean_message) {
                    // Skip if the buyer is our own bot — Hypixel sends "[Auction] OurName
                    // bought X for Y coins" as a purchase notification to the buyer as well.
                    // Without this check the bot would treat its own purchase as a sale,
                    // producing a false "Item Sold (Loss)" webhook with 0s time-to-sell.
                    let own_name = state.ingame_name.read().clone();
                    if !own_name.is_empty() && buyer.eq_ignore_ascii_case(&own_name) {
                        debug!("[Auction] Ignoring own purchase notification: \"{}\" bought \"{}\" for {}", buyer, item_name, price);
                    } else {
                        let item_key = crate::bot::handlers::BotEventHandlers::remove_color_codes(&item_name).to_lowercase();
                        // Housekeeping: remove from active listings if present
                        state.active_auction_listings.write().remove(&item_key);
                        // Try to extract the auction UUID from the JSON representation of the
                        // chat message first — Hypixel embeds "/viewauction <UUID>" in the
                        // clickEvent of the "CLICK" component, which is invisible in plain text
                        // but present in the serialised FormattedText JSON.  We try the JSON
                        // path first because for Hypixel sold messages the UUID is *only* in
                        // the click event, so trying plain text first would always fail.
                        let uuid = serde_json::to_string(&chat.message()).ok()
                            .as_deref()
                            .and_then(extract_viewauction_uuid)
                            .or_else(|| extract_viewauction_uuid(&clean_message));
                        if let Some(ref u) = uuid {
                            info!("[AH] Extracted viewauction UUID for claim: {}", u);
                            let mut sold_queue = state.claim_sold_uuid_queue.write();
                            if !sold_queue.iter().any(|queued| queued == u) {
                                if sold_queue.len() >= MAX_CLAIM_SOLD_UUID_QUEUE {
                                    sold_queue.pop_front();
                                }
                                sold_queue.push_back(u.clone());
                            }
                        }
                        *state.claim_sold_uuid.write() = uuid;
                        // An auction sold — a slot is now free; clear the auction-limit flag.
                        if state.auction_at_limit.load(Ordering::Relaxed) {
                            info!("[Auction] Auction sold, clearing auction-limit flag");
                            state.auction_at_limit.store(false, Ordering::Relaxed);
                        }
                        let _ = state.event_tx.send(BotEvent::ItemSold { item_name, price, buyer });
                    }
                }
            } else if clean_message.contains("You already have an item in the auction slot") {
                // Hypixel rejected our item placement — there was already an item in the slot.
                // Close the window (returning the stuck item to inventory) and retry the flow,
                // up to MAX_AUCTION_STUCK_ITEM_RETRIES times to avoid packet-spam kicks.
                if *state.bot_state.read() == BotState::Selling {
                    // fetch_add returns the *previous* value, so attempt 0..2 are the
                    // 3 retry attempts (when MAX is 3); attempt 3 triggers the give-up.
                    let attempt = state.auction_stuck_item_retries.fetch_add(1, Ordering::Relaxed);

                    // Common to both paths: abort current flow.
                    state.auction_sell_aborted.store(true, Ordering::Relaxed);
                    let window_id = *state.last_window_id.read();
                    if window_id > 0 {
                        // Click slot 13 (auction preview slot) to explicitly remove
                        // the stuck item before closing.  Simply closing the window
                        // does NOT always clear Hypixel's internal auction-slot state,
                        // causing the same error on retry.  Clicking the slot tells the
                        // server to return the item to the player's inventory.
                        info!("[Auction] Clicking slot 13 to clear stuck item before closing window");
                        send_raw_click(&bot, window_id, 13);
                        send_raw_close(&bot, window_id, &state.handlers);
                    }

                    if attempt >= MAX_AUCTION_STUCK_ITEM_RETRIES {
                        warn!(
                            "[Auction] ABORTING: stuck item in auction slot after {} retries — giving up and blocking further listings",
                            attempt
                        );
                        state.auction_slot_blocked.store(true, Ordering::Relaxed);
                        *state.bot_state.write() = BotState::Idle;
                    } else {
                        warn!(
                            "[Auction] ABORTING: \"{}\" — closing window to remove stuck item and retrying (attempt {}/{})",
                            clean_message, attempt + 1, MAX_AUCTION_STUCK_ITEM_RETRIES
                        );
                        // Restart the auction flow: reset step and re-open /ah after a
                        // delay so Hypixel processes the window close first.
                        *state.auction_step.write() = AuctionStep::Initial;
                        let bot_clone = bot.clone();
                        let state_clone = state.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(tokio::time::Duration::from_millis(AUCTION_RETRY_AFTER_STUCK_ITEM_MS)).await;
                            // Only retry if still in Selling state (not interrupted by another command)
                            if *state_clone.bot_state.read() == BotState::Selling {
                                info!("[Auction] Retrying auction after removing stuck item — sending /ah");
                                state_clone.auction_sell_aborted.store(false, Ordering::Relaxed);
                                send_chat_command(&bot_clone, "/ah");
                            }
                        });
                    }
                }
            } else if clean_message.contains("BIN Auction started for") {
                // "BIN Auction started for <item>!" — Hypixel's confirmation that our listing
                // was accepted.  Emit AuctionListed using the context stored in state.
                // This matches TypeScript sellHandler.ts messageListener pattern.
                let item = state.auction_item_name.read().clone();
                let bid  = *state.auction_starting_bid.read();
                let dur  = *state.auction_duration_hours.read();

                // Diagnostic safety check: verify the listed item roughly matches what
                // we intended.  Hypixel includes reforge/star prefixes (e.g. "Withered
                // Valkyrie ✪✪✪✪✪➌") that may not appear in our item_name, so we do a
                // best-effort bidirectional substring comparison.  This is purely a log
                // diagnostic — the auction is already created at this point.
                if let Some(actual_item) = clean_message
                    .split("BIN Auction started for ")
                    .nth(1)
                    .and_then(|s| s.strip_suffix('!'))
                {
                    let actual_clean = crate::bot::handlers::BotEventHandlers::remove_color_codes(actual_item)
                        .trim().to_lowercase();
                    let intended_clean = crate::bot::handlers::BotEventHandlers::remove_color_codes(&item)
                        .trim().to_lowercase();
                    if !intended_clean.is_empty() && !actual_clean.contains(&intended_clean) && !intended_clean.contains(&actual_clean) {
                        error!(
                            "[Auction] ITEM MISMATCH! Intended: \"{}\" but Hypixel listed: \"{}\". \
                             This may indicate the wrong item was sold!",
                            item, actual_item
                        );
                    }
                }

                // Track this as our active listing for coop sale filtering
                if !item.is_empty() {
                    let item_key = crate::bot::handlers::BotEventHandlers::remove_color_codes(&item).to_lowercase();
                    state.active_auction_listings.write().insert(item_key);
                }
                // Listing succeeded — clear any stale auction-limit flag and
                // reset the stuck-item retry counter so the next SellToAuction
                // starts with a fresh count.
                state.auction_at_limit.store(false, Ordering::Relaxed);
                state.auction_stuck_item_retries.store(0, Ordering::Relaxed);
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
                        let click_interval_ms = state.bed_spam_click_delay.max(1);
                        info!("[AH] Grace period detected — starting bed spam ({} ms interval)", click_interval_ms);
                        tokio::spawn(async move {
                            const MAX_FAILED_CLICKS: usize = 5;
                            let mut failed_clicks: usize = 0;
                            loop {
                                tokio::time::sleep(tokio::time::Duration::from_millis(click_interval_ms)).await;
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
                                    click_window_slot(&bot_clone, &shared_window_id, window_id, 31).await;
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
                // An order was collected — a slot is now free
                if state.bazaar_at_limit.load(Ordering::Relaxed) {
                    info!("[Bazaar] Order collected, clearing order-limit flag");
                    state.bazaar_at_limit.store(false, Ordering::Relaxed);
                }
            }

            // Detect bazaar daily sell value limit
            if clean_message.contains("You reached the daily limit") && clean_message.contains("bazaar") {
                warn!("[Bazaar] Daily sell value limit reached — pausing bazaar flips until 0:00 UTC");
                state.bazaar_daily_limit.store(true, Ordering::Relaxed);
            }

            // Detect bazaar order rejection ("Your price isn't competitive enough")
            // so the confirm handler knows not to emit BazaarOrderPlaced.
            if clean_message.contains("[Bazaar]") && clean_message.contains("Your price isn't competitive enough") {
                warn!("[Bazaar] Order rejected — price not competitive");
                state.bazaar_order_rejected.store(true, Ordering::Relaxed);
            }

            // Detect "[Bazaar] Your Buy Order/Sell Offer for X was filled!" — trigger a
            // ManageOrders run so the filled items are collected promptly.
            if let Some((filled_item, is_buy)) = parse_bazaar_filled_notification(&clean_message) {
                info!("[BazaarOrders] Order fill notification — {} \"{}\"", if is_buy { "BUY" } else { "SELL" }, filled_item);
                let _ = state.event_tx.send(BotEvent::BazaarOrderFilled {
                    item_name: filled_item,
                    is_buy_order: is_buy,
                });
            }

            // Detect "You don't have the space required to claim that!" and set the
            // inventory_full flag so ManageOrders can stop and log remaining orders.
            // Verify actual slot count to avoid a stale flag if inventory was freed
            // between the server sending the message and us processing it.
            if clean_message.contains("don't have the space required to claim") {
                let empty = count_empty_player_slots(&bot);
                if empty < MIN_FREE_SLOTS_FOR_BUY as usize {
                    warn!("[ManageOrders] Inventory full — {} empty slots, logging unclaimed orders", empty);
                    state.inventory_full.store(true, Ordering::Relaxed);
                } else {
                    info!("[ManageOrders] Claim-space warning but {} empty slots — ignoring stale message", empty);
                }
            }

            // Detect "You have X item(s) stashed away!" — Hypixel sends this both
            // when items are newly stashed AND as a periodic reminder while any
            // stashed items exist.  Only set inventory_full if the player
            // inventory actually has very few free slots (≤ 2), because the
            // reminder keeps firing long after the player frees space via
            // instasell or other means.
            if clean_message.contains("stashed away") {
                let empty = count_empty_player_slots(&bot);
                if empty < MIN_FREE_SLOTS_FOR_BUY as usize {
                    warn!("[ManageOrders] Items stashed and inventory nearly full ({} empty slots)", empty);
                    state.inventory_full.store(true, Ordering::Relaxed);
                } else {
                    debug!("[ManageOrders] Stashed-away reminder ignored — inventory has {} empty slots", empty);
                }
            }

            // Detect "Inventory full? Don't forget to check out your Storage
            // inside the SkyBlock Menu!" — Hypixel sends this frequently when
            // the player's inventory is full.  Only set the flag when inventory
            // truly has very few free slots, in case the message arrives after
            // the player freed space (e.g. via manual instasell).
            if clean_message.contains("Inventory full?") {
                let empty = count_empty_player_slots(&bot);
                if empty < MIN_FREE_SLOTS_FOR_BUY as usize {
                    warn!("[ManageOrders] Inventory full hint detected ({} empty slots)", empty);
                    state.inventory_full.store(true, Ordering::Relaxed);
                } else {
                    debug!("[ManageOrders] Inventory-full hint ignored — inventory has {} empty slots", empty);
                }
            }

            // Detect "You don't have anything to sell!" during SellingInventoryBz
            // — Hypixel sends this when inventory has no instasellable items.
            if clean_message.contains("don't have anything to sell")
                && *state.bot_state.read() == BotState::SellingInventoryBz
            {
                info!("[SellInventoryBz] Nothing to sell — closing window and going idle");
                *state.bazaar_step.write() = BazaarStep::Initial;
                let wid = *state.last_window_id.read();
                if wid > 0 {
                    send_raw_close(&bot, wid, &state.handlers);
                }
                *state.bot_state.write() = BotState::Idle;
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
                        let command_queue_startup = state.command_queue.clone();
                        let startup_in_progress_startup = state.startup_in_progress.clone();
                        let enable_bazaar_flips_startup = state.enable_bazaar_flips.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(tokio::time::Duration::from_secs(ISLAND_TELEPORT_DELAY_SECS)).await;
                            send_chat_command(&bot_clone, "/is");
                            
                            // Wait for teleport to complete
                            tokio::time::sleep(tokio::time::Duration::from_secs(TELEPORT_COMPLETION_WAIT_SECS)).await;

                            run_startup_workflow(bot_clone, bot_state, event_tx_startup, manage_orders_cancelled_startup, auto_cookie_startup, command_queue_startup, startup_in_progress_startup, enable_bazaar_flips_startup).await;
                        });
                    }
                }
            }
        }
        
        Event::Packet(packet) => {
            // Handle specific packets for window open/close and inventory updates
            match packet.as_ref() {
                ClientboundGamePacket::OpenScreen(open_screen) => {
                    // Record the instant the OpenScreen packet reaches our
                    // event handler via the Event::Packet channel pipeline.
                    let event_handler_at = std::time::Instant::now();

                    // If a purchase is in-flight, compare the ECS observer timestamp
                    // (set by PacketAcceleratorPlugin during apply_deferred) with the
                    // Event::Packet handler timestamp to measure pipeline overhead.
                    if let Some(t0) = *state.purchase_start_time.read() {
                        let total_ms = t0.elapsed().as_secs_f64() * 1000.0;
                        // Read the observer timestamp, dropping the lock immediately.
                        let observer_ts = state.window_open_info.read()
                            .as_ref()
                            .map(|info| info.timestamp);
                        if let Some(obs_ts) = observer_ts {
                            let observer_ms = obs_ts.duration_since(t0).as_secs_f64() * 1000.0;
                            let pipeline_ms = event_handler_at
                                .duration_since(obs_ts)
                                .as_secs_f64() * 1000.0;
                            info!(
                                "[Timing] /viewauction → window: ECS observer {:.1}ms, \
                                 Event::Packet handler {:.1}ms (+{:.1}ms pipeline overhead)",
                                observer_ms, total_ms, pipeline_ms
                            );
                        } else {
                            info!(
                                "[Timing] /viewauction → OpenScreen handler: {:.1}ms \
                                 (ECS observer did not fire — check PacketAcceleratorPlugin)",
                                total_ms
                            );
                        }
                    }

                    let window_id = open_screen.container_id;
                    let window_type = format!("{:?}", open_screen.menu_type);
                    let title = open_screen.title.to_string();
                    
                    // Parse the title from JSON format
                    let parsed_title = state.handlers.parse_window_title(&title);
                    
                    // Store window ID
                    *state.last_window_id.write() = window_id as u8;
                    
                    state.handlers.handle_window_open(window_id as u8, &window_type, &parsed_title).await;

                    // Log the synchronous overhead of the OpenScreen handler
                    // itself (title parsing + state writes + logging).  This
                    // should be <1 ms; if it is significantly higher, a lock
                    // contention problem exists.
                    if let Some(t0) = *state.purchase_start_time.read() {
                        let handler_ms = event_handler_at.elapsed().as_secs_f64() * 1000.0;
                        let total_ms = t0.elapsed().as_secs_f64() * 1000.0;
                        info!(
                            "[Timing] OpenScreen handler overhead: {:.2}ms \
                             (total since /viewauction: {:.1}ms)",
                            handler_ms, total_ms
                        );
                    }
                    // Defer the (expensive) window-JSON rebuild so the event
                    // handler returns faster.  On the purchase path this is
                    // critical: ContainerSetContent may arrive in the very next
                    // packet frame and its handler must fire
                    // slot_data_notify.notify_waiters() without waiting for the
                    // JSON rebuild to finish.
                    {
                        let cache_bot = bot.clone();
                        let cache_state = state.clone();
                        tokio::spawn(async move {
                            rebuild_cached_window_json(&cache_bot, &cache_state);
                        });
                    }
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
                        let wdog_deadline = state.manage_orders_deadline.clone();
                        let wdog_bz_limit = state.bazaar_at_limit.clone();
                        let wdog_handlers = state.handlers.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                            let still_open  = *wdog_last.read() == wdog_wid;
                            let cur_state   = *wdog_state.read();
                            let is_bed      = wdog_bed.load(Ordering::Relaxed);
                            let is_interactive = matches!(cur_state,
                                BotState::Purchasing | BotState::Bazaar | BotState::Selling
                                | BotState::ClaimingPurchased | BotState::ClaimingSold
                                | BotState::InstaSelling
                                | BotState::CancellingAuction | BotState::SellingInventoryBz
                                | BotState::ManagingOrders
                            );
                            // Only fire if no new command started since this window was opened.
                            let gen_unchanged = wdog_gen.load(Ordering::SeqCst) == wdog_gen_at_open;
                            if still_open && is_interactive && !is_bed && gen_unchanged {
                                warn!("[GUI] Window {} open for >5 s in state {:?} — auto-closing", wdog_wid, cur_state);
                                send_raw_close(&wdog_bot, wdog_wid, &wdog_handlers);
                                // Clean up ManagingOrders-specific state so the bot
                                // doesn't remain stuck with a stale deadline or the
                                // bazaar order-limit flag blocking new flips.
                                if cur_state == BotState::ManagingOrders {
                                    *wdog_deadline.write() = None;
                                    wdog_bz_limit.store(false, Ordering::Relaxed);
                                }
                                *wdog_state.write() = BotState::Idle;
                                wdog_spam.store(false, Ordering::Relaxed);
                            }
                        });
                    }

                    // Handle window interactions in a spawned task so this event
                    // handler returns immediately.  This is critical for the
                    // purchase flow: if ContainerSetContent arrives in a
                    // separate packet frame, its handler must be able to fire
                    // slot_data_notify.notify_waiters() without waiting for the
                    // OpenScreen handler to finish.
                    {
                        let bot_s = bot.clone();
                        let state_s = state.clone();
                        let title_s = parsed_title.clone();
                        tokio::spawn(async move {
                            let result = std::panic::AssertUnwindSafe(
                                handle_window_interaction(&bot_s, &state_s, window_id as u8, &title_s)
                            );
                            if let Err(e) = futures::FutureExt::catch_unwind(result).await {
                                error!("[WindowHandler] panic in handle_window_interaction: {:?}", e);
                            }
                        });
                    }
                }
                
                ClientboundGamePacket::ContainerClose(_) => {
                    // Clear grace-period spam and bed-timing flags so a new BIN Auction View
                    // can start fresh.
                    state.grace_period_spam_active.store(false, Ordering::Relaxed);
                    *state.pending_purchase_at_ms.write() = None;
                    state.bed_timing_active.store(false, Ordering::Relaxed);
                    state.handlers.handle_window_close().await;
                    // Defer the window-JSON rebuild so the event handler returns
                    // quickly.  bot.menu() briefly locks the ECS World mutex; doing
                    // the full NBT-extraction + JSON-serialisation synchronously
                    // keeps that contention window open and delays the next ECS
                    // schedule cycle, contributing to slow frames.
                    {
                        let bot_close = bot.clone();
                        let state_close = state.clone();
                        tokio::spawn(async move {
                            rebuild_cached_window_json(&bot_close, &state_close);
                        });
                    }
                    if state.event_tx.send(BotEvent::WindowClose).is_err() {
                        debug!("Failed to send WindowClose event - receiver dropped");
                    }
                }
                
                ClientboundGamePacket::ContainerSetSlot(_slot_update) => {
                    // Wake the purchase handler FIRST so it can react to slot 31
                    // data on another thread without waiting for the inventory
                    // JSON rebuild.
                    state.slot_data_notify.notify_waiters();
                    // Debounce the inventory-JSON rebuild: individual slot updates
                    // can fire dozens of times per GUI interaction.  Each call locks
                    // the ECS World mutex (via bot.menu()) and serialises all
                    // inventory slots to JSON.  Coalescing into a single rebuild
                    // after the debounce window dramatically reduces both CPU usage
                    // and World-lock contention that causes slow ECS frames.
                    if !state.inventory_cache_rebuild_scheduled.swap(true, Ordering::Relaxed) {
                        let bot_inv = bot.clone();
                        let state_inv = state.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(tokio::time::Duration::from_millis(INVENTORY_CACHE_REBUILD_DEBOUNCE_MS)).await;
                            rebuild_cached_inventory_json(&bot_inv, &state_inv);
                            state_inv.inventory_cache_rebuild_scheduled.store(false, Ordering::Relaxed);
                        });
                    }
                    // Debounce the window-JSON rebuild: individual slot updates can fire
                    // dozens of times per GUI interaction.  Coalesce them into a single
                    // rebuild after the debounce window to avoid excessive CPU from
                    // repeated NBT extraction + JSON serialisation.
                    if !state.window_cache_rebuild_scheduled.swap(true, Ordering::Relaxed) {
                        let bot_clone = bot.clone();
                        let state_clone = state.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(tokio::time::Duration::from_millis(WINDOW_CACHE_REBUILD_DEBOUNCE_MS)).await;
                            rebuild_cached_window_json(&bot_clone, &state_clone);
                            state_clone.window_cache_rebuild_scheduled.store(false, Ordering::Relaxed);
                        });
                    }
                }
                
                ClientboundGamePacket::ContainerSetContent(_content) => {
                    // Log when ContainerSetContent arrives during a purchase
                    // flow — this populates slot 31 and unblocks the buy-click.
                    if let Some(t0) = *state.purchase_start_time.read() {
                        if *state.bot_state.read() == BotState::Purchasing {
                            info!(
                                "[Timing] /viewauction → ContainerSetContent: {:.1}ms",
                                t0.elapsed().as_secs_f64() * 1000.0
                            );
                        }
                    }
                    // Wake the purchase handler FIRST so it can react to slot 31
                    // data on another thread without waiting for the inventory
                    // JSON rebuild.
                    state.slot_data_notify.notify_waiters();
                    // Defer both JSON rebuilds so the event handler returns quickly.
                    // Each rebuild calls bot.menu() which briefly locks the ECS World
                    // mutex; running them synchronously keeps the handler blocked for
                    // 20-50 ms of NBT extraction + JSON serialisation, starving
                    // subsequent event processing and increasing World-lock contention
                    // with the ECS schedule loop (causing slow frames).
                    {
                        let bot_inv = bot.clone();
                        let state_inv = state.clone();
                        tokio::spawn(async move {
                            rebuild_cached_inventory_json(&bot_inv, &state_inv);
                        });
                    }
                    {
                        let bot_win = bot.clone();
                        let state_win = state.clone();
                        tokio::spawn(async move {
                            rebuild_cached_window_json(&bot_win, &state_win);
                        });
                    }
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
                        if state.auction_sell_aborted.load(Ordering::Relaxed) {
                            warn!("[Auction] Sign opened but auction sell aborted — skipping");
                            return Ok(());
                        }
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

/// If `disconnect_requested` is set, clears it, calls `bot.disconnect()`, and returns `true`.
/// The caller should return immediately when this returns `true`.
/// Force the client offline and keep it offline.
///
/// Azalea's AutoReconnectPlugin rejoins the server ~5s after ANY disconnect, so
/// a single `bot.disconnect()` is silently undone mid-break. This:
/// 1. Sets `AutoReconnectDelay` to `Duration::MAX` (treated as "disabled") so no
///    rejoin is scheduled for this disconnect, and
/// 2. Issues the disconnect.
///
/// It is idempotent and cheap, so `event_handler` calls it on *every* event
/// while `disconnect_requested` is set — that way if a reconnect slips through,
/// the `Login` event it produces immediately drops the connection again. The
/// post-break process restart spawns a fresh ECS with auto-reconnect re-enabled.
fn enforce_offline(bot: &Client) {
    {
        let mut ecs = bot.ecs.lock();
        if let Some(mut delay) = ecs.get_resource_mut::<AutoReconnectDelay>() {
            delay.delay = std::time::Duration::MAX;
        }
    }
    bot.disconnect();
}

fn handle_disconnect_if_requested(bot: &Client, state: &BotClientState) -> bool {
    if state.disconnect_requested.load(Ordering::Acquire) {
        info!("[CommandProcessor] Disconnect requested — going offline (auto-reconnect disabled)");
        // NOTE: deliberately do NOT clear `disconnect_requested`. event_handler
        // keeps enforcing offline on every event so any auto-reconnect is
        // immediately dropped again. The flag is reset only by the post-break
        // process restart.
        enforce_offline(bot);
        true
    } else {
        false
    }
}

/// Process queued bot commands independently from the Azalea event stream.
///
/// Previously commands were only drained opportunistically from `event_handler`,
/// which meant a `PurchaseAuction` could sit idle until the next packet/event.
/// This dedicated loop removes that extra scheduling jitter.
async fn command_processor(bot: Client, state: BotClientState) {
    loop {
        // Check for a pending disconnect request before waiting for the next command.
        if handle_disconnect_if_requested(&bot, &state) {
            return;
        }

        let command = {
            let mut command_rx = state.command_rx.lock().await;
            // Use a short timeout so we can re-check disconnect_requested periodically.
            tokio::time::timeout(
                tokio::time::Duration::from_millis(200),
                command_rx.recv(),
            )
            .await
        };

        match command {
            // Timeout — loop back and re-check disconnect_requested.
            Err(_elapsed) => continue,
            Ok(None) => {
                debug!("Bot command channel closed; stopping command processor");
                break;
            }
            Ok(Some(cmd)) => {
                // Re-check disconnect flag; don't execute more commands if we're about to disconnect.
                if handle_disconnect_if_requested(&bot, &state) {
                    return;
                }
                execute_command(&bot, &cmd, &state).await;
            }
        }
    }
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

    // Safety net: close any stale GUI window that is still open before executing
    // a new command.  Sending chat commands (e.g. /pickupstash, /ah, /bz) while a
    // window is open causes Hypixel to respond "You cannot pick these items up while
    // in an inventory" or silently fail.  Closing the window first prevents the bot
    // from being stuck "in an inventory".
    if let Some(open_wid) = state.handlers.current_window_id() {
        warn!("[SafeClose] Closing window {} before executing {}", open_wid, command.command_type.display_name());
        send_raw_close(bot, open_wid, &state.handlers);
        tokio::time::sleep(tokio::time::Duration::from_millis(WINDOW_CLOSE_DELAY_MS)).await;
    }

    match &command.command_type {
        CommandType::SendChat { message } => {
            // Send chat message to Minecraft
            info!("Sending chat message: {}", message);
            if message.starts_with('/') {
                send_chat_command(bot, message);
            } else {
                bot.write_chat_packet(message);
            }
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

            // Clear stale observer data from any previous window so the timing
            // comparison in the OpenScreen handler is accurate for THIS purchase.
            *state.window_open_info.write() = None;

            // Send /viewauction immediately — do NOT wait for tick alignment.
            // The server processes incoming packets at the end of each 50ms
            // tick anyway, so sending instantly means the packet lands in the
            // very next tick window.  Waiting for a tick boundary only added
            // up to 50ms of unnecessary latency.

            // Record buy-speed start time right before sending /viewauction
            // so the measurement covers command-send → coins-in-escrow (the
            // relevant metric), NOT flip-receive → escrow which includes
            // unrelated queue wait time.
            // Safe: commands execute sequentially from the queue processor.
            *state.purchase_start_time.write() = Some(std::time::Instant::now());
            // Clear the BIN-view-open marker; it's set when the auction view
            // actually opens, and buy speed is measured from there.
            *state.bin_view_open_time.write() = None;

            send_raw_chat_command(bot, &chat_command);

            // Store raw COFL purchaseAt timestamp.  It is only converted to a
            // local Instant later — and only when the auction turns out to be a
            // bed (grace-period) and bedtiming mode is enabled.
            *state.pending_purchase_at_ms.write() = flip.purchase_at_ms;
            
            // Reset fast-buy flag before setting state to Purchasing.
            state.fast_buy_clicked.store(false, Ordering::Relaxed);
            
            // Set state to purchasing
            *state.bot_state.write() = BotState::Purchasing;

            // ---- Fast-path purchase task ----
            // Spawn a task that waits on the ECS observer's window_open_notify
            // (fires during apply_deferred, 20-70ms BEFORE Event::Packet delivers
            // the OpenScreen event).  When the BIN Auction View opens, this task
            // reads slot 31 and sends buy-click + skip-click immediately, bypassing
            // the Event::Packet pipeline overhead entirely.
            //
            // Anti-cheat safety:
            // - Still waits for the server to send OpenScreen before clicking
            // - Still checks slot 31 contains gold_nugget before any click
            // - Uses identical ServerboundContainerClick packets
            // - TCP ordering guarantees the server has prepared the window
            // - This is equivalent to having lower client-side latency
            {
                let fast_bot = bot.clone();
                let fast_state = state.clone();
                tokio::spawn(async move {
                    // Wait for the ECS observer to detect OpenScreen.
                    // The observer fires during apply_deferred (~20-70ms before
                    // Event::Packet would deliver it).  The /viewauction RTT is
                    // at least 50ms (one server tick) + network RTT, so this task
                    // is always waiting before the notification fires.
                    fast_state.window_open_notify.notified().await;

                    // Verify we're still purchasing (could have been aborted).
                    if *fast_state.bot_state.read() != BotState::Purchasing {
                        return;
                    }

                    // Read window info set by the ECS observer.
                    let info = fast_state.window_open_info.read().clone();
                    let info = match info {
                        Some(i) => i,
                        None => return,
                    };

                    let window_id = info.window_id as u8;
                    let title = fast_state.handlers.parse_window_title(&info.title);

                    if !title.contains("BIN Auction View") && !title.contains("Auction View") {
                        return;
                    }

                    // Mark when the BIN Auction View opened — buy speed is measured
                    // from here. Only set once per purchase (the fast path is the
                    // earliest detector).
                    {
                        let mut w = fast_state.bin_view_open_time.write();
                        if w.is_none() {
                            *w = Some(std::time::Instant::now());
                        }
                    }

                    if let Some(t0) = *fast_state.purchase_start_time.read() {
                        info!(
                            "[Timing] /viewauction → fast-path started: {:.1}ms",
                            t0.elapsed().as_secs_f64() * 1000.0
                        );
                    }

                    // Wait for slot 31 data — same safety gate as the Event::Packet
                    // path.  If OpenScreen and ContainerSetContent arrive in the same
                    // TCP segment (common), the ECS frame processes both before
                    // releasing the World lock, so slot 31 is already populated when
                    // we read it.
                    let slot_31_kind = {
                        let poll_deadline = tokio::time::Instant::now()
                            + tokio::time::Duration::from_millis(500);
                        let mut kind;
                        loop {
                            let notified = fast_state.slot_data_notify.notified();
                            let menu = fast_bot.menu();
                            let slots = menu.slots();
                            kind = slots.get(31).map(|s| {
                                if s.is_empty() { "air".to_string() }
                                else { s.kind().to_string().to_lowercase() }
                            }).unwrap_or_else(|| "air".to_string());
                            if kind != "air" || tokio::time::Instant::now() >= poll_deadline {
                                break;
                            }
                            let remaining = poll_deadline - tokio::time::Instant::now();
                            tokio::select! {
                                _ = notified => {}
                                _ = tokio::time::sleep(remaining) => {}
                            }
                        }
                        kind
                    };

                    if !slot_31_kind.contains("gold") {
                        // Not a buyable auction (bed, potato, feather, etc.).
                        // Let the Event::Packet handler deal with non-buyable cases.
                        return;
                    }

                    // gold_nugget confirmed — slot 31 is the real, server-sent
                    // buy button, so clicking it now is clicking something that
                    // is actually there.  Click immediately (event-driven: we
                    // react the moment the confirmed state arrives, so the action
                    // resolves within one tick).
                    if let Some(t0) = *fast_state.purchase_start_time.read() {
                        info!(
                            "[Timing] /viewauction → fast-path buy click: {:.1}ms",
                            t0.elapsed().as_secs_f64() * 1000.0
                        );
                    }

                    // Buy click on the confirmed window/slot 31.
                    send_raw_click(&fast_bot, window_id, 31);

                    if fast_state.skip {
                        // Skip pre-click: click slot 11 of the Confirm Purchase
                        // window in the SAME TCP burst as the buy click.  The
                        // confirm window does not exist yet, but the buy click
                        // (processed first, same tick) opens it as id
                        // window_id+1 — so this is a click on a window we KNOW
                        // will be there.  This is the one sanctioned click-ahead.
                        //
                        // NOTE: we deliberately do NOT re-click slot 31 here.
                        // The buy click already replaced the BIN Auction View
                        // with the Confirm Purchase window, so a second slot-31
                        // click would land on a window that no longer exists —
                        // an impossible action that Watchdog flags.
                        let next_wid = if window_id == 255 { 1u8 } else { window_id + 1 };
                        info!("[AH] Fast-path skip: pre-clicking confirm slot 11 on window {} (same burst)", next_wid);
                        fast_state.skip_click_sent.store(true, Ordering::Relaxed);
                        send_raw_click(&fast_bot, next_wid, 11);
                    } else {
                        info!("[AH] Fast-path: gold_nugget confirmed — buy click sent");
                    }

                    // Signal that the fast path handled the buy click so the
                    // Event::Packet handler doesn't send a duplicate click.
                    fast_state.fast_buy_clicked.store(true, Ordering::Relaxed);
                });
            }
        }
        CommandType::BazaarBuyOrder { item_name, item_tag, amount, price_per_unit } => {
            // Abort immediately if at the bazaar order limit — no point opening
            // the GUI only to have the order rejected.
            if state.bazaar_at_limit.load(Ordering::Relaxed) {
                warn!("[Bazaar] Skipping BUY order for \"{}\" — already at bazaar order limit", item_name);
                return;
            }
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
            send_chat_command(bot, &cmd);
            *state.bot_state.write() = BotState::Bazaar;
        }
        CommandType::BazaarSellOrder { item_name, item_tag, amount, price_per_unit } => {
            // Do NOT abort SELL orders when at the bazaar order limit.
            // The server may reject the order, but it's better to try
            // (the limit flag may be stale) than to silently drop a sell
            // recommendation that would have freed inventory space.
            if state.bazaar_at_limit.load(Ordering::Relaxed) {
                info!("[Bazaar] Attempting SELL order for \"{}\" despite at_limit flag (sell orders are critical)", item_name);
            }
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
            send_chat_command(bot, &cmd);
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
                click_window_slot(bot, &state.last_window_id, window_id, *slot).await;
            } else {
                warn!("No window open (window_id=0), cannot click slot {}", slot);
            }
        }
        CommandType::SwapProfile { profile_name } => {
            info!("Swapping to profile: {}", profile_name);
            // TypeScript: sends /profiles command and clicks on profile
            send_chat_command(bot, "/profiles");
            // TODO: Implement profile selection from menu when window opens
            warn!("SwapProfile implementation incomplete - needs window interaction");
        }
        CommandType::AcceptTrade { player_name } => {
            info!("Accepting trade with player: {}", player_name);
            // TypeScript: sends /trade <player> command
            send_chat_command(bot, &format!("/trade {}", player_name));
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
            state.auction_sell_aborted.store(false, Ordering::Relaxed);
            // NOTE: Do NOT reset auction_stuck_item_retries here.  If a previous
            // SellToAuction hit "You already have an item in the auction slot!" and
            // the 5-second GUI watchdog auto-closed the window (setting state to Idle),
            // the command queue dispatches a new SellToAuction which would reset the
            // counter, creating an infinite "attempt 1/3" loop.  The counter is
            // instead reset when a listing actually succeeds ("BIN Auction started")
            // or when the auction_slot_blocked flag is cleared.
            // Open auction house — window handler takes over from here
            send_chat_command(bot, "/ah");
            *state.bot_state.write() = BotState::Selling;
        }
        CommandType::ClaimSoldItem => {
            *state.claiming_purchased.write() = false;
            let uuid = state.claim_sold_uuid_queue.write().pop_front()
                .or_else(|| state.claim_sold_uuid.write().take());
            if let Some(uuid) = uuid {
                info!("Claiming sold item via direct /viewauction {}", uuid);
                send_chat_command(bot, &format!("/viewauction {}", uuid));
            } else {
                info!("Claiming sold items via /ah");
                send_chat_command(bot, "/ah");
            }
            *state.bot_state.write() = BotState::ClaimingSold;
        }
        CommandType::ClaimPurchasedItem => {
            *state.claiming_purchased.write() = true;
            *state.claim_sold_uuid.write() = None;
            state.claim_sold_uuid_queue.write().clear();
            info!("Claiming purchased item via /ah");
            send_chat_command(bot, "/ah");
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
            send_chat_command(bot, "/sbmenu");
            *state.bot_state.write() = BotState::CheckingCookie;
        }
        CommandType::ManageOrders { cancel_open, target_item } => {
            let mode = if *cancel_open { "startup (cancel+collect)" } else { "collect-only" };
            info!("[ManageOrders] Triggered ({}) — opening /bz", mode);
            if let Some((ref name, is_buy)) = target_item {
                let side = if *is_buy { "BUY" } else { "SELL" };
                info!("[ManageOrders] Targeting specific order: {} ({})", name, side);
            }
            *state.manage_orders_cancelled.write() = 0;
            // NOTE: inventory_full is intentionally NOT cleared here.  Clearing
            // it caused a tight loop where ManageOrders would reset the flag,
            // try to collect a BUY order, fail ("no space"), re-set the flag,
            // and repeat every cycle.  The flag is now cleared only by:
            //   1. The periodic order-check timer (after a 90 s cooldown).
            //   2. InstaSell completion.
            // This lets ManageOrders skip BUY orders when the flag is set while
            // still collecting SELL orders (which yield coins, not items).
            state.manage_orders_cancel_open.store(*cancel_open, Ordering::Relaxed);
            *state.manage_orders_target_item.write() = target_item.clone();
            state.manage_orders_processed.write().clear();
            // Do NOT clear order_cancel_failures here — let failures accumulate
            // across ManageOrders cycles so MAX_CANCEL_RETRIES actually works.
            // The counter is only cleared on login/reconnect (Login event handler).
            // Set an internal deadline so the handler can bail out cleanly
            // (closing windows) instead of burning through the external timeout.
            // Only processes ONE order per cycle, so 10s is plenty.
            *state.manage_orders_deadline.write() = Some(
                tokio::time::Instant::now() + tokio::time::Duration::from_secs(10)
            );
            let initial_wid = *state.last_window_id.read();
            send_chat_command(bot, "/bz");
            *state.bot_state.write() = BotState::ManagingOrders;

            // Safety: if no window opens within 5 seconds (e.g. chat throttle,
            // server lag), retry /bz once.  If still no window after another 5 s,
            // give up and return to Idle so the queue isn't blocked for 60 s.
            {
                let retry_state = state.bot_state.clone();
                let retry_wid = state.last_window_id.clone();
                let retry_bot = bot.clone();
                let saved_wid = initial_wid;
                tokio::spawn(async move {
                    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                    if *retry_state.read() != BotState::ManagingOrders { return; }
                    if *retry_wid.read() != saved_wid { return; } // window opened, handler working
                    warn!("[ManageOrders] No window after 5 s — retrying /bz");
                    send_chat_command(&retry_bot, "/bz");

                    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                    if *retry_state.read() != BotState::ManagingOrders { return; }
                    if *retry_wid.read() != saved_wid { return; }
                    warn!("[ManageOrders] Still no window after retry — forcing Idle");
                    *retry_state.write() = BotState::Idle;
                });
            }
        }
        CommandType::DiscoverOrders | CommandType::ExecuteOrders => {
            info!("Command type not yet fully implemented in execute_command: {:?}", command.command_type);
        }
        CommandType::CancelAuction { item_name, starting_bid } => {
            info!("[CancelAuction] Cancelling auction: {} (bid: {})", item_name, starting_bid);
            *state.cancel_auction_item_name.write() = item_name.clone();
            *state.cancel_auction_starting_bid.write() = *starting_bid;
            send_chat_command(bot, "/ah");
            *state.bot_state.write() = BotState::CancellingAuction;
        }
        CommandType::SellInventoryBz => {
            info!("[SellInventoryBz] Opening /bz to sell inventory instantly");
            *state.bazaar_step.write() = BazaarStep::Initial;
            send_chat_command(bot, "/bz");
            *state.bot_state.write() = BotState::SellingInventoryBz;
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
    // Cache auction data whenever "My Auctions" / "Manage Auctions" opens,
    // regardless of bot state, so the web panel always has fresh data.
    if is_my_auctions_window_title(window_title) {
        // Give ContainerSetContent a moment to arrive
        tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
        let menu = bot.menu();
        let slots = menu.slots();
        build_cached_my_auctions_json(&slots, state);
    }

    let bot_state = *state.bot_state.read();
    
    match bot_state {
        BotState::Purchasing => {
            if window_title.contains("BIN Auction View") || window_title.contains("Auction View") {
                // Mark when the BIN Auction View opened (buy-speed start). Only
                // set if the fast-path observer didn't already record it.
                {
                    let mut w = state.bin_view_open_time.write();
                    if w.is_none() {
                        *w = Some(std::time::Instant::now());
                    }
                }

                // Log the time from /viewauction to the spawned task
                // actually starting.  The delta between this and the
                // OpenScreen handler log shows the tokio::spawn overhead
                // (typically <1 ms unless the runtime is saturated).
                if let Some(t0) = *state.purchase_start_time.read() {
                    info!(
                        "[Timing] /viewauction → interaction handler started: {:.1}ms",
                        t0.elapsed().as_secs_f64() * 1000.0
                    );
                }

                // ---- Wait for slot 31 before clicking (ANTI-CHEAT GATE) ----
                // Do NOT click slot 31 or send skip-click until we know what
                // item the server placed there.  Clicking on a non-interactive
                // item (e.g. feather = loading placeholder) is an "impossible
                // action" that can trigger Hypixel anti-cheat.  The buy-click
                // and skip-click are sent AFTER gold_nugget is confirmed.
                //
                // The PacketAcceleratorPlugin fires slot_data_notify earlier
                // (during ECS apply_deferred rather than via Event::Packet),
                // which makes this loop exit ~20-70ms sooner on busy servers.
                // This is safe because the gold_nugget check still runs before
                // ANY click is sent — the accelerator just detects the server's
                // ContainerSetContent packet faster, not before it arrives.
                //
                // Wait for ContainerSetContent / ContainerSetSlot to populate
                // slot 31.  Uses a Notify that fires instantly when the packet
                // arrives instead of polling every 10ms.
                let slot_31_kind = {
                    let poll_deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(500);
                    let mut kind;
                    loop {
                        // Register the Notify listener BEFORE reading slots.
                        // notify_waiters() drops the notification when no task
                        // is waiting, so if ContainerSetContent fires between
                        // the slot read and the select! below, the notification
                        // would be lost and we'd stall for the full 500ms
                        // deadline.  Registering first guarantees we capture it.
                        let notified = state.slot_data_notify.notified();

                        let menu = bot.menu();
                        let slots = menu.slots();
                        kind = slots.get(31).map(|s| {
                            if s.is_empty() { "air".to_string() }
                            else { s.kind().to_string().to_lowercase() }
                        }).unwrap_or_else(|| "air".to_string());
                        if kind != "air" || tokio::time::Instant::now() >= poll_deadline {
                            break;
                        }
                        let remaining = poll_deadline - tokio::time::Instant::now();
                        tokio::select! {
                            _ = notified => {}
                            _ = tokio::time::sleep(remaining) => {}
                        }
                    }
                    kind
                };

                // Log when slot 31 data is ready — the delta between this
                // and the interaction handler start shows how long we waited
                // for ContainerSetContent / ContainerSetSlot.  When the
                // server sends both OpenScreen and ContainerSetContent in the
                // same TCP segment this is ~0 ms; otherwise it is one extra
                // ECS cycle (~3.3 ms at 300 fps).
                if let Some(t0) = *state.purchase_start_time.read() {
                    info!(
                        "[Timing] /viewauction → slot 31 ready ({}): {:.1}ms",
                        slot_31_kind,
                        t0.elapsed().as_secs_f64() * 1000.0
                    );
                }

                if slot_31_kind.contains("bed") {
                    // Bed = auction is still in grace period.
                    // No buy-click or skip-click was sent (we waited for
                    // confirmation first).  The bed-spam loop below
                    // repeatedly clicks slot 31 until the grace period ends
                    // and the item becomes purchasable (gold_nugget appears).
                    // Signal the 5-second GUI watchdog to leave this window open.
                    state.bed_timing_active.store(true, Ordering::Relaxed);

                    // Bed-timing fires a tight 20ms burst (like TPM) starting
                    // bed_pre_click_ms (~30ms) before COFL's purchaseAt, so a click
                    // brackets the exact grace-period→buyable transition. Hypixel
                    // throttles window clicks to ~5 per 500ms, so the burst is capped
                    // to that budget (CLICK_BUDGET / CLICK_WINDOW) — 5 clicks across
                    // ~100ms land right on the deadline without tripping the throttle.
                    const BED_TIMING_CLICK_INTERVAL_MS: u64 = 20;
                    const BED_WAIT_POLL_MS: u64 = 20;
                    const MAX_FAILED_CLICKS: usize = 5;
                    const CLICK_BUDGET: usize = 5;
                    const CLICK_WINDOW: tokio::time::Duration = tokio::time::Duration::from_millis(500);
                    let click_interval_ms = if state.bedtiming {
                        BED_TIMING_CLICK_INTERVAL_MS
                    } else {
                        state.bed_spam_click_delay.max(1)
                    };

                    if state.bedtiming {
                        // Bedtiming mode: use COFL purchaseAt timing for bed auctions.
                        // Start clicking bed_pre_click_ms (default 30ms) before the deadline.
                        // If purchaseAt is not available, fall through to immediate bed spam.
                        let pre_click_lead_ms = state.bed_pre_click_ms;
                        // Convert the raw epoch-ms timestamp to a remaining-ms delta.
                        let remaining_ms_from_purchase_at = state.pending_purchase_at_ms.read()
                            .and_then(|purchase_at_ms| {
                                let now_ms = std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .ok()
                                    .map(|d| d.as_millis() as i64)?;
                                let diff = purchase_at_ms - now_ms;
                                if diff <= 0 { None } else { Some(diff as u64) }
                            });

                        if let Some(remaining_ms) = remaining_ms_from_purchase_at {
                            let wait_ms = remaining_ms.saturating_sub(pre_click_lead_ms);
                            if wait_ms > 0 {
                                info!("[AH] Bed timing: purchaseAt in {}ms — waiting {}ms, then clicking at {}ms intervals (lead: {}ms)",
                                    remaining_ms, wait_ms, click_interval_ms, pre_click_lead_ms);
                                let wait_deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(wait_ms);
                                loop {
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
                                    tokio::time::sleep(tokio::time::Duration::from_millis(BED_WAIT_POLL_MS)).await;
                                }
                                info!("[AH] Bed timing: entering rapid-click phase ({}ms interval)", click_interval_ms);
                            } else {
                                info!("[AH] Bed timing: purchaseAt imminent — starting clicks at {}ms interval", click_interval_ms);
                            }
                        } else {
                            info!("[AH] Bed timing: no purchaseAt — starting immediate bed spam at {}ms interval", click_interval_ms);
                        }
                    } else {
                        // Default mode: simple immediate bed spam at bed_spam_click_delay.
                        info!("[AH] Bed detected in slot 31 — starting bed spam at {}ms interval", click_interval_ms);
                    }

                    let bed_deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(70);
                    let mut failed_clicks: usize = 0;
                    // Poll the slot state frequently (a cheap local ECS read) so the
                    // bed → gold_nugget transition is caught within DETECT_POLL_MS,
                    // while bed pre-clicks stay rate-limited to click_interval_ms so we
                    // never exceed Hypixel's packet limit. Faster detection = the buy
                    // click lands sooner, without sending any extra packets.
                    const DETECT_POLL_MS: u64 = 5;
                    let mut last_bed_click: Option<tokio::time::Instant> = None;
                    // Timestamps of recent bed pre-clicks, used to enforce the
                    // ~5-clicks-per-500ms throttle budget.
                    let mut recent_clicks: std::collections::VecDeque<tokio::time::Instant> =
                        std::collections::VecDeque::with_capacity(CLICK_BUDGET);
                    loop {
                        if tokio::time::Instant::now() >= bed_deadline {
                            warn!("[AH] Bed timing: grace period did not end — giving up");
                            state.bed_timing_active.store(false, Ordering::Relaxed);
                            send_raw_close(bot, window_id, &state.handlers);
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
                            info!("[AH] Bed timing: window closed");
                            state.bed_timing_active.store(false, Ordering::Relaxed);
                            send_raw_close(bot, window_id, &state.handlers);
                            *state.bot_state.write() = BotState::Idle;
                            return;
                        } else if current_kind.contains("gold_nugget") {
                            info!("[AH] Bed timing: gold_nugget appeared, clicking slot 31");
                            state.bed_timing_active.store(false, Ordering::Relaxed);
                            if *state.last_window_id.read() == window_id {
                                send_raw_click(bot, window_id, 31);
                            }
                            break;
                        } else if current_kind.contains("potato") {
                            warn!("[AH] Bed timing: potato detected — auction not purchasable, closing");
                            let _ = state.event_tx.send(BotEvent::ChatMessage(
                                "\u{00A7}f[\u{00A7}4BAF\u{00A7}f]: \u{00A7}cAuction already bought by someone else".to_string()
                            ));
                            state.bed_timing_active.store(false, Ordering::Relaxed);
                            send_raw_close(bot, window_id, &state.handlers);
                            *state.bot_state.write() = BotState::Idle;
                            return;
                        } else if current_kind.contains("bed") {
                            if state.bedtiming {
                                let now = tokio::time::Instant::now();
                                // Drop click timestamps older than the throttle window.
                                while recent_clicks
                                    .front()
                                    .is_some_and(|t| now.duration_since(*t) >= CLICK_WINDOW)
                                {
                                    recent_clicks.pop_front();
                                }
                                // Fire at the 20ms burst cadence, but only while we still
                                // have throttle budget left in the current 500ms window.
                                let cadence_ok = last_bed_click
                                    .map_or(true, |t| now.duration_since(t) >= tokio::time::Duration::from_millis(click_interval_ms));
                                let budget_ok = recent_clicks.len() < CLICK_BUDGET;
                                if cadence_ok && budget_ok {
                                    debug!("[AH] Bed timing: grace period active, pre-clicking slot 31");
                                    if *state.last_window_id.read() == window_id {
                                        send_raw_click(bot, window_id, 31);
                                    }
                                    last_bed_click = Some(now);
                                    recent_clicks.push_back(now);
                                }
                            } else {
                                debug!("[AH] Bed timing: grace period active, waiting for gold_nugget");
                            }
                        } else {
                            failed_clicks += 1;
                            debug!("[AH] Bed timing: slot 31 = {} (failed {}/{})", current_kind, failed_clicks, MAX_FAILED_CLICKS);
                            if failed_clicks >= MAX_FAILED_CLICKS {
                                warn!("[AH] Bed timing: stopped after {} unexpected slot states", failed_clicks);
                                state.bed_timing_active.store(false, Ordering::Relaxed);
                                send_raw_close(bot, window_id, &state.handlers);
                                *state.bot_state.write() = BotState::Idle;
                                return;
                            }
                        }
                        tokio::time::sleep(tokio::time::Duration::from_millis(DETECT_POLL_MS)).await;
                    }
                } else if slot_31_kind.contains("gold_nugget") {
                    // ---- Buyable auction (SAFE: gold_nugget confirmed) ----
                    // Check if the fast-path purchase task (spawned in
                    // execute_command, triggered by ECS observer) already sent
                    // the buy-click and skip-click.  If so, skip to avoid
                    // duplicate clicks.
                    if state.fast_buy_clicked.load(Ordering::Relaxed) {
                        info!("[AH] Fast-path already sent buy click — skipping Event::Packet click");
                    } else {
                        // Fallback: fast path did not fire (e.g. window was not
                        // BIN Auction View, or the observer didn't trigger).
                        // slot 31 is confirmed gold_nugget — the real buy button
                        // is there — so click it immediately.
                        if let Some(t0) = *state.purchase_start_time.read() {
                            info!(
                                "[Timing] /viewauction → buy click sent (fallback): {:.1}ms",
                                t0.elapsed().as_secs_f64() * 1000.0
                            );
                        }
                        // Buy click on the confirmed window/slot 31.
                        send_raw_click(bot, window_id, 31);

                        if state.skip {
                            // Skip pre-click on the Confirm Purchase window in the
                            // same TCP burst — the buy click opens it as id
                            // window_id+1, so it is a window we KNOW will be there.
                            // No second slot-31 click: after the buy click that
                            // window is gone, so re-clicking it would be an
                            // impossible action.
                            let next_wid = if window_id == 255 { 1u8 } else { window_id + 1 };
                            info!("[AH] Skip: pre-clicking confirm slot 11 on window {} (same burst, fallback)", next_wid);
                            state.skip_click_sent.store(true, Ordering::Relaxed);
                            send_raw_click(bot, next_wid, 11);
                        } else {
                            info!("[AH] gold_nugget confirmed — buy click sent (fallback)");
                        }
                    }
                } else if !slot_31_kind.contains("air") {
                    // ---- Non-buyable auction ----
                    // Slot 31 is a non-purchasable item placed by the server:
                    // feather = auction not available / cannot be purchased,
                    // potato = already bought by someone else,
                    // poisonous_potato = can't afford,
                    // stained_glass_pane = edge case.
                    // No clicks were sent (we waited for confirmation first),
                    // so just close the window cleanly.
                    // Use write lock for atomic check-and-set to prevent a
                    // double-close race with the terminal-failure chat handler.
                    let should_close = {
                        let mut bs = state.bot_state.write();
                        if *bs == BotState::Purchasing {
                            *bs = BotState::Idle;
                            true
                        } else {
                            false
                        }
                    };
                    if should_close {
                        let msg = if slot_31_kind.contains("poisonous_potato") {
                            "\u{00A7}f[\u{00A7}4BAF\u{00A7}f]: \u{00A7}cCan't afford auction"
                        } else if slot_31_kind.contains("potato") {
                            "\u{00A7}f[\u{00A7}4BAF\u{00A7}f]: \u{00A7}cAuction already bought by someone else"
                        } else if slot_31_kind.contains("feather") {
                            "\u{00A7}f[\u{00A7}4BAF\u{00A7}f]: \u{00A7}cAuction no longer available"
                        } else {
                            "\u{00A7}f[\u{00A7}4BAF\u{00A7}f]: \u{00A7}cAuction not purchasable"
                        };
                        warn!("[AH] Slot 31 = {} — auction not purchasable, closing window", slot_31_kind);
                        let _ = state.event_tx.send(BotEvent::ChatMessage(msg.to_string()));
                        *state.purchase_start_time.write() = None;
                        *state.pending_purchase_at_ms.write() = None;
                        send_raw_close(bot, window_id, &state.handlers);
                    }
                }
                // air = slot 31 never populated within the poll deadline.
                // The terminal-failure chat handler or 5s GUI watchdog will
                // clean up.
            } else if window_title.contains("Confirm Purchase") {
                // Log time from /viewauction to Confirm Purchase window.
                // This is the buy-click → server-processes → OpenScreen RTT.
                if let Some(t0) = *state.purchase_start_time.read() {
                    info!(
                        "[Timing] /viewauction → Confirm Purchase opened: {:.1}ms",
                        t0.elapsed().as_secs_f64() * 1000.0
                    );
                }
                // If a skip pre-click was already sent for this window, the
                // confirm click is already queued on the server for this same
                // window — don't fire a redundant reactive click.
                let skip_was_sent = state.skip_click_sent.swap(false, Ordering::Relaxed);
                if skip_was_sent {
                    info!("[AH] Skip pre-click already sent for this confirm window — not re-clicking");
                } else {
                    // No skip pre-click — this Confirm Purchase window is open
                    // right now (the server just sent it), so clicking slot 11 is
                    // clicking something that is actually there.  Click it
                    // immediately via the raw connection.
                    send_raw_click(bot, window_id, 11);
                }

                tokio::time::sleep(tokio::time::Duration::from_millis(CONFIRM_PURCHASE_RETRY_MS)).await;

                // Safety retry loop: if the window is still open (pre-click failed,
                // click was lost, or the server needs more time), keep retrying.
                while state.handlers.current_window_title()
                    .as_deref()
                    .map(|t| t.contains("Confirm Purchase"))
                    .unwrap_or(false)
                {
                    send_raw_click(bot, window_id, 11);
                    tokio::time::sleep(tokio::time::Duration::from_millis(CONFIRM_PURCHASE_RETRY_MS)).await;
                }

                send_raw_close(bot, window_id, &state.handlers);
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
                    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                };

                if let Some(i) = order_button_slot {
                    // Final guard before clicking — reject if a newer window has taken over.
                    if *state.last_window_id.read() != window_id {
                        debug!("[Bazaar] Window {} superseded before order-button click, aborting", window_id);
                        return;
                    }
                    info!("[Bazaar] Item detail: clicking \"{}\" at slot {}", order_btn_name, i);
                    *state.bazaar_step.write() = BazaarStep::SelectOrderType;
                    // Add randomized human-like delay before clicking (200-500ms)
                    let jitter = 200 + (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().subsec_nanos() % 300) as u64;
                    tokio::time::sleep(tokio::time::Duration::from_millis(jitter)).await;
                    if *state.last_window_id.read() != window_id { return; }
                    click_window_slot(bot, &state.last_window_id, window_id, i as i16).await;
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
                    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                };

                match found {
                    Some(i) => {
                        if *state.last_window_id.read() != window_id { return; }
                        info!("[Bazaar] Found item at slot {}", i);
                        // Add randomized human-like delay (200-450ms)
                        let jitter = 200 + (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().subsec_nanos() % 250) as u64;
                        tokio::time::sleep(tokio::time::Duration::from_millis(jitter)).await;
                        if *state.last_window_id.read() != window_id { return; }
                        click_window_slot(bot, &state.last_window_id, window_id, i as i16).await;
                    }
                    None => {
                        warn!("[Bazaar] Item \"{}\" not found in search results; going idle", item_name);
                        send_raw_close(bot, window_id, &state.handlers);
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
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
            };

            // Step 3: Amount screen (buy orders only)
            if let (Some(i), true) = (amount_slot,
                is_buy_order && current_step == BazaarStep::SelectOrderType)
            {
                if *state.last_window_id.read() != window_id { return; }
                info!("[Bazaar] Amount screen: clicking Custom Amount at slot {}", i);
                *state.bazaar_step.write() = BazaarStep::SetAmount;
                click_window_slot(bot, &state.last_window_id, window_id, i as i16).await;
                // Sign response is sent in the OpenSignEditor packet handler
            }
            // Step 4: Price screen
            else if let (Some(i), true) = (price_slot,
                current_step == BazaarStep::SelectOrderType || current_step == BazaarStep::SetAmount)
            {
                if *state.last_window_id.read() != window_id { return; }
                info!("[Bazaar] Price screen: clicking Custom Price at slot {}", i);
                *state.bazaar_step.write() = BazaarStep::SetPrice;
                click_window_slot(bot, &state.last_window_id, window_id, i as i16).await;
                // Sign response is sent in the OpenSignEditor packet handler
            }
            // Step 5: Confirm screen — anything that opens after SetPrice
            else if current_step == BazaarStep::SetPrice {
                if *state.last_window_id.read() != window_id { return; }
                info!("[Bazaar] Confirm screen: clicking slot 13");
                *state.bazaar_step.write() = BazaarStep::Confirm;
                // Clear rejection flag before clicking so we only capture the
                // response to *this* placement attempt.
                state.bazaar_order_rejected.store(false, Ordering::Relaxed);
                // Add randomized human-like delay before confirming (300-700ms)
                let jitter = 300 + (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().subsec_nanos() % 400) as u64;
                tokio::time::sleep(tokio::time::Duration::from_millis(jitter)).await;
                click_window_slot(bot, &state.last_window_id, window_id, 13).await;

                // Wait briefly for the server to respond (limit/rejection message arrives asynchronously)
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

                if state.bazaar_at_limit.load(Ordering::Relaxed) {
                    warn!("[Bazaar] Order rejected (at limit) — not emitting BazaarOrderPlaced");
                } else if state.bazaar_order_rejected.load(Ordering::Relaxed) {
                    warn!("[Bazaar] Order rejected (price not competitive) — not emitting BazaarOrderPlaced");
                } else {
                    let item = item_name.clone();
                    let amount = *state.bazaar_amount.read();
                    let price_per_unit = *state.bazaar_price_per_unit.read();
                    let total_value = amount as f64 * price_per_unit;
                    log_bazaar_order_placed(is_buy_order, &item, total_value);
                    let _ = state.event_tx.send(BotEvent::BazaarOrderPlaced {
                        item_name: item,
                        amount,
                        price_per_unit,
                        is_buy_order,
                    });
                    info!("[Bazaar] ===== ORDER COMPLETE =====");
                }
                send_raw_close(bot, window_id, &state.handlers);
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
                    warn!("[InstaSell] No item name stored, closing window and going idle");
                    send_raw_close(bot, window_id, &state.handlers);
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
                    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                };
                match item_slot {
                    Some(i) => {
                        if *state.last_window_id.read() != window_id { return; }
                        info!("[InstaSell] Found \"{}\" at slot {}, clicking", item_name, i);
                        *state.bazaar_step.write() = BazaarStep::SearchResults;
                        click_window_slot(bot, &state.last_window_id, window_id, i as i16).await;
                    }
                    None => {
                        warn!("[InstaSell] Item \"{}\" not found in bazaar search, closing window and going idle", item_name);
                        send_raw_close(bot, window_id, &state.handlers);
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
                    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                };
                match sell_slot {
                    Some(i) => {
                        if *state.last_window_id.read() != window_id { return; }
                        info!("[InstaSell] Clicking \"Sell Instantly\" at slot {}", i);
                        *state.bazaar_step.write() = BazaarStep::SelectOrderType;
                        click_window_slot(bot, &state.last_window_id, window_id, i as i16).await;
                    }
                    None => {
                        warn!("[InstaSell] \"Sell Instantly\" not found, closing window and going idle");
                        send_raw_close(bot, window_id, &state.handlers);
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
                        click_window_slot(bot, &state.last_window_id, window_id, i as i16).await;
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
                send_chat_command(bot, "/bz");
            }
        }
        BotState::ClaimingPurchased => {
            if window_title.contains("Auction House") {
                // Hardcoded slot 13 for "Your Bids" navigation — matches TypeScript clickWindow(bot, 13)
                tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
                info!("[ClaimPurchased] Auction House opened - clicking slot 13 (Your Bids)");
                click_window_slot(bot, &state.last_window_id, window_id, 13).await;
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
                    click_window_slot(bot, &state.last_window_id, window_id, i as i16).await;
                    send_raw_close(bot, window_id, &state.handlers);
                    *state.bot_state.write() = BotState::Idle;
                    found = true;
                }
                if !found {
                    // Look for a claimable purchased item using the same
                    // indicators as ClaimingSold (sold!, ended, expired,
                    // click to claim, claim your) — not just "Status: Sold".
                    // Only scan window slots, not player inventory.
                    let window_slot_count = window_content_slot_count(slots.len());
                    for (i, item) in slots.iter().enumerate().take(window_slot_count) {
                        if is_claimable_auction_slot(item) {
                            info!("[ClaimPurchased] Found claimable item at slot {}", i);
                            click_window_slot(bot, &state.last_window_id, window_id, i as i16).await;
                            // Stay in ClaimingPurchased — next window should be BIN Auction View
                            found = true;
                            break;
                        }
                    }
                    if !found {
                        info!("[ClaimPurchased] Nothing to claim, closing window and going idle");
                        send_raw_close(bot, window_id, &state.handlers);
                        *state.bot_state.write() = BotState::Idle;
                    }
                }
            } else if window_title.contains("BIN Auction View") || window_title.contains("Auction View") {
                info!("[ClaimPurchased] Auction View opened - clicking slot 31 to collect");
                tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                click_window_slot(bot, &state.last_window_id, window_id, 31).await;
                send_raw_close(bot, window_id, &state.handlers);
                *state.bot_state.write() = BotState::Idle;
            } else {
                // Unrecognized window while claiming purchased — close and go idle.
                warn!("[ClaimPurchased] Unexpected window '{}' — closing and going idle", window_title);
                send_raw_close(bot, window_id, &state.handlers);
                *state.bot_state.write() = BotState::Idle;
            }
        }
        BotState::ClaimingSold => {
            if window_title.contains("Auction House") {
                info!("[ClaimSold] Auction House opened - navigating to Manage Auctions (slot 15)");
                // Wait for ContainerSetContent to arrive and populate slots
                tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
                let menu = bot.menu();
                let slots = menu.slots();
                // Prefer name-based match so a Hypixel UI shift is handled automatically;
                // fall back to the well-known slot 15 (same fixed slot the Selling flow uses).
                if let Some(i) = find_slot_by_name(&slots, "Manage Auctions")
                    .or_else(|| find_slot_by_name(&slots, "My Auctions"))
                {
                    info!("[ClaimSold] Clicking Manage/My Auctions at slot {}", i);
                    click_window_slot(bot, &state.last_window_id, window_id, i as i16).await;
                } else {
                    info!("[ClaimSold] Manage/My Auctions not found by name, clicking slot 15");
                    click_window_slot(bot, &state.last_window_id, window_id, 15).await;
                }
            } else if is_my_auctions_window_title(window_title) {
                info!("[ClaimSold] My/Manage Auctions opened - looking for claimable items");
                // Wait for ContainerSetContent to arrive and populate slots
                tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
                let menu = bot.menu();
                let slots = menu.slots();
                // Only scan the window slots (typically 0..27 for a 3-row GUI),
                // not the player inventory at the bottom, to avoid false matches.
                let window_slot_count = window_content_slot_count(slots.len());
                // Look for Claim All first
                if let Some(i) = find_slot_by_name(&slots, "Claim All") {
                    info!("[ClaimSold] Clicking Claim All at slot {}", i);
                    click_window_slot(bot, &state.last_window_id, window_id, i as i16).await;
                    // Claim All finishes everything — close window and go idle
                    send_raw_close(bot, window_id, &state.handlers);
                    state.auction_slot_blocked.store(false, Ordering::Relaxed);
                    *state.bot_state.write() = BotState::Idle;
                } else {
                    // Look for first claimable item (only window slots)
                    let mut found = false;
                    for (i, item) in slots.iter().enumerate().take(window_slot_count) {
                        if is_claimable_auction_slot(item) {
                            info!("[ClaimSold] Clicking claimable item at slot {}", i);
                            click_window_slot(bot, &state.last_window_id, window_id, i as i16).await;
                            // Stay in ClaimingSold — Hypixel re-opens Manage Auctions after the detail
                            found = true;
                            break;
                        }
                    }
                    if !found {
                        info!("[ClaimSold] Nothing to claim, closing window and going idle");
                        send_raw_close(bot, window_id, &state.handlers);
                        state.auction_slot_blocked.store(false, Ordering::Relaxed);
                        *state.bot_state.write() = BotState::Idle;
                    }
                }
            } else if window_title.contains("BIN Auction View") || window_title.contains("Auction View") {
                info!("[ClaimSold] Auction detail opened - looking for Claim button");
                tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                let menu = bot.menu();
                let slots = menu.slots();
                // Prefer fixed slot 31 in auction detail; use name matching only as fallback.
                let slot_31_name = slots.get(31).and_then(get_item_display_name_from_slot).unwrap_or_default();
                let slot_31_lower = remove_mc_colors(&slot_31_name).to_lowercase();
                if slot_31_lower.contains("claim") {
                    info!("[ClaimSold] Clicking preferred Claim slot 31");
                    click_window_slot(bot, &state.last_window_id, window_id, 31).await;
                } else if let Some(i) = find_slot_by_name(&slots, "Claim") {
                    info!("[ClaimSold] Slot 31 not claimable, falling back to Claim name match at slot {}", i);
                    click_window_slot(bot, &state.last_window_id, window_id, i as i16).await;
                } else {
                    info!("[ClaimSold] Claim button not found, clicking slot 31 fallback");
                    click_window_slot(bot, &state.last_window_id, window_id, 31).await;
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
            } else {
                // Unrecognized window while claiming — close and go idle to avoid
                // getting stuck in an unexpected GUI.
                warn!("[ClaimSold] Unexpected window '{}' — closing and going idle", window_title);
                send_raw_close(bot, window_id, &state.handlers);
                state.auction_slot_blocked.store(false, Ordering::Relaxed);
                *state.bot_state.write() = BotState::Idle;
            }
        }
        BotState::CancellingAuction => {
            // Cancel auction flow: /ah → Manage Auctions → find auction → Cancel Auction → Confirm
            if window_title.contains("Auction House") {
                info!("[CancelAuction] Auction House opened - navigating to Manage Auctions");
                tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
                let menu = bot.menu();
                let slots = menu.slots();
                if let Some(i) = find_slot_by_name(&slots, "Manage Auctions")
                    .or_else(|| find_slot_by_name(&slots, "My Auctions"))
                {
                    info!("[CancelAuction] Clicking Manage/My Auctions at slot {}", i);
                    click_window_slot(bot, &state.last_window_id, window_id, i as i16).await;
                } else {
                    info!("[CancelAuction] Manage/My Auctions not found by name, clicking slot 15");
                    click_window_slot(bot, &state.last_window_id, window_id, 15).await;
                }
            } else if is_my_auctions_window_title(window_title) {
                info!("[CancelAuction] Manage Auctions opened - searching for target auction");
                tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
                let menu = bot.menu();
                let slots = menu.slots();
                let target_name = state.cancel_auction_item_name.read().clone();
                let target_bid = *state.cancel_auction_starting_bid.read();
                let target_lower = target_name.to_lowercase();
                // Find the auction slot matching item_name + starting_bid
                let mut found = false;
                for (i, item) in slots.iter().enumerate() {
                    if item.is_empty() { continue; }
                    let display_name = match get_item_display_name_from_slot(item) {
                        Some(n) => n,
                        None => continue,
                    };
                    let clean = remove_mc_colors(&display_name).to_lowercase();
                    if !clean.contains(&target_lower) { continue; }
                    // Verify price matches for accurate identification
                    let lore = get_item_lore_from_slot(item);
                    let price = extract_price_from_lore(&lore);
                    if let Some(p) = price {
                        if p != target_bid { continue; }
                    }
                    // Verify this is an active auction (not sold/expired)
                    let combined_lower = lore.join("\n").to_lowercase();
                    if combined_lower.contains("sold!")
                        || combined_lower.contains("expired")
                        || contains_word_ended(&combined_lower)
                    {
                        continue;
                    }
                    info!("[CancelAuction] Found matching auction '{}' at slot {} (price: {:?})", display_name, i, price);
                    click_window_slot(bot, &state.last_window_id, window_id, i as i16).await;
                    found = true;
                    break;
                }
                if !found {
                    info!("[CancelAuction] Target auction not found in Manage Auctions, closing window and going idle");
                    send_raw_close(bot, window_id, &state.handlers);
                    *state.bot_state.write() = BotState::Idle;
                }
            } else if window_title.contains("BIN Auction View") || window_title.contains("Auction View") {
                info!("[CancelAuction] Auction detail opened - looking for Cancel Auction button");
                tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                let menu = bot.menu();
                let slots = menu.slots();
                if let Some(i) = find_slot_by_name(&slots, "Cancel Auction") {
                    info!("[CancelAuction] Clicking Cancel Auction at slot {}", i);
                    click_window_slot(bot, &state.last_window_id, window_id, i as i16).await;
                } else {
                    info!("[CancelAuction] Cancel Auction button not found, closing window and going idle");
                    send_raw_close(bot, window_id, &state.handlers);
                    *state.bot_state.write() = BotState::Idle;
                }
                // Watchdog: go idle if no follow-up window in 2s
                let cancel_state_ref = state.bot_state.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(tokio::time::Duration::from_millis(2000)).await;
                    if *cancel_state_ref.read() == BotState::CancellingAuction {
                        info!("[CancelAuction] No follow-up window after 2s, going idle");
                        *cancel_state_ref.write() = BotState::Idle;
                    }
                });
            } else if window_title.contains("Confirm") {
                info!("[CancelAuction] Confirm window opened - confirming cancellation");
                tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                let menu = bot.menu();
                let slots = menu.slots();
                // Click Confirm button — typically slot 11 or find by name
                if let Some(i) = find_slot_by_name(&slots, "Confirm") {
                    info!("[CancelAuction] Clicking Confirm at slot {}", i);
                    click_window_slot(bot, &state.last_window_id, window_id, i as i16).await;
                } else {
                    info!("[CancelAuction] Confirm not found by name, clicking slot 11");
                    click_window_slot(bot, &state.last_window_id, window_id, 11).await;
                }
                info!("[CancelAuction] Auction cancellation confirmed, closing window and going idle");
                send_raw_close(bot, window_id, &state.handlers);
                let cancelled_name = state.cancel_auction_item_name.read().clone();
                let cancelled_bid = *state.cancel_auction_starting_bid.read();
                let _ = state.event_tx.send(BotEvent::AuctionCancelled {
                    item_name: cancelled_name,
                    starting_bid: cancelled_bid as u64,
                });
                *state.bot_state.write() = BotState::Idle;
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

            // If the sell was aborted (e.g. stuck item in auction slot), do not
            // proceed — close the window and bail.  The retry task spawned by the
            // chat handler will re-open /ah once the window is closed.
            if state.auction_sell_aborted.load(Ordering::Relaxed) {
                warn!("[Auction] Window opened but auction sell aborted — closing window {}", window_id);
                send_raw_close(bot, window_id, &state.handlers);
                return;
            }

            match step {
                AuctionStep::Initial => {
                    // "Auction House" opened — click slot 15 (nav to Manage Auctions)
                    if window_title.contains("Auction House") {
                        info!("[Auction] AH opened, clicking slot 15 (Manage Auctions nav)");
                        *state.auction_step.write() = AuctionStep::OpenManage;
                        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                        click_window_slot(bot, &state.last_window_id, window_id, 15).await;
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
                                state.auction_at_limit.store(true, Ordering::Relaxed);
                                send_raw_close(bot, window_id, &state.handlers);
                                *state.bot_state.write() = BotState::Idle;
                                return;
                            }
                            info!("[Auction] Clicking Create Auction at slot {}", i);
                            *state.auction_step.write() = AuctionStep::ClickCreate;
                            tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                            click_window_slot(bot, &state.last_window_id, window_id, i as i16).await;
                        } else {
                            warn!("[Auction] Create Auction not found in Manage Auctions, going idle");
                            send_raw_close(bot, window_id, &state.handlers);
                            *state.bot_state.write() = BotState::Idle;
                        }
                    } else if window_title.contains("Create Auction") && !window_title.contains("BIN") {
                        // Co-op AH or similar: jumped directly to "Create Auction" — click slot 48 (BIN)
                        info!("[Auction] Skipped Manage Auctions, in Create Auction — clicking slot 48 (BIN)");
                        *state.auction_step.write() = AuctionStep::SelectBIN;
                        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                        click_window_slot(bot, &state.last_window_id, window_id, 48).await;
                    } else if window_title.contains("Create BIN Auction") {
                        // Co-op AH opened "Create BIN Auction" directly (skipping Manage Auctions).
                        // Run the SelectBIN logic inline.
                        info!("[Auction] Co-op AH: jumped straight to Create BIN Auction, handling as SelectBIN");
                        clear_auction_preview_slot(bot, state, window_id, &slots).await;
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
                        // Safety: only ever click a real, non-empty PLAYER-inventory slot.
                        // A name-search fallback can otherwise resolve to a GUI/control slot
                        // or an emptied slot; clicking that to "sell" a non-item is an
                        // impossible action Hypixel Watchdog flags. Invalid → fall through to
                        // the abort branch below instead of clicking garbage.
                        let target_slot = target_slot
                            .filter(|&i| i >= player_start && i < slots.len() && !slots[i].is_empty());
                        if let Some(i) = target_slot {
                            info!("[Auction] Co-op AH: clicking item at slot {}", i);
                            let item_to_carry = slots[i].clone();
                            tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                            click_window_slot(bot, &state.last_window_id, window_id, i as i16).await;
                            tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
                            info!("[Auction] Co-op AH: clicking slot 31 (price setter)");
                            *state.auction_step.write() = AuctionStep::PriceSign;
                            click_window_slot_carrying(bot, &state.last_window_id, window_id, 31, &item_to_carry).await;
                        } else {
                            warn!("[Auction] Co-op AH: item \"{}\" not found, going idle", item_name);
                            send_raw_close(bot, window_id, &state.handlers);
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
                        click_window_slot(bot, &state.last_window_id, window_id, 48).await;
                    } else if window_title.contains("Create BIN Auction") {
                        // Hypixel sometimes opens "Create BIN Auction" directly after clicking
                        // "Create Auction" in Manage Auctions (skipping the type-select step).
                        // Run SelectBIN logic inline so the flow continues without getting stuck.
                        info!("[Auction] ClickCreate: jumped straight to Create BIN Auction, handling as SelectBIN");
                        // Clear a stuck item in the auction preview slot (same guard as SelectBIN).
                        clear_auction_preview_slot(bot, state, window_id, &slots).await;
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
                        // Safety: only ever click a real, non-empty PLAYER-inventory slot —
                        // never a GUI/display slot or an empty slot (selling a non-item is an
                        // impossible action Watchdog flags). But if the resolved slot is
                        // rejected (e.g. a whole-window name search matched a GUI display
                        // slot), fall back to searching ONLY the player inventory so a
                        // legitimately-held item is still listed instead of aborting the sale.
                        let target_slot = target_slot
                            .filter(|&i| i >= player_start && i < slots.len() && !slots[i].is_empty())
                            .or_else(|| find_inventory_slot_by_name(&slots, player_start, &item_name));
                        if let Some(i) = target_slot {
                            info!("[Auction] ClickCreate→SelectBIN: clicking item at slot {}", i);
                            let item_to_carry = slots[i].clone();
                            tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                            click_window_slot(bot, &state.last_window_id, window_id, i as i16).await;
                            tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
                            info!("[Auction] ClickCreate→SelectBIN: clicking slot 31 (price setter)");
                            *state.auction_step.write() = AuctionStep::PriceSign;
                            click_window_slot_carrying(bot, &state.last_window_id, window_id, 31, &item_to_carry).await;
                        } else {
                            warn!("[Auction] ClickCreate→SelectBIN: item \"{}\" not found, going idle", item_name);
                            send_raw_close(bot, window_id, &state.handlers);
                            *state.bot_state.write() = BotState::Idle;
                        }
                    }
                }
                AuctionStep::SelectBIN => {
                    // "Create BIN Auction" opened first time (setPrice=false in TS)
                    // Find item by slot or by name, click it, then click slot 31 for price sign
                    if window_title.contains("Create BIN Auction") {
                        // Proactive fix: if the auction item preview slot (slot 13) already
                        // has an item (e.g. from a previously failed auction creation or a
                        // purchased item auto-placed by the server), click it first to clear
                        // the slot so our item can be placed without triggering "You already
                        // have an item in the auction slot!".
                        clear_auction_preview_slot(bot, state, window_id, &slots).await;

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

                        // Safety: only ever click a real, non-empty PLAYER-inventory slot —
                        // never a GUI/display slot or an empty slot (selling a non-item is an
                        // impossible action Watchdog flags). But if the resolved slot is
                        // rejected (e.g. a whole-window name search matched a GUI display
                        // slot), fall back to searching ONLY the player inventory so a
                        // legitimately-held item is still listed instead of aborting the sale.
                        let target_slot = target_slot
                            .filter(|&i| i >= player_start && i < slots.len() && !slots[i].is_empty())
                            .or_else(|| find_inventory_slot_by_name(&slots, player_start, &item_name));
                        if let Some(i) = target_slot {
                            info!("[Auction] Clicking item at slot {}", i);
                            let item_to_carry = slots[i].clone();
                            tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                            click_window_slot(bot, &state.last_window_id, window_id, i as i16).await;
                            // Click slot 31 (price setter) — sign will open, handled in OpenSignEditor
                            tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
                            info!("[Auction] Clicking slot 31 (price setter)");
                            *state.auction_step.write() = AuctionStep::PriceSign;
                            click_window_slot_carrying(bot, &state.last_window_id, window_id, 31, &item_to_carry).await;
                        } else {
                            warn!("[Auction] Item \"{}\" not found in Create BIN Auction window, going idle", item_name);
                            send_raw_close(bot, window_id, &state.handlers);
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
                        click_window_slot(bot, &state.last_window_id, window_id, 33).await;
                    }
                }
                AuctionStep::DurationSign => {
                    // "Auction Duration" window opened — click slot 16 to open sign for duration
                    if window_title.contains("Auction Duration") {
                        info!("[Auction] Auction Duration window opened, clicking slot 16 (sign trigger)");
                        // Sign handler (OpenSignEditor) will fire and advance step to ConfirmSell
                        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                        click_window_slot(bot, &state.last_window_id, window_id, 16).await;
                    }
                }
                AuctionStep::ConfirmSell => {
                    // "Create BIN Auction" opened third time (setPrice=true, durationSet=true in TS)
                    // Click slot 29 to proceed to "Confirm BIN Auction"
                    if window_title.contains("Create BIN Auction") {
                        // Check if the sell was aborted (e.g. wrong item in auction slot)
                        if state.auction_sell_aborted.load(Ordering::Relaxed) {
                            warn!("[Auction] ConfirmSell aborted — wrong item detected, closing window");
                            send_raw_close(bot, window_id, &state.handlers);
                            // Stay in Selling state so the retry task can re-open /ah
                            return;
                        }
                        info!("[Auction] Both price and duration set, clicking slot 29 (confirm item)");
                        *state.auction_step.write() = AuctionStep::FinalConfirm;
                        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                        click_window_slot(bot, &state.last_window_id, window_id, 29).await;
                    }
                }
                AuctionStep::FinalConfirm => {
                    // "Confirm BIN Auction" window — click slot 11 to finalize.
                    // AuctionListed event is emitted from the chat handler when Hypixel sends
                    // "BIN Auction started for ..." (matches TypeScript sellHandler.ts).
                    if window_title.contains("Confirm BIN Auction") || window_title.contains("Confirm") {
                        // Check if the sell was aborted (e.g. wrong item in auction slot)
                        if state.auction_sell_aborted.load(Ordering::Relaxed) {
                            warn!("[Auction] FinalConfirm aborted — wrong item detected, closing window");
                            send_raw_close(bot, window_id, &state.handlers);
                            // Stay in Selling state so the retry task can re-open /ah
                            return;
                        }
                        info!("[Auction] Confirm BIN Auction window, clicking slot 11 (final confirm)");
                        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                        click_window_slot(bot, &state.last_window_id, window_id, 11).await;
                        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                        info!("[Auction] ===== AUCTION CREATED =====");
                        send_raw_close(bot, window_id, &state.handlers);
                        *state.bot_state.write() = BotState::Idle;
                    }
                }
                // PriceSign step: no window interaction needed; sign handler does the work
                AuctionStep::PriceSign => {}
            }
        }
        BotState::ManagingOrders => {
            // Bazaar order management. In startup mode (cancel_open=true) all existing orders
            // are cancelled after collecting filled ones. In collect-only mode (cancel_open=false,
            // used when triggered by BazaarOrderFilled or periodic checks) only filled orders
            // are collected and open orders are left untouched.
            // Flow: Bazaar window → find & click "Manage Orders" → iterate orders → handle each.

            // Check the internal deadline at every window event.  If exceeded, close
            // this window and return to Idle so the command queue isn't blocked.
            if check_manage_orders_deadline(bot, state, window_id) {
                return;
            }

            let cancel_open = state.manage_orders_cancel_open.load(Ordering::Relaxed);
            let target_item = state.manage_orders_target_item.read().clone();
            if window_title.contains("Bazaar") && !is_bazaar_orders_window_title(window_title) {
                // Bazaar page (main or category) — find "Manage Orders" button
                // dynamically.  Hypixel may rearrange slots across updates, so we
                // search by name instead of relying on a hardcoded slot index.
                tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
                if *state.last_window_id.read() != window_id { return; }
                let slots = bot.menu().slots();
                let manage_slot = find_slot_by_name(&slots, "Manage Orders").unwrap_or(MANAGE_ORDERS_FALLBACK_SLOT);
                info!("[ManageOrders] Bazaar window open, clicking Manage Orders (slot {})", manage_slot);
                click_window_slot(bot, &state.last_window_id, window_id, manage_slot as i16).await;
            } else if is_bazaar_orders_window_title(window_title) {
                // ── Process ONE order per ManageOrders cycle ──
                // On Hypixel, clicking a filled order slot directly collects items/coins.
                // If the order is open (nothing to collect), "Order options" opens instead.
                // We process only one order then go Idle so bazaar flips aren't blocked.
                let mode_str = if cancel_open { "cancel+collect" } else { "collect-only" };
                info!("[ManageOrders] Processing orders ({}) — single order per cycle", mode_str);
                let persistent_processed = &state.manage_orders_processed;

                // Wait for ContainerSetContent to populate the window
                tokio::time::sleep(tokio::time::Duration::from_millis(250)).await;
                if check_manage_orders_deadline(bot, state, window_id) { return; }
                if *state.last_window_id.read() != window_id { return; }

                let slots = bot.menu().slots();

                // ── Scan all order slots and pick the highest-priority one ──
                // Priority (collect-only mode): claimable/filled orders first,
                //   SELL before BUY within each tier.
                // Priority (cancel mode): SELL first, then BUY (all orders).
                //
                // Tuple: (slot, name, identity, order_key, claimable, filled_amount, order_amount, unit_price)
                type OrderEntry = (usize, String, Option<(bool, String)>, String, bool, Option<u64>, u64, f64);
                let mut sell_orders: Vec<OrderEntry> = Vec::new();
                let mut buy_orders: Vec<OrderEntry> = Vec::new();

                for (i, item) in slots.iter().enumerate() {
                    if let Some(name) = get_item_display_name_from_slot(item) {
                        let lore = get_item_lore_from_slot(item);
                        let name_key = normalize_bazaar_order_text(&name);
                        if persistent_processed.read().contains(&name_key) {
                            continue;
                        }
                        let identity = parse_bazaar_order_identity(&name, &lore);
                        if !should_treat_as_bazaar_order_slot(&name, identity.as_ref()) {
                            continue;
                        }
                        let is_buy = identity.as_ref()
                            .map(|(b, _)| *b)
                            .unwrap_or_else(|| is_buy_bazaar_order_name(&name));
                        let claimable = is_order_claimable_from_lore(&lore);
                        let filled_amount = parse_filled_amount_from_lore(&lore).map(|(f, _)| f);
                        let order_amount = parse_order_amount_from_lore(&lore).unwrap_or(0);
                        let unit_price = parse_unit_price_from_lore(&lore).unwrap_or(0.0);
                        let order_key = format!("{}::{}", i, name_key);
                        if is_buy {
                            buy_orders.push((i, name, identity, order_key, claimable, filled_amount, order_amount, unit_price));
                        } else {
                            sell_orders.push((i, name, identity, order_key, claimable, filled_amount, order_amount, unit_price));
                        }
                    }
                }

                let total_orders = sell_orders.len() + buy_orders.len();

                // Emit a reconciliation snapshot so the tracker can remove
                // stale entries that no longer exist in-game.
                {
                    let mut ingame_orders = Vec::with_capacity(total_orders);
                    for (_, name, identity, _, _, _, order_amount, unit_price) in sell_orders.iter().chain(buy_orders.iter()) {
                        let is_buy = identity.as_ref()
                            .map(|(b, _)| *b)
                            .unwrap_or_else(|| is_buy_bazaar_order_name(name));
                        let clean = clean_order_item_name(name, identity);
                        ingame_orders.push((clean, is_buy, *order_amount, *unit_price));
                    }
                    let _ = state.event_tx.send(BotEvent::BazaarOrdersSnapshot { ingame_orders });
                }

                // In collect-only mode, ONLY click claimable orders.  Open
                // (unfilled) orders have nothing to collect and clicking them
                // opens "Order options" which wastes a cycle and can leave the
                // bot stuck for the entire periodic-check interval.
                // However, stale orders that exceed the cancel-due-to-age threshold
                // must still be selected so they can be cancelled.
                let cancel_mins = state.bazaar_order_cancel_minutes_per_million;
                let mut inv_full = state.inventory_full.load(Ordering::Relaxed);
                // When the flag is set, double-check the actual inventory.
                // The flag can become stale after a manual instasell or
                // delayed Hypixel reminder ("stashed away").  If there are
                // enough free slots to hold a stack, clear the flag and let
                // BUY orders through.
                if inv_full {
                    let empty = count_empty_player_slots(&bot);
                    if empty >= MIN_FREE_SLOTS_FOR_BUY as usize {
                        info!("[ManageOrders] inventory_full flag was set but {} empty slots found — clearing flag", empty);
                        state.inventory_full.store(false, Ordering::Relaxed);
                        inv_full = false;
                    }
                }
                let chosen_order: Option<OrderEntry> = if let Some((ref tgt_name, tgt_is_buy)) = target_item {
                    // Targeted cancel: find the specific order matching the web GUI request.
                    let tgt_norm = crate::bazaar_tracker::normalize_for_match_pub(tgt_name);
                    let mut all_orders = sell_orders.iter().chain(buy_orders.iter());
                    all_orders.find(|(_, name, identity, _, _, _, _, _)| {
                        let order_is_buy = identity.as_ref()
                            .map(|(b, _)| *b)
                            .unwrap_or_else(|| is_buy_bazaar_order_name(name));
                        if order_is_buy != tgt_is_buy { return false; }
                        let clean = clean_order_item_name(name, identity);
                        crate::bazaar_tracker::normalize_for_match_pub(&clean) == tgt_norm
                    }).cloned()
                } else if !cancel_open {
                    // Always try claimable sell orders first (yield coins, no
                    // inventory space needed).
                    sell_orders.iter().find(|&(_, _, _, _, claimable, _, _, _)| *claimable).cloned()
                        // Claimable buy orders — skip entirely when inventory is
                        // full so we don't waste a ManageOrders cycle opening /bz,
                        // navigating to the order, and then aborting.
                        .or_else(|| {
                            if inv_full { None } else {
                                buy_orders.iter().find(|&(_, _, _, _, claimable, _, _, _)| *claimable).cloned()
                            }
                        })
                        .or_else(|| sell_orders.iter().find(|&(_, _, identity, _, claimable, _, _, _)| {
                            !claimable && should_cancel_open_order_due_to_age(identity.clone(), cancel_mins)
                        }).cloned())
                        .or_else(|| buy_orders.iter().find(|&(_, _, identity, _, claimable, _, _, _)| {
                            !claimable && should_cancel_open_order_due_to_age(identity.clone(), cancel_mins)
                        }).cloned())
                } else {
                    // cancel mode: sell first, then buy (original behaviour)
                    sell_orders.into_iter().next()
                        .or_else(|| buy_orders.into_iter().next())
                };

                match chosen_order {
                    None => {
                        // No orders to process — done.
                        info!("[ManageOrders] No actionable orders, closing window");
                        send_raw_close(bot, window_id, &state.handlers);
                        *state.manage_orders_deadline.write() = None;
                        state.bazaar_at_limit.store(false, Ordering::Relaxed);
                        *state.bot_state.write() = BotState::Idle;
                    }
                    Some((i, order_name, order_identity, _processed_key, _claimable, order_filled_amount, _, _)) => {
                        let order_is_buy = order_identity.as_ref()
                            .map(|(b, _)| *b)
                            .unwrap_or_else(|| is_buy_bazaar_order_name(&order_name));

                        // Skip BUY orders when inventory is full (no room for items).
                        // Exception: cancel_open mode still clicks to cancel.
                        // Re-check actual inventory to catch space freed by manual
                        // instasell or other actions since the flag was set.
                        if order_is_buy && !cancel_open {
                            let still_full = state.inventory_full.load(Ordering::Relaxed);
                            if still_full {
                                let empty = count_empty_player_slots(&bot);
                                if empty >= MIN_FREE_SLOTS_FOR_BUY as usize {
                                    info!("[ManageOrders] inventory_full flag stale — {} empty slots, proceeding with BUY order \"{}\"", empty, order_name);
                                    state.inventory_full.store(false, Ordering::Relaxed);
                                } else {
                                    warn!("[ManageOrders] Inventory full ({} empty slots) — skipping BUY order \"{}\"", empty, order_name);
                                    log_pending_claim(&order_name);
                                    persistent_processed.write().insert(normalize_bazaar_order_text(&order_name));
                                    send_raw_close(bot, window_id, &state.handlers);
                                    *state.manage_orders_deadline.write() = None;
                                    *state.bot_state.write() = BotState::Idle;
                                    return;
                                }
                            }
                        }

                        // Store context for the Order options handler (Branch C)
                        *state.managing_order_context.write() = Some((order_is_buy, order_name.clone(), order_identity.clone(), order_filled_amount));

                        info!("[ManageOrders] Clicking order at slot {}: \"{}\"", i, order_name);
                        click_window_slot(bot, &state.last_window_id, window_id, i as i16).await;

                        // Wait up to 5 seconds for Hypixel's response.
                        // • If "Order options" opens → order is open, Branch C handles it.
                        // • If the window doesn't change → the order was collected directly
                        //   by clicking the slot (Hypixel collects filled orders on click).
                        // • If a new "Bazaar Orders" window opens (same list, new ID) →
                        //   the order was collected and Hypixel refreshed the list.
                        let click_deadline = tokio::time::Instant::now()
                            + tokio::time::Duration::from_secs(5);
                        let mut order_options_opened = false;
                        loop {
                            tokio::time::sleep(tokio::time::Duration::from_millis(150)).await;
                            if *state.last_window_id.read() != window_id {
                                // A new window opened — check its title to distinguish
                                // "Order options" (order is open) from a refreshed
                                // "Bazaar Orders" list (order was collected on click).
                                if let Some(new_title) = state.handlers.current_window_title() {
                                    if new_title.to_lowercase().contains("order options") {
                                        order_options_opened = true;
                                    }
                                    // else: Hypixel re-opened the orders list after
                                    // collecting — treat as successful direct collection.
                                } else {
                                    // No title available — assume Order options for safety.
                                    order_options_opened = true;
                                }
                                break;
                            }
                            if state.inventory_full.load(Ordering::Relaxed) && order_is_buy {
                                // Inventory is full — BUY items can't be claimed.
                                // Treat as if Order options opened so we do NOT falsely
                                // emit a BazaarOrderCollected event.
                                order_options_opened = true;
                                break;
                            }
                            if tokio::time::Instant::now() >= click_deadline {
                                break;
                            }
                        }

                        if !order_options_opened {
                            // No "Order options" window → order was collected by clicking.
                            // Emit the collected event.
                            info!("[ManageOrders] Order \"{}\" collected (no Order options opened)", order_name);
                            let _ = state.event_tx.send(BotEvent::BazaarOrderCollected {
                                item_name: clean_order_item_name(&order_name, &order_identity),
                                is_buy_order: order_is_buy,
                                // Direct collection = fully filled. Use parsed filled
                                // amount from lore; falls back to tracker amount in handler.
                                claimed_amount: order_filled_amount,
                            });
                            persistent_processed.write().insert(normalize_bazaar_order_text(&order_name));
                            state.bazaar_at_limit.store(false, Ordering::Relaxed);
                        }
                        // else: "Order options" opened — Branch C handler takes over.
                        // It will handle collect/cancel/skip and then go Idle.

                        // Close this window and go Idle (one order per cycle).
                        if *state.last_window_id.read() == window_id {
                            send_raw_close(bot, window_id, &state.handlers);
                        }
                        if !order_options_opened {
                            // Only transition to Idle when the order was collected
                            // directly (no Order options window).  When Order options
                            // opened, the Order options handler (Branch C) takes over
                            // and is responsible for closing, clearing the deadline,
                            // and transitioning to Idle.  Setting Idle here races with
                            // the Branch C handler and can cause it to miss the
                            // ManagingOrders state, leaving the window stuck open.
                            *state.manage_orders_deadline.write() = None;
                            *state.bot_state.write() = BotState::Idle;
                        }

                        // If there are more orders to process, immediately
                        // re-queue ManageOrders so we don't wait for the
                        // periodic timer (up to 60s) between each order.
                        // Only re-queue when the order was collected directly
                        // (order_options_opened == false).  When Order options
                        // opened, the Order options handler takes over and will
                        // go Idle after it finishes — we must not re-queue here
                        // because that handler is still active.
                        if total_orders > 1 && !order_options_opened {
                            if let Some(queue) = state.command_queue.read().as_ref() {
                                if !queue.has_manage_orders() && target_item.is_none() {
                                    info!("[ManageOrders] {} more order(s) remain — re-queuing ManageOrders", total_orders - 1);
                                    queue.enqueue(
                                        crate::types::CommandType::ManageOrders { cancel_open, target_item: None },
                                        crate::types::CommandPriority::Normal,
                                        false,
                                    );
                                }
                            }
                        }
                    }
                }
            } else if window_title.to_lowercase().contains("order options") {
                // Hypixel opened "Order options" after clicking an order in the list.
                // This means the order is OPEN (not filled — filled orders collect on click).
                // After handling ONE order, close and go Idle (one order per cycle).

                let order_ctx = state.managing_order_context.read().clone();
                let (order_name, order_identity, order_filled_amount) = match &order_ctx {
                    Some((_is_buy, name, identity, filled)) => (name.clone(), identity.clone(), *filled),
                    None => (String::new(), None, None),
                };
                let order_identity_for_clean = order_ctx.as_ref().and_then(|(_, _, id, _)| id.clone());

                let name_key = normalize_bazaar_order_text(&order_name);
                if !name_key.is_empty() && state.manage_orders_processed.read().contains(&name_key) {
                    debug!("[ManageOrders] Order \"{}\" already processed — closing", order_name);
                    send_raw_close(bot, window_id, &state.handlers);
                    *state.manage_orders_deadline.write() = None;
                    *state.bot_state.write() = BotState::Idle;
                } else {
                // Protect recently-placed orders (< 5 min) from cancellation,
                // even in cancel_open (startup) mode.
                let too_young_to_cancel = is_order_below_min_cancel_age(&order_identity);
                if too_young_to_cancel {
                    info!(
                        "[ManageOrders] Order \"{}\" is less than {} seconds old — too young to cancel",
                        order_name, MIN_ORDER_AGE_BEFORE_CANCEL_SECS
                    );
                }

                // Determine cancel_due_to_age BEFORE deciding whether to skip.
                // This allows stale orders to be cancelled even in collect-only mode.
                let cancel_due_to_age = !cancel_open
                    && should_cancel_open_order_due_to_age(order_identity, state.bazaar_order_cancel_minutes_per_million);

                // Check cancel retry limit early — if exceeded, close immediately.
                let cancel_fail_key = normalize_bazaar_order_text(&order_name);
                let prior_failures = *state.order_cancel_failures.read().get(&cancel_fail_key).unwrap_or(&0);
                let cancel_exceeded = prior_failures >= MAX_CANCEL_RETRIES;

                if cancel_exceeded && (cancel_open || cancel_due_to_age) && !too_young_to_cancel {
                    warn!(
                        "[ManageOrders] Order \"{}\" exceeded {} cancel attempts — closing GUI and giving up",
                        order_name, MAX_CANCEL_RETRIES
                    );
                    if !name_key.is_empty() {
                        state.manage_orders_processed.write().insert(name_key);
                    }
                    if *state.last_window_id.read() == window_id {
                        send_raw_close(bot, window_id, &state.handlers);
                    }
                    *state.manage_orders_deadline.write() = None;
                    state.bazaar_at_limit.store(false, Ordering::Relaxed);
                    *state.bot_state.write() = BotState::Idle;
                } else if (!cancel_open && !cancel_due_to_age) || too_young_to_cancel {
                    // Collect-only mode (or order too young to cancel).
                    // "Order options" opened, so this order is not 100% filled (those
                    // collect on click).  It may be PARTIALLY filled (has a Collect
                    // button) or completely open (no Collect button).
                    // Look for a Collect button first — if found, collect the partial
                    // fill before closing.
                    let probe_deadline =
                        tokio::time::Instant::now() + tokio::time::Duration::from_secs(2);
                    let mut collect_slot_probe: Option<usize> = None;
                    loop {
                        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                        if *state.last_window_id.read() != window_id { break; }
                        let slots_probe = bot.menu().slots();
                        collect_slot_probe = find_slot_by_name(&slots_probe, "Collect")
                            .or_else(|| find_slot_by_name(&slots_probe, "Claim"))
                            .or_else(|| find_slot_by_lore_contains(&slots_probe, "click to collect"))
                            .or_else(|| find_slot_by_lore_contains(&slots_probe, "claim your"));
                        if collect_slot_probe.is_some() { break; }
                        // Also check for Cancel — if present but no Collect, order is
                        // truly open with nothing to collect.
                        let has_cancel = find_slot_by_name(&slots_probe, "Cancel").is_some()
                            || find_slot_by_lore_contains(&slots_probe, "click to cancel").is_some();
                        if has_cancel { break; }
                        if tokio::time::Instant::now() >= probe_deadline { break; }
                    }

                    if let Some(cs) = collect_slot_probe {
                        // Partially filled order — collect before closing.
                        info!("[ManageOrders] Partially filled order \"{}\" — clicking Collect at slot {} (collect-only)", order_name, cs);
                        if *state.last_window_id.read() == window_id {
                            click_window_slot(bot, &state.last_window_id, window_id, cs as i16).await;
                            if wait_for_collect_confirmation(bot, &state.last_window_id, window_id).await {
                                if let Some((ctx_is_buy, _, _, _)) = order_ctx.as_ref() {
                                    let _ = state.event_tx.send(BotEvent::BazaarOrderCollected {
                                        item_name: clean_order_item_name(&order_name, &order_identity_for_clean),
                                        is_buy_order: *ctx_is_buy,
                                        // Partial fill — use filled amount parsed from
                                        // the Manage Orders lore before we clicked.
                                        claimed_amount: order_filled_amount,
                                    });
                                    // For BUY orders, cancel the remaining order after
                                    // collecting the partial fill.  This lets cofl send
                                    // sell recommendations for all collected items at
                                    // once instead of waiting for the full order to fill.
                                    // Skip if order is too young (< 5 min) — give it
                                    // more time to fill.
                                    if *ctx_is_buy && !too_young_to_cancel {
                                        let cancel_slot = find_slot_by_name(&bot.menu().slots(), "Cancel")
                                            .or_else(|| find_slot_by_lore_contains(&bot.menu().slots(), "click to cancel"))
                                            .or_else(|| find_slot_by_lore_contains(&bot.menu().slots(), "cancel order"));
                                        if let Some(cs) = cancel_slot {
                                            if *state.last_window_id.read() == window_id {
                                                info!("[ManageOrders] Cancelling remaining BUY order \"{}\" after partial collect — prefer immediate sell via cofl", order_name);
                                                click_window_slot(bot, &state.last_window_id, window_id, cs as i16).await;
                                                if wait_for_cancel_confirmation(bot, &state.last_window_id, window_id).await {
                                                    *state.manage_orders_cancelled.write() += 1;
                                                    state.order_cancel_failures.write().remove(&cancel_fail_key);
                                                    let _ = state.event_tx.send(BotEvent::BazaarOrderCancelled {
                                                        item_name: clean_order_item_name(&order_name, &order_identity_for_clean),
                                                        is_buy_order: true,
                                                        already_collected: true,
                                                    });
                                                } else {
                                                    warn!("[ManageOrders] Cancel after partial BUY collect for \"{}\" was not confirmed", order_name);
                                                }
                                            }
                                        }
                                    }
                                }
                            } else {
                                warn!("[ManageOrders] Collect click for partially filled \"{}\" was not confirmed", order_name);
                            }
                        }
                    } else {
                        debug!("[ManageOrders] Order \"{}\" is open — skipping ({})", order_name,
                            if too_young_to_cancel { "too young to cancel" } else { "collect-only mode" });
                    }

                    if !name_key.is_empty() {
                        state.manage_orders_processed.write().insert(name_key);
                    }
                    if *state.last_window_id.read() == window_id {
                        send_raw_close(bot, window_id, &state.handlers);
                    }
                    *state.manage_orders_deadline.write() = None;
                    *state.bot_state.write() = BotState::Idle;

                    // Re-queue so remaining orders are processed without
                    // waiting for the full periodic-check interval.
                    if collect_slot_probe.is_some() {
                        if let Some(queue) = state.command_queue.read().as_ref() {
                            if !queue.has_manage_orders() {
                                info!("[ManageOrders] Re-queuing ManageOrders after partial collect in Order options");
                                queue.enqueue(
                                    crate::types::CommandType::ManageOrders { cancel_open, target_item: None },
                                    crate::types::CommandPriority::Normal,
                                    false,
                                );
                            }
                        }
                    }
                } else {
                // cancel_open mode OR cancel_due_to_age: look for Cancel/Collect buttons.
                info!(
                    "[ManageOrders] Order options window opened ({}) — looking for Cancel/Collect buttons",
                    if cancel_open { "startup cancel mode" } else { "cancel due to age" }
                );

                let action_deadline =
                    tokio::time::Instant::now() + tokio::time::Duration::from_secs(3);
                let mut cancel_slot: Option<usize> = None;
                let mut collect_slot: Option<usize> = None;
                loop {
                    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                    if *state.last_window_id.read() != window_id {
                        break;
                    }
                    let slots2 = bot.menu().slots();
                    collect_slot = find_slot_by_name(&slots2, "Collect")
                        .or_else(|| find_slot_by_name(&slots2, "Claim"))
                        .or_else(|| find_slot_by_lore_contains(&slots2, "click to collect"))
                        .or_else(|| find_slot_by_lore_contains(&slots2, "claim your"));
                    cancel_slot = find_slot_by_name(&slots2, "Cancel")
                        .or_else(|| find_slot_by_lore_contains(&slots2, "click to cancel"))
                        .or_else(|| find_slot_by_lore_contains(&slots2, "cancel order"));
                    if collect_slot.is_some() || cancel_slot.is_some() {
                        break;
                    }
                    if tokio::time::Instant::now() >= action_deadline {
                        let slot_names: Vec<String> = slots2
                            .iter()
                            .enumerate()
                            .filter_map(|(idx, item)| {
                                get_item_display_name_from_slot(item).map(|n| format!("{}={}", idx, n))
                            })
                            .collect();
                        warn!(
                            "[ManageOrders] No Collect/Cancel button found in Order options — visible slots: [{}]",
                            slot_names.join(", ")
                        );
                        break;
                    }
                }

                if cancel_due_to_age && cancel_slot.is_some() {
                    info!(
                        "[ManageOrders] Open order \"{}\" exceeds cancel threshold ({}m/M) — will cancel (Order options)",
                        order_name, state.bazaar_order_cancel_minutes_per_million
                    );
                }

                if let Some(cs) = collect_slot {
                    if *state.last_window_id.read() == window_id {
                        info!("[ManageOrders] Clicking Collect at slot {} in Order options", cs);
                        click_window_slot(bot, &state.last_window_id, window_id, cs as i16).await;
                        if wait_for_collect_confirmation(bot, &state.last_window_id, window_id).await {
                            if let Some((ctx_is_buy, _, _, _)) = order_ctx.as_ref() {
                                let _ = state.event_tx.send(BotEvent::BazaarOrderCollected {
                                    item_name: clean_order_item_name(&order_name, &order_identity_for_clean),
                                    is_buy_order: *ctx_is_buy,
                                    // Partial or full fill from Order Options — use
                                    // filled amount parsed from the Manage Orders lore.
                                    claimed_amount: order_filled_amount,
                                });
                            }
                        } else {
                            warn!("[ManageOrders] Collect click for \"{}\" was not confirmed in Order options", order_name);
                        }
                        if (cancel_open || cancel_due_to_age) && !cancel_exceeded {
                            if let Some(cancel_after) = find_slot_by_name(&bot.menu().slots(), "Cancel") {
                                if *state.last_window_id.read() == window_id {
                                    info!("[ManageOrders] Clicking Cancel at slot {} after collecting in Order options", cancel_after);
                                    click_window_slot(bot, &state.last_window_id, window_id, cancel_after as i16).await;
                                    if wait_for_cancel_confirmation(bot, &state.last_window_id, window_id).await {
                                        *state.manage_orders_cancelled.write() += 1;
                                        state.order_cancel_failures.write().remove(&cancel_fail_key);
                                        if let Some((ctx_is_buy, _, _, _)) = order_ctx.as_ref() {
                                            let _ = state.event_tx.send(BotEvent::BazaarOrderCancelled {
                                                item_name: clean_order_item_name(&order_name, &order_identity_for_clean),
                                                is_buy_order: *ctx_is_buy,
                                                already_collected: true,
                                            });
                                        }
                                    } else {
                                        *state.order_cancel_failures.write().entry(cancel_fail_key.clone()).or_insert(0) += 1;
                                        warn!("[ManageOrders] Cancel click for \"{}\" was not confirmed in Order options (attempt {})", order_name, prior_failures + 1);
                                    }
                                }
                            }
                        }
                    }
                } else if let Some(cs) = cancel_slot {
                    if (cancel_open || cancel_due_to_age) && !cancel_exceeded {
                        if *state.last_window_id.read() == window_id {
                            info!("[ManageOrders] Clicking Cancel at slot {} in Order options", cs);
                            click_window_slot(bot, &state.last_window_id, window_id, cs as i16).await;
                            if wait_for_cancel_confirmation(bot, &state.last_window_id, window_id).await {
                                *state.manage_orders_cancelled.write() += 1;
                                state.order_cancel_failures.write().remove(&cancel_fail_key);
                                if let Some((ctx_is_buy, _, _, _)) = order_ctx.as_ref() {
                                    let _ = state.event_tx.send(BotEvent::BazaarOrderCancelled {
                                        item_name: clean_order_item_name(&order_name, &order_identity_for_clean),
                                        is_buy_order: *ctx_is_buy,
                                        already_collected: false,
                                    });
                                }
                            } else {
                                *state.order_cancel_failures.write().entry(cancel_fail_key.clone()).or_insert(0) += 1;
                                warn!("[ManageOrders] Cancel click for \"{}\" was not confirmed in Order options (attempt {})", order_name, prior_failures + 1);
                            }
                        }
                    } else if !cancel_open && !cancel_due_to_age {
                        debug!("[ManageOrders] Skipping open order \"{}\" in Order options (collect-only mode)", order_name);
                    }
                } else {
                    warn!("[ManageOrders] No actionable button in Order options for \"{}\"", order_name);
                }

                // Mark as processed, close, and go Idle (one order per cycle)
                if !name_key.is_empty() {
                    state.manage_orders_processed.write().insert(name_key);
                }
                if *state.last_window_id.read() == window_id {
                    send_raw_close(bot, window_id, &state.handlers);
                }
                *state.manage_orders_deadline.write() = None;
                state.bazaar_at_limit.store(false, Ordering::Relaxed);
                *state.bot_state.write() = BotState::Idle;

                // Re-queue so remaining orders are processed promptly.
                if let Some(queue) = state.command_queue.read().as_ref() {
                    if !queue.has_manage_orders() {
                        info!("[ManageOrders] Re-queuing ManageOrders after Order options (cancel/collect)");
                        queue.enqueue(
                            crate::types::CommandType::ManageOrders { cancel_open, target_item: None },
                            crate::types::CommandPriority::Normal,
                            false,
                        );
                    }
                }
                }
                }
            } else {
                // Unexpected window title while in ManagingOrders state.
                // Re-navigate to /bz to get back on track.
                warn!(
                    "[ManageOrders] Unexpected window \"{}\" — closing and re-opening /bz",
                    window_title
                );
                close_window_and_reopen_bz(bot, state, window_id).await;
            }
        }
        BotState::SellingInventoryBz => {
            // Sell whole inventory instantly via /bz → "Sell Inventory Now"
            // → "Selling whole inventory" (slot 11).
            //
            // Flow (reuses bazaar_step for sub-state):
            //   Initial          — bazaar main page: find & click "Sell Inventory Now"
            //   SearchResults    — confirmation page: click slot 11 to confirm
            //
            // If inventory has no instasellable items, Hypixel sends
            // "You don't have anything to sell!" in chat and does not open
            // the confirmation window.
            let step = *state.bazaar_step.read();
            if *state.last_window_id.read() != window_id {
                return;
            }

            if step == BazaarStep::Initial && window_title.contains("Bazaar") {
                // Bazaar page (main or category) — find "Sell Inventory Now"
                // button dynamically.
                tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
                if *state.last_window_id.read() != window_id { return; }
                let slots = bot.menu().slots();
                let sell_inv_slot = find_slot_by_name(&slots, "Sell Inventory Now").unwrap_or(SELL_INVENTORY_NOW_FALLBACK_SLOT);
                info!("[SellInventoryBz] Bazaar window open — clicking 'Sell Inventory Now' at slot {}", sell_inv_slot);
                *state.bazaar_step.write() = BazaarStep::SearchResults;
                click_window_slot(bot, &state.last_window_id, window_id, sell_inv_slot as i16).await;
            } else if step == BazaarStep::SearchResults {
                // Confirmation page — click slot 11 ("Selling whole inventory")
                info!("[SellInventoryBz] Confirmation window open — clicking slot 11 to sell");
                tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
                if *state.last_window_id.read() != window_id { return; }
                click_window_slot(bot, &state.last_window_id, window_id, 11).await;
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                info!("[SellInventoryBz] Done — closing window and going idle");
                send_raw_close(bot, window_id, &state.handlers);
                *state.bazaar_step.write() = BazaarStep::Initial;
                *state.bot_state.write() = BotState::Idle;
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
            send_raw_close(bot, window_id, &state.handlers);
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
                            send_chat_command(bot, "/bz Booster Cookie");
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
                click_window_slot(bot, &state.last_window_id, window_id, 11).await;
                *state.cookie_step.write() = CookieStep::ItemDetail;
            } else if step == CookieStep::ItemDetail {
                // Cookie item detail: click slot 10 (Buy Instantly)
                info!("[Cookie] Cookie detail — clicking Buy Instantly (slot 10)");
                click_window_slot(bot, &state.last_window_id, window_id, 10).await;
                *state.cookie_step.write() = CookieStep::BuyConfirm;
            } else if step == CookieStep::BuyConfirm {
                // Atomically advance to WaitingForCookie before any sleeps.
                // This prevents concurrent window events (e.g. the Bazaar re-opening the
                // item-detail page after purchase) from triggering additional buys.
                // Lock is acquired, checked, updated, then released before any I/O.
                let claimed = {
                    let mut step_write = state.cookie_step.write();
                    if *step_write == CookieStep::BuyConfirm {
                        *step_write = CookieStep::WaitingForCookie;
                        true
                    } else {
                        false
                    }
                };
                if !claimed {
                    // Another concurrent handler already processed this step — close
                    // any stale window and bail out.
                    info!("[Cookie] BuyConfirm already handled by another task — closing window");
                    send_raw_close(bot, window_id, &state.handlers);
                    return;
                }

                // Purchase confirmation: click slot 10 to confirm
                info!("[Cookie] Buy confirmation — clicking Confirm (slot 10)");
                click_window_slot(bot, &state.last_window_id, window_id, 10).await;
                // Let the purchase process; the Bazaar may re-open the item-detail window
                // after purchase — that is handled by the WaitingForCookie branch below.
                tokio::time::sleep(tokio::time::Duration::from_millis(2000)).await;
                // Close the purchase/item-detail window if still open
                send_raw_close(bot, window_id, &state.handlers);
                tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

                // Find cookie in inventory and consume it by right-clicking.
                // Matches TypeScript: bot.equip(item, 'hand') → bot.activateItem() → click slot 11.
                let menu = bot.menu();
                let all_slots = menu.slots();
                let player_range = menu.player_slots_range();
                let cookie_slot = all_slots[player_range.clone()].iter().enumerate().find_map(|(i, item)| {
                    let name = get_item_display_name_from_slot(item).unwrap_or_default().to_lowercase();
                    if name.contains("booster cookie") || name.contains("cookie") {
                        Some(i)
                    } else {
                        None
                    }
                });

                match cookie_slot {
                    Some(idx) => {
                        info!("[Cookie] Found cookie at player inventory index {} — equipping and consuming", idx);
                        // Convert player-range-relative index to hotbar slot (0-8).
                        // Player slots: 0-26 = main inventory, 27-35 = hotbar (slots 36-44 in menu).
                        // If cookie is already in hotbar (idx >= 27), select that hotbar slot.
                        // Otherwise, move it to hotbar slot 0 first.
                        let hotbar_slot: u16 = if idx >= 27 {
                            // Already in hotbar — map to hotbar index 0-8
                            (idx - 27) as u16
                        } else {
                            // Cookie is in main inventory — need to swap it to hotbar.
                            // Open player inventory (container 0), click cookie slot, then hotbar slot 0.
                            let inv_slot = (*player_range.start() + idx) as i16;
                            // Pick up cookie
                            click_window_slot(bot, &state.last_window_id, 0, inv_slot).await;
                            tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                            // Place in hotbar slot 0 (slot 36 in player inventory container)
                            click_window_slot(bot, &state.last_window_id, 0, 36).await;
                            tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                            0
                        };

                        // Select the hotbar slot
                        bot.write_packet(ServerboundSetCarriedItem { slot: hotbar_slot });
                        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

                        // Right-click to open the cookie GUI
                        bot.write_packet(ServerboundUseItem {
                            hand: InteractionHand::MainHand,
                            seq: 0,
                            y_rot: 0.0,
                            x_rot: 0.0,
                        });
                        info!("[Cookie] Right-clicked cookie — waiting for cookie GUI");

                        // Transition to ConsumingCookie so the next OpenScreen event
                        // (the cookie activation GUI) is handled correctly.
                        *state.cookie_step.write() = CookieStep::ConsumingCookie;
                    }
                    None => {
                        warn!("[Cookie] Cookie not found in inventory after purchase");
                        let _ = state.event_tx.send(BotEvent::ChatMessage(
                            "§f[§4BAF§f]: §c[AutoCookie] Cookie purchased but not found in inventory".to_string()
                        ));
                        *state.bot_state.write() = BotState::Idle;
                    }
                }
            } else if step == CookieStep::WaitingForCookie {
                // Between clicking Confirm and right-clicking the cookie in inventory.
                // Any window that opens here (e.g. Bazaar re-opening item detail) is
                // unexpected — close it immediately and do nothing else.
                info!("[Cookie] Unexpected window while waiting for cookie in inventory — closing");
                send_raw_close(bot, window_id, &state.handlers);
            } else if step == CookieStep::ConsumingCookie {
                // Atomically claim the consume step so only one concurrent handler fires.
                // Lock is acquired, checked, updated, then released before any I/O.
                let claimed = {
                    let mut step_write = state.cookie_step.write();
                    if *step_write == CookieStep::ConsumingCookie {
                        // Advance past ConsumingCookie to prevent re-entry.
                        *step_write = CookieStep::Initial;
                        true
                    } else {
                        false
                    }
                };
                if !claimed {
                    // Already handled — close stale window and bail.
                    info!("[Cookie] ConsumingCookie already handled by another task — closing window");
                    send_raw_close(bot, window_id, &state.handlers);
                    return;
                }

                // Cookie GUI opened — click slot 11 to consume the cookie
                info!("[Cookie] Cookie GUI opened — clicking slot 11 to consume");
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                click_window_slot(bot, &state.last_window_id, window_id, 11).await;
                tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

                // Close the cookie GUI
                send_raw_close(bot, window_id, &state.handlers);

                let current_time = *state.cookie_time_secs.read();
                let new_hours = (current_time + 4 * 86400) / 3600;
                let old_hours = current_time / 3600;
                let _ = state.event_tx.send(BotEvent::ChatMessage(format!(
                    "§f[§4BAF§f]: §aBought and consumed booster cookie! Time: {}h → {}h",
                    old_hours, new_hours
                )));
                info!("[Cookie] Cookie consumed successfully! Time: {}h → {}h", old_hours, new_hours);
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
                // Full component_patch failed to serialize (e.g. HashMap<Enchantment, i32>
                // keys don't pass serde_json's strict map-key check). Fall back to
                // extracting individual components that serialize cleanly.
                // This preserves COFL-critical data: ExtraAttributes (custom_data),
                // display name (custom_name), lore, head texture (profile), and
                // tooltip visibility (tooltip_display).
                let obj = extract_serializable_components(item_data);
                if obj.is_empty() {
                    debug!(
                        "[Inventory] Skipping component patch NBT extraction due to expected serialization limitation"
                    );
                    serde_json::Value::Null
                } else {
                    debug!("[Inventory] Full component_patch failed; extracted {} individual components", obj.len());
                    serde_json::Value::Object(obj)
                }
            } else {
                warn!(
                    "[Inventory] Failed to serialize component patch for NBT extraction: {}",
                    e
                );
                serde_json::Value::Null
            }
        }
    }
}

/// Extract individual item components that are known to serialize without errors.
/// Used as a fallback when the full component_patch serialization fails (e.g., due to
/// HashMap<Enchantment, i32> non-string map keys in enchanted items).
fn extract_serializable_components(item_data: &azalea_inventory::ItemStackData) -> serde_json::Map<String, serde_json::Value> {
    use azalea_inventory::components::{CustomData, CustomName, Lore, Profile, TooltipDisplay};
    let mut obj = serde_json::Map::new();

    // minecraft:custom_data — Hypixel SkyBlock ExtraAttributes (item id, uuid, etc.)
    if let Some(custom_data) = item_data.component_patch.get::<CustomData>() {
        if let Ok(nbt_val) = serde_json::to_value(&custom_data.nbt) {
            if !nbt_val.is_null() {
                obj.insert("minecraft:custom_data".to_string(), nbt_val);
            }
        }
    }
    // minecraft:custom_name — human-readable display name (e.g. "Stellar Mithril Drill SX-R326")
    if let Some(cn) = item_data.component_patch.get::<CustomName>() {
        if let Ok(val) = serde_json::to_value(cn) {
            if !val.is_null() {
                obj.insert("minecraft:custom_name".to_string(), val);
            }
        }
    }
    // minecraft:lore — item description lines shown in tooltip
    if let Some(lore) = item_data.component_patch.get::<Lore>() {
        if let Ok(val) = serde_json::to_value(lore) {
            if !val.is_null() {
                obj.insert("minecraft:lore".to_string(), val);
            }
        }
    }
    // minecraft:profile — skull/player-head skin (critical for drills and pets)
    if let Some(profile) = item_data.component_patch.get::<Profile>() {
        if let Ok(val) = serde_json::to_value(profile) {
            if !val.is_null() {
                obj.insert("minecraft:profile".to_string(), val);
            }
        }
    }
    // minecraft:tooltip_display — hidden_components list
    if let Some(tooltip) = item_data.component_patch.get::<TooltipDisplay>() {
        if let Ok(val) = serde_json::to_value(tooltip) {
            if !val.is_null() {
                obj.insert("minecraft:tooltip_display".to_string(), val);
            }
        }
    }
    obj
}

fn should_suppress_component_patch_serialization_warning(error: &serde_json::Error) -> bool {
    error.to_string().contains("key must be a string")
}

/// Extract the SkyBlock item tag (ExtraAttributes.id or direct id) from the CustomData component.
/// This bypasses the full component_patch serialization and works even when that fails
/// (e.g., due to enchantment HashMap key serialization issues).
fn extract_skyblock_tag_from_custom_data(item_data: &azalea_inventory::ItemStackData) -> Option<String> {
    use azalea_inventory::components::CustomData;
    let custom_data = item_data.component_patch.get::<CustomData>()?;
    // Serialize just the CustomData.nbt compound to JSON and extract the id
    let nbt_val = serde_json::to_value(&custom_data.nbt).ok()?;
    // Try ExtraAttributes.id first, then direct id (cosmetics, skins, etc.)
    nbt_val
        .get("ExtraAttributes")
        .and_then(|ea| ea.get("id"))
        .and_then(|id| id.as_str())
        .or_else(|| nbt_val.get("id").and_then(|id| id.as_str()))
        .map(|s| s.to_string())
}

/// Resolve pet-specific tag from the generic "PET" tag.
/// Pets all share ExtraAttributes.id = "PET". The actual pet type (e.g. "MAMMOTH")
/// is inside the petInfo JSON string at ExtraAttributes.petInfo or custom_data.petInfo.
/// Returns e.g. "PET_MAMMOTH" for Coflnet icon lookup, or the original tag unchanged.
fn resolve_pet_tag(tag: &str, nbt_data: &serde_json::Value) -> String {
    if tag != "PET" {
        return tag.to_string();
    }
    // Try to extract petInfo from various NBT paths
    let pet_info_str = nbt_data.get("minecraft:custom_data")
        .and_then(|cd| {
            // Path: custom_data.nbt.ExtraAttributes.petInfo
            cd.get("nbt")
                .and_then(|n| n.get("ExtraAttributes"))
                .and_then(|ea| ea.get("petInfo"))
                .and_then(|p| p.as_str())
                // Fallback: custom_data.ExtraAttributes.petInfo
                .or_else(|| cd.get("ExtraAttributes").and_then(|ea| ea.get("petInfo")).and_then(|p| p.as_str()))
                // Fallback: custom_data.petInfo (direct)
                .or_else(|| cd.get("petInfo").and_then(|p| p.as_str()))
        });
    if let Some(info_str) = pet_info_str {
        if let Ok(info) = serde_json::from_str::<serde_json::Value>(info_str) {
            if let Some(pet_type) = info.get("type").and_then(|t| t.as_str()) {
                return format!("PET_{}", pet_type);
            }
        }
    }
    tag.to_string()
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
            // Use minecraft registry ID (e.g. "minecraft:player_head")
            // COFL expects registry IDs. ItemKind::to_string() already includes the prefix.
            let item_name = item.kind().to_string();
            // Also include the display name for COFL item identification
            let display_name = get_item_display_name_from_slot(item).unwrap_or_default();
            // Log display name status for debugging; items without a displayName will fall
            // back to the registry name (e.g. minecraft:prismarine_shard) in createAuction.
            if display_name.is_empty() {
                debug!("[Inventory] slot {}: {}x {} — no displayName (NBT keys: {})",
                    mineflayer_slot, item.count(), item_name,
                    nbt_data.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>().join(", ")).unwrap_or_default()
                );
            }
            let display_label = if display_name.is_empty() { "no-name" } else { &display_name };
            slot_descriptions.push(format!("slot {}: {}x {} ({})", mineflayer_slot, item.count(), item_name, display_label));
            let mut slot_obj = serde_json::json!({
                "type": item_type,
                "count": item.count(),
                "metadata": 0,
                "nbt": nbt_data,
                "name": item_name,
                "slot": mineflayer_slot
            });
            if !display_name.is_empty() {
                slot_obj.as_object_mut().expect("slot_obj should be a JSON object").insert(
                    "displayName".to_string(),
                    serde_json::Value::String(display_name),
                );
            }
            // Add colored display name (with §-codes) for rarity-colored tooltip title
            if let Some(colored_name) = get_item_display_name_with_colors_from_slot(item) {
                slot_obj.as_object_mut().expect("slot_obj should be a JSON object").insert(
                    "displayNameColored".to_string(),
                    serde_json::Value::String(colored_name),
                );
            }
            // Extract SkyBlock item tag for icon lookup.
            // The NBT path differs between the extraction paths:
            //   full component_patch: nbt["minecraft:custom_data"]["nbt"]["ExtraAttributes"]["id"]
            //   fallback (individual components): nbt["minecraft:custom_data"]["ExtraAttributes"]["id"]
            //   direct id (cosmetics etc.): nbt["minecraft:custom_data"]["id"]
            //   direct id under nbt wrapper: nbt["minecraft:custom_data"]["nbt"]["id"]
            let tag_from_json = nbt_data.get("minecraft:custom_data")
                .and_then(|cd| {
                    // Try full component_patch path first (has extra "nbt" wrapper)
                    cd.get("nbt")
                        .and_then(|n| n.get("ExtraAttributes"))
                        .and_then(|ea| ea.get("id"))
                        .and_then(|id| id.as_str())
                        // Fallback path puts ExtraAttributes directly under custom_data
                        .or_else(|| {
                            cd.get("ExtraAttributes")
                                .and_then(|ea| ea.get("id"))
                                .and_then(|id| id.as_str())
                        })
                        // Direct id under nbt wrapper (no ExtraAttributes)
                        .or_else(|| {
                            cd.get("nbt")
                                .and_then(|n| n.get("id"))
                                .and_then(|id| id.as_str())
                        })
                        // Direct id under custom_data (cosmetics, skins, etc.)
                        .or_else(|| {
                            cd.get("id")
                                .and_then(|id| id.as_str())
                        })
                })
                .map(|s| s.to_string());

            // If JSON extraction failed, try extracting directly from the CustomData component
            let tag = tag_from_json.or_else(|| {
                if let Some(item_data) = item.as_present() {
                    extract_skyblock_tag_from_custom_data(item_data)
                } else {
                    None
                }
            });

            // Resolve pet-specific tags (PET → PET_MAMMOTH etc.)
            let tag = tag.map(|t| resolve_pet_tag(&t, &nbt_data));

            if let Some(ref tag_str) = tag {
                slot_obj.as_object_mut().expect("slot_obj should be a JSON object").insert(
                    "tag".to_string(),
                    serde_json::Value::String(tag_str.clone()),
                );
            }
            // Add lore lines with §-color codes for colorful tooltip display in the web panel.
            let lore_lines = get_item_lore_with_colors_from_slot(item);
            if !lore_lines.is_empty() {
                slot_obj.as_object_mut().expect("slot_obj is a JSON object created via json! macro").insert(
                    "lore".to_string(),
                    serde_json::Value::Array(lore_lines.into_iter().map(serde_json::Value::String).collect()),
                );
            }
            slots_array[mineflayer_slot] = slot_obj;
        }
    }

    debug!("[Inventory] Rebuilt cache: {} non-empty slots — {}", slot_descriptions.len(), slot_descriptions.join(", "));

    // Update cached empty-slot count.  The player inventory occupies mineflayer slots 9..44.
    // Count nulls in slots_array which correspond to empty (air) slots.
    let empty_count = (9..=44usize)
        .filter(|&s| slots_array.get(s).map(|v| v.is_null()).unwrap_or(true))
        .count() as u8;
    let prev = state.cached_empty_player_slots.swap(empty_count, Ordering::Relaxed);
    if prev != empty_count {
        debug!("[Inventory] Empty player slots: {} → {}", prev, empty_count);
    }
    // Auto-clear a stale inventory_full flag when inventory clearly has space.
    // This handles manual instasells, external trades, and any other action that
    // frees inventory without going through the bot's InstaSell flow.
    if empty_count >= MIN_FREE_SLOTS_FOR_BUY && state.inventory_full.load(Ordering::Relaxed) {
        info!("[Inventory] Clearing stale inventory_full flag — {} empty slots detected", empty_count);
        state.inventory_full.store(false, Ordering::Relaxed);
    }

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

/// Rebuild the cached GUI-window JSON so the web panel "Game View" tab always
/// shows the current state.  Called on:
///   - OpenScreen (new window opened)
///   - ContainerSetContent (window contents fully replaced)
///   - ContainerSetSlot (individual slot updated in current window)
///   - ContainerClose (window closed → clear the cache)
fn rebuild_cached_window_json(bot: &Client, state: &BotClientState) {
    let window_title = state.handlers.current_window_title();
    let window_id_opt = state.handlers.current_window_id();
    let bot_state = format!("{:?}", *state.bot_state.read());

    // If no window is open, store a "no window" JSON
    let (window_id, title) = match (window_id_opt, window_title) {
        (Some(id), Some(t)) => (id, t),
        _ => {
            let json = serde_json::json!({
                "open": false,
                "botState": bot_state,
                "windowId": null,
                "title": null,
                "slots": [],
            });
            if let Ok(s) = serde_json::to_string(&json) {
                *state.cached_window_json.write() = Some(s);
            }
            return;
        }
    };

    // Read ALL window slots (GUI slots + player inventory in the window)
    let menu = bot.menu();
    let all_slots = menu.slots();

    let mut slots_json: Vec<serde_json::Value> = Vec::with_capacity(all_slots.len());
    for (i, item) in all_slots.iter().enumerate() {
        if item.is_empty() {
            slots_json.push(serde_json::json!({
                "slot": i,
                "empty": true,
            }));
        } else {
            let display_name = get_item_display_name_from_slot(item).unwrap_or_default();
            let colored_name = get_item_display_name_with_colors_from_slot(item);
            let item_name = item.kind().to_string();

            // Extract SkyBlock tag for icon lookup
            let nbt_data = if let Some(item_data) = item.as_present() {
                extract_item_nbt_components(item_data)
            } else {
                serde_json::Value::Null
            };

            let tag = nbt_data.get("minecraft:custom_data")
                .and_then(|cd| {
                    cd.get("nbt")
                        .and_then(|n| n.get("ExtraAttributes"))
                        .and_then(|ea| ea.get("id"))
                        .and_then(|id| id.as_str())
                        .or_else(|| cd.get("ExtraAttributes").and_then(|ea| ea.get("id")).and_then(|id| id.as_str()))
                        .or_else(|| cd.get("nbt").and_then(|n| n.get("id")).and_then(|id| id.as_str()))
                        .or_else(|| cd.get("id").and_then(|id| id.as_str()))
                })
                .map(|s| s.to_string())
                .or_else(|| {
                    if let Some(item_data) = item.as_present() {
                        extract_skyblock_tag_from_custom_data(item_data)
                    } else {
                        None
                    }
                })
                .map(|t| resolve_pet_tag(&t, &nbt_data));

            let lore_lines = get_item_lore_with_colors_from_slot(item);

            let mut slot_obj = serde_json::json!({
                "slot": i,
                "empty": false,
                "name": item_name,
                "displayName": display_name,
                "count": item.count(),
            });
            if let Some(cn) = colored_name {
                slot_obj.as_object_mut().unwrap().insert("displayNameColored".to_string(), serde_json::Value::String(cn));
            }
            if let Some(ref t) = tag {
                slot_obj.as_object_mut().unwrap().insert("tag".to_string(), serde_json::Value::String(t.clone()));
            }
            if !lore_lines.is_empty() {
                slot_obj.as_object_mut().unwrap().insert("lore".to_string(),
                    serde_json::Value::Array(lore_lines.into_iter().map(serde_json::Value::String).collect()));
            }
            slots_json.push(slot_obj);
        }
    }

    let json = serde_json::json!({
        "open": true,
        "botState": bot_state,
        "windowId": window_id,
        "title": title,
        "slotCount": all_slots.len(),
        "slots": slots_json,
    });

    if let Ok(s) = serde_json::to_string(&json) {
        *state.cached_window_json.write() = Some(s);
    }
}

/// Parse auction slots from the "Manage Auctions" / "My Auctions" GUI window
/// and cache the result so the web panel can display active auctions without
/// calling the Hypixel or Coflnet APIs.
///
/// Each auction slot in Hypixel's "Manage Auctions" window shows:
/// - Display name: the item name (e.g. "§6Enchanted Diamond")
/// - Lore: contains bid/price info, time remaining, and status
///
/// We extract the item name, lore, tag, and parse bid/time/status from lore.
fn build_cached_my_auctions_json(slots: &[azalea_inventory::ItemStack], state: &BotClientState) {
    let mut auctions: Vec<serde_json::Value> = Vec::new();

    // Auction slots are typically in the first rows of the chest (slots 0-53).
    // Skip navigation items (like Close, Create Auction, page arrows, etc.)
    for item in slots.iter() {
        if item.is_empty() {
            continue;
        }
        let display_name = match get_item_display_name_from_slot(item) {
            Some(n) => n,
            None => continue,
        };
        let lore = get_item_lore_from_slot(item);
        if lore.is_empty() {
            continue;
        }
        // Colorful lore for web panel tooltip display
        let lore_colored = get_item_lore_with_colors_from_slot(item);

        let combined_lower = lore.join("\n").to_lowercase();

        // Skip GUI buttons/navigation items (no auction indicators at all)
        let is_auction_slot = combined_lower.contains("ends in")
            || combined_lower.contains("buy it now")
            || combined_lower.contains("starting bid")
            || combined_lower.contains("sold")
            || combined_lower.contains("expired")
            || contains_word_ended(&combined_lower)
            || combined_lower.contains("click to claim");
        if !is_auction_slot {
            continue;
        }

        // Determine status
        let status = if combined_lower.contains("sold!") || combined_lower.contains("click to claim") {
            "sold"
        } else if combined_lower.contains("expired") || contains_word_ended(&combined_lower) {
            "expired"
        } else {
            "active"
        };

        // Determine BIN vs Auction
        let bin = combined_lower.contains("buy it now");

        // Extract price from lore (Buy It Now: X coins / Starting bid: X coins / Top bid: X coins)
        let price = extract_price_from_lore(&lore);

        // Extract time remaining
        let time_remaining_secs = if status == "active" {
            extract_time_remaining_from_lore(&lore)
        } else {
            None
        };

        // Extract tag for icon lookup
        let tag = if let Some(item_data) = item.as_present() {
            let nbt_data = extract_item_nbt_components(item_data);
            let raw_tag = nbt_data.get("minecraft:custom_data")
                .and_then(|cd| {
                    cd.get("nbt")
                        .and_then(|n| n.get("ExtraAttributes"))
                        .and_then(|ea| ea.get("id"))
                        .and_then(|id| id.as_str())
                        .or_else(|| cd.get("ExtraAttributes").and_then(|ea| ea.get("id")).and_then(|id| id.as_str()))
                        .or_else(|| cd.get("nbt").and_then(|n| n.get("id")).and_then(|id| id.as_str()))
                        .or_else(|| cd.get("id").and_then(|id| id.as_str()))
                })
                .map(|s| s.to_string())
                .or_else(|| extract_skyblock_tag_from_custom_data(item_data));
            // Resolve pet-specific tags (PET → PET_MAMMOTH etc.)
            raw_tag.map(|t| resolve_pet_tag(&t, &nbt_data))
        } else {
            None
        };

        let mut entry = serde_json::json!({
            "item_name": remove_mc_colors(&display_name),
            "status": status,
            "bin": bin,
            "starting_bid": price.unwrap_or(0),
            "highest_bid": 0,
            "lore": lore_colored,
            "time_remaining_seconds": time_remaining_secs.unwrap_or(0),
        });

        // Add colored item name (with §-codes) for rarity-colored tooltip title
        if let Some(colored_name) = get_item_display_name_with_colors_from_slot(item) {
            entry.as_object_mut().unwrap().insert(
                "item_name_colored".to_string(),
                serde_json::Value::String(colored_name),
            );
        }

        if let Some(tag_str) = &tag {
            entry.as_object_mut().unwrap().insert("tag".to_string(), serde_json::Value::String(tag_str.clone()));
        }

        auctions.push(entry);
    }

    info!("[MyAuctions] Cached {} auction entries from Manage Auctions window", auctions.len());

    if let Ok(json_str) = serde_json::to_string(&auctions) {
        *state.cached_my_auctions_json.write() = Some(json_str);
    }
}

/// Extract a price value from lore lines.
/// Looks for patterns like "Buy It Now: 1,234,567 coins" or "Starting bid: 100 coins"
/// or "Top bid: 5,000 coins"
fn extract_price_from_lore(lore: &[String]) -> Option<i64> {
    for line in lore {
        let clean = remove_mc_colors(line).to_lowercase();
        // Match various price patterns
        for prefix in &["buy it now:", "starting bid:", "top bid:", "sold for:"] {
            if let Some(rest) = clean.strip_prefix(prefix) {
                let num_str: String = rest.chars()
                    .filter(|c| c.is_ascii_digit() || *c == ',')
                    .collect::<String>()
                    .replace(',', "");
                if let Ok(n) = num_str.parse::<i64>() {
                    return Some(n);
                }
            }
        }
    }
    None
}

/// Extract time remaining from lore lines.
/// Looks for patterns like "Ends in: 1d 2h 30m" or "Ends in: 45m 20s"
fn extract_time_remaining_from_lore(lore: &[String]) -> Option<i64> {
    static RE_DAYS: Lazy<regex::Regex> = Lazy::new(|| regex::Regex::new(r"(\d+)\s*d").unwrap());
    static RE_HOURS: Lazy<regex::Regex> = Lazy::new(|| regex::Regex::new(r"(\d+)\s*h").unwrap());
    static RE_MINS: Lazy<regex::Regex> = Lazy::new(|| regex::Regex::new(r"(\d+)\s*m(?:[^s]|$)").unwrap());
    static RE_SECS: Lazy<regex::Regex> = Lazy::new(|| regex::Regex::new(r"(\d+)\s*s").unwrap());

    for line in lore {
        let clean = remove_mc_colors(line).to_lowercase();
        if !clean.contains("ends in") {
            continue;
        }
        let mut total_secs: i64 = 0;
        if let Some(caps) = RE_DAYS.captures(&clean) {
            if let Some(d) = caps.get(1) {
                total_secs += d.as_str().parse::<i64>().unwrap_or(0) * 86400;
            }
        }
        if let Some(caps) = RE_HOURS.captures(&clean) {
            if let Some(h) = caps.get(1) {
                total_secs += h.as_str().parse::<i64>().unwrap_or(0) * 3600;
            }
        }
        if let Some(caps) = RE_MINS.captures(&clean) {
            if let Some(m) = caps.get(1) {
                total_secs += m.as_str().parse::<i64>().unwrap_or(0) * 60;
            }
        }
        if let Some(caps) = RE_SECS.captures(&clean) {
            if let Some(s) = caps.get(1) {
                total_secs += s.as_str().parse::<i64>().unwrap_or(0);
            }
        }
        if total_secs > 0 {
            return Some(total_secs);
        }
    }
    None
}

fn bazaar_order_log_path() -> std::path::PathBuf {
    match std::env::current_exe() {
        Ok(exe) => exe.parent().map(|p| p.join("bazaar_orders.log"))
            .unwrap_or_else(|| std::path::PathBuf::from("bazaar_orders.log")),
        Err(_) => std::path::PathBuf::from("bazaar_orders.log"),
    }
}

/// Persist a placed bazaar order for later stale-order checks in ManageOrders.
fn log_bazaar_order_placed(is_buy: bool, item_name: &str, total_value: f64) {
    use std::io::Write;
    let side = if is_buy { "buy" } else { "sell" };
    let normalized_item = normalize_bazaar_order_text(item_name);
    if normalized_item.is_empty() {
        return;
    }
    let line = format!("{}|{}|{}|{:.0}\n", chrono::Utc::now().timestamp(), side, normalized_item, total_value);
    let log_path = bazaar_order_log_path();
    match std::fs::OpenOptions::new().create(true).append(true).open(&log_path) {
        Ok(mut f) => {
            if let Err(e) = f.write_all(line.as_bytes()) {
                warn!("[ManageOrders] Failed to append bazaar_orders.log: {}", e);
            }
        }
        Err(e) => warn!("[ManageOrders] Failed to open bazaar_orders.log: {}", e),
    }
}

/// Returns (timestamp, total_value_coins) for the most recent matching logged order.
/// Old log entries without a value field default to 1_000_000.0 (1M coins) for backward compat.
fn last_logged_order_info(is_buy: bool, item_name: &str) -> Option<(i64, f64)> {
    let target_side = if is_buy { "buy" } else { "sell" };
    let target_item = normalize_bazaar_order_text(item_name);
    if target_item.is_empty() {
        return None;
    }
    let content = std::fs::read_to_string(bazaar_order_log_path()).ok()?;
    for line in content.lines().rev() {
        let mut parts = line.splitn(4, '|');
        let ts = parts.next()?.parse::<i64>().ok()?;
        let side = parts.next()?.trim();
        let item = parts.next()?.trim();
        if side == target_side && item == target_item {
            let total_value = parts.next()
                .and_then(|v| v.trim().parse::<f64>().ok())
                .unwrap_or(1_000_000.0); // backward compat: old entries default to 1M
            return Some((ts, total_value));
        }
    }
    None
}

#[cfg(test)]
fn last_logged_order_timestamp(is_buy: bool, item_name: &str) -> Option<i64> {
    last_logged_order_info(is_buy, item_name).map(|(ts, _)| ts)
}

/// Minimum age (seconds) an order must exist before it can be cancelled.
/// Cancelling orders that have only existed for a minute or two is wasteful —
/// give them at least 5 minutes to fill.
const MIN_ORDER_AGE_BEFORE_CANCEL_SECS: u64 = 300; // 5 minutes

fn should_cancel_open_order_due_to_age(order_identity: Option<(bool, String)>, cancel_minutes_per_million: u64) -> bool {
    if cancel_minutes_per_million == 0 {
        return false;
    }
    let (is_buy, item_name) = match order_identity {
        Some(identity) => identity,
        None => return false,
    };
    let (last_logged, total_value) = match last_logged_order_info(is_buy, &item_name) {
        Some(info) => info,
        // No log entry for this order — it was placed before tracking started
        // or the log was cleared.  We cannot determine the age, so err on the
        // side of caution and leave the order alone.  It will be cleaned up
        // on the next startup (cancel_open mode) or when the log is populated.
        None => return false,
    };
    let now = chrono::Utc::now().timestamp();
    let age_secs = if now > last_logged {
        (now - last_logged) as u64
    } else {
        0
    };
    // Never cancel orders younger than 5 minutes — give them time to fill.
    if age_secs < MIN_ORDER_AGE_BEFORE_CANCEL_SECS {
        return false;
    }
    // Scale cancel timeout by total order value: minutes_per_million * (total_value / 1_000_000)
    // Floor at 1.0M so orders under 1M coins still get the base cancel_minutes_per_million
    // timeout (e.g. a 100K order with 5m/M → 5 min, not 0.5 min).
    let millions = (total_value / 1_000_000.0).max(1.0);
    let cancel_secs = (cancel_minutes_per_million as f64 * millions * 60.0) as u64;
    age_secs >= cancel_secs
}

/// Returns true if the order has a known placement time that is less than
/// `MIN_ORDER_AGE_BEFORE_CANCEL_SECS` ago.  Used to protect recently-placed
/// orders from cancellation even in cancel_open (startup) mode.
/// Returns false when the identity is unknown or no log entry exists (cannot
/// determine age → assume old enough).
fn is_order_below_min_cancel_age(order_identity: &Option<(bool, String)>) -> bool {
    let (is_buy, item_name) = match order_identity {
        Some(ref id) => (id.0, id.1.as_str()),
        None => return false,
    };
    match last_logged_order_info(is_buy, item_name) {
        Some((last_logged, _)) => {
            let now = chrono::Utc::now().timestamp();
            let age_secs = if now > last_logged { (now - last_logged) as u64 } else { 0 };
            age_secs < MIN_ORDER_AGE_BEFORE_CANCEL_SECS
        }
        None => false,
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

/// Count the number of empty (air) slots in the player's inventory (36 slots).
/// Used to verify whether inventory is actually full before skipping BUY orders.
fn count_empty_player_slots(bot: &Client) -> usize {
    let menu = bot.menu();
    let all_slots = menu.slots();
    let player_range = menu.player_slots_range();
    let player_slots = &all_slots[player_range];
    player_slots.iter().filter(|s| s.is_empty()).count()
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

/// Click a window slot via raw TCP, matching the transport used by
/// `send_raw_close` so that click → close ordering is preserved on the wire.
/// Previously this used `bot.write_packet()` (ECS trigger pipeline) which
/// queued the click *behind* an `apply_deferred` boundary, while
/// `send_raw_close` wrote directly to TCP — causing the close to reach the
/// server before the click, silently discarding Claim All / Collect actions.
async fn click_window_slot(bot: &Client, last_window_id: &Arc<RwLock<u8>>, window_id: u8, slot: i16) {
    // Window 0 is the player's own inventory — always valid, no container guard.
    // For all GUI containers, refuse to click if a newer window has replaced this one;
    // clicking a closed/stale container is physically impossible for a real player and
    // is a known Hypixel anti-cheat trigger.
    if window_id != 0 && *last_window_id.read() != window_id {
        warn!(
            "Blocked stale window click: attempted slot {} in window {} but current window is {}",
            slot, window_id, *last_window_id.read()
        );
        return;
    }
    send_raw_click(bot, window_id, slot);
}

/// Send a chat command via `write_packet(ServerboundChatCommand)`, which uses
/// the ECS trigger/observer path.  This is fine for non-critical commands.
///
/// `content` should include the leading `/` (e.g. `"/viewauction <uuid>"`).
/// The function strips it before putting the command string into the packet.
fn send_chat_command(bot: &Client, content: &str) {
    let command = content.strip_prefix('/').unwrap_or_else(|| {
        debug!("send_chat_command called without leading '/' — sending as-is: {}", content);
        content
    });
    bot.write_packet(ServerboundChatCommand {
        command: command.to_string(),
    });
}

/// Send a chat command directly to the TCP socket via `with_raw_connection_mut`,
/// completely bypassing the ECS command queue and trigger/observer pipeline.
///
/// `write_packet` queues a Bevy Trigger that is applied during `apply_deferred`,
/// meaning the packet only reaches TCP when the ECS schedule processes commands.
/// By writing to `RawConnection` directly, the packet is serialized and pushed
/// into the TCP write buffer **immediately**, shaving off the entire ECS cycle
/// overhead.  Use this for time-critical packets like `/viewauction`, buy clicks,
/// and confirm clicks on the auction purchase path.
///
/// `content` should include the leading `/` (e.g. `"/viewauction <uuid>"`).
fn send_raw_chat_command(bot: &Client, content: &str) {
    let command = content.strip_prefix('/').unwrap_or_else(|| {
        debug!("send_raw_chat_command called without leading '/' — sending as-is: {}", content);
        content
    });
    let cmd_packet = ServerboundChatCommand {
        command: command.to_string(),
    };
    bot.with_raw_connection_mut(|mut raw_conn| {
        if let Err(e) = raw_conn.write(cmd_packet) {
            error!("raw chat command write failed: {e}");
        }
    });
}

/// Send a container click packet directly to the TCP socket via
/// `with_raw_connection_mut`, bypassing the ECS command queue.
///
/// Used for time-critical slot clicks (buy button, confirm button) on the
/// auction purchase path where every millisecond matters.
fn send_raw_click(bot: &Client, window_id: u8, slot: i16) {
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
    bot.with_raw_connection_mut(|mut raw_conn| {
        if let Err(e) = raw_conn.write(packet) {
            error!("raw click write failed (window {} slot {}): {e}", window_id, slot);
        }
    });
    info!("Raw-clicked slot {} in window {}", slot, window_id);
}

/// Send a container close packet directly to the TCP socket via
/// `with_raw_connection_mut`, bypassing the ECS command queue.
///
/// The ECS trigger pipeline (`write_packet`) can silently fail to deliver
/// `ServerboundContainerClose` when the schedule is busy, leaving windows
/// stuck open indefinitely.  Writing directly to TCP guarantees the close
/// packet reaches the server immediately, matching the reliability of
/// `send_raw_click` and `send_raw_chat_command`.
///
/// Also eagerly clears the local window tracking state (`current_window_id`,
/// `current_window_title`, `current_window_type`) so that subsequent code
/// never sees a stale window ID.  Hypixel does not always respond with a
/// `ClientboundContainerClose` packet, so relying on the server response to
/// clear client-side state left windows "stuck open" and forced SafeClose to
/// fire on the next command.  Clearing eagerly is harmless: if the server
/// does send `ClientboundContainerClose`, the `ContainerClose` handler
/// re-clears the already-`None` fields.
fn send_raw_close(bot: &Client, window_id: u8, handlers: &BotEventHandlers) {
    bot.with_raw_connection_mut(|mut raw_conn| {
        if let Err(e) = raw_conn.write(ServerboundContainerClose {
            container_id: window_id as i32,
        }) {
            error!("raw container close write failed (window {}): {e}", window_id);
        }
    });
    handlers.clear_window_tracking();
    debug!("Raw-closed window {}", window_id);
}

/// After clicking a Cancel button in a bazaar order management window, wait for
/// confirmation that the server processed the cancellation.  Returns `true` when
/// the Cancel button disappears from the window (or the window itself changes),
/// `false` on timeout (meaning the cancel click was likely not processed).
async fn wait_for_cancel_confirmation(
    bot: &Client,
    last_window_id: &Arc<RwLock<u8>>,
    window_id: u8,
) -> bool {
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(ORDER_ACTION_CONFIRMATION_TIMEOUT_SECS);
    loop {
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
        // Window changed — server processed the action
        if *last_window_id.read() != window_id {
            return true;
        }
        // Cancel button gone — cancellation confirmed
        let slots = bot.menu().slots();
        if find_slot_by_name(&slots, "Cancel").is_none() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
    }
}

/// After clicking a Collect button in a bazaar order management window, wait for
/// confirmation that the server processed the collection.  Returns `true` when
/// the Collect button disappears from the window (or the window itself changes),
/// `false` on timeout (meaning the collect click was likely not processed).
async fn wait_for_collect_confirmation(
    bot: &Client,
    last_window_id: &Arc<RwLock<u8>>,
    window_id: u8,
) -> bool {
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(ORDER_ACTION_CONFIRMATION_TIMEOUT_SECS);
    loop {
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
        // Window changed — server processed the action
        if *last_window_id.read() != window_id {
            return true;
        }
        // Collect button gone — collection confirmed
        let slots = bot.menu().slots();
        if find_slot_by_name(&slots, "Collect").is_none()
            && find_slot_by_name(&slots, "Claim").is_none()
        {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
    }
}

/// Check the internal ManageOrders deadline.  If exceeded, close the current window
/// and return the bot to Idle so the command queue isn't blocked for the full 60-second
/// external timeout.  Returns `true` when the deadline was hit (caller should `return`).
fn check_manage_orders_deadline(
    bot: &Client,
    state: &BotClientState,
    window_id: u8,
) -> bool {
    if let Some(deadline) = *state.manage_orders_deadline.read() {
        if tokio::time::Instant::now() >= deadline {
            warn!("[ManageOrders] Internal deadline exceeded — closing window and going Idle");
            send_raw_close(bot, window_id, &state.handlers);
            *state.manage_orders_deadline.write() = None;
            // Clear the order-limit flag so new flips aren't blocked after timeout.
            state.bazaar_at_limit.store(false, Ordering::Relaxed);
            *state.bot_state.write() = BotState::Idle;
            return true;
        }
    }
    // Also abort if AH flips are incoming — ManageOrders would block the
    // AH purchase flow.  The order will be re-queued after the AH pause.
    if state.bazaar_flips_paused.load(Ordering::Relaxed) {
        info!("[ManageOrders] AH flips incoming — aborting ManageOrders to free queue");
        send_raw_close(bot, window_id, &state.handlers);
        *state.manage_orders_deadline.write() = None;
        state.bazaar_at_limit.store(false, Ordering::Relaxed);
        *state.bot_state.write() = BotState::Idle;
        return true;
    }
    false
}

/// Close the current window and re-navigate to /bz so the ManagingOrders flow
/// can continue from a clean state.  A delay between the close and the
/// chat command gives the server time to process the ContainerClose before the
/// new /bz command arrives, and avoids "Sending packets too fast!" kicks.
/// Raised from 500 → 800 ms to reduce kick frequency under heavy GUI cycling.
async fn close_window_and_reopen_bz(
    bot: &Client,
    state: &BotClientState,
    window_id: u8,
) {
    if *state.last_window_id.read() == window_id {
        send_raw_close(bot, window_id, &state.handlers);
        tokio::time::sleep(tokio::time::Duration::from_millis(800)).await;
        send_chat_command(bot, "/bz");
    }
}

/// If the auction item preview slot (slot 13) in "Create BIN Auction" is already
/// occupied by a real item (e.g. from a previously failed auction creation or a
/// purchased item auto-placed by the server), click it to return the stuck item
/// to inventory so our item can be placed without triggering "You already have an
/// item in the auction slot!".
///
/// Glass panes and other GUI filler/decoration items are ignored — Hypixel uses
/// them as placeholder visuals and clicking them is a no-op that wastes time.
async fn clear_auction_preview_slot(
    bot: &Client,
    state: &BotClientState,
    window_id: u8,
    slots: &[azalea_inventory::ItemStack],
) {
    if slots.len() > 13 && !slots[13].is_empty() {
        let kind = slots[13].kind().to_string().to_lowercase();
        if kind.contains("glass_pane") || kind.contains("barrier") || kind.contains("stone_button") {
            debug!("[Auction] Slot 13 is GUI filler ({}) — not a stuck item, skipping", kind);
            return;
        }
        warn!("[Auction] Slot 13 already occupied ({}) — clicking to clear stuck item before placing ours", kind);
        click_window_slot(bot, &state.last_window_id, window_id, 13).await;
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    }
}

/// Click a window slot while reporting the item currently on the cursor.
/// Used when the cursor already holds an item from a previous pick-up click,
/// so the server receives the correct `carried_item` and processes the interaction
/// (e.g. placing the item in the auction item-slot to trigger the price sign).
///
/// Uses raw TCP (matching `click_window_slot` and `send_raw_close`) so that
/// click → close ordering is preserved on the wire.
async fn click_window_slot_carrying(
    bot: &Client,
    last_window_id: &Arc<RwLock<u8>>,
    window_id: u8,
    slot: i16,
    carried: &azalea_inventory::ItemStack,
) {
    // Same stale-window guard as click_window_slot.
    if window_id != 0 && *last_window_id.read() != window_id {
        warn!(
            "Blocked stale window click (carrying): attempted slot {} in window {} but current window is {}",
            slot, window_id, *last_window_id.read()
        );
        return;
    }
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

    bot.with_raw_connection_mut(|mut raw_conn| {
        if let Err(e) = raw_conn.write(packet) {
            error!("raw click (carrying) write failed (window {} slot {}): {e}", window_id, slot);
        }
    });
    info!("Raw-clicked slot {} in window {} (carrying item)", slot, window_id);
}

/// Shared startup workflow: cancel old orders, claim sold items, then emit StartupComplete.
/// Called from both the chat-based detection path and the 30-second watchdog.
/// Matches TypeScript BAF.ts `runStartupWorkflow` all 4 steps.
///
/// Commands are enqueued through the shared CommandQueue so they go through
/// `execute_command`, which handles all required initialisation (deadlines,
/// processed-set clearing, SafeClose of stale windows, etc.).
async fn run_startup_workflow(
    _bot: Client,
    bot_state: Arc<RwLock<BotState>>,
    event_tx: tokio::sync::mpsc::UnboundedSender<BotEvent>,
    manage_orders_cancelled: Arc<RwLock<u64>>,
    auto_cookie_hours: Arc<RwLock<u64>>,
    command_queue: Arc<RwLock<Option<CommandQueue>>>,
    startup_in_progress: Arc<AtomicBool>,
    enable_bazaar_flips: Arc<AtomicBool>,
) {
    use crate::types::{CommandType, CommandPriority};

    // Prevent duplicate startup runs.
    if startup_in_progress.swap(true, Ordering::SeqCst) {
        debug!("[Startup] Workflow already running, skipping duplicate start");
        return;
    }

    // Do not run startup steps while another interactive flow is active.
    // Wait briefly for idle/grace period; abort if the bot stays busy.
    let entry_deadline = tokio::time::Instant::now()
        + tokio::time::Duration::from_secs(STARTUP_ENTRY_TIMEOUT_SECS);
    loop {
        let current_state = *bot_state.read();
        if matches!(current_state, BotState::GracePeriod | BotState::Idle) {
            break;
        }
        if tokio::time::Instant::now() >= entry_deadline {
            warn!("[Startup] Skipping startup workflow: bot stayed busy in state {:?}", current_state);
            startup_in_progress.store(false, Ordering::Relaxed);
            return;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(250)).await;
    }

    // Grab a clone of the command queue. If none is set (unit tests / standalone),
    // falling back to the legacy direct-drive approach would be complex, so we just
    // skip — the queue is always set in production via main.rs.
    let queue = {
        let guard = command_queue.read();
        match guard.as_ref() {
            Some(q) => q.clone(),
            None => {
                warn!("[Startup] No command queue available — skipping queue-based startup");
                startup_in_progress.store(false, Ordering::Relaxed);
                return;
            }
        }
    };

    info!("╔══════════════════════════════════════╗");
    info!("║        BAF Startup Workflow          ║");
    info!("╚══════════════════════════════════════╝");

    // Helper: enqueue a command and wait until the queue processor completes it.
    // Returns true if the command completed, false on timeout.
    async fn await_queued_command(
        queue: &CommandQueue,
        _bot_state: &Arc<RwLock<BotState>>,
        cmd_type: CommandType,
        timeout_secs: u64,
    ) -> bool {
        let cmd_id = queue.enqueue(cmd_type, CommandPriority::Critical, false);
        let deadline = tokio::time::Instant::now()
            + tokio::time::Duration::from_secs(timeout_secs);
        loop {
            tokio::time::sleep(tokio::time::Duration::from_millis(250)).await;
            // Command has been fully processed by the queue processor
            if !queue.contains_command_id(&cmd_id) {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                warn!("[Startup] Timed out waiting for command {:?}", cmd_id);
                return false;
            }
        }
    }

    // Step 1/4: Cookie check
    let cookie_hours = *auto_cookie_hours.read();
    if cookie_hours > 0 {
        info!("[Startup] Step 1/4: Checking cookie status...");
        let _ = event_tx.send(BotEvent::ChatMessage(
            "§f[§4BAF§f]: §7[Startup] §bStep 1/4: §fChecking cookie status...".to_string()
        ));
        await_queued_command(&queue, &bot_state, CommandType::CheckCookie, 60).await;
        info!("[Startup] Step 1/4: Cookie check complete");
        let _ = event_tx.send(BotEvent::ChatMessage(
            "§f[§4BAF§f]: §a[Startup] Cookie check complete".to_string()
        ));
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    } else {
        info!("[Startup] Step 1/4: Cookie check skipped (AUTO_COOKIE=0)");
    }

    // Step 2/4: Bazaar order management.
    //
    // Only do the *destructive* startup cancel ("clean slate") when the bazaar
    // finder is actually enabled.  When bazaar flips are disabled the user may
    // have orders they placed manually or want to keep — cancelling them all on
    // startup "could end bad".  In that case we run a non-destructive
    // collect-only pass: filled orders are still claimed and open orders are
    // discovered/tracked for the web panel, but nothing is cancelled.  Once a
    // flip actually comes in, the order-filled path will trigger management.
    let cancel_open = enable_bazaar_flips.load(Ordering::Relaxed);
    let mode_str = if cancel_open { "cancel + collect" } else { "collect-only (bz finder disabled)" };
    info!("[Startup] Step 2/4: Managing bazaar orders (startup: {})...", mode_str);
    await_queued_command(
        &queue,
        &bot_state,
        CommandType::ManageOrders { cancel_open, target_item: None },
        50,
    ).await;
    let orders_cancelled = *manage_orders_cancelled.read();
    info!("[Startup] Step 2/4: Order management complete — {} order(s) cancelled", orders_cancelled);

    // Step 3/4: Claim sold items
    info!("[Startup] Step 3/4: Claiming sold items...");
    await_queued_command(&queue, &bot_state, CommandType::ClaimSoldItem, 60).await;

    // Step 3b/4: Claim purchased items
    info!("[Startup] Step 3b/4: Claiming purchased items (Your Bids)...");
    await_queued_command(&queue, &bot_state, CommandType::ClaimPurchasedItem, 60).await;

    // Ensure Idle before emitting completion
    *bot_state.write() = BotState::Idle;

    // Step 4/4: Emit StartupComplete — main.rs requests bazaar flips and sends webhook
    info!("[Startup] Step 4/4: Startup complete - bot is ready to flip!");
    startup_in_progress.store(false, Ordering::Relaxed);
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

#[cfg(test)]
fn parse_claimed_sold_event_from_lore(item_name: &str, lore: &[String]) -> Option<(String, u64, String)> {
    if lore.is_empty() {
        return None;
    }
    let combined = lore.join("\n");
    let combined_lower = combined.to_lowercase();
    let sold_status = (combined_lower.contains("status:") && combined_lower.contains("sold"))
        || combined_lower.contains("sold for");
    if !sold_status {
        return None;
    }

    let price_caps = SOLD_FOR_PRICE_RE.captures(&combined)?;
    let price_match = price_caps.get(1)?;
    let price: u64 = price_match.as_str().replace(',', "").trim().parse().ok()?;

    let buyer = SOLD_BUYER_RE
        .captures(&combined)
        .and_then(|caps| caps.get(1).map(|m| m.as_str().trim().to_string()))
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "Unknown".to_string());

    Some((item_name.to_string(), price, buyer))
}

/// Extract UUID from a message that might contain "/viewauction <UUID>".
/// Works in both plain-text context (UUID ends at whitespace) and JSON context
/// (UUID ends at `"` after the value string, e.g. from a serialized clickEvent).
/// Minecraft UUIDs consist only of hex digits and dashes, so `"` is never a valid
/// UUID character — using it as a terminator is unconditionally safe.
fn extract_viewauction_uuid(msg: &str) -> Option<String> {
    let idx = msg.find("/viewauction ")?;
    let rest = &msg[idx + 13..];
    let end = rest.find(|c: char| c.is_whitespace() || c == '"').unwrap_or(rest.len());
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
    fn test_parse_bazaar_filled_buy_order() {
        let msg = "[Bazaar] Your Buy Order for Kuudra Mandible was filled!";
        let result = parse_bazaar_filled_notification(msg);
        assert_eq!(result, Some(("Kuudra Mandible".to_string(), true)));
    }

    #[test]
    fn test_parse_bazaar_filled_sell_offer() {
        let msg = "[Bazaar] Your Sell Offer for Perfect Ruby Gemstone was filled!";
        let result = parse_bazaar_filled_notification(msg);
        assert_eq!(result, Some(("Perfect Ruby Gemstone".to_string(), false)));
    }

    #[test]
    fn test_parse_bazaar_filled_no_match() {
        let msg = "[Bazaar] Order placed successfully!";
        let result = parse_bazaar_filled_notification(msg);
        assert_eq!(result, None);
    }

    #[test]
    fn test_parse_bazaar_filled_not_bazaar() {
        let msg = "Your Buy Order for Diamond was filled!";
        let result = parse_bazaar_filled_notification(msg);
        assert_eq!(result, None);
    }

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
    fn test_parse_claimed_sold_event_from_lore_with_buyer() {
        let lore = vec![
            "Status: Sold!".to_string(),
            "Sold for: 45,000,000 coins".to_string(),
            "Buyer: SomePlayer".to_string(),
            "Click to claim".to_string(),
        ];
        let result = parse_claimed_sold_event_from_lore("Gemstone Fuel Tank", &lore);
        assert_eq!(
            result,
            Some(("Gemstone Fuel Tank".to_string(), 45_000_000, "SomePlayer".to_string()))
        );
    }

    #[test]
    fn test_parse_claimed_sold_event_from_lore_without_buyer() {
        let lore = vec![
            "Status: Sold!".to_string(),
            "Sold for 1,000,000 coins".to_string(),
        ];
        let result = parse_claimed_sold_event_from_lore("Golden Pickaxe", &lore);
        assert_eq!(
            result,
            Some(("Golden Pickaxe".to_string(), 1_000_000, "Unknown".to_string()))
        );
    }

    #[test]
    fn test_parse_claimed_sold_event_from_lore_non_sold() {
        let lore = vec![
            "Status: Active".to_string(),
            "Ends in: 5m".to_string(),
        ];
        let result = parse_claimed_sold_event_from_lore("Golden Pickaxe", &lore);
        assert!(result.is_none());
    }

    #[test]
    fn test_extract_viewauction_uuid() {
        let msg = "click /viewauction 26e353e9556a4b9791f5e03710ddc505 to view";
        let result = extract_viewauction_uuid(msg);
        assert_eq!(result, Some("26e353e9556a4b9791f5e03710ddc505".to_string()));
    }

    #[test]
    fn test_extract_viewauction_uuid_json_context() {
        // Simulates the JSON representation of a Hypixel sold-auction chat message where
        // the UUID sits inside a clickEvent value string, terminated by a JSON quote.
        let json = r#"{"text":"[Auction] Buyer bought Item for 1,000 coins ","extra":[{"text":"CLICK","clickEvent":{"action":"run_command","value":"/viewauction abc123def456"}}]}"#;
        let result = extract_viewauction_uuid(json);
        assert_eq!(result, Some("abc123def456".to_string()));
    }

    #[test]
    fn test_extract_viewauction_uuid_end_of_string() {
        // UUID at the very end of the string (no trailing delimiter)
        let msg = "/viewauction myuuid";
        let result = extract_viewauction_uuid(msg);
        assert_eq!(result, Some("myuuid".to_string()));
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
        assert!(is_terminal_purchase_failure_message("You don't have the space required to claim that!"));
        assert!(is_terminal_purchase_failure_message("Your inventory is full!"));
        assert!(!is_terminal_purchase_failure_message("Putting coins in escrow..."));
    }

    #[test]
    fn test_is_my_auctions_window_title() {
        assert!(is_my_auctions_window_title("Manage Auctions"));
        assert!(is_my_auctions_window_title("My Auctions"));
        assert!(!is_my_auctions_window_title("Auction House"));
    }

    #[test]
    fn test_bazaar_orders_window_title_variants() {
        assert!(is_bazaar_orders_window_title("Manage Orders"));
        assert!(is_bazaar_orders_window_title("Your Orders"));
        assert!(is_bazaar_orders_window_title("Your Bazaar Orders"));
        assert!(!is_bazaar_orders_window_title("Bazaar"));
    }

    #[test]
    fn test_bazaar_order_entry_name_variants() {
        assert!(is_bazaar_order_entry_name("BUY ENCHANTED DIAMOND"));
        assert!(is_bazaar_order_entry_name("SELL ENCHANTED DIAMOND"));
        assert!(is_bazaar_order_entry_name("Buy Order: ENCHANTED DIAMOND"));
        assert!(is_bazaar_order_entry_name("Sell Offer: ENCHANTED DIAMOND"));
        assert!(is_bazaar_order_entry_name("buy order: enchanted diamond"));
        assert!(is_bazaar_order_entry_name("sell offer: enchanted diamond"));
        assert!(!is_bazaar_order_entry_name("Buy OrderX ENCHANTED DIAMOND"));
        assert!(!is_bazaar_order_entry_name("Sell OfferY ENCHANTED DIAMOND"));
        assert!(!is_bazaar_order_entry_name("Booster Cookie"));
    }

    #[test]
    fn test_is_buy_bazaar_order_name_variants() {
        assert!(is_buy_bazaar_order_name("BUY ENCHANTED DIAMOND"));
        assert!(is_buy_bazaar_order_name("Buy Order: ENCHANTED DIAMOND"));
        assert!(!is_buy_bazaar_order_name("SELL ENCHANTED DIAMOND"));
        assert!(!is_buy_bazaar_order_name("Sell Offer: ENCHANTED DIAMOND"));
    }

    #[test]
    fn test_is_order_claimable_from_lore_fully_filled() {
        let lore = vec![
            "Buy Order".to_string(),
            "Order amount: 64x".to_string(),
            "Filled: 64/64 100%!".to_string(),
            "Price per unit: 1,958.0 coins".to_string(),
            "You have 64 items to claim!".to_string(),
            "Click to claim!".to_string(),
        ];
        assert!(is_order_claimable_from_lore(&lore));
    }

    #[test]
    fn test_is_order_claimable_from_lore_open_order() {
        let lore = vec![
            "Buy Order".to_string(),
            "Order amount: 64x".to_string(),
            "Filled: 0/64 0%".to_string(),
            "Price per unit: 1,000.0 coins".to_string(),
        ];
        assert!(!is_order_claimable_from_lore(&lore));
    }

    #[test]
    fn test_is_order_claimable_from_lore_partially_filled() {
        let lore = vec![
            "Buy Order".to_string(),
            "Order amount: 64x".to_string(),
            "Filled: 32/64 50%".to_string(),
            "You have 32 items to claim!".to_string(),
        ];
        assert!(is_order_claimable_from_lore(&lore));
    }

    #[test]
    fn test_is_order_claimable_from_lore_with_color_codes() {
        let lore = vec![
            "§7Buy Order".to_string(),
            "§7Filled: §a64/64 §6100%!".to_string(),
            "§aClick to claim!".to_string(),
        ];
        assert!(is_order_claimable_from_lore(&lore));
    }

    #[test]
    fn test_is_order_claimable_from_lore_empty() {
        let lore: Vec<String> = vec![];
        assert!(!is_order_claimable_from_lore(&lore));
    }

    #[test]
    fn test_parse_filled_amount_fully_filled() {
        let lore = vec![
            "Buy Order".to_string(),
            "Filled: 64/64 100%!".to_string(),
        ];
        assert_eq!(parse_filled_amount_from_lore(&lore), Some((64, 64)));
    }

    #[test]
    fn test_parse_filled_amount_partially_filled() {
        let lore = vec![
            "Buy Order".to_string(),
            "Order amount: 64x".to_string(),
            "Filled: 1/4 25%".to_string(),
            "You have 1 items to claim!".to_string(),
        ];
        assert_eq!(parse_filled_amount_from_lore(&lore), Some((1, 4)));
    }

    #[test]
    fn test_parse_filled_amount_with_color_codes() {
        let lore = vec![
            "§7Buy Order".to_string(),
            "§7Filled: §a32/64 §650%".to_string(),
        ];
        assert_eq!(parse_filled_amount_from_lore(&lore), Some((32, 64)));
    }

    #[test]
    fn test_parse_filled_amount_zero() {
        let lore = vec![
            "Filled: 0/64 0%".to_string(),
        ];
        assert_eq!(parse_filled_amount_from_lore(&lore), Some((0, 64)));
    }

    #[test]
    fn test_parse_filled_amount_no_filled_line() {
        let lore = vec![
            "Buy Order".to_string(),
            "Price per unit: 1,000.0 coins".to_string(),
        ];
        assert_eq!(parse_filled_amount_from_lore(&lore), None);
    }

    #[test]
    fn test_parse_filled_amount_large_numbers() {
        let lore = vec![
            "Filled: 1,280/2,560 50%".to_string(),
        ];
        assert_eq!(parse_filled_amount_from_lore(&lore), Some((1280, 2560)));
    }

    #[test]
    fn test_parse_bazaar_order_identity_from_name_variants() {
        assert_eq!(
            parse_bazaar_order_identity_from_name("BUY Enchanted Diamond"),
            Some((true, "enchanted diamond".to_string()))
        );
        assert_eq!(
            parse_bazaar_order_identity_from_name("Sell Offer: Hyper Catalyst"),
            Some((false, "hyper catalyst".to_string()))
        );
        assert_eq!(parse_bazaar_order_identity_from_name("Buy Order"), None);
    }

    #[test]
    fn test_parse_bazaar_order_identity_from_lore() {
        let lore = vec![
            "Buy Order".to_string(),
            "Product: Enchanted Diamond".to_string(),
            "Amount: 1,024".to_string(),
        ];
        assert_eq!(
            parse_bazaar_order_identity("Buy Order", &lore),
            Some((true, "enchanted diamond".to_string()))
        );
    }

    #[test]
    fn test_should_treat_as_bazaar_order_slot_with_lore_identity_only() {
        let lore = vec![
            "§7Status: §aOpen".to_string(),
            "§7Sell Offer".to_string(),
            "§7Product: Booster Cookie".to_string(),
        ];
        let identity = parse_bazaar_order_identity("Booster Cookie", &lore);
        assert!(should_treat_as_bazaar_order_slot("Booster Cookie", identity.as_ref()));
    }

    #[test]
    fn test_should_treat_as_bazaar_order_slot_variants() {
        let lore_only = vec![
            "Status: Open".to_string(),
            "Buy Order".to_string(),
            "Product: Enchanted Diamond".to_string(),
        ];
        let lore_identity = parse_bazaar_order_identity("Enchanted Diamond", &lore_only);
        assert!(should_treat_as_bazaar_order_slot("Enchanted Diamond", lore_identity.as_ref()));
        assert!(should_treat_as_bazaar_order_slot("Buy Order: Enchanted Diamond", None));
        assert!(!should_treat_as_bazaar_order_slot("Booster Cookie", None));
    }

    #[test]
    fn test_lore_contains_phrase_case_insensitive() {
        let lore = vec![
            "§7Click to Cancel Order".to_string(),
            "§7Status: §aOpen".to_string(),
        ];
        assert!(lore_contains_phrase(&lore, "click to cancel"));
        assert!(lore_contains_phrase(&lore, "CANCEL ORDER"));
        assert!(!lore_contains_phrase(&lore, "collect"));
    }

    #[test]
    fn test_bazaar_order_placed_log_roundtrip() {
        let item_name = format!(
            "unit_test_item_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        log_bazaar_order_placed(true, &item_name, 5_000_000.0);
        let last = last_logged_order_timestamp(true, &item_name);
        assert!(last.is_some(), "placed order should be present in bazaar_orders.log");
    }

    #[test]
    fn test_is_order_below_min_cancel_age_none_identity() {
        // Unknown identity → cannot determine age → not too young
        assert!(!is_order_below_min_cancel_age(&None));
    }

    #[test]
    fn test_is_order_below_min_cancel_age_no_log_entry() {
        // Order with no log entry → cannot determine age → not too young
        let identity = Some((true, "nonexistent_item_xyz_9999".to_string()));
        assert!(!is_order_below_min_cancel_age(&identity));
    }

    #[test]
    fn test_is_order_below_min_cancel_age_recently_placed() {
        // Log an order just now — it should be below the min cancel age
        let item_name = format!(
            "unit_test_young_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        log_bazaar_order_placed(true, &item_name, 1_000_000.0);
        let identity = Some((true, item_name));
        assert!(is_order_below_min_cancel_age(&identity), "just-placed order should be below min cancel age");
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

    #[test]
    fn test_cookie_step_enum_has_consuming_cookie() {
        // Verify the ConsumingCookie step exists and is distinct from other steps
        let step = CookieStep::ConsumingCookie;
        assert_ne!(step, CookieStep::Initial);
        assert_ne!(step, CookieStep::ItemDetail);
        assert_ne!(step, CookieStep::BuyConfirm);
        assert_ne!(step, CookieStep::WaitingForCookie);
    }

    #[test]
    fn test_cookie_step_waiting_for_cookie_is_distinct() {
        // WaitingForCookie must be distinct from all other steps so the atomic
        // check-and-advance in BuyConfirm and ConsumingCookie works correctly.
        let step = CookieStep::WaitingForCookie;
        assert_ne!(step, CookieStep::Initial);
        assert_ne!(step, CookieStep::ItemDetail);
        assert_ne!(step, CookieStep::BuyConfirm);
        assert_ne!(step, CookieStep::ConsumingCookie);
    }

    #[test]
    fn test_inventory_item_name_uses_minecraft_prefix() {
        // Verify that ItemKind::to_string() produces minecraft: prefixed names
        let kind = ItemKind::PlayerHead;
        let name = kind.to_string();
        assert!(name.starts_with("minecraft:"), "Expected minecraft: prefix, got: {}", name);
        assert_eq!(name, "minecraft:player_head");
    }

    #[test]
    fn test_inventory_item_name_stone() {
        let kind = ItemKind::Stone;
        let name = kind.to_string();
        assert_eq!(name, "minecraft:stone");
    }

    #[test]
    fn test_coop_sale_filtering_our_listing() {
        // Simulate tracking an active listing
        let listings: std::collections::HashSet<String> = {
            let mut set = std::collections::HashSet::new();
            set.insert("gemstone fuel tank".to_string());
            set
        };

        let msg = "[Auction] SomePlayer bought Gemstone Fuel Tank for 45,000,000 coins!";
        let result = parse_sold_message(msg);
        assert!(result.is_some());
        let (_, item_name, _) = result.unwrap();
        let item_key = crate::bot::handlers::BotEventHandlers::remove_color_codes(&item_name).to_lowercase();
        assert!(listings.contains(&item_key), "Our listing should be detected");
    }

    #[test]
    fn test_coop_sale_filtering_not_our_listing() {
        // Simulate tracking active listings (we didn't list "Golden Pickaxe")
        let listings: std::collections::HashSet<String> = {
            let mut set = std::collections::HashSet::new();
            set.insert("gemstone fuel tank".to_string());
            set
        };

        let msg = "[Auction] OtherPlayer bought Golden Pickaxe for 1,000,000 coins!";
        let result = parse_sold_message(msg);
        assert!(result.is_some());
        let (_, item_name, _) = result.unwrap();
        let item_key = crate::bot::handlers::BotEventHandlers::remove_color_codes(&item_name).to_lowercase();
        assert!(!listings.contains(&item_key), "Coop member's listing should not match");
    }

    #[test]
    fn test_parse_cookie_duration_various_formats() {
        // Test various duration formats
        assert_eq!(parse_cookie_duration_secs("Duration: 3d 5h"), 3 * 86400 + 5 * 3600);
        assert_eq!(parse_cookie_duration_secs("Duration: 23h 45m"), 23 * 3600 + 45 * 60);
        assert_eq!(parse_cookie_duration_secs("Duration: 1h 30m"), 1 * 3600 + 30 * 60);
        assert_eq!(parse_cookie_duration_secs("Duration: 0d 0h 0m"), 0);
    }

    #[test]
    fn test_extract_serializable_components_empty() {
        use azalea::registry::builtin::ItemKind;
        use azalea_inventory::ItemStackData;
        // An item with no extra components should return an empty map
        let item_data = ItemStackData::from(ItemKind::Stone);
        let result = extract_serializable_components(&item_data);
        assert!(result.is_empty(), "Stone with no components should have empty serializable components");
    }

    #[test]
    fn test_extract_serializable_components_with_map_id() {
        use azalea::registry::builtin::ItemKind;
        use azalea_inventory::{ItemStack, components::MapId};
        // An item with MapId only — no custom_data, custom_name, lore, profile, tooltip_display
        // should return an empty map (since we only extract those 5 specific components).
        let item = ItemStack::from(ItemKind::Map).with_component(MapId { id: 42 });
        let item_data = item.as_present().expect("map item should be present");
        let result = extract_serializable_components(item_data);
        // MapId is not in our extraction list; result should be empty
        assert!(!result.contains_key("minecraft:custom_data"));
        assert!(!result.contains_key("minecraft:custom_name"));
        assert!(!result.contains_key("minecraft:lore"));
        assert!(!result.contains_key("minecraft:profile"));
        assert!(!result.contains_key("minecraft:tooltip_display"));
    }

    #[test]
    fn test_resolve_pet_tag_mammoth() {
        let nbt = serde_json::json!({
            "minecraft:custom_data": {
                "id": "PET",
                "petInfo": "{\"type\":\"MAMMOTH\",\"active\":false,\"exp\":3.82685830919321E7,\"tier\":\"LEGENDARY\"}"
            }
        });
        assert_eq!(resolve_pet_tag("PET", &nbt), "PET_MAMMOTH");
    }

    #[test]
    fn test_resolve_pet_tag_nested_nbt() {
        let nbt = serde_json::json!({
            "minecraft:custom_data": {
                "nbt": {
                    "ExtraAttributes": {
                        "id": "PET",
                        "petInfo": "{\"type\":\"PHOENIX\",\"tier\":\"LEGENDARY\"}"
                    }
                }
            }
        });
        assert_eq!(resolve_pet_tag("PET", &nbt), "PET_PHOENIX");
    }

    #[test]
    fn test_resolve_pet_tag_non_pet_unchanged() {
        let nbt = serde_json::json!({
            "minecraft:custom_data": {
                "id": "ASPECT_OF_THE_END"
            }
        });
        assert_eq!(resolve_pet_tag("ASPECT_OF_THE_END", &nbt), "ASPECT_OF_THE_END");
    }

    #[test]
    fn test_resolve_pet_tag_no_pet_info_falls_back() {
        let nbt = serde_json::json!({
            "minecraft:custom_data": {
                "id": "PET"
            }
        });
        // No petInfo available — falls back to "PET"
        assert_eq!(resolve_pet_tag("PET", &nbt), "PET");
    }

    // ---- normalize_for_matching tests ----

    #[test]
    fn test_normalize_strips_hyphens_and_lowercases() {
        assert_eq!(normalize_for_matching("Turbo-Wheat V"), "turbo wheat v");
    }

    #[test]
    fn test_normalize_strips_underscores() {
        assert_eq!(normalize_for_matching("TURBO_WHEAT_V"), "turbo wheat v");
    }

    #[test]
    fn test_normalize_collapses_whitespace() {
        assert_eq!(normalize_for_matching("  turbo   wheat   v  "), "turbo wheat v");
    }

    #[test]
    fn test_normalize_strips_mc_color_codes() {
        assert_eq!(normalize_for_matching("§aTurbo-Wheat §eV"), "turbo wheat v");
    }

    #[test]
    fn test_normalize_strips_decorative_symbols() {
        assert_eq!(normalize_for_matching("✪ Turbo-Wheat V ☘"), "turbo wheat v");
    }

    #[test]
    fn test_normalize_plain_button_name_unchanged() {
        assert_eq!(normalize_for_matching("Custom Amount"), "custom amount");
    }

    // ---- find_slot_by_name tests ----

    /// Helper: build a minimal `ItemStack` whose display name is `display_name`.
    fn make_named_item(display_name: &str) -> ItemStack {
        use azalea_chat::{FormattedText, text_component::TextComponent};
        use azalea_inventory::components::CustomName;
        let cn = CustomName {
            name: FormattedText::Text(TextComponent::new(display_name.to_string())),
        };
        ItemStack::from(ItemKind::Paper).with_component(cn)
    }

    #[test]
    fn test_find_slot_exact_contains() {
        let slots = vec![
            ItemStack::Empty,
            make_named_item("Turbo-Wheat V"),
            ItemStack::Empty,
        ];
        // Exact contains: "Turbo-Wheat V" contains "Turbo-Wheat V"
        assert_eq!(find_slot_by_name(&slots, "Turbo-Wheat V"), Some(1));
    }

    #[test]
    fn test_find_slot_normalized_hyphen_vs_space() {
        let slots = vec![
            ItemStack::Empty,
            make_named_item("Turbo-Wheat V"),
            ItemStack::Empty,
        ];
        // "turbo wheat v" should match "Turbo-Wheat V" via normalized phase
        assert_eq!(find_slot_by_name(&slots, "turbo wheat v"), Some(1));
    }

    #[test]
    fn test_find_slot_case_insensitive() {
        let slots = vec![
            make_named_item("Create Buy Order"),
        ];
        assert_eq!(find_slot_by_name(&slots, "create buy order"), Some(0));
    }

    #[test]
    fn test_find_slot_token_matching() {
        let slots = vec![
            ItemStack::Empty,
            make_named_item("Turbo-Wheat Hoe V"),
        ];
        // Token matching: "turbo", "wheat", "v" all in "turbo wheat hoe v"
        assert_eq!(find_slot_by_name(&slots, "turbo wheat v"), Some(1));
    }

    #[test]
    fn test_find_slot_no_match() {
        let slots = vec![
            make_named_item("Enchanted Bread"),
            make_named_item("Wheat"),
        ];
        assert_eq!(find_slot_by_name(&slots, "turbo wheat v"), None);
    }

    #[test]
    fn test_format_price_for_sign_whole_number() {
        // Whole-number prices: integers with comma separators, no ".0" suffix.
        assert_eq!(format_price_for_sign(7500000.0), "7,500,000");
        assert_eq!(format_price_for_sign(100.0), "100");
        assert_eq!(format_price_for_sign(8.0), "8");
        assert_eq!(format_price_for_sign(1662.0), "1,662");
        assert_eq!(format_price_for_sign(60000000.0), "60,000,000");
        assert_eq!(format_price_for_sign(50000002.0), "50,000,002");
    }

    #[test]
    fn test_format_price_for_sign_with_decimal() {
        // Fractional prices: comma-separated integer part with one decimal digit.
        assert_eq!(format_price_for_sign(60000000.2), "60,000,000.2");
        assert_eq!(format_price_for_sign(1234567.9), "1,234,567.9");
        assert_eq!(format_price_for_sign(100.5), "100.5");
        assert_eq!(format_price_for_sign(50000000.1), "50,000,000.1");
        assert_eq!(format_price_for_sign(91238937.4), "91,238,937.4");
    }

    #[test]
    fn test_format_price_for_sign_rounds_to_one_decimal() {
        // Prices with >1 decimal are rounded to nearest 0.1.
        assert_eq!(format_price_for_sign(1234567.89), "1,234,567.9");
        assert_eq!(format_price_for_sign(1234567.84), "1,234,567.8");
        // 1234567.06 * 10 = 12345670.6 → rounds to 12345671 → frac_digit = 1
        assert_eq!(format_price_for_sign(1234567.06), "1,234,567.1");
    }
}
