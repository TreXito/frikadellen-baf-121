//! Client for the central **baf-backend** gateway.
//!
//! The bot dials out to the backend (NAT-friendly), authenticates with a shared
//! bearer token, announces its identity + a one-time link code, then:
//!   * streams owner-identified activity events (buys/sells/listings/orders) for
//!     profit tracking, and
//!   * executes control commands the backend pushes (pause/resume/list/…),
//!     replying with a `command_result`.
//!
//! See `src/protocol.ts` in the baf-backend repo for the wire format.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, info, warn};

use crate::bot::BotClient;
use crate::profit::ProfitTracker;
use crate::state::command_queue::CommandQueue;
use crate::types::{CommandPriority, CommandType};

/// The shared instance token, baked in at build time via `BAF_BACKEND_TOKEN`
/// (CI), with a runtime env-var fallback for local development. When unset the
/// backend connection is skipped.
fn backend_token() -> Option<String> {
    const COMPILE_TIME: Option<&str> = option_env!("BAF_BACKEND_TOKEN");
    COMPILE_TIME
        .filter(|s| !s.is_empty())
        .map(|s| s.to_owned())
        .or_else(|| std::env::var("BAF_BACKEND_TOKEN").ok().filter(|s| !s.is_empty()))
}

/// Handle used by the rest of the bot to push events to the backend. Cloneable
/// and cheap; a no-op when the backend is disabled/unconfigured.
#[derive(Clone)]
pub struct BackendHandle {
    tx: Option<mpsc::UnboundedSender<String>>,
}

impl BackendHandle {
    /// A handle that drops everything (backend disabled).
    pub fn disabled() -> Self {
        Self { tx: None }
    }

    fn send_raw(&self, value: Value) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(value.to_string());
        }
    }

    /// Report an owner-identified activity event for profit tracking.
    #[allow(clippy::too_many_arguments)]
    pub fn report_event(
        &self,
        kind: &str,
        ingame_name: &str,
        item_name: Option<&str>,
        amount: Option<u64>,
        price: Option<i64>,
        profit: Option<i64>,
        is_bazaar: bool,
    ) {
        if self.tx.is_none() {
            return;
        }
        self.send_raw(json!({
            "type": "event",
            "kind": kind,
            "ingameName": ingame_name,
            "itemName": item_name,
            "amount": amount,
            "price": price,
            "profit": profit,
            "isBazaar": is_bazaar,
        }));
    }

    /// Report a purchase (a flip) with the full detail the backend renders as a
    /// purchase webhook in the all-flips channel.
    #[allow(clippy::too_many_arguments)]
    pub fn report_purchase(
        &self,
        ingame_name: &str,
        item_name: &str,
        price: i64,
        target: Option<i64>,
        profit: Option<i64>,
        buy_speed_ms: Option<u64>,
        finder: Option<&str>,
        purse: Option<u64>,
        auction_uuid: Option<&str>,
    ) {
        if self.tx.is_none() {
            return;
        }
        self.send_raw(json!({
            "type": "event",
            "kind": "buy",
            "ingameName": ingame_name,
            "itemName": item_name,
            "price": price,
            "profit": profit,
            "isBazaar": false,
            "data": {
                "target": target,
                "buySpeedMs": buy_speed_ms,
                "finder": finder,
                "purse": purse,
                "auctionUuid": auction_uuid,
            },
        }));
    }
}

/// Everything the backend client needs to authenticate and execute commands.
pub struct BackendDeps {
    pub url: String,
    pub instance_id: String,
    pub cofl_owner_id: Option<String>,
    pub ingame_names: Vec<String>,
    pub version: String,
    pub link_code: String,
    /// The configured Discord owner id (auto-ownership; TPM-style).
    pub discord_id: Option<String>,
    /// Extra Discord ids allowed to control this bot.
    pub allowed_ids: Vec<String>,
    pub macro_paused: Arc<AtomicBool>,
    /// Toggles for AH / Bazaar flipping (shared with the bot loops + web panel),
    /// so the backend can offer the same enable/disable controls the web GUI does.
    pub enable_ah_flips: Arc<AtomicBool>,
    pub enable_bazaar_flips: Arc<AtomicBool>,
    pub command_queue: CommandQueue,
    pub bot_client: BotClient,
    /// Tracks live bazaar orders for the get_bazaar_orders / cancel controls.
    pub bazaar_tracker: Arc<crate::bazaar_tracker::BazaarOrderTracker>,
    pub profit_tracker: Arc<ProfitTracker>,
    /// Persists config changes (e.g. discord_id written on /link).
    pub config_loader: Arc<crate::config::ConfigLoader>,
    /// Set once this instance is linked/owned, so the terminal stops re-printing
    /// the link code.
    pub linked: Arc<AtomicBool>,
}

/// Spawn the backend client task. Returns a [`BackendHandle`]; if the token is
/// not configured the handle is disabled and no connection is attempted.
pub fn spawn(deps: BackendDeps) -> BackendHandle {
    let Some(token) = backend_token() else {
        info!("[Backend] BAF_BACKEND_TOKEN not set — central backend disabled");
        return BackendHandle::disabled();
    };

    let (tx, rx) = mpsc::unbounded_channel::<String>();
    let handle = BackendHandle { tx: Some(tx.clone()) };
    tokio::spawn(run(deps, token, rx, tx));
    handle
}

async fn run(
    deps: BackendDeps,
    token: String,
    mut rx: mpsc::UnboundedReceiver<String>,
    tx: mpsc::UnboundedSender<String>,
) {
    let mut backoff = 5u64;
    loop {
        match connect_async(&deps.url).await {
            Ok((ws, _)) => {
                info!("[Backend] connected to {}", deps.url);
                backoff = 5;
                let (mut write, mut read) = ws.split();

                // First frame: hello (auth + identity + link code).
                let hello = json!({
                    "type": "hello",
                    "token": token,
                    "instanceId": deps.instance_id,
                    "coflOwnerId": deps.cofl_owner_id,
                    "ingameNames": deps.ingame_names,
                    "version": deps.version,
                    "discordId": deps.discord_id,
                    "allowedIds": deps.allowed_ids,
                    "linkCode": deps.link_code,
                })
                .to_string();
                if write.send(Message::Text(hello)).await.is_err() {
                    warn!("[Backend] failed to send hello — reconnecting");
                    sleep_backoff(&mut backoff).await;
                    continue;
                }

                loop {
                    tokio::select! {
                        inbound = read.next() => {
                            match inbound {
                                Some(Ok(Message::Text(text))) => {
                                    handle_inbound(&text, &deps, &tx);
                                }
                                Some(Ok(Message::Close(_))) => {
                                    warn!("[Backend] connection closed by server");
                                    break;
                                }
                                Some(Err(e)) => {
                                    warn!("[Backend] socket error: {}", e);
                                    break;
                                }
                                None => break,
                                _ => {}
                            }
                        }
                        outgoing = rx.recv() => {
                            match outgoing {
                                Some(s) => {
                                    if write.send(Message::Text(s)).await.is_err() {
                                        break;
                                    }
                                }
                                None => {
                                    // All handles dropped — shut the task down.
                                    return;
                                }
                            }
                        }
                    }
                }
            }
            Err(e) => warn!("[Backend] connect to {} failed: {}", deps.url, e),
        }
        sleep_backoff(&mut backoff).await;
    }
}

async fn sleep_backoff(backoff: &mut u64) {
    tokio::time::sleep(Duration::from_secs(*backoff)).await;
    *backoff = (*backoff * 2).min(60);
}

fn handle_inbound(text: &str, deps: &BackendDeps, tx: &mpsc::UnboundedSender<String>) {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        debug!("[Backend] ignoring non-JSON message");
        return;
    };
    match value.get("type").and_then(|v| v.as_str()) {
        Some("welcome") => {
            let owner = value.get("ownerDiscordId").and_then(|v| v.as_str());
            if owner.is_some() {
                deps.linked.store(true, Ordering::Relaxed);
            }
            info!("[Backend] authenticated (owner: {})", owner.unwrap_or("unlinked"));
        }
        Some("ping") => {
            let _ = tx.send(json!({ "type": "pong" }).to_string());
        }
        Some("error") => {
            warn!(
                "[Backend] server error: {}",
                value.get("message").and_then(|v| v.as_str()).unwrap_or("?")
            );
        }
        Some("command") => {
            let id = value.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let action = value.get("action").and_then(|v| v.as_str()).unwrap_or("");
            let args = value.get("args");
            // Data-returning queries (inventory/auctions) carry a `data` payload;
            // everything else is a control command with an ok/message result.
            if let Some(data) = execute_query(action, deps) {
                let _ = tx.send(
                    json!({ "type": "command_result", "id": id, "ok": true, "data": data })
                        .to_string(),
                );
            } else {
                let (ok, message) = execute_command(action, args, deps);
                let _ = tx.send(
                    json!({ "type": "command_result", "id": id, "ok": ok, "message": message })
                        .to_string(),
                );
            }
        }
        other => debug!("[Backend] unhandled message type: {:?}", other),
    }
}

/// Data-returning queries. Returns `Some(data)` when the action is a query (so
/// the caller replies with a `data` payload), or `None` for control commands.
fn execute_query(action: &str, deps: &BackendDeps) -> Option<Value> {
    match action {
        "get_inventory" => {
            let inv = deps
                .bot_client
                .get_cached_inventory_json()
                .and_then(|j| serde_json::from_str::<Value>(&j).ok())
                .unwrap_or_else(|| Value::Array(vec![]));
            Some(json!({ "inventory": inv }))
        }
        "get_auctions" => {
            let auctions = deps
                .bot_client
                .get_cached_my_auctions_json()
                .and_then(|j| serde_json::from_str::<Value>(&j).ok())
                .unwrap_or(Value::Null);
            Some(json!({ "auctions": auctions }))
        }
        // Live bazaar orders (same data the web panel's Bazaar Orders tab shows).
        "get_bazaar_orders" => {
            let orders = serde_json::to_value(deps.bazaar_tracker.get_orders())
                .unwrap_or(Value::Array(vec![]));
            Some(json!({ "orders": orders }))
        }
        // Command-queue snapshot (web panel "Queue" tab).
        "get_queue" => {
            let queue = serde_json::to_value(deps.command_queue.queue_snapshot())
                .unwrap_or(Value::Array(vec![]));
            Some(json!({ "queue": queue }))
        }
        // Structured status for the GUI (richer than the text `status` command).
        "get_status" => {
            let (ah, bz) = deps.profit_tracker.totals();
            Some(json!({
                "paused": deps.macro_paused.load(Ordering::Relaxed),
                "ahFlips": deps.enable_ah_flips.load(Ordering::Relaxed),
                "bazaarFlips": deps.enable_bazaar_flips.load(Ordering::Relaxed),
                "purse": deps.bot_client.get_purse(),
                "freeSlots": deps.bot_client.empty_slot_count(),
                "listings": deps.bot_client.active_auction_count(),
                "bazaarOrders": deps.bazaar_tracker.order_count(),
                "profitAh": ah,
                "profitBz": bz,
            }))
        }
        // Operator log download: returns the tail of this instance's live log so
        // the central backend GUI can offer a per-bot log download.
        "get_logs" => Some(json!({ "logs": read_log_tail() })),
        _ => None,
    }
}

/// Maximum number of bytes of the live log returned to the operator. Bounds the
/// websocket frame size for very long-running sessions.
const MAX_LOG_BYTES: usize = 400_000;

/// Read the tail (last [`MAX_LOG_BYTES`]) of the active `latest.log`.
fn read_log_tail() -> String {
    let path = crate::logging::get_logs_dir().join("latest.log");
    match std::fs::read(&path) {
        Ok(bytes) => {
            let start = bytes.len().saturating_sub(MAX_LOG_BYTES);
            // Slice on a char boundary fallback via lossy conversion.
            String::from_utf8_lossy(&bytes[start..]).into_owned()
        }
        Err(e) => format!("(could not read log file: {})", e),
    }
}

fn execute_command(action: &str, args: Option<&Value>, deps: &BackendDeps) -> (bool, String) {
    match action {
        "set_discord_id" => {
            let Some(id) = args.and_then(|a| a.get("discord_id")).and_then(|v| v.as_str()) else {
                return (false, "missing discord_id".to_string());
            };
            // Persist the owner's Discord id so ownership survives restarts.
            match deps.config_loader.load() {
                Ok(mut cfg) => {
                    cfg.discord_id = Some(id.to_string());
                    if let Err(e) = deps.config_loader.save(&cfg) {
                        return (false, format!("failed to save config: {}", e));
                    }
                    deps.linked.store(true, Ordering::Relaxed);
                    info!("[Backend] Linked — saved discord_id {} to config", id);
                    (true, "linked; discord_id saved to config".to_string())
                }
                Err(e) => (false, format!("failed to load config: {}", e)),
            }
        }
        "pause" => {
            deps.macro_paused.store(true, Ordering::Relaxed);
            (true, "macro paused".to_string())
        }
        "resume" => {
            deps.macro_paused.store(false, Ordering::Relaxed);
            (true, "macro resumed".to_string())
        }
        "status" => {
            let (ah, bz) = deps.profit_tracker.totals();
            let paused = deps.macro_paused.load(Ordering::Relaxed);
            let purse = deps.bot_client.get_purse();
            let free = deps.bot_client.empty_slot_count();
            let auctions = deps.bot_client.active_auction_count();
            let msg = format!(
                "{} • profit AH {} / BZ {} • {} free slots • {} listings{}",
                if paused { "paused" } else { "running" },
                ah,
                bz,
                free,
                auctions,
                purse.map(|p| format!(" • purse {}", p)).unwrap_or_default(),
            );
            (true, msg)
        }
        "list_item" => {
            let Some(item_name) = args.and_then(|a| a.get("item_name")).and_then(|v| v.as_str()) else {
                return (false, "missing item_name".to_string());
            };
            let starting_bid = args
                .and_then(|a| a.get("starting_bid"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            if starting_bid == 0 {
                return (false, "missing or zero starting_bid".to_string());
            }
            // Optional exact inventory slot (from /list picking an item). When
            // present the lister targets that slot instead of searching by name.
            let item_slot = args.and_then(|a| a.get("item_slot")).and_then(|v| v.as_u64());
            deps.command_queue.enqueue(
                CommandType::SellToAuction {
                    item_name: item_name.to_string(),
                    starting_bid,
                    duration_hours: 24,
                    item_slot,
                    item_id: None,
                },
                CommandPriority::Normal,
                false,
            );
            (true, format!("queued listing for {}", item_name))
        }
        "cancel_auction" => {
            let Some(item_name) = args.and_then(|a| a.get("item_name")).and_then(|v| v.as_str()) else {
                return (false, "missing item_name".to_string());
            };
            let starting_bid = args
                .and_then(|a| a.get("starting_bid"))
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            deps.command_queue.enqueue(
                CommandType::CancelAuction {
                    item_name: item_name.to_string(),
                    starting_bid,
                },
                CommandPriority::High,
                false,
            );
            (true, format!("queued cancel for {}", item_name))
        }
        "claim_purchases" => {
            deps.command_queue
                .enqueue(CommandType::ClaimPurchasedItem, CommandPriority::High, false);
            (true, "queued claim".to_string())
        }
        "collect_bz_orders" => {
            deps.command_queue.enqueue(
                CommandType::ManageOrders { cancel_open: false, target_item: None },
                CommandPriority::High,
                false,
            );
            (true, "queued bazaar order collection".to_string())
        }
        // Same as claim_bz_orders on the web panel: read + claim filled orders.
        "claim_bz_orders" => {
            deps.command_queue.enqueue(
                CommandType::ManageOrders { cancel_open: false, target_item: None },
                CommandPriority::High,
                false,
            );
            (true, "queued bazaar order claim".to_string())
        }
        // Instasell the inventory on bazaar (web "Sell inventory on bazaar").
        "sell_inventory_bz" => {
            deps.command_queue
                .enqueue(CommandType::SellInventoryBz, CommandPriority::High, false);
            (true, "queued bazaar inventory sell".to_string())
        }
        "cancel_bz_order" => {
            let Some(item_name) = args.and_then(|a| a.get("item_name")).and_then(|v| v.as_str()) else {
                return (false, "missing item_name".to_string());
            };
            let is_buy_order = args
                .and_then(|a| a.get("is_buy_order"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            // Mirror the web handler: reflect intent in the tracker immediately,
            // then queue a targeted ManageOrders cancel.
            deps.bazaar_tracker.mark_cancelling(item_name, is_buy_order);
            deps.bazaar_tracker.remove_order(item_name, is_buy_order);
            deps.command_queue.enqueue(
                CommandType::ManageOrders {
                    cancel_open: true,
                    target_item: Some((item_name.to_string(), is_buy_order)),
                },
                CommandPriority::High,
                false,
            );
            (true, format!("queued bazaar cancel for {}", item_name))
        }
        "cancel_all_bz_orders" => {
            let removed = deps.bazaar_tracker.clear_all_orders();
            deps.command_queue.enqueue(
                CommandType::ManageOrders { cancel_open: true, target_item: None },
                CommandPriority::High,
                false,
            );
            (true, format!("queued cancel of all bazaar orders ({} tracked)", removed))
        }
        "toggle_ah" => {
            let enabled = args.and_then(|a| a.get("enabled")).and_then(|v| v.as_bool());
            let enabled = match enabled {
                Some(e) => e,
                None => !deps.enable_ah_flips.load(Ordering::Relaxed),
            };
            deps.enable_ah_flips.store(enabled, Ordering::Relaxed);
            let _ = deps.config_loader.update_property(|c| c.enable_ah_flips = enabled);
            (true, format!("AH flips {}", if enabled { "enabled" } else { "disabled" }))
        }
        "toggle_bazaar" => {
            let enabled = args.and_then(|a| a.get("enabled")).and_then(|v| v.as_bool());
            let enabled = match enabled {
                Some(e) => e,
                None => !deps.enable_bazaar_flips.load(Ordering::Relaxed),
            };
            deps.enable_bazaar_flips.store(enabled, Ordering::Relaxed);
            let _ = deps.config_loader.update_property(|c| c.enable_bazaar_flips = enabled);
            (true, format!("Bazaar flips {}", if enabled { "enabled" } else { "disabled" }))
        }
        "switch_account" => (false, "account switching not supported via backend yet".to_string()),
        // Operator kill switch: lets the central backend GUI shut a bot down
        // remotely. Exit shortly after so the command_result is flushed first.
        "shutdown" => {
            warn!("[Backend] shutdown requested by operator — exiting");
            std::thread::spawn(|| {
                std::thread::sleep(Duration::from_millis(500));
                std::process::exit(0);
            });
            (true, "shutting down".to_string())
        }
        other => (false, format!("unknown action: {}", other)),
    }
}
