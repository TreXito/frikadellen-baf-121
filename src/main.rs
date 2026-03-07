use anyhow::Result;
use dialoguer::{Input, Confirm};
use frikadellen_baf::{
    config::ConfigLoader,
    logging::{init_logger, print_mc_chat},
    state::CommandQueue,
    web::{WebEventLog, WebState},
    websocket::CoflWebSocket,
    bot::BotClient,
    types::Flip,
};
use tracing::{debug, error, info, warn};
use tokio::time::{sleep, Duration};
use serde_json;
use std::sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}};
use std::collections::HashMap;
use std::time::Instant;

const VERSION: &str = "af-3.0";

/// Calculate Hypixel AH fee based on price tier (matches TypeScript calculateAuctionHouseFee).
/// - <10M  → 1%
/// - <100M → 2%
/// - ≥100M → 2.5%
fn calculate_ah_fee(price: u64) -> u64 {
    if price < 10_000_000 {
        price / 100
    } else if price < 100_000_000 {
        price * 2 / 100
    } else {
        price * 25 / 1000
    }
}

/// Format a coin amount with thousands separators.
/// e.g. `24000000` → `"24,000,000"`, `-500000` → `"-500,000"`
fn format_coins(amount: i64) -> String {
    let negative = amount < 0;
    let abs = amount.unsigned_abs();
    let s = abs.to_string();
    let mut result = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(c);
    }
    let formatted: String = result.chars().rev().collect();
    if negative { format!("-{}", formatted) } else { formatted }
}

fn is_ban_disconnect(reason: &str) -> bool {
    let lower = reason.to_ascii_lowercase();
    lower.contains("temporarily banned")
        || lower.contains("permanently banned")
        || lower.contains("ban id:")
}

/// Flip tracker entry: (flip, actual_buy_price, purchase_instant, flip_receive_instant)
/// buy_price is 0 until ItemPurchased fires and updates it.
/// flip_receive_instant is set when the flip is received and never changed (used for buy-speed).
type FlipTrackerMap = Arc<Mutex<HashMap<String, (Flip, u64, Instant, Instant)>>>;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    init_logger()?;
    info!("Starting Frikadellen BAF v{}", VERSION);

    // Load or create configuration
    let config_loader = ConfigLoader::new();
    let mut config = config_loader.load()?;

    // Prompt for username if not set
    if config.ingame_name.is_none() {
        let name: String = Input::new()
            .with_prompt("Enter your ingame name")
            .interact_text()?;
        config.ingame_name = Some(name);
        config_loader.save(&config)?;
    }

    if config.enable_ah_flips && config.enable_bazaar_flips {
        // Both are enabled, ask user
    } else if !config.enable_ah_flips && !config.enable_bazaar_flips {
        // Neither is configured, ask user
        let enable_ah = Confirm::new()
            .with_prompt("Enable auction house flips?")
            .default(true)
            .interact()?;
        config.enable_ah_flips = enable_ah;

        let enable_bazaar = Confirm::new()
            .with_prompt("Enable bazaar flips?")
            .default(true)
            .interact()?;
        config.enable_bazaar_flips = enable_bazaar;

        config_loader.save(&config)?;
    }

    // Prompt for webhook URL if not yet configured (matches TypeScript configHelper.ts pattern
    // of adding new default values to existing config on first run of newer version)
    if config.webhook_url.is_none() {
        let wants_webhook = Confirm::new()
            .with_prompt("Configure Discord webhook for notifications? (optional)")
            .default(false)
            .interact()?;
        if wants_webhook {
            let url: String = Input::new()
                .with_prompt("Enter Discord webhook URL")
                .interact_text()?;
            config.webhook_url = Some(url);
        } else {
            // Mark as configured (empty = disabled) so we don't ask again
            config.webhook_url = Some(String::new());
        }
        config_loader.save(&config)?;
    }

    let ingame_name = config.ingame_name.clone().unwrap();
    
    info!("Configuration loaded for player: {}", ingame_name);
    info!("AH Flips: {}", if config.enable_ah_flips { "ENABLED" } else { "DISABLED" });
    info!("Bazaar Flips: {}", if config.enable_bazaar_flips { "ENABLED" } else { "DISABLED" });
    info!("Web GUI Port: {}", config.web_gui_port);

    // Initialize command queue
    let command_queue = CommandQueue::new();

    // Bazaar-flip pause flag (matches TypeScript bazaarFlipPauser.ts).
    // Set to true for 20 seconds when a `countdown` message arrives (AH flips incoming).
    let bazaar_flips_paused = Arc::new(AtomicBool::new(false));

    // Flip tracker: stores pending/active AH flips for profit reporting in webhooks.
    // Key = clean item_name (lowercase), value = (flip, actual_buy_price, purchase_time).
    // buy_price starts at 0 until ItemPurchased fires and sets it to the real price.
    let flip_tracker: FlipTrackerMap = Arc::new(Mutex::new(HashMap::new()));

    // Coflnet connection ID — parsed from "Your connection id is XXXX" chat message.
    // Included in startup webhooks (matches TypeScript getCoflnetPremiumInfo().connectionId).
    let cofl_connection_id: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    // Coflnet premium info — parsed from "You have PremiumPlus until ..." writeToChat message.
    // Tuple: (tier, expires_str) e.g. ("Premium Plus", "2026-Feb-10 08:55 UTC").
    let cofl_premium: Arc<Mutex<Option<(String, String)>>> = Arc::new(Mutex::new(None));

    // Get or generate session ID for Coflnet (matching TypeScript coflSessionManager.ts)
    let session_id = if let Some(session) = config.sessions.get(&ingame_name) {
        // Check if session is expired
        if session.expires < chrono::Utc::now() {
            // Session expired, generate new one
            info!("Session expired for {}, generating new session ID", ingame_name);
            let new_id = uuid::Uuid::new_v4().to_string();
            let new_session = frikadellen_baf::config::types::CoflSession {
                id: new_id.clone(),
                expires: chrono::Utc::now() + chrono::Duration::days(180), // 180 days like TypeScript
            };
            config.sessions.insert(ingame_name.clone(), new_session);
            config_loader.save(&config)?;
            new_id
        } else {
            // Session still valid
            info!("Using existing session ID for {}", ingame_name);
            session.id.clone()
        }
    } else {
        // No session exists, create new one
        info!("No session found for {}, generating new session ID", ingame_name);
        let new_id = uuid::Uuid::new_v4().to_string();
        let new_session = frikadellen_baf::config::types::CoflSession {
            id: new_id.clone(),
            expires: chrono::Utc::now() + chrono::Duration::days(180), // 180 days like TypeScript
        };
        config.sessions.insert(ingame_name.clone(), new_session);
        config_loader.save(&config)?;
        new_id
    };

    // ── Web event log (shared with the web GUI dashboard) ──────────
    let web_event_log = WebEventLog::new();

    info!("Connecting to Coflnet WebSocket...");
    
    // Connect to Coflnet WebSocket
    let (ws_client, mut ws_rx) = CoflWebSocket::connect(
        config.websocket_url.clone(),
        ingame_name.clone(),
        VERSION.to_string(),
        session_id.clone(),
    ).await?;

    info!("WebSocket connected successfully");

    // Send "initialized" webhook notification
    if let Some(webhook_url) = config.active_webhook_url() {
        let url = webhook_url.to_string();
        let name = ingame_name.clone();
        let ah = config.enable_ah_flips;
        let bz = config.enable_bazaar_flips;
        // Connection ID and premium may not be available yet at startup (COFL sends them shortly
        // after WS connect), so we delay 3s to give COFL time to send those messages first.
        let conn_id_init = cofl_connection_id.clone();
        let premium_init = cofl_premium.clone();
        tokio::spawn(async move {
            tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
            let conn_id = conn_id_init.lock().ok().and_then(|g| g.clone());
            let premium = premium_init.lock().ok().and_then(|g| g.clone());
            frikadellen_baf::webhook::send_webhook_initialized(&name, ah, bz, conn_id.as_deref(), premium.as_ref().map(|(t, e)| (t.as_str(), e.as_str())), &url).await;
        });
    }

    // Initialize and connect bot client
    info!("Initializing Minecraft bot...");
    info!("Authenticating with Microsoft account...");
    info!("A browser window will open for you to log in");
    
    let mut bot_client = BotClient::new();
    bot_client.fastbuy = config.fastbuy_enabled();
    bot_client.set_auto_cookie_hours(config.auto_cookie);
    bot_client.freemoney = config.freemoney_enabled();
    
    // Connect to Hypixel - Azalea will handle Microsoft OAuth in browser
    match bot_client.connect(ingame_name.clone(), Some(ws_client.clone())).await {
        Ok(_) => {
            info!("Bot connection initiated successfully");
        }
        Err(e) => {
            warn!("Failed to connect bot: {}", e);
            warn!("The bot will continue running in limited mode (WebSocket only)");
            warn!("Please ensure your Microsoft account is valid and you have access to Hypixel");
        }
    }

    // ── Spawn the web GUI server ──────────────────────────────────────
    {
        let web_state = Arc::new(WebState {
            bot_client: bot_client.clone(),
            command_queue: command_queue.clone(),
            ws_client: ws_client.clone(),
            event_log: web_event_log.clone(),
            config: config.clone(),
            start_time: std::time::Instant::now(),
            ingame_name: ingame_name.clone(),
        });
        tokio::spawn(frikadellen_baf::web::start_web_server(
            web_state,
            config.web_gui_port,
        ));
    }

    // Spawn bot event handler
    let bot_client_clone = bot_client.clone();
    let ws_client_for_events = ws_client.clone();
    let config_for_events = config.clone();
    let command_queue_clone = command_queue.clone();
    let ingame_name_for_events = ingame_name.clone();
    let flip_tracker_events = flip_tracker.clone();
    let cofl_connection_id_events = cofl_connection_id.clone();
    let cofl_premium_events = cofl_premium.clone();
    let web_log_events = web_event_log.clone();
    tokio::spawn(async move {
        while let Some(event) = bot_client_clone.next_event().await {
            match event {
                frikadellen_baf::bot::BotEvent::Login => {
                    info!("✓ Bot logged into Minecraft successfully");
                    web_log_events.push("system", "Bot logged into Minecraft".to_string());
                }
                frikadellen_baf::bot::BotEvent::Spawn => {
                    info!("✓ Bot spawned in world and ready");
                    web_log_events.push("system", "Bot spawned in world".to_string());
                }
                frikadellen_baf::bot::BotEvent::ChatMessage(msg) => {
                    print_mc_chat(&msg);
                    web_log_events.push("chat", msg.clone());
                }
                frikadellen_baf::bot::BotEvent::WindowOpen(id, window_type, title) => {
                    debug!("Window opened: {} (ID: {}, Type: {})", title, id, window_type);
                }
                frikadellen_baf::bot::BotEvent::WindowClose => {
                    debug!("Window closed");
                }
                frikadellen_baf::bot::BotEvent::Disconnected(reason) => {
                    warn!("Bot disconnected: {}", reason);
                    web_log_events.push("error", format!("Disconnected: {}", reason));
                    if is_ban_disconnect(&reason) {
                        if let Some(webhook_url) = config_for_events.active_webhook_url() {
                            let url = webhook_url.to_string();
                            let name = ingame_name_for_events.clone();
                            let ban_reason = reason.clone();
                            tokio::spawn(async move {
                                frikadellen_baf::webhook::send_webhook_banned(&name, &ban_reason, &url).await;
                            });
                        }
                    }
                }
                frikadellen_baf::bot::BotEvent::Kicked(reason) => {
                    warn!("Bot kicked: {}", reason);
                    web_log_events.push("error", format!("Kicked: {}", reason));
                }
                frikadellen_baf::bot::BotEvent::StartupComplete { orders_cancelled } => {
                    info!("[Startup] Startup complete - bot is ready to flip! ({} order(s) cancelled)", orders_cancelled);
                    web_log_events.push("system", format!("Startup complete — {} order(s) cancelled", orders_cancelled));
                    // Upload scoreboard to COFL (with real data matching TypeScript runStartupWorkflow)
                    {
                        let scoreboard_lines = bot_client_clone.get_scoreboard_lines();
                        let ws = ws_client_for_events.clone();
                        tokio::spawn(async move {
                            let data_json = serde_json::to_string(&scoreboard_lines).unwrap_or_else(|_| "[]".to_string());
                            let scoreboard_msg = serde_json::json!({"type": "uploadScoreboard", "data": data_json}).to_string();
                            let tab_msg = serde_json::json!({"type": "uploadTab", "data": "[]"}).to_string();
                            debug!("[Startup] Sending uploadScoreboard to COFL: {:?}", scoreboard_lines);
                            let _ = ws.send_message(&scoreboard_msg).await;
                            debug!("[Startup] Sending uploadTab to COFL (empty)");
                            let _ = ws.send_message(&tab_msg).await;
                            debug!("[Startup] Uploaded scoreboard ({} lines)", scoreboard_lines.len());
                        });
                    }
                    // Request bazaar flips immediately after startup (matching TypeScript runStartupWorkflow)
                    if config_for_events.enable_bazaar_flips {
                        let msg = serde_json::json!({
                            "type": "getbazaarflips",
                            "data": serde_json::to_string("").unwrap_or_default()
                        }).to_string();
                        if let Err(e) = ws_client_for_events.send_message(&msg).await {
                            error!("Failed to send getbazaarflips after startup: {}", e);
                        } else {
                            info!("[Startup] Requested bazaar flips");
                        }
                    }
                    // Send startup complete webhook
                    if let Some(webhook_url) = config_for_events.active_webhook_url() {
                        let url = webhook_url.to_string();
                        let name = ingame_name_for_events.clone();
                        let ah = config_for_events.enable_ah_flips;
                        let bz = config_for_events.enable_bazaar_flips;
                        let conn_id = cofl_connection_id_events.lock().ok().and_then(|g| g.clone());
                        let premium = cofl_premium_events.lock().ok().and_then(|g| g.clone());
                        tokio::spawn(async move {
                            frikadellen_baf::webhook::send_webhook_startup_complete(&name, orders_cancelled, ah, bz, conn_id.as_deref(), premium.as_ref().map(|(t, e)| (t.as_str(), e.as_str())), &url).await;
                        });
                    }
                }
                frikadellen_baf::bot::BotEvent::ItemPurchased { item_name, price, buy_speed_ms: event_buy_speed_ms } => {
                    // Send uploadScoreboard (with real data) and uploadTab to COFL
                    let ws = ws_client_for_events.clone();
                    let scoreboard_lines = bot_client_clone.get_scoreboard_lines();
                    tokio::spawn(async move {
                        let data_json = serde_json::to_string(&scoreboard_lines).unwrap_or_else(|_| "[]".to_string());
                        let scoreboard_msg = serde_json::json!({"type": "uploadScoreboard", "data": data_json}).to_string();
                        let tab_msg = serde_json::json!({"type": "uploadTab", "data": "[]"}).to_string();
                        debug!("[ItemPurchased] Sending uploadScoreboard to COFL: {:?}", scoreboard_lines);
                        let _ = ws.send_message(&scoreboard_msg).await;
                        debug!("[ItemPurchased] Sending uploadTab to COFL (empty)");
                        let _ = ws.send_message(&tab_msg).await;
                    });
                    // Queue claim at Normal priority so any pending High-priority flip
                    // purchases run before we open the AH windows to collect.
                    command_queue_clone.enqueue(
                        frikadellen_baf::types::CommandType::ClaimPurchasedItem,
                        frikadellen_baf::types::CommandPriority::Normal,
                        false,
                    );
                    // Look up stored flip data and update with real buy price + purchase time.
                    // Also grab the color-coded item name from the flip for colorful output.
                    // Buy speed comes from the event (BIN Auction View open → escrow message),
                    // which is more accurate than the flip-receive-to-purchase tracker timing.
                    let (opt_target, opt_profit, colored_name, opt_auction_uuid) = {
                        let key = frikadellen_baf::utils::remove_minecraft_colors(&item_name).to_lowercase();
                        match flip_tracker_events.lock() {
                            Ok(mut tracker) => {
                                if let Some(entry) = tracker.get_mut(&key) {
                                    entry.1 = price; // actual buy price
                                    entry.2 = Instant::now(); // purchase time
                                    let target = entry.0.target;
                                    let ah_fee = calculate_ah_fee(target);
                                    let expected_profit = target as i64 - price as i64 - ah_fee as i64;
                                    let uuid = entry.0.uuid.clone();
                                    (Some(target), Some(expected_profit), entry.0.item_name.clone(), uuid)
                                } else {
                                    (None, None, item_name.clone(), None)
                                }
                            }
                            Err(e) => {
                                warn!("Flip tracker lock failed at ItemPurchased: {}", e);
                                (None, None, item_name.clone(), None)
                            }
                        }
                    };
                    // Print colorful purchase announcement (item rarity shown via color code)
                    let profit_str = opt_profit.map(|p| {
                        let color = if p >= 0 { "§a" } else { "§c" };
                        format!(" §7| Expected profit: {}{}§r", color, format_coins(p))
                    }).unwrap_or_default();
                    let speed_str = event_buy_speed_ms.map(|ms| format!(" §7| Buy speed: §e{}ms§r", ms)).unwrap_or_default();
                    print_mc_chat(&format!(
                        "§f[§4BAF§f]: §a✦ PURCHASED §r{}§r §7for §6{}§7 coins!{}{}",
                        colored_name, format_coins(price as i64), profit_str, speed_str
                    ));
                    web_log_events.push("purchase", format!(
                        "§a✦ PURCHASED §r{}§r §7for §6{}§7 coins!{}{}",
                        colored_name, format_coins(price as i64), profit_str, speed_str
                    ));
                    // Send webhook
                    if let Some(webhook_url) = config_for_events.active_webhook_url() {
                        let url = webhook_url.to_string();
                        let name = ingame_name_for_events.clone();
                        let item = item_name.clone();
                        let purse = bot_client_clone.get_purse();
                        let uuid_str = opt_auction_uuid.clone();
                        tokio::spawn(async move {
                            frikadellen_baf::webhook::send_webhook_item_purchased(
                                &name, &item, price, opt_target, opt_profit, purse,
                                event_buy_speed_ms, uuid_str.as_deref(), &url,
                            ).await;
                        });
                    }
                }
                frikadellen_baf::bot::BotEvent::ItemSold { item_name, price, buyer } => {
                    command_queue_clone.enqueue(
                        frikadellen_baf::types::CommandType::ClaimSoldItem,
                        frikadellen_baf::types::CommandPriority::High,
                        true,
                    );
                    // Look up flip data to calculate actual profit + time to sell
                    let (opt_profit, opt_buy_price, opt_time_secs, opt_auction_uuid) = {
                        let key = frikadellen_baf::utils::remove_minecraft_colors(&item_name).to_lowercase();
                        match flip_tracker_events.lock() {
                            Ok(mut tracker) => {
                                if let Some(entry) = tracker.remove(&key) {
                                    let (flip, buy_price, purchase_time, _receive_time) = entry;
                                    if buy_price > 0 {
                                        let ah_fee = calculate_ah_fee(price);
                                        let profit = price as i64 - buy_price as i64 - ah_fee as i64;
                                        let time_secs = purchase_time.elapsed().as_secs();
                                        (Some(profit), Some(buy_price), Some(time_secs), flip.uuid)
                                    } else {
                                        (None, None, None, flip.uuid)
                                    }
                                } else {
                                    (None, None, None, None)
                                }
                            }
                            Err(e) => {
                                warn!("Flip tracker lock failed at ItemSold: {}", e);
                                (None, None, None, None)
                            }
                        }
                    };
                    // Print colorful sold announcement
                    let profit_str = opt_profit.map(|p| {
                        let color = if p >= 0 { "§a" } else { "§c" };
                        format!(" §7| Profit: {}{}§r", color, format_coins(p))
                    }).unwrap_or_default();
                    print_mc_chat(&format!(
                        "§f[§4BAF§f]: §6⚡ SOLD §r{} §7to §e{}§7 for §6{}§7 coins!{}",
                        item_name, buyer, format_coins(price as i64), profit_str
                    ));
                    web_log_events.push("sold", format!(
                        "§6⚡ SOLD §r{} §7to §e{}§7 for §6{}§7 coins!{}",
                        item_name, buyer, format_coins(price as i64), profit_str
                    ));
                    if let Some(webhook_url) = config_for_events.active_webhook_url() {
                        let url = webhook_url.to_string();
                        let name = ingame_name_for_events.clone();
                        let item = item_name.clone();
                        let b = buyer.clone();
                        let purse = bot_client_clone.get_purse();
                        let uuid_str = opt_auction_uuid.clone();
                        tokio::spawn(async move {
                            frikadellen_baf::webhook::send_webhook_item_sold(
                                &name, &item, price, &b, opt_profit, opt_buy_price,
                                opt_time_secs, purse, uuid_str.as_deref(), &url,
                            ).await;
                        });
                    }
                }
                frikadellen_baf::bot::BotEvent::BazaarOrderPlaced { item_name, amount, price_per_unit, is_buy_order } => {
                    let (order_color, order_type) = if is_buy_order { ("§a", "BUY") } else { ("§c", "SELL") };
                    print_mc_chat(&format!(
                        "§f[§4BAF§f]: §6[BZ] {}{}§7 order placed: {}x {} @ §6{}§7 coins/unit",
                        order_color, order_type, amount, item_name, format_coins(price_per_unit as i64)
                    ));
                    web_log_events.push("bazaar", format!(
                        "§6[BZ] {}{}§7 order placed: {}x {} @ §6{}§7 coins/unit",
                        order_color, order_type, amount, item_name, format_coins(price_per_unit as i64)
                    ));
                    if let Some(webhook_url) = config_for_events.active_webhook_url() {
                        let url = webhook_url.to_string();
                        let name = ingame_name_for_events.clone();
                        let item = item_name.clone();
                        let total = price_per_unit * amount as f64;
                        let purse = bot_client_clone.get_purse();
                        tokio::spawn(async move {
                            frikadellen_baf::webhook::send_webhook_bazaar_order_placed(
                                &name, &item, amount, price_per_unit, total, is_buy_order, purse, &url,
                            ).await;
                        });
                    }
                }
                frikadellen_baf::bot::BotEvent::AuctionListed { item_name, starting_bid, duration_hours } => {
                    print_mc_chat(&format!(
                        "§f[§4BAF§f]: §a🏷️ BIN listed: §r{} §7@ §6{}§7 coins for §e{}h",
                        item_name, format_coins(starting_bid as i64), duration_hours
                    ));
                    web_log_events.push("sold", format!(
                        "§a🏷️ BIN listed: §r{} §7@ §6{}§7 coins for §e{}h",
                        item_name, format_coins(starting_bid as i64), duration_hours
                    ));
                    if let Some(webhook_url) = config_for_events.active_webhook_url() {
                        let url = webhook_url.to_string();
                        let name = ingame_name_for_events.clone();
                        let item = item_name.clone();
                        let purse = bot_client_clone.get_purse();
                        tokio::spawn(async move {
                            frikadellen_baf::webhook::send_webhook_auction_listed(
                                &name, &item, starting_bid, duration_hours, purse, &url,
                            ).await;
                        });
                    }
                }
                frikadellen_baf::bot::BotEvent::BazaarOrderFilled => {
                    // A bazaar buy/sell order was filled — trigger a ManageOrders run
                    // immediately so the items are collected without waiting for the next
                    // periodic check.  Only enqueue if bazaar flips are enabled.
                    if config_for_events.enable_bazaar_flips {
                        info!("[BazaarOrders] Order filled — queuing ManageOrders");
                        command_queue_clone.enqueue(
                            frikadellen_baf::types::CommandType::ManageOrders,
                            frikadellen_baf::types::CommandPriority::High,
                            false,
                        );
                    }
                }
            }
        }
    });

    // Spawn WebSocket message handler
    let command_queue_clone = command_queue.clone();
    let config_clone = config.clone();
    let ws_client_clone = ws_client.clone();
    let bot_client_for_ws = bot_client.clone();
    let bazaar_flips_paused_ws = bazaar_flips_paused.clone();
    let flip_tracker_ws = flip_tracker.clone();
    let cofl_connection_id_ws = cofl_connection_id.clone();
    let cofl_premium_ws = cofl_premium.clone();
    let web_log_ws = web_event_log.clone();
    
    tokio::spawn(async move {
        use frikadellen_baf::websocket::CoflEvent;
        use frikadellen_baf::types::{CommandType, CommandPriority};

        while let Some(event) = ws_rx.recv().await {
            match event {
                CoflEvent::AuctionFlip(flip) => {
                    // Skip if AH flips are disabled
                    if !config_clone.enable_ah_flips {
                        continue;
                    }

                    // Skip if in startup/claiming state - use bot_client state (authoritative source)
                    if !bot_client_for_ws.state().allows_commands() {
                        debug!("Skipping flip — bot busy ({:?}): {}", bot_client_for_ws.state(), flip.item_name);
                        continue;
                    }

                    // Print colorful flip announcement (item name keeps its rarity color code)
                    let profit = flip.target.saturating_sub(flip.starting_bid);
                    print_mc_chat(&format!(
                        "§f[§4BAF§f]: §eTrying to purchase flip: §r{}§r §7for §6{}§7 coins §7(Target: §6{}§7, Profit: §a{}§7)",
                        flip.item_name,
                        format_coins(flip.starting_bid as i64),
                        format_coins(flip.target as i64),
                        format_coins(profit as i64)
                    ));
                    web_log_ws.push("flip", format!(
                        "§eTrying to purchase: §r{}§r §7for §6{}§7 (Target: §6{}§7, Profit: §a{}§7)",
                        flip.item_name,
                        format_coins(flip.starting_bid as i64),
                        format_coins(flip.target as i64),
                        format_coins(profit as i64)
                    ));

                    // Store flip in tracker so ItemPurchased / ItemSold webhooks can include profit
                    {
                        let key = frikadellen_baf::utils::remove_minecraft_colors(&flip.item_name).to_lowercase();
                        if let Ok(mut tracker) = flip_tracker_ws.lock() {
                            let now = Instant::now();
                            tracker.insert(key, (flip.clone(), 0, now, now));
                        }
                    }

                    // Queue the flip command
                    command_queue_clone.enqueue(
                        CommandType::PurchaseAuction { flip },
                        CommandPriority::Normal,
                        false, // Not interruptible
                    );
                }
                CoflEvent::BazaarFlip(bazaar_flip) => {
                    // Skip if bazaar flips are disabled
                    if !config_clone.enable_bazaar_flips {
                        continue;
                    }

                    // Only skip during active startup phases (Startup / ManagingOrders).
                    // During ClaimingSold / ClaimingPurchased the flip is queued and will
                    // execute once the claim command finishes — matching TypeScript behaviour.
                    let bot_state = bot_client_for_ws.state();
                    if matches!(bot_state, frikadellen_baf::types::BotState::Startup | frikadellen_baf::types::BotState::ManagingOrders) {
                        debug!("Skipping bazaar flip during startup ({:?}): {}", bot_state, bazaar_flip.item_name);
                        continue;
                    }

                    // Skip if at the Bazaar order limit (21 orders)
                    if bot_client_for_ws.is_bazaar_at_limit() {
                        debug!("Skipping bazaar flip — at order limit: {}", bazaar_flip.item_name);
                        continue;
                    }

                    // Skip if bazaar flips are paused due to incoming AH flip (matching bazaarFlipPauser.ts)
                    if bazaar_flips_paused_ws.load(Ordering::Relaxed) {
                        debug!("Bazaar flips paused (AH flip incoming), skipping: {}", bazaar_flip.item_name);
                        continue;
                    }

                    // Print colorful bazaar flip announcement
                    let effective_is_buy = bazaar_flip.effective_is_buy_order();
                    let (order_color, order_label) = if effective_is_buy { ("§a", "BUY") } else { ("§c", "SELL") };
                    print_mc_chat(&format!(
                        "§f[§4BAF§f]: §6[BZ] {}{}§7 order: §r{}§r §7x{} @ §6{}§7 coins/unit",
                        order_color, order_label,
                        bazaar_flip.item_name,
                        bazaar_flip.amount,
                        format_coins(bazaar_flip.price_per_unit as i64)
                    ));
                    web_log_ws.push("bazaar", format!(
                        "§6[BZ] {}{}§7 order: §r{}§r §7x{} @ §6{}§7 coins/unit",
                        order_color, order_label,
                        bazaar_flip.item_name,
                        bazaar_flip.amount,
                        format_coins(bazaar_flip.price_per_unit as i64)
                    ));

                    // Queue the bazaar command.
                    // Matching TypeScript: SELL orders use HIGH priority (free up inventory),
                    // BUY orders use NORMAL priority. Both are interruptible by AH flips.
                    let priority = if effective_is_buy {
                        CommandPriority::Normal
                    } else {
                        CommandPriority::High
                    };
                    let command_type = if effective_is_buy {
                        CommandType::BazaarBuyOrder {
                            item_name: bazaar_flip.item_name.clone(),
                            item_tag: bazaar_flip.item_tag.clone(),
                            amount: bazaar_flip.amount,
                            price_per_unit: bazaar_flip.price_per_unit,
                        }
                    } else {
                        CommandType::BazaarSellOrder {
                            item_name: bazaar_flip.item_name.clone(),
                            item_tag: bazaar_flip.item_tag.clone(),
                            amount: bazaar_flip.amount,
                            price_per_unit: bazaar_flip.price_per_unit,
                        }
                    };

                    command_queue_clone.enqueue(
                        command_type,
                        priority,
                        true, // Interruptible by AH flips
                    );
                }
                CoflEvent::ChatMessage(msg) => {
                    // Parse "Your connection id is XXXX" (from chatMessage, matches TypeScript BAF.ts)
                    if let Some(cap) = msg.find("Your connection id is ") {
                        let rest = &msg[cap + "Your connection id is ".len()..];
                        let conn_id: String = rest.chars()
                            .take_while(|c| c.is_ascii_hexdigit())
                            .collect();
                        if conn_id.len() == 32 {
                            info!("[Coflnet] Connection ID: {}", conn_id);
                            if let Ok(mut g) = cofl_connection_id_ws.lock() {
                                *g = Some(conn_id);
                            }
                        }
                    }
                    // Parse "You have X until Y" premium info (from writeToChat/chatMessage)
                    // Format: "You have Premium Plus until 2026-Feb-10 08:55 UTC"
                    if let Some(cap) = msg.find("You have ") {
                        let rest = &msg[cap + "You have ".len()..];
                        if let Some(until_pos) = rest.find(" until ") {
                            let tier = rest[..until_pos].trim().to_string();
                            let expires_raw = &rest[until_pos + " until ".len()..];
                            let expires: String = expires_raw.chars()
                                .take_while(|&c| c != '\n' && c != '\\')
                                .collect();
                            let expires = expires.trim().to_string();
                            if !tier.is_empty() && !expires.is_empty() {
                                info!("[Coflnet] Premium: {} until {}", tier, expires);
                                if let Ok(mut g) = cofl_premium_ws.lock() {
                                    *g = Some((tier, expires));
                                }
                            }
                        }
                    }
                    // Display COFL chat messages with proper color formatting
                    // These are informational messages and should NOT be sent to Hypixel server
                    if config_clone.use_cofl_chat {
                        // Print with color codes if the message contains them
                        print_mc_chat(&msg);
                        web_log_ws.push("chat", msg.clone());
                    } else {
                        // Still show in debug mode but without color formatting
                        debug!("[COFL Chat] {}", msg);
                    }
                }
                CoflEvent::Command(cmd) => {
                    info!("Received command from Coflnet: {}", cmd);
                    
                    // Check if this is a /cofl or /baf command that should be sent back to websocket
                    // Match TypeScript consoleHandler.ts - parse and route commands properly
                    let lowercase_cmd = cmd.trim().to_lowercase();
                    if lowercase_cmd.starts_with("/cofl") || lowercase_cmd.starts_with("/baf") {
                        // Parse /cofl command like the console handler does
                        let parts: Vec<&str> = cmd.trim().split_whitespace().collect();
                        if parts.len() > 1 {
                            let command = parts[1].to_string(); // Clone to own the data
                            let args = parts[2..].join(" ");
                            
                            // Send to websocket with command as type (JSON-stringified data)
                            let ws = ws_client_clone.clone();
                            tokio::spawn(async move {
                                let data_json = serde_json::to_string(&args).unwrap_or_else(|_| "\"\"".to_string());
                                let message = serde_json::json!({
                                    "type": command,
                                    "data": data_json
                                }).to_string();
                                
                                if let Err(e) = ws.send_message(&message).await {
                                    error!("Failed to send /cofl command to websocket: {}", e);
                                } else {
                                    info!("Sent /cofl {} to websocket", command);
                                }
                            });
                        }
                    } else {
                        // Execute non-cofl commands sent by Coflnet to Minecraft
                        // This matches TypeScript behavior: bot.chat(data) for non-cofl commands
                        command_queue_clone.enqueue(
                            CommandType::SendChat { message: cmd },
                            CommandPriority::High,
                            false, // Not interruptible
                        );
                    }
                }
                // Handle advanced message types (matching TypeScript BAF.ts)
                CoflEvent::GetInventory => {
                    // TypeScript handles getInventory DIRECTLY in the WS message handler,
                    // calling JSON.stringify(bot.inventory) and sending immediately — no queue.
                    // Hypixel and COFL are separate entities; inventory upload never needs to
                    // wait for a Hypixel command slot, so we do the same here.
                    info!("COFL requested getInventory — sending cached inventory");
                    if let Some(inv_json) = bot_client_for_ws.get_cached_inventory_json() {
                        let payload_bytes = inv_json.len();
                        debug!("[Inventory] Uploading to COFL: payload {} bytes", payload_bytes);
                        info!("[Inventory] uploadInventory payload: {}", inv_json);
                        let message = serde_json::json!({
                            "type": "uploadInventory",
                            "data": inv_json
                        }).to_string();
                        let ws = ws_client_clone.clone();
                        tokio::spawn(async move {
                            if let Err(e) = ws.send_message(&message).await {
                                error!("Failed to upload inventory to websocket: {}", e);
                            } else {
                                info!("Uploaded inventory to COFL ({} bytes)", payload_bytes);
                            }
                        });
                    } else {
                        warn!("getInventory received but no cached inventory yet — ignoring");
                    }
                }
                CoflEvent::TradeResponse => {
                    debug!("Processing tradeResponse - clicking accept button");
                    // TypeScript: clicks slot 39 after checking for "Deal!" or "Warning!"
                    // Sleep is handled in TypeScript before clicking - we'll do the same
                    command_queue_clone.enqueue(
                        CommandType::ClickSlot { slot: 39 },
                        CommandPriority::High,
                        false,
                    );
                }
                CoflEvent::PrivacySettings(data) => {
                    // TypeScript stores this in bot.privacySettings
                    debug!("Received privacySettings: {}", data);
                }
                CoflEvent::SwapProfile(profile_name) => {
                    info!("Processing swapProfile request: {}", profile_name);
                    command_queue_clone.enqueue(
                        CommandType::SwapProfile { profile_name },
                        CommandPriority::High,
                        false,
                    );
                }
                CoflEvent::CreateAuction(data) => {
                    info!("Processing createAuction request");
                    // Parse the auction data
                    match serde_json::from_str::<serde_json::Value>(&data) {
                        Ok(auction_data) => {
                            // Field is "price" in COFL protocol (not "startingBid")
                            let item_raw = auction_data.get("itemName").and_then(|v| v.as_str());
                            let price = auction_data.get("price").and_then(|v| v.as_u64());
                            let duration = auction_data.get("duration").and_then(|v| v.as_u64());
                            // Also extract slot (mineflayer inventory slot 9-44) and id
                            let item_slot = auction_data.get("slot").and_then(|v| v.as_u64());
                            let item_id = auction_data.get("id").and_then(|v| v.as_str()).map(|s| s.to_string());
                            match (item_raw, price, duration) {
                                (Some(item_raw), Some(price), Some(duration)) => {
                                    // Strip Minecraft color codes (§X) from item name
                                    let item_name = frikadellen_baf::utils::remove_minecraft_colors(item_raw);
                                    let cmd = CommandType::SellToAuction {
                                        item_name,
                                        starting_bid: price,
                                        duration_hours: duration,
                                        item_slot,
                                        item_id,
                                    };
                                    // If bazaar flips are paused (AH flip window active), defer
                                    // listing until the window ends so the listing flow does not
                                    // race with ongoing AH purchases.
                                    if bazaar_flips_paused_ws.load(Ordering::Relaxed) {
                                        info!("[createAuction] AH flip window active — deferring listing until bazaar flips resume");
                                        let flag = bazaar_flips_paused_ws.clone();
                                        let queue = command_queue_clone.clone();
                                        tokio::spawn(async move {
                                            let deadline = tokio::time::Instant::now()
                                                + tokio::time::Duration::from_secs(30);
                                            loop {
                                                sleep(Duration::from_millis(250)).await;
                                                if !flag.load(Ordering::Relaxed)
                                                    || tokio::time::Instant::now() >= deadline
                                                {
                                                    break;
                                                }
                                            }
                                            info!("[createAuction] Deferral complete — enqueueing SellToAuction");
                                            queue.enqueue(cmd, CommandPriority::High, false);
                                        });
                                    } else {
                                        command_queue_clone.enqueue(cmd, CommandPriority::High, false);
                                    }
                                }
                                _ => {
                                    warn!("createAuction missing required fields (itemName, price, duration): {}", data);
                                }
                            }
                        }
                        Err(e) => {
                            error!("Failed to parse createAuction JSON: {}", e);
                        }
                    }
                }
                CoflEvent::Trade(data) => {
                    debug!("Processing trade request");
                    // Parse trade data to get player name
                    if let Ok(trade_data) = serde_json::from_str::<serde_json::Value>(&data) {
                        if let Some(player) = trade_data.get("playerName").and_then(|v| v.as_str()) {
                            command_queue_clone.enqueue(
                                CommandType::AcceptTrade {
                                    player_name: player.to_string(),
                                },
                                CommandPriority::High,
                                false,
                            );
                        } else {
                            warn!("Failed to parse trade data: {}", data);
                        }
                    }
                }
                CoflEvent::RunSequence(data) => {
                    debug!("Received runSequence: {}", data);
                    warn!("runSequence is not yet fully implemented");
                }
                CoflEvent::Countdown => {
                    // COFL sends this ~10 seconds before AH flips arrive.
                    // Matching TypeScript bazaarFlipPauser.ts: pause bazaar flips for 20 seconds
                    // when both AH flips and bazaar flips are enabled.
                    if config_clone.enable_bazaar_flips && config_clone.enable_ah_flips {
                        print_mc_chat("§f[§4BAF§f]: §cAH Flips incoming, pausing bazaar flips");
                        let flag = bazaar_flips_paused_ws.clone();
                        flag.store(true, Ordering::Relaxed);
                        let ws = ws_client_clone.clone();
                        let enable_bz = config_clone.enable_bazaar_flips;
                        tokio::spawn(async move {
                            sleep(Duration::from_secs(20)).await;
                            flag.store(false, Ordering::Relaxed);
                            // Notify user that bazaar flips are resuming (matching TypeScript bazaarFlipPauser.ts)
                            print_mc_chat("§f[§4BAF§f]: §aBazaar flips resumed, requesting new recommendations...");
                            info!("[BazaarFlips] Bazaar flips resumed after AH flip window");
                            // Re-request bazaar flips to get fresh recommendations after the pause
                            if enable_bz {
                                let msg = serde_json::json!({
                                    "type": "getbazaarflips",
                                    "data": serde_json::to_string("").unwrap_or_default()
                                }).to_string();
                                if let Err(e) = ws.send_message(&msg).await {
                                    error!("Failed to request bazaar flips after AH flip pause: {}", e);
                                } else {
                                    debug!("[BazaarFlips] Requested fresh bazaar flips after AH flip window");
                                }
                            }
                        });
                    }
                }
            }
        }

        warn!("WebSocket event loop ended");
    });

    // Spawn command processor
    let command_queue_processor = command_queue.clone();
    let bot_client_clone = bot_client.clone();
    let bazaar_flips_paused_proc = bazaar_flips_paused.clone();
    let command_delay_ms = config.command_delay_ms;
    tokio::spawn(async move {
        use frikadellen_baf::types::BotState;
        loop {
            // Process commands from queue
            if let Some(cmd) = command_queue_processor.start_current() {
                debug!("Processing command: {:?}", cmd.command_type);

                // Bazaar-related commands are silently dropped while the AH flip window
                // is active (bazaar_flips_paused = true). This covers BazaarBuyOrder,
                // BazaarSellOrder, and ManageOrders.
                let is_bazaar_related = matches!(
                    cmd.command_type,
                    frikadellen_baf::types::CommandType::BazaarBuyOrder { .. }
                    | frikadellen_baf::types::CommandType::BazaarSellOrder { .. }
                    | frikadellen_baf::types::CommandType::ManageOrders
                );
                if is_bazaar_related && bazaar_flips_paused_proc.load(Ordering::Relaxed) {
                    debug!("[Queue] Dropping bazaar command {:?} — AH flip window active", cmd.command_type);
                    command_queue_processor.complete_current();
                    sleep(Duration::from_millis(50)).await;
                    continue;
                }

                // Send command to bot for execution
                if let Err(e) = bot_client_clone.send_command(cmd.clone()) {
                    warn!("Failed to send command to bot: {}", e);
                }

                // Per-command-type timeout: how long to wait for the bot to leave the
                // busy state before declaring it stuck and forcing a reset.
                let timeout_secs: u64 = match cmd.command_type {
                    frikadellen_baf::types::CommandType::ClaimPurchasedItem
                    | frikadellen_baf::types::CommandType::ClaimSoldItem
                    | frikadellen_baf::types::CommandType::CheckCookie
                    | frikadellen_baf::types::CommandType::ManageOrders => 60,
                    frikadellen_baf::types::CommandType::BazaarBuyOrder { .. }
                    | frikadellen_baf::types::CommandType::BazaarSellOrder { .. } => 20,
                    frikadellen_baf::types::CommandType::SellToAuction { .. } => 15,
                    _ => 10,
                };

                // Poll until the bot returns to an allows_commands() state or we hit the
                // per-type timeout. A single loop replaces the previous per-type if/else chain.
                let deadline = std::time::Instant::now()
                    + std::time::Duration::from_secs(timeout_secs);
                loop {
                    sleep(Duration::from_millis(250)).await;
                    if bot_client_clone.state().allows_commands()
                        || std::time::Instant::now() >= deadline
                    {
                        break;
                    }
                }

                // Safety reset: if the bot is still in a busy state after the timeout,
                // force it back to Idle so the queue can continue.
                if !bot_client_clone.state().allows_commands() {
                    warn!(
                        "[Queue] Command {:?} timed out after {}s — forcing Idle",
                        cmd.command_type, timeout_secs
                    );
                    bot_client_clone.set_state(BotState::Idle);
                }

                command_queue_processor.complete_current();

                // Always wait the configurable inter-command delay so Hypixel interactions
                // don't run back-to-back.
                sleep(Duration::from_millis(command_delay_ms)).await;
            }
            
            // Small delay to prevent busy loop
            sleep(Duration::from_millis(50)).await;
        }
    });

    // Bot will complete its startup sequence automatically
    // The state will transition from Startup -> Idle after initialization
    info!("BAF initialization started - waiting for bot to complete setup...");

    // Set up console input handler for commands
    info!("Console interface ready - type commands and press Enter:");
    info!("  /cofl <command> - Send command to COFL websocket");
    info!("  /<command> - Send command to Minecraft");
    info!("  <text> - Send chat message to COFL websocket");
    
    // Spawn console input handler
    let ws_client_for_console = ws_client.clone();
    let command_queue_for_console = command_queue.clone();
    
    tokio::spawn(async move {
        use tokio::io::{AsyncBufReadExt, BufReader};
        use tokio::io::stdin;
        
        let stdin = stdin();
        let reader = BufReader::new(stdin);
        let mut lines = reader.lines();
        
        while let Ok(Some(line)) = lines.next_line().await {
            let input = line.trim();
            if input.is_empty() {
                continue;
            }
            
            let lowercase_input = input.to_lowercase();
            
            // Handle /cofl and /baf commands (matching TypeScript consoleHandler.ts)
            if lowercase_input.starts_with("/cofl") || lowercase_input.starts_with("/baf") {
                let parts: Vec<&str> = input.split_whitespace().collect();
                if parts.len() > 1 {
                    let command = parts[1];
                    let args = parts[2..].join(" ");
                    
                    // Handle locally-processed commands (matching TypeScript consoleHandler.ts)
                    match command.to_lowercase().as_str() {
                        "queue" => {
                            // Show command queue status
                            let depth = command_queue_for_console.len();
                            info!("━━━━━━━ Command Queue Status ━━━━━━━");
                            info!("Queue depth: {}", depth);
                            info!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
                            continue;
                        }
                        "clearqueue" => {
                            // Clear command queue
                            command_queue_for_console.clear();
                            info!("Command queue cleared");
                            continue;
                        }
                        // TODO: Add other local commands like forceClaim, connect, sellbz when implemented
                        _ => {
                            // Fall through to send to websocket
                        }
                    }
                    
                    // Send to websocket with command as type
                    // Match TypeScript: data field must be JSON-stringified (double-encoded)
                    let data_json = match serde_json::to_string(&args) {
                        Ok(json) => json,
                        Err(e) => {
                            error!("Failed to serialize command args: {}", e);
                            "\"\"".to_string()
                        }
                    };
                    let message = serde_json::json!({
                        "type": command,
                        "data": data_json  // JSON-stringified to match TypeScript JSON.stringify()
                    }).to_string();
                    
                    if let Err(e) = ws_client_for_console.send_message(&message).await {
                        error!("Failed to send command to websocket: {}", e);
                    } else {
                        info!("Sent command to COFL: {} {}", command, args);
                    }
                } else {
                    // Bare /cofl or /baf command - send as chat type with empty data
                    let data_json = serde_json::to_string("").unwrap();
                    let message = serde_json::json!({
                        "type": "chat",
                        "data": data_json
                    }).to_string();
                    
                    if let Err(e) = ws_client_for_console.send_message(&message).await {
                        error!("Failed to send bare /cofl command to websocket: {}", e);
                    }
                }
            } 
            // Handle other slash commands - send to Minecraft
            else if input.starts_with('/') {
                command_queue_for_console.enqueue(
                    frikadellen_baf::types::CommandType::SendChat { 
                        message: input.to_string() 
                    },
                    frikadellen_baf::types::CommandPriority::High,
                    false,
                );
                info!("Queued Minecraft command: {}", input);
            }
            // Non-slash messages go to websocket as chat (matching TypeScript)
            else {
                // Match TypeScript: data field must be JSON-stringified
                let data_json = match serde_json::to_string(&input) {
                    Ok(json) => json,
                    Err(e) => {
                        error!("Failed to serialize chat message: {}", e);
                        "\"\"".to_string()
                    }
                };
                let message = serde_json::json!({
                    "type": "chat",
                    "data": data_json  // JSON-stringified to match TypeScript JSON.stringify()
                }).to_string();
                
                if let Err(e) = ws_client_for_console.send_message(&message).await {
                    error!("Failed to send chat to websocket: {}", e);
                } else {
                    debug!("Sent chat to COFL: {}", input);
                }
            }
        }
    });
    
    // Periodic bazaar flip requests every 5 minutes (matching TypeScript startBazaarFlipRequests)
    if config.enable_bazaar_flips {
        let ws_client_periodic = ws_client.clone();
        let bot_client_periodic = bot_client.clone();
        tokio::spawn(async move {
            loop {
                sleep(Duration::from_secs(300)).await; // 5 minutes
                if bot_client_periodic.state().allows_commands() {
                    let msg = serde_json::json!({
                        "type": "getbazaarflips",
                        "data": serde_json::to_string("").unwrap_or_default()
                    }).to_string();
                    if let Err(e) = ws_client_periodic.send_message(&msg).await {
                        error!("Failed to send periodic getbazaarflips: {}", e);
                    } else {
                        debug!("[BazaarFlips] Auto-requested bazaar flips (periodic)");
                    }
                }
            }
        });
    }

    // Periodic scoreboard upload every 5 seconds (matching TypeScript setInterval purse update)
    {
        let ws_client_scoreboard = ws_client.clone();
        let bot_client_scoreboard = bot_client.clone();
        tokio::spawn(async move {
            loop {
                sleep(Duration::from_secs(5)).await;
                if bot_client_scoreboard.state().allows_commands() {
                    let scoreboard_lines = bot_client_scoreboard.get_scoreboard_lines();
                    if !scoreboard_lines.is_empty() {
                        let data_json = serde_json::to_string(&scoreboard_lines).unwrap_or_else(|_| "[]".to_string());
                        let msg = serde_json::json!({"type": "uploadScoreboard", "data": data_json}).to_string();
                        if let Err(e) = ws_client_scoreboard.send_message(&msg).await {
                            debug!("Failed to send periodic scoreboard upload: {}", e);
                        } else {
                            debug!("[Scoreboard] Uploaded to COFL: {:?}", scoreboard_lines);
                        }
                    }
                }
            }
        });
    }

    // Periodic bazaar order check — collect filled orders and cancel stale ones.
    // Driven by config.bazaar_order_check_interval_seconds (default 30s).
    if config.enable_bazaar_flips {
        let bot_client_orders = bot_client.clone();
        let command_queue_orders = command_queue.clone();
        let order_interval = config.bazaar_order_check_interval_seconds;
        tokio::spawn(async move {
            use frikadellen_baf::types::{CommandType, CommandPriority};
            // Give startup workflow time to complete before starting periodic checks
            sleep(Duration::from_secs(120)).await;
            loop {
                sleep(Duration::from_secs(order_interval)).await;
                if bot_client_orders.state().allows_commands() {
                    debug!("[BazaarOrders] Periodic order check triggered (every {}s)", order_interval);
                    command_queue_orders.enqueue(
                        CommandType::ManageOrders,
                        CommandPriority::Normal,
                        false,
                    );
                }
            }
        });
    }

    // Island guard: if "Your Island" is not in the scoreboard, send
    // /lobby → /play sb → /is to return to the island.
    // Matching TypeScript AFKHandler.ts tryToTeleportToIsland() logic.
    {
        let bot_client_island = bot_client.clone();
        let command_queue_island = command_queue.clone();
        tokio::spawn(async move {
            use frikadellen_baf::types::{CommandType, CommandPriority, BotState};

            // Give the startup workflow time to complete before we start checking.
            sleep(Duration::from_secs(60)).await;

            loop {
                sleep(Duration::from_secs(10)).await;

                // Don't interfere during startup / order-management workflows.
                if matches!(
                    bot_client_island.state(),
                    BotState::Startup | BotState::ManagingOrders
                ) {
                    continue;
                }

                let lines = bot_client_island.get_scoreboard_lines();

                // Scoreboard not yet populated — skip until it has data.
                if lines.is_empty() {
                    continue;
                }

                // If "Your Island" is in the sidebar we are home — nothing to do.
                if lines.iter().any(|l| l.contains("Your Island")) {
                    continue;
                }

                // Not on island — send the return sequence.
                print_mc_chat(
                    "§f[§4BAF§f]: §eNot detected on island — returning to island...",
                );
                info!("[AFKHandler] Not on island — sending /lobby → /play sb → /is");

                for msg in ["/lobby", "/play sb", "/is"] {
                    command_queue_island.enqueue(
                        CommandType::SendChat { message: msg.to_string() },
                        CommandPriority::High,
                        false,
                    );
                }

                // Wait for the navigation to complete before checking again.
                sleep(Duration::from_secs(30)).await;
            }
        });
    }

    // Keep the application running
    info!("BAF is now running. Type commands below or press Ctrl+C to exit.");
    
    // Wait indefinitely
    loop {
        sleep(Duration::from_secs(60)).await;
        debug!("Status: {} commands in queue", command_queue.len());
    }
}

#[cfg(test)]
mod tests {
    use super::is_ban_disconnect;

    #[test]
    fn detects_temporary_ban_disconnect() {
        assert!(is_ban_disconnect("You are temporarily banned for 29d from this server!"));
    }

    #[test]
    fn detects_ban_id_disconnect() {
        assert!(is_ban_disconnect("Disconnect reason ... Ban ID: #692672FA"));
    }

    #[test]
    fn detects_permanent_ban_disconnect() {
        assert!(is_ban_disconnect("You are permanently banned from this server!"));
    }

    #[test]
    fn ignores_non_ban_disconnect() {
        assert!(!is_ban_disconnect("Disconnected: Timed out"));
    }
}
