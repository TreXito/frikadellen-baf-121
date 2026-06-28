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
}

/// Everything the backend client needs to authenticate and execute commands.
pub struct BackendDeps {
    pub url: String,
    pub instance_id: String,
    pub cofl_owner_id: Option<String>,
    pub ingame_names: Vec<String>,
    pub version: String,
    pub link_code: String,
    pub macro_paused: Arc<AtomicBool>,
    pub command_queue: CommandQueue,
    pub bot_client: BotClient,
    pub profit_tracker: Arc<ProfitTracker>,
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
            let (ok, message) = execute_command(action, args, deps);
            let _ = tx.send(
                json!({ "type": "command_result", "id": id, "ok": ok, "message": message })
                    .to_string(),
            );
        }
        other => debug!("[Backend] unhandled message type: {:?}", other),
    }
}

fn execute_command(action: &str, args: Option<&Value>, deps: &BackendDeps) -> (bool, String) {
    match action {
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
            deps.command_queue.enqueue(
                CommandType::SellToAuction {
                    item_name: item_name.to_string(),
                    starting_bid,
                    duration_hours: 24,
                    item_slot: None,
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
        "switch_account" => (false, "account switching not supported via backend yet".to_string()),
        other => (false, format!("unknown action: {}", other)),
    }
}
