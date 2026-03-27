use anyhow::Result;
use dialoguer::{Input, Confirm};
use rustyline;
use frikadellen_baf::{
    config::ConfigLoader,
    logging::{init_logger, print_mc_chat},
    state::CommandQueue,
    websocket::CoflWebSocket,
    bot::BotClient,
    types::Flip,
    web::{start_web_server, WebSharedState},
};
use tracing::{debug, error, info, warn};
use tokio::time::{sleep, Duration};
use tokio::sync::broadcast;
use serde_json;
use std::sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}};
use std::collections::HashMap;
use std::time::Instant;
use frikadellen_baf::utils::restart_process;

const VERSION: &str = "af-3.0";
const PERIODIC_AH_CLAIM_CHECK_INTERVAL_SECS: u64 = 300;

/// Base delay per consecutive rejoin attempt (seconds).
const REJOIN_BACKOFF_BASE_SECS: u64 = 60;
/// Maximum backoff delay between rejoin attempts (seconds).
const REJOIN_MAX_BACKOFF_SECS: u64 = 300;
/// After this many consecutive rejoin attempts the counter resets so the
/// backoff does not grow unbounded.
const REJOIN_MAX_ATTEMPTS: u32 = 5;

/// Debounce delay before displaying the `/cofl bz l` profit summary (seconds).
/// Coflnet sends each flip as a separate chat message; we wait for the full list
/// to arrive before computing and displaying the total.
const BZ_LIST_DEBOUNCE_SECS: u64 = 2;

/// Delay (seconds) before sending `/cofl bz l` after a SELL order is filled.
/// Coflnet needs a brief window to register the completed flip in its database
/// before the list is requested; 3 seconds covers typical processing latency.
const BZ_LIST_REQUEST_DELAY_SECS: u64 = 3;

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

/// Format an f64 coin amount with comma separators, preserving one decimal
/// digit when the fractional part is non-zero (e.g. 600000.5 → "600,000.5").
fn format_coins_f64(amount: f64) -> String {
    let tenths = (amount * 10.0).round() as i64;
    let int_part = tenths / 10;
    let frac_digit = (tenths % 10).abs();
    let int_str = format_coins(int_part);
    if frac_digit == 0 {
        int_str
    } else {
        format!("{}.{}", int_str, frac_digit)
    }
}

fn is_ban_disconnect(reason: &str) -> bool {
    let lower = reason.to_ascii_lowercase();
    lower.contains("temporarily banned")
        || lower.contains("permanently banned")
        || lower.contains("ban id:")
}

/// Parse a Coflnet `/cofl profit` response and return the total profit in coins.
///
/// Expected format (color-stripped):
/// `"According to our data <ign> made <amount> in the last <days> days across <N> auctions"`
///
/// `<amount>` may be a short notation like `82.7M`, `1.5B`, `250K`, or a plain number.
fn parse_cofl_profit_response(clean_msg: &str) -> Option<i64> {
    let rest = clean_msg.strip_prefix("According to our data ")?;
    let made_idx = rest.find(" made ")?;
    let after_made = &rest[made_idx + 6..];
    let end = after_made.find(" in the last ")?;
    let amount_str = after_made[..end].trim();
    parse_short_number(amount_str)
}

/// Parse a human-readable short number like `82.7M`, `1.5B`, `250K`, or `500`.
fn parse_short_number(s: &str) -> Option<i64> {
    let s = s.replace(',', "");
    let (num_part, multiplier) = if let Some(n) = s.strip_suffix('B').or_else(|| s.strip_suffix('b')) {
        (n, 1_000_000_000f64)
    } else if let Some(n) = s.strip_suffix('M').or_else(|| s.strip_suffix('m')) {
        (n, 1_000_000f64)
    } else if let Some(n) = s.strip_suffix('K').or_else(|| s.strip_suffix('k')) {
        (n, 1_000f64)
    } else {
        (s.as_str(), 1f64)
    };
    let val: f64 = num_part.parse().ok()?;
    Some((val * multiplier) as i64)
}

/// Parse a single flip line from `/cofl bz l` output and return the profit.
///
/// Expected format (color-stripped):
///   `"2xJungle Key: 1.05M -> 287K => -768K(1)"`
///   `"128xWorm Membrane: 7.16M -> 7.91M => 741K(7)"`
///
/// The profit is the value between `=> ` and `(`.
fn parse_bz_list_flip_profit(line: &str) -> Option<i64> {
    let arrow_idx = line.find("=> ")?;
    let after_arrow = &line[arrow_idx + 3..];
    let paren_idx = after_arrow.find('(')?;
    let profit_str = after_arrow[..paren_idx].trim();
    parse_short_number(profit_str)
}

/// Parse a single flip line from `/cofl bz l` output and return item name,
/// profit, and flip count.
///
/// Expected format (color-stripped):
///   `"2xJungle Key: 1.05M -> 287K => -768K(1)"`
///   `"128xWorm Membrane: 7.16M -> 7.91M => 741K(7)"`
///
/// Returns `(item_name, profit, flip_count)`.
fn parse_bz_list_flip_detail(line: &str) -> Option<(String, i64, u32)> {
    // Amount prefix: digits before 'x'
    let x_idx = line.find('x')?;
    let amount_str = line[..x_idx].trim();
    if amount_str.is_empty() || !amount_str.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let rest = &line[x_idx + 1..];
    let colon_idx = rest.find(':')?;
    let item_name = rest[..colon_idx].trim().to_string();
    if item_name.is_empty() {
        return None;
    }

    // Profit: between "=> " and "("
    let arrow_idx = rest.find("=> ")?;
    let after_arrow = &rest[arrow_idx + 3..];
    let paren_idx = after_arrow.find('(')?;
    let profit_str = after_arrow[..paren_idx].trim();
    let profit = parse_short_number(profit_str)?;

    // Flip count: between "(" and ")"
    let after_paren = &after_arrow[paren_idx + 1..];
    let close_paren = after_paren.find(')')?;
    let count: u32 = after_paren[..close_paren].trim().parse().ok()?;

    Some((item_name, profit, count))
}

fn should_enqueue_periodic_auction_claim(
    bot_state: frikadellen_baf::types::BotState,
    queue_empty: bool,
) -> bool {
    bot_state.allows_commands() && queue_empty
}

fn should_drop_bazaar_command_during_ah_pause(
    command_type: &frikadellen_baf::types::CommandType,
    bazaar_flips_paused: bool,
) -> bool {
    bazaar_flips_paused
        && matches!(
            command_type,
            frikadellen_baf::types::CommandType::BazaarBuyOrder { .. }
            | frikadellen_baf::types::CommandType::ManageOrders { .. }
        )
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
    let config_loader = Arc::new(ConfigLoader::new());
    let mut config = config_loader.load()?;

    // Prompt for username if not set
    if config.ingame_name.is_none() {
        let name: String = Input::new()
            .with_prompt("Enter your ingame name(s) (comma-separated for multiple accounts)")
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

    // Prompt for Discord ID if not yet configured (for pinging on legendary/divine flips and bans)
    if config.discord_id.is_none() {
        let wants_discord_id = Confirm::new()
            .with_prompt("Configure Discord user ID for ping notifications? (optional)")
            .default(false)
            .interact()?;
        if wants_discord_id {
            let id: String = Input::new()
                .with_prompt("Enter your Discord user ID")
                .interact_text()?;
            config.discord_id = Some(id);
        } else {
            config.discord_id = Some(String::new());
        }
        config_loader.save(&config)?;
    }

    // Resolve the active ingame name.
    // When multiple names are configured, the account index is advanced at runtime by the
    // account-switching timer (see below) and the process restarts with exit(0) so that an
    // external supervisor (systemd, a shell loop, etc.) launches the next iteration.
    // We persist the current index in a small sidecar file next to the config so the next
    // invocation knows which account to start with.
    let ingame_names = config.ingame_names();
    if ingame_names.is_empty() {
        anyhow::bail!("No ingame name configured — please set ingame_name in config.toml");
    }

    // Read and advance the stored account index (wraps around the list).
    let account_index_path = match std::env::current_exe() {
        Ok(p) => p.parent().map(|d| d.join("account_index")).unwrap_or_else(|| std::path::PathBuf::from("account_index")),
        Err(_) => std::path::PathBuf::from("account_index"),
    };

    let current_account_index: usize = if ingame_names.len() > 1 {
        match std::fs::read_to_string(&account_index_path) {
            Ok(s) => s.trim().parse::<usize>().unwrap_or(0) % ingame_names.len(),
            Err(_) => 0,
        }
    } else {
        0
    };

    let ingame_name = ingame_names[current_account_index].clone();

    info!("Configuration loaded for player: {} (account {}/{})", ingame_name, current_account_index + 1, ingame_names.len());
    info!("AH Flips: {}", if config.enable_ah_flips { "ENABLED" } else { "DISABLED" });
    info!("Bazaar Flips: {}", if config.enable_bazaar_flips { "ENABLED" } else { "DISABLED" });
    info!("Web GUI Port: {}", config.web_gui_port);

    if config.proxy_enabled {
        info!("Proxy: ENABLED — address: {:?}", config.proxy_address);
    }

    // Initialize command queue
    let command_queue = CommandQueue::new();

    // Bazaar-flip pause flag (matches TypeScript bazaarFlipPauser.ts).
    // Set to true for 20 seconds when a `countdown` message arrives (AH flips incoming).
    let bazaar_flips_paused = Arc::new(AtomicBool::new(false));

    // Master macro pause — web panel can set this to pause all command processing.
    let macro_paused = Arc::new(AtomicBool::new(false));

    // Shared enable flags — web panel can toggle these at runtime.
    let enable_ah_flips = Arc::new(AtomicBool::new(config.enable_ah_flips));
    let enable_bazaar_flips = Arc::new(AtomicBool::new(config.enable_bazaar_flips));
    let anonymize_webhook_name = Arc::new(AtomicBool::new(false));

    // Broadcast channel for chat messages → web panel clients.
    let (chat_tx, _chat_rx) = broadcast::channel::<String>(256);

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

    // Auto-detected COFL license index for the first account's IGN.
    // Populated at startup by requesting `/cofl licenses list <first_ign>` and
    // parsing the `N>` numbered index from the response.
    // 0 means no license detected.
    let detected_cofl_license = Arc::new(std::sync::atomic::AtomicU32::new(0));

    // Coflnet authentication flag — set to true when the COFL "Hello <IGN>"
    // message is received. Flip processing is blocked until this is true so the
    // bot does not attempt purchases before COFL auth is complete.
    let cofl_authenticated = Arc::new(AtomicBool::new(false));

    // Set once after auto-sending `/cofl license default <ign>` to prevent repeat attempts.
    let license_default_sent = Arc::new(AtomicBool::new(false));

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

    // When multi-account is enabled, request the COFL licenses list at startup
    // searching by the current account's IGN so we get its global license index.
    // Delay slightly to let the WS authenticate first.
    if ingame_names.len() > 1 {
        let ws_license = ws_client.clone();
        let current_ign = ingame_name.clone();
        tokio::spawn(async move {
            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
            let args = format!("list {}", current_ign);
            let data_json = serde_json::json!(args).to_string();
            let message = serde_json::json!({
                "type": "licenses",
                "data": data_json
            }).to_string();
            if let Err(e) = ws_license.send_message(&message).await {
                warn!("[LicenseDetect] Failed to request licenses list: {}", e);
            } else {
                info!("[LicenseDetect] Requested COFL licenses list for '{}'", current_ign);
            }
        });
    }

    // Initialize bot client (not connected yet — web server starts first so
    // the chat GUI is available during Microsoft auth)
    let mut bot_client = BotClient::new();
    bot_client.set_auto_cookie_hours(config.auto_cookie);
    bot_client.freemoney = config.freemoney_enabled();
    bot_client.fastbuy = config.fastbuy_enabled();
    bot_client.bed_spam_click_delay = config.bed_spam_click_delay;
    bot_client.bed_pre_click_ms = config.bed_pre_click_ms;
    bot_client.bazaar_order_cancel_minutes_per_million = config.bazaar_order_cancel_minutes_per_million;
    bot_client.bazaar_flips_paused = bazaar_flips_paused.clone();
    bot_client.enable_bazaar_flips = enable_bazaar_flips.clone();
    bot_client.set_command_queue(command_queue.clone());
    *bot_client.ingame_name.write() = ingame_name.clone();

    // Shared profit tracker for AH and Bazaar realized profits.
    let profit_tracker = Arc::new(frikadellen_baf::profit::ProfitTracker::new());

    // Shared tracker for active bazaar orders (web panel + profit calculation).
    let bazaar_tracker = Arc::new(frikadellen_baf::bazaar_tracker::BazaarOrderTracker::new());

    // Start web control panel server BEFORE bot connect so the chat GUI
    // is available to show login links during Microsoft/Coflnet auth.
    {
        let web_state = WebSharedState {
            bot_client: bot_client.clone(),
            command_queue: command_queue.clone(),
            ws_client: ws_client.clone(),
            bazaar_flips_paused: bazaar_flips_paused.clone(),
            macro_paused: macro_paused.clone(),
            enable_ah_flips: enable_ah_flips.clone(),
            enable_bazaar_flips: enable_bazaar_flips.clone(),
            ingame_names: ingame_names.clone(),
            current_account_index,
            account_index_path: account_index_path.clone(),
            chat_tx: chat_tx.clone(),
            web_gui_password: config.web_gui_password.clone(),
            valid_sessions: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashSet::new(),
            )),
            player_uuid: std::sync::Arc::new(tokio::sync::RwLock::new(None)),
            started_at: std::time::Instant::now(),
            hypixel_api_key: config.hypixel_api_key.clone(),
            detected_cofl_license: detected_cofl_license.clone(),
            profit_tracker: profit_tracker.clone(),
            anonymize_webhook_name: anonymize_webhook_name.clone(),
            bazaar_tracker: bazaar_tracker.clone(),
            config_loader: config_loader.clone(),
        };
        let web_port = config.web_gui_port;
        tokio::spawn(async move {
            start_web_server(web_state, web_port).await;
        });
    }

    // Connect to Hypixel — Azalea will handle Microsoft OAuth (device-code URL
    // is printed to the terminal; the Coflnet auth link is sent via chat_tx and
    // appears in the web panel automatically).
    //
    // Retry with exponential backoff on auth failure.  Running without a
    // Minecraft connection is useless (no flips, no bazaar, nothing to do),
    // so after exhausting retries we restart the process to re-run the full
    // startup sequence (config reload, fresh WebSocket, etc.).
    info!("Initializing Minecraft bot...");
    info!("Authenticating with Microsoft account...");
    info!("A browser window will open for you to log in");
    {
        const AUTH_MAX_RETRIES: u32 = 3;
        const AUTH_INITIAL_BACKOFF_SECS: u64 = 10;

        let mut last_err: Option<String> = None;
        for attempt in 1..=AUTH_MAX_RETRIES {
            match bot_client.connect(ingame_name.clone(), Some(ws_client.clone())).await {
                Ok(_) => {
                    info!("Bot connection initiated successfully");
                    last_err = None;
                    break;
                }
                Err(e) => {
                    let backoff = AUTH_INITIAL_BACKOFF_SECS.saturating_mul(1u64 << (attempt - 1).min(5)); // 10s, 20s, 40s for 3 retries; .min(5) caps shift for safety
                    warn!(
                        "Failed to connect bot (attempt {}/{}): {} — retrying in {}s",
                        attempt, AUTH_MAX_RETRIES, e, backoff
                    );
                    let baf_msg = format!(
                        "§f[§4BAF§f]: §cAuth failed (attempt {}/{}): {} — retrying in {}s",
                        attempt, AUTH_MAX_RETRIES, e, backoff
                    );
                    print_mc_chat(&baf_msg);
                    let _ = chat_tx.send(baf_msg);
                    last_err = Some(format!("{}", e));
                    tokio::time::sleep(Duration::from_secs(backoff)).await;
                }
            }
        }
        if let Some(err) = last_err {
            error!(
                "All {} auth attempts failed (last error: {}) — restarting process",
                AUTH_MAX_RETRIES, err
            );
            let baf_msg = format!(
                "§f[§4BAF§f]: §cAll {} auth attempts failed — restarting...",
                AUTH_MAX_RETRIES
            );
            print_mc_chat(&baf_msg);
            let _ = chat_tx.send(baf_msg);
            // Short delay so the message is visible before restart.
            tokio::time::sleep(Duration::from_secs(2)).await;
            restart_process();
        }
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
    let chat_tx_events = chat_tx.clone();
    let enable_bazaar_flips_events = enable_bazaar_flips.clone();
    let profit_tracker_events = profit_tracker.clone();
    let bazaar_tracker_events = bazaar_tracker.clone();
    let session_start = std::time::Instant::now();
    tokio::spawn(async move {
        while let Some(event) = bot_client_clone.next_event().await {
            match event {
                frikadellen_baf::bot::BotEvent::Login => {
                    info!("✓ Bot logged into Minecraft successfully");
                }
                frikadellen_baf::bot::BotEvent::Spawn => {
                    info!("✓ Bot spawned in world and ready");
                }
                frikadellen_baf::bot::BotEvent::ChatMessage(msg) => {
                    // Print Minecraft chat with color codes converted to ANSI
                    print_mc_chat(&msg);
                    // Broadcast to web panel clients
                    let _ = chat_tx_events.send(msg.clone());

                    // Parse Coflnet profit response:
                    // "According to our data <ign> made <amount> in the last <days> days across <N> auctions"
                    let clean = frikadellen_baf::utils::remove_minecraft_colors(&msg);
                    if let Some(profit) = parse_cofl_profit_response(&clean) {
                        profit_tracker_events.set_ah_total(profit);
                        tracing::info!("[CoflProfit] Updated AH total from Coflnet: {} coins", profit);
                    }
                }
                frikadellen_baf::bot::BotEvent::WindowOpen(id, window_type, title) => {
                    debug!("Window opened: {} (ID: {}, Type: {})", title, id, window_type);
                }
                frikadellen_baf::bot::BotEvent::WindowClose => {
                    debug!("Window closed");
                }
                frikadellen_baf::bot::BotEvent::Disconnected(reason) => {
                    warn!("Bot disconnected: {}", reason);
                    if is_ban_disconnect(&reason) {
                        if let Some(webhook_url) = config_for_events.active_webhook_url() {
                            let url = webhook_url.to_string();
                            let name = ingame_name_for_events.clone();
                            let ban_reason = reason.clone();
                            let did = config_for_events.active_discord_id().map(|s| s.to_string());
                            tokio::spawn(async move {
                                frikadellen_baf::webhook::send_webhook_banned(&name, &ban_reason, did.as_deref(), &url).await;
                            });
                        }
                    }
                }
                frikadellen_baf::bot::BotEvent::Kicked(reason) => {
                    warn!("Bot kicked: {}", reason);
                }
                frikadellen_baf::bot::BotEvent::StartupComplete { orders_cancelled } => {
                    info!("[Startup] Startup complete - bot is ready to flip! ({} order(s) cancelled)", orders_cancelled);
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
                    // Buy speed comes from the event (flip received → escrow message).
                    let (opt_target, opt_profit, colored_name, opt_auction_uuid, opt_finder) = {
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
                                    let finder = entry.0.finder.clone();
                                    (Some(target), Some(expected_profit), entry.0.item_name.clone(), uuid, finder)
                                } else {
                                    (None, None, item_name.clone(), None, None)
                                }
                            }
                            Err(e) => {
                                warn!("Flip tracker lock failed at ItemPurchased: {}", e);
                                (None, None, item_name.clone(), None, None)
                            }
                        }
                    };
                    // Print colorful purchase announcement (item rarity shown via color code)
                    let profit_str = opt_profit.map(|p| {
                        let color = if p >= 0 { "§a" } else { "§c" };
                        format!(" §7| Expected profit: {}{}§r", color, format_coins(p))
                    }).unwrap_or_default();
                    let speed_str = event_buy_speed_ms.map(|ms| format!(" §7| Buy speed: §e{}ms§r", ms)).unwrap_or_default();
                    let baf_msg = format!(
                        "§f[§4BAF§f]: §a✦ PURCHASED §r{}§r §7for §6{}§7 coins!{}{}",
                        colored_name, format_coins(price as i64), profit_str, speed_str
                    );
                    print_mc_chat(&baf_msg);
                    let _ = chat_tx_events.send(baf_msg);
                    // Send webhook: for legendary/divine flips, send the styled
                    // webhook (with ping + color) instead of the regular purchase one.
                    let is_legendary_flip = opt_profit.map_or(false, |p| p >= frikadellen_baf::webhook::LEGENDARY_PROFIT_THRESHOLD as i64);
                    let opt_finder_for_flip = opt_finder.clone();
                    if is_legendary_flip {
                        // Send to shared anonymous channel (if not opted out)
                        if let Some(profit) = opt_profit {
                            if config_for_events.share_legendary_flips {
                                let item_for_channel = item_name.clone();
                                let finder_for_channel = opt_finder_for_flip.clone();
                                tokio::spawn(async move {
                                    frikadellen_baf::webhook::send_webhook_flip_channel(
                                        &item_for_channel, price, opt_target, profit,
                                        event_buy_speed_ms, finder_for_channel.as_deref(),
                                    ).await;
                                });
                            }

                            // Send legendary/divine styled webhook to user (replaces regular purchase webhook)
                            if let Some(webhook_url) = config_for_events.active_webhook_url() {
                                let url = webhook_url.to_string();
                                let name = ingame_name_for_events.clone();
                                let item = item_name.clone();
                                let did = config_for_events.active_discord_id().map(|s| s.to_string());
                                let purse = bot_client_clone.get_purse();
                                let uuid_str = opt_auction_uuid.clone();
                                let finder = opt_finder_for_flip.clone();
                                if profit >= frikadellen_baf::webhook::DIVINE_PROFIT_THRESHOLD as i64 {
                                    tokio::spawn(async move {
                                        frikadellen_baf::webhook::send_webhook_divine_flip(
                                            &name, &item, price, opt_target, profit, purse,
                                            event_buy_speed_ms, uuid_str.as_deref(), finder.as_deref(),
                                            did.as_deref(), &url,
                                        ).await;
                                    });
                                } else {
                                    tokio::spawn(async move {
                                        frikadellen_baf::webhook::send_webhook_legendary_flip(
                                            &name, &item, price, opt_target, profit, purse,
                                            event_buy_speed_ms, uuid_str.as_deref(), finder.as_deref(),
                                            did.as_deref(), &url,
                                        ).await;
                                    });
                                }
                            }
                        }
                    } else {
                        // Regular purchase webhook for non-legendary flips
                        if let Some(webhook_url) = config_for_events.active_webhook_url() {
                            let url = webhook_url.to_string();
                            let name = ingame_name_for_events.clone();
                            let item = item_name.clone();
                            let purse = bot_client_clone.get_purse();
                            let uuid_str = opt_auction_uuid.clone();
                            tokio::spawn(async move {
                                frikadellen_baf::webhook::send_webhook_item_purchased(
                                    &name, &item, price, opt_target, opt_profit, purse,
                                    event_buy_speed_ms, uuid_str.as_deref(), opt_finder.as_deref(), &url,
                                ).await;
                            });
                        }
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
                    // Record realized AH profit
                    if let Some(profit) = opt_profit {
                        profit_tracker_events.record_ah_profit(profit);
                    }
                    // Print colorful sold announcement
                    let profit_str = opt_profit.map(|p| {
                        let color = if p >= 0 { "§a" } else { "§c" };
                        format!(" §7| Profit: {}{}§r", color, format_coins(p))
                    }).unwrap_or_default();
                    let baf_msg = format!(
                        "§f[§4BAF§f]: §6⚡ SOLD §r{} §7to §e{}§7 for §6{}§7 coins!{}",
                        item_name, buyer, format_coins(price as i64), profit_str
                    );
                    print_mc_chat(&baf_msg);
                    let _ = chat_tx_events.send(baf_msg);
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
                    // Query Coflnet for authoritative session profit after each sale.
                    // `/cofl profit <ign> <days>` returns the total AH profit over
                    // the session window so the tracker stays in sync with Coflnet.
                    // Skip if session is too short for meaningful data (< ~15 min).
                    {
                        let days = session_start.elapsed().as_secs_f64() / 86400.0;
                        if days >= 0.01 {
                            let ign = ingame_name_for_events.clone();
                            let args = format!("{} {:.4}", ign, days);
                            let data_json = serde_json::json!(args).to_string();
                            let message = serde_json::json!({
                                "type": "profit",
                                "data": data_json
                            }).to_string();
                            let ws = ws_client_for_events.clone();
                            tokio::spawn(async move {
                                if let Err(e) = ws.send_message(&message).await {
                                    tracing::warn!("[CoflProfit] Failed to send /cofl profit: {}", e);
                                }
                            });
                        }
                    }
                }
                frikadellen_baf::bot::BotEvent::BazaarOrderPlaced { item_name, amount, price_per_unit, is_buy_order } => {
                    // Track the order for the web panel and profit calculation on collect.
                    bazaar_tracker_events.add_order(item_name.clone(), amount, price_per_unit, is_buy_order);
                    let (order_color, order_type) = if is_buy_order { ("§a", "BUY") } else { ("§c", "SELL") };
                    let baf_msg = format!(
                        "§f[§4BAF§f]: §6[BZ] {}{}§7 order placed: {}x {} @ §6{}§7 coins/unit",
                        order_color, order_type, amount, item_name, format_coins_f64(price_per_unit)
                    );
                    print_mc_chat(&baf_msg);
                    let _ = chat_tx_events.send(baf_msg);
                    if let Some(webhook_url) = config_for_events.active_bazaar_webhook_url() {
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
                    let baf_msg = format!(
                        "§f[§4BAF§f]: §a🏷️ BIN listed: §r{} §7@ §6{}§7 coins for §e{}h",
                        item_name, format_coins(starting_bid as i64), duration_hours
                    );
                    print_mc_chat(&baf_msg);
                    let _ = chat_tx_events.send(baf_msg);
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
                frikadellen_baf::bot::BotEvent::AuctionCancelled { item_name, starting_bid } => {
                    let baf_msg = format!(
                        "§f[§4BAF§f]: §c❌ Auction cancelled: §r{} §7@ §6{}§7 coins",
                        item_name, format_coins(starting_bid as i64)
                    );
                    print_mc_chat(&baf_msg);
                    let _ = chat_tx_events.send(baf_msg);
                    if let Some(webhook_url) = config_for_events.active_webhook_url() {
                        let url = webhook_url.to_string();
                        let name = ingame_name_for_events.clone();
                        let item = item_name.clone();
                        let purse = bot_client_clone.get_purse();
                        tokio::spawn(async move {
                            frikadellen_baf::webhook::send_webhook_auction_cancelled(
                                &name, &item, starting_bid, purse, &url,
                            ).await;
                        });
                    }
                }
                frikadellen_baf::bot::BotEvent::BazaarOrderCollected { item_name, is_buy_order, claimed_amount } => {
                    // Remove from tracker.
                    let order_data = bazaar_tracker_events.remove_order(&item_name, is_buy_order);
                    // Determine the actual quantity collected.  `claimed_amount` is
                    // parsed from the "Filled: X/Y" lore in the Manage Orders window.
                    // Fall back to the tracker's original order amount when unavailable.
                    let actual_amount = claimed_amount
                        .or_else(|| order_data.as_ref().map(|o| o.amount))
                        .unwrap_or(0);
                    if let Some(ref order) = order_data {
                        // Store buy cost so we can compute profit when the sell offer is collected.
                        // BUY collections do NOT record profit — profit is only realized on SELL.
                        // Only record cost for the actually claimed quantity — a partial fill
                        // should not inflate the buy cost with the unfilled remainder.
                        if is_buy_order {
                            bazaar_tracker_events.record_buy_cost(&item_name, order.price_per_unit, actual_amount);
                            info!("[BazaarProfit] Recorded buy cost for {} — {} x {:.0} coins/unit",
                                item_name, actual_amount, order.price_per_unit);
                        }
                    } else {
                        debug!("[BazaarProfit] No tracked order for collected {} {} (may be from a previous session)",
                            if is_buy_order { "BUY" } else { "SELL" }, item_name);
                    }
                    // Compute profit/loss for sell offers: sell_total - buy_total - tax.
                    // This is used for the immediate chat display; the session profit
                    // total is driven by `/cofl bz l` via set_bz_total().
                    // Bazaar tax is applied to sell proceeds (default 1.25%).
                    //
                    // When a sell is partially filled, only use the actual sold quantity
                    // for both sell revenue AND buy cost comparison.  Using per-unit buy
                    // cost × actual_sold prevents the false loss that occurred when comparing
                    // partial sell revenue against the TOTAL buy cost for all purchased units.
                    let bazaar_tax_rate = config_for_events.bazaar_tax_rate;
                    let opt_profit: Option<i64> = if !is_buy_order {
                        if let Some(ref sell_order) = order_data {
                            let sell_total = sell_order.price_per_unit * actual_amount as f64;
                            let tax = sell_total * (bazaar_tax_rate / 100.0);
                            let sell_after_tax = sell_total - tax;
                            if let Some((buy_ppu, _buy_amt)) = bazaar_tracker_events.take_buy_cost(&item_name) {
                                // Use per-unit buy cost × actual sold quantity, NOT
                                // buy_ppu × total_buy_amount.  This correctly handles
                                // partial sells (e.g. sold 21 of 64 bought).
                                let buy_total = buy_ppu * actual_amount as f64;
                                let profit = (sell_after_tax - buy_total).round() as i64;
                                // Session BZ profit is tracked via /cofl bz l (set_bz_total),
                                // so we don't call record_bz_profit here.
                                info!("[BazaarProfit] SELL {} — {} units, sell: {:.0}, tax: {:.0} ({:.2}%), buy: {:.0} ({:.0}/ea), profit: {}",
                                    item_name, actual_amount, sell_total, tax, bazaar_tax_rate, buy_total, buy_ppu, profit);
                                Some(profit)
                            } else if let Some(bz_list_profit) = bazaar_tracker_events.get_bz_list_profit(&item_name) {
                                // Fallback: use profit from /cofl bz l for this item
                                info!("[BazaarProfit] SELL {} — sell: {:.0}, tax: {:.0}, no local buy cost, using /cofl bz l profit: {}",
                                    item_name, sell_total, tax, bz_list_profit);
                                Some(bz_list_profit)
                            } else {
                                // No buy cost recorded and no /cofl bz l data —
                                // do NOT report sell proceeds as profit (the item
                                // was not free).
                                info!("[BazaarProfit] SELL {} — sell: {:.0}, tax: {:.0}, no buy cost or /cofl bz l data, skipping profit",
                                    item_name, sell_total, tax);
                                None
                            }
                        } else {
                            None
                        }
                    } else {
                        None
                    };
                    let order_type = if is_buy_order { "BUY" } else { "SELL" };
                    info!("[BazaarOrders] Order collected: {} ({}) x{}", item_name, order_type, actual_amount);
                    // Build the collection message with prices and optional profit.
                    // Use actual_amount (from lore) instead of the tracker's original
                    // order amount so partial fills display correctly (e.g. "1x" not "4x").
                    let price_info = if let Some(ref order) = order_data {
                        let total = order.price_per_unit * actual_amount as f64;
                        format!(" §7({}x @ §6{}§7 = §6{}§7 coins)",
                            actual_amount,
                            format_coins_f64(order.price_per_unit),
                            format_coins_f64(total))
                    } else {
                        String::new()
                    };
                    let profit_info = if let Some(profit) = opt_profit {
                        let (color, sign) = if profit >= 0 { ("§a", "+") } else { ("§c", "") };
                        format!(" §7→ {}{}{}§7 profit", color, sign, format_coins(profit))
                    } else {
                        String::new()
                    };
                    let baf_msg = format!(
                        "§f[§4BAF§f]: §a✅ [BZ] {}§7 order collected: §r{}{}{}",
                        if is_buy_order { "BUY" } else { "SELL" }, item_name, price_info, profit_info
                    );
                    print_mc_chat(&baf_msg);
                    let _ = chat_tx_events.send(baf_msg);
                    // Send bazaar legendary flip to public channel (100M+ profit on SELL)
                    if !is_buy_order {
                        if let Some(profit) = opt_profit {
                            if profit >= frikadellen_baf::webhook::LEGENDARY_PROFIT_THRESHOLD as i64
                                && config_for_events.share_legendary_flips
                            {
                                let item_for_channel = item_name.clone();
                                let opt_amount = order_data.as_ref().map(|o| o.amount);
                                let opt_ppu = order_data.as_ref().map(|o| o.price_per_unit);
                                tokio::spawn(async move {
                                    frikadellen_baf::webhook::send_webhook_bazaar_flip_channel(
                                        &item_for_channel,
                                        opt_amount.unwrap_or(0),
                                        opt_ppu.unwrap_or(0.0),
                                        profit,
                                    ).await;
                                });
                            }
                        }
                    }
                    if let Some(webhook_url) = config_for_events.active_bazaar_webhook_url() {
                        let url = webhook_url.to_string();
                        let name = ingame_name_for_events.clone();
                        let item = item_name.clone();
                        let purse = bot_client_clone.get_purse();
                        let opt_amount = order_data.as_ref().map(|o| o.amount);
                        let opt_ppu = order_data.as_ref().map(|o| o.price_per_unit);
                        tokio::spawn(async move {
                            frikadellen_baf::webhook::send_webhook_bazaar_order_collected(
                                &name, &item, is_buy_order,
                                opt_amount, opt_ppu,
                                opt_profit, purse, &url,
                            ).await;
                        });
                    }
                }
                frikadellen_baf::bot::BotEvent::BazaarOrderCancelled { item_name, is_buy_order } => {
                    // Remove from tracker without recording profit (cancelled orders don't count).
                    let _ = bazaar_tracker_events.remove_order(&item_name, is_buy_order);
                    let order_type = if is_buy_order { "BUY" } else { "SELL" };
                    info!("[BazaarOrders] Order cancelled: {} ({})", item_name, order_type);
                    let baf_msg = format!(
                        "§f[§4BAF§f]: §c🚫 [BZ] {}§7 order cancelled: §r{}",
                        if is_buy_order { "BUY" } else { "SELL" }, item_name
                    );
                    print_mc_chat(&baf_msg);
                    let _ = chat_tx_events.send(baf_msg);
                    if let Some(webhook_url) = config_for_events.active_bazaar_webhook_url() {
                        let url = webhook_url.to_string();
                        let name = ingame_name_for_events.clone();
                        let item = item_name.clone();
                        let purse = bot_client_clone.get_purse();
                        tokio::spawn(async move {
                            frikadellen_baf::webhook::send_webhook_bazaar_order_cancelled(
                                &name, &item, is_buy_order, purse, &url,
                            ).await;
                        });
                    }
                }
                frikadellen_baf::bot::BotEvent::BazaarOrderFilled { item_name, is_buy_order } => {
                    // Mark the order as filled in the tracker so the periodic timer
                    // can skip ManageOrders when nothing needs collection.
                    if !item_name.is_empty() {
                        bazaar_tracker_events.mark_filled(&item_name, is_buy_order);
                    }
                    // When a SELL order is filled the flip is complete in Coflnet's
                    // view.  Request `/cofl bz l` (with a short delay so Coflnet
                    // finishes recording the flip) — the response handler will parse
                    // profits and update the session BZ total via set_bz_total().
                    if !is_buy_order {
                        let ws = ws_client_for_events.clone();
                        tokio::spawn(async move {
                            // Small delay to let Coflnet register the completed flip.
                            tokio::time::sleep(tokio::time::Duration::from_secs(BZ_LIST_REQUEST_DELAY_SECS)).await;
                            let data_json = serde_json::json!("l").to_string();
                            let message = serde_json::json!({
                                "type": "bz",
                                "data": data_json
                            }).to_string();
                            if let Err(e) = ws.send_message(&message).await {
                                tracing::warn!("[BZList] Failed to send /cofl bz l: {}", e);
                            } else {
                                tracing::info!("[BZList] Auto-requested /cofl bz l after SELL fill");
                            }
                        });
                    }
                    // A bazaar buy/sell order was filled — trigger a ManageOrders run
                    // immediately so the items are collected without waiting for the next
                    // periodic check.  Only enqueue if bazaar flips are enabled and no
                    // ManageOrders is already queued/running (prevents duplicate processing
                    // that causes double cancel/collect Hypixel chat messages).
                    //
                    // When inventory is full and the fill is a BUY order, skip
                    // triggering ManageOrders — collecting BUY items requires free
                    // inventory space that we don't have.  The periodic order-check
                    // timer will retry after the 90 s cooldown clears the flag.
                    // SELL fills are always collected (they yield coins, not items).
                    if enable_bazaar_flips_events.load(Ordering::Relaxed) {
                        if is_buy_order && bot_client_clone.is_inventory_full() {
                            info!("[BazaarOrders] BUY order filled but inventory full — deferring ManageOrders for \"{}\"", item_name);
                        } else if command_queue_clone.has_manage_orders() {
                            info!("[BazaarOrders] Order filled — ManageOrders already queued/running, skipping duplicate");
                        } else {
                            info!("[BazaarOrders] Order filled — queuing ManageOrders");
                            command_queue_clone.enqueue(
                                frikadellen_baf::types::CommandType::ManageOrders { cancel_open: false },
                                frikadellen_baf::types::CommandPriority::High,
                                true,
                            );
                        }
                    }
                }
                frikadellen_baf::bot::BotEvent::BazaarOrdersSnapshot { ingame_orders } => {
                    // Reconcile the tracker with the orders actually visible
                    // in the Bazaar Orders window so the web GUI stays in sync.
                    let removed = bazaar_tracker_events.reconcile_with_ingame(&ingame_orders);
                    if removed > 0 {
                        info!("[BazaarOrders] Reconciled tracker: removed {} stale entries not found in-game", removed);
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
    let enable_ah_flips_ws = enable_ah_flips.clone();
    let enable_bazaar_flips_ws = enable_bazaar_flips.clone();
    let chat_tx_ws = chat_tx.clone();
    let detected_cofl_license_ws = detected_cofl_license.clone();
    let cofl_authenticated_ws = cofl_authenticated.clone();
    let ingame_names_ws = ingame_names.clone();
    let license_default_sent_ws = license_default_sent.clone();
    let ingame_name_ws = ingame_name.clone();
    let bazaar_tracker_ws = bazaar_tracker.clone();
    let profit_tracker_ws = profit_tracker.clone();
    // Accumulator for `/cofl bz l` output: (total_profit, flip_count, last_update).
    // Reset when "Last Completed Bazaar Flips" header is seen; each parsed flip
    // line adds to the total.  A debounce task displays the summary after 2s idle.
    let bz_list_accum: Arc<std::sync::Mutex<(i64, usize, std::time::Instant)>> =
        Arc::new(std::sync::Mutex::new((0, 0, std::time::Instant::now())));
    // Per-item profit accumulator for `/cofl bz l` output, used as a fallback
    // for per-order profit when local buy-cost tracking has no data.
    let bz_list_items: Arc<std::sync::Mutex<std::collections::HashMap<String, (i64, u32)>>> =
        Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    
    tokio::spawn(async move {
        use frikadellen_baf::websocket::CoflEvent;
        use frikadellen_baf::types::{CommandType, CommandPriority};

        while let Some(event) = ws_rx.recv().await {
            match event {
                CoflEvent::AuctionFlip(flip) => {
                    // Skip if AH flips are disabled
                    if !enable_ah_flips_ws.load(Ordering::Relaxed) {
                        continue;
                    }

                    // Block flips until Coflnet auth is confirmed
                    if !cofl_authenticated_ws.load(Ordering::Relaxed) {
                        debug!("Skipping flip — Coflnet not yet authenticated: {}", flip.item_name);
                        continue;
                    }

                    // Block flips until startup workflow is complete — the bot
                    // state can briefly be Idle between queued startup commands,
                    // so checking is_startup_in_progress() covers that gap.
                    if bot_client_for_ws.is_startup_in_progress() {
                        debug!("Skipping AH flip during startup: {}", flip.item_name);
                        continue;
                    }

                    // Skip if in startup/claiming state - use bot_client state (authoritative source)
                    if !bot_client_for_ws.state().allows_commands() {
                        debug!("Skipping flip — bot busy ({:?}): {}", bot_client_for_ws.state(), flip.item_name);
                        continue;
                    }

                    // Print colorful flip announcement (item name keeps its rarity color code)
                    let profit = flip.target.saturating_sub(flip.starting_bid);
                    let baf_msg = format!(
                        "§f[§4BAF§f]: §eTrying to purchase flip: §r{}§r §7for §6{}§7 coins §7(Target: §6{}§7, Profit: §a{}§7)",
                        flip.item_name,
                        format_coins(flip.starting_bid as i64),
                        format_coins(flip.target as i64),
                        format_coins(profit as i64)
                    );
                    print_mc_chat(&baf_msg);
                    let _ = chat_tx_ws.send(baf_msg);

                    // Store flip in tracker so ItemPurchased / ItemSold webhooks can include profit
                    {
                        let key = frikadellen_baf::utils::remove_minecraft_colors(&flip.item_name).to_lowercase();
                        if let Ok(mut tracker) = flip_tracker_ws.lock() {
                            let now = Instant::now();
                            tracker.insert(key, (flip.clone(), 0, now, now));
                        }
                    }

                    // Queue the flip command
                    // Buy-speed start time is now set in execute_command when
                    // /viewauction is sent, so the measurement covers the
                    // relevant path: command-send → coins-in-escrow.
                    command_queue_clone.enqueue(
                        CommandType::PurchaseAuction { flip },
                        CommandPriority::Critical,
                        false, // Not interruptible
                    );
                }
                CoflEvent::BazaarFlip(bazaar_flip) => {
                    // Skip if bazaar flips are disabled
                    if !enable_bazaar_flips_ws.load(Ordering::Relaxed) {
                        continue;
                    }

                    // Block flips until Coflnet auth is confirmed
                    if !cofl_authenticated_ws.load(Ordering::Relaxed) {
                        debug!("Skipping bazaar flip — Coflnet not yet authenticated: {}", bazaar_flip.item_name);
                        continue;
                    }

                    // Only skip during active startup phases (Startup / ManagingOrders).
                    // During ClaimingSold / ClaimingPurchased the flip is queued and will
                    // execute once the claim command finishes — matching TypeScript behaviour.
                    let bot_state = bot_client_for_ws.state();
                    if matches!(bot_state, frikadellen_baf::types::BotState::Startup)
                        || bot_client_for_ws.is_startup_in_progress()
                    {
                        debug!("Skipping bazaar flip during startup ({:?}): {}", bot_state, bazaar_flip.item_name);
                        continue;
                    }

                    // Skip if at the Bazaar order limit (21 orders)
                    if bot_client_for_ws.is_bazaar_at_limit() {
                        debug!("Skipping bazaar flip — at order limit: {}", bazaar_flip.item_name);
                        continue;
                    }

                    // Skip if there are filled orders waiting to be collected —
                    // they still occupy a slot until ManageOrders collects them.
                    if bazaar_tracker_ws.has_filled_orders() {
                        debug!("Skipping bazaar flip — filled orders pending collection: {}", bazaar_flip.item_name);
                        continue;
                    }

                    // Skip if bazaar flips are paused due to incoming AH flip (matching bazaarFlipPauser.ts)
                    if bazaar_flips_paused_ws.load(Ordering::Relaxed) {
                        debug!("Bazaar flips paused (AH flip incoming), skipping: {}", bazaar_flip.item_name);
                        continue;
                    }

                    // Print colorful bazaar flip announcement
                    let effective_is_buy = bazaar_flip.effective_is_buy_order();

                    // Skip BUY orders when inventory is full — items can't be
                    // collected from the bazaar without free inventory space.
                    // SELL orders are still accepted because they remove items
                    // from inventory, freeing space.
                    if effective_is_buy && bot_client_for_ws.is_inventory_full() {
                        debug!("Skipping BUY bazaar flip — inventory full: {}", bazaar_flip.item_name);
                        continue;
                    }

                    let (order_color, order_label) = if effective_is_buy { ("§a", "BUY") } else { ("§c", "SELL") };
                    let baf_msg = format!(
                        "§f[§4BAF§f]: §6[BZ] {}{}§7 order: §r{}§r §7x{} @ §6{}§7 coins/unit",
                        order_color, order_label,
                        bazaar_flip.item_name,
                        bazaar_flip.amount,
                        format_coins_f64(bazaar_flip.price_per_unit)
                    );
                    print_mc_chat(&baf_msg);
                    let _ = chat_tx_ws.send(baf_msg);

                    // Queue the bazaar command.
                    // SELL orders always get Critical priority — having items in
                    // inventory is worse than not having them (items block BUY
                    // collection and inventory space). BUY orders use Normal.
                    let priority = if effective_is_buy {
                        CommandPriority::Normal
                    } else {
                        CommandPriority::Critical
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
                    // Detect Coflnet authentication success.
                    // COFL sends "Hello <IGN> (<email>)" after successful auth, e.g.:
                    //   "[Coflnet]: Hello iLoveTreXitoCfg (tre********@****l.com)"
                    // The message may contain §-color codes. We look for "Hello "
                    // followed by a parenthesized email (with '@' inside) to avoid
                    // matching unrelated messages.
                    if !cofl_authenticated_ws.load(Ordering::Relaxed) {
                        if let Some(hello_pos) = msg.find("Hello ") {
                            let after_hello = &msg[hello_pos..];
                            // Expect "(…@…)" somewhere after "Hello "
                            if let (Some(open), Some(close)) = (after_hello.find('('), after_hello.find(')')) {
                                if open < close && after_hello[open..close].contains('@') {
                                    info!("[Coflnet] Authentication confirmed — flips enabled");
                                    cofl_authenticated_ws.store(true, Ordering::Relaxed);
                                    let baf_msg = "§f[§4BAF§f]: §aCoflnet authenticated — flip buying enabled".to_string();
                                    print_mc_chat(&baf_msg);
                                    let _ = chat_tx_ws.send(baf_msg);
                                }
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
                                // License is already active on the current account —
                                // no need to send `/cofl license default` later.
                                license_default_sent_ws.store(true, Ordering::Relaxed);
                            }
                        }
                    }
                    // Detect "You don't have a license for <ign>" and auto-send
                    // `/cofl license default <current_ign>` so the user's default
                    // account tier is applied to the current account.
                    if !license_default_sent_ws.load(Ordering::Relaxed) {
                        let clean_msg = frikadellen_baf::utils::remove_minecraft_colors(&msg);
                        if clean_msg.contains("don't have a license for") {
                            license_default_sent_ws.store(true, Ordering::Relaxed);
                            let ws = ws_client_clone.clone();
                            let ign = ingame_name_ws.clone();
                            info!("[LicenseDefault] No license detected — sending /cofl license default {}", ign);
                            let baf_msg = format!(
                                "§f[§4BAF§f]: §eNo license for §b{}§e — setting as default account...",
                                ign
                            );
                            print_mc_chat(&baf_msg);
                            let _ = chat_tx_ws.send(baf_msg);
                            tokio::spawn(async move {
                                if let Err(e) = ws.set_default_license(&ign).await {
                                    warn!("[LicenseDefault] Failed to set default license: {}", e);
                                }
                            });
                        }
                    }
                    // ---- `/cofl bz l` output parsing ----
                    // Coflnet sends "Last Completed Bazaar Flips" followed by lines like:
                    //   "2xJungle Key: 1.05M -> 287K => -768K(1)"
                    // Parse each flip line's profit and accumulate a running total.
                    // Per-item data is stored in the bazaar tracker so it can be used
                    // as a fallback profit source when local buy-cost tracking has
                    // no data for a sell.
                    {
                        let clean = frikadellen_baf::utils::remove_minecraft_colors(&msg);
                        if clean.contains("Last Completed Bazaar Flips") {
                            // Header line — reset the accumulators.
                            if let Ok(mut acc) = bz_list_accum.lock() {
                                *acc = (0, 0, std::time::Instant::now());
                            }
                            if let Ok(mut items) = bz_list_items.lock() {
                                items.clear();
                            }
                        } else if let Some(profit) = parse_bz_list_flip_profit(&clean) {
                            // Also parse per-item detail for fallback profit lookup.
                            if let Some((item_name, item_profit, flip_count)) = parse_bz_list_flip_detail(&clean) {
                                if let Ok(mut items) = bz_list_items.lock() {
                                    let entry = items.entry(item_name).or_insert((0, 0));
                                    entry.0 += item_profit;
                                    entry.1 += flip_count;
                                }
                            }
                            let should_spawn_summary = {
                                if let Ok(mut acc) = bz_list_accum.lock() {
                                    acc.0 += profit;
                                    acc.1 += 1;
                                    acc.2 = std::time::Instant::now();
                                    // Only spawn a summary task for the first flip
                                    // to avoid many duplicate summary outputs.
                                    acc.1 == 1
                                } else {
                                    false
                                }
                            };
                            if should_spawn_summary {
                                let accum = bz_list_accum.clone();
                                let items_clone = bz_list_items.clone();
                                let tracker = bazaar_tracker_ws.clone();
                                let tx = chat_tx_ws.clone();
                                let pt = profit_tracker_ws.clone();
                                tokio::spawn(async move {
                                    // Wait for the full list to arrive.
                                    tokio::time::sleep(tokio::time::Duration::from_secs(BZ_LIST_DEBOUNCE_SECS)).await;
                                    if let Ok(acc) = accum.lock() {
                                        let (total, count, _) = *acc;
                                        if count > 0 {
                                            // Use the `/cofl bz l` total as the authoritative
                                            // BZ session profit (replaces local calculation).
                                            pt.set_bz_total(total);
                                            tracing::info!("[BZList] Updated BZ profit from /cofl bz l: {} coins ({} flips)", total, count);
                                            let (color, sign) = if total >= 0 { ("§a", "+") } else { ("§c", "") };
                                            let summary = format!(
                                                "§f[§4BAF§f]: §6[BZ List] §7{} flips, total profit: {}{}{}",
                                                count, color, sign, format_coins(total)
                                            );
                                            print_mc_chat(&summary);
                                            let _ = tx.send(summary);
                                        }
                                    }
                                    // Push per-item profit data to the tracker for
                                    // fallback use when computing sell order profit.
                                    if let Ok(items) = items_clone.lock() {
                                        if !items.is_empty() {
                                            tracker.set_bz_list_profits(items.clone());
                                            tracing::debug!("[BZList] Stored per-item profits for {} items", items.len());
                                        }
                                    }
                                });
                            }
                        }
                    }
                    // Try to parse the Coflnet chat message as a bazaar flip recommendation.
                    // Coflnet may send flip recommendations as chat messages (e.g.
                    // "Recommending sell order: 2x Item at 30.1K per unit(1)") without a
                    // corresponding structured `bazaarFlip` WebSocket message.
                    if enable_bazaar_flips_ws.load(Ordering::Relaxed)
                        && cofl_authenticated_ws.load(Ordering::Relaxed)
                        && !bazaar_flips_paused_ws.load(Ordering::Relaxed)
                        && !bot_client_for_ws.is_bazaar_at_limit()
                    {
                        if let Ok(Some(rec)) = frikadellen_baf::handlers::BazaarFlipHandler::parse_bazaar_flip_message(&msg) {
                            let bot_state = bot_client_for_ws.state();
                            if !matches!(bot_state, frikadellen_baf::types::BotState::Startup) {
                                let effective_is_buy = rec.effective_is_buy_order();

                                // Skip BUY orders when inventory is full
                                if effective_is_buy && bot_client_for_ws.is_inventory_full() {
                                    debug!("Skipping BUY bazaar flip from chat — inventory full: {}", rec.item_name);
                                } else if effective_is_buy && bazaar_tracker_ws.has_filled_orders() {
                                    debug!("Skipping BUY bazaar flip from chat — filled orders pending: {}", rec.item_name);
                                } else {

                                let (order_color, order_label) = if effective_is_buy { ("§a", "BUY") } else { ("§c", "SELL") };
                                let baf_msg = format!(
                                    "§f[§4BAF§f]: §6[BZ] {}{}§7 order: §r{}§r §7x{} @ §6{}§7 coins/unit",
                                    order_color, order_label,
                                    rec.item_name,
                                    rec.amount,
                                    format_coins_f64(rec.price_per_unit)
                                );
                                print_mc_chat(&baf_msg);
                                let _ = chat_tx_ws.send(baf_msg);

                                let priority = if effective_is_buy {
                                    CommandPriority::Normal
                                } else {
                                    CommandPriority::Critical
                                };
                                let command_type = if effective_is_buy {
                                    CommandType::BazaarBuyOrder {
                                        item_name: rec.item_name.clone(),
                                        item_tag: rec.item_tag.clone(),
                                        amount: rec.amount,
                                        price_per_unit: rec.price_per_unit,
                                    }
                                } else {
                                    CommandType::BazaarSellOrder {
                                        item_name: rec.item_name.clone(),
                                        item_tag: rec.item_tag.clone(),
                                        amount: rec.amount,
                                        price_per_unit: rec.price_per_unit,
                                    }
                                };
                                command_queue_clone.enqueue(command_type, priority, true);
                                info!("[BazaarFlips] Queued {} order from chat message: {} x{} @ {:.0}",
                                    order_label, rec.item_name, rec.amount, rec.price_per_unit);
                                } // end inventory/filled check
                            }
                        }
                    }
                    // Display COFL chat messages with proper color formatting
                    // These are informational messages and should NOT be sent to Hypixel server
                    if config_clone.use_cofl_chat {
                        // Print with color codes if the message contains them
                        print_mc_chat(&msg);
                    } else {
                        // Still show in debug mode but without color formatting
                        debug!("[COFL Chat] {}", msg);
                    }
                    // Broadcast to web panel clients
                    let _ = chat_tx_ws.send(msg);
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
                            let inv_client = bot_client_for_ws.clone();
                            let is_sellinventory = command == "sellinventory";
                            tokio::spawn(async move {
                                // For sellinventory: upload the current inventory first so COFL
                                // has fresh data before processing the sell command.
                                if is_sellinventory {
                                    if let Some(inv_json) = inv_client.get_cached_inventory_json() {
                                        info!("[Inventory] sellinventory: uploading inventory first ({} bytes)", inv_json.len());
                                        let upload_msg = serde_json::json!({
                                            "type": "uploadInventory",
                                            "data": inv_json
                                        }).to_string();
                                        if let Err(e) = ws.send_message(&upload_msg).await {
                                            error!("[Inventory] sellinventory: failed to pre-upload inventory: {}", e);
                                        }
                                    } else {
                                        warn!("[Inventory] sellinventory: no cached inventory to upload");
                                    }
                                }

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

                            // If itemName is null/absent, fall back to looking up the display
                            // name from the bot's cached inventory at the given slot.
                            // COFL sends null itemName in some protocol versions.
                            let item_raw_resolved: Option<String> = item_raw
                                .map(|s| s.to_string())
                                .or_else(|| {
                                    let slot = item_slot?;
                                    // Mineflayer inventory slots are 9-44 (player inventory).
                                    // Reject values outside this range to avoid silent OOB access.
                                    if !(9..=44).contains(&slot) {
                                        warn!("[createAuction] slot {} is out of valid inventory range 9-44", slot);
                                        return None;
                                    }
                                    let inv_json = bot_client_for_ws.get_cached_inventory_json()?;
                                    let inv: serde_json::Value = serde_json::from_str(&inv_json).ok()?;
                                    let slots = inv.get("slots")?.as_array()?;
                                    let item = slots.get(slot as usize)?;
                                    if item.is_null() {
                                        return None;
                                    }
                                    // Prefer displayName (human-readable), then registry name
                                    item.get("displayName")
                                        .and_then(|v| v.as_str())
                                        .or_else(|| item.get("name").and_then(|v| v.as_str()))
                                        .map(|s| s.to_string())
                                });

                            if item_raw.is_none() {
                                if let Some(ref resolved) = item_raw_resolved {
                                    info!("[createAuction] Resolved null itemName from inventory slot {:?}: {}", item_slot, resolved);
                                } else {
                                    warn!("[createAuction] itemName is null and could not be resolved from inventory slot {:?}", item_slot);
                                }
                            }

                            match (item_raw_resolved.as_deref(), price, duration) {
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
                    // Relaxed ordering is fine here — these are simple toggle flags where
                    // eventual visibility across threads is sufficient.
                    if enable_bazaar_flips_ws.load(Ordering::Relaxed) && enable_ah_flips_ws.load(Ordering::Relaxed) {
                        let baf_msg = "§f[§4BAF§f]: §cAH Flips incoming, pausing bazaar flips".to_string();
                        print_mc_chat(&baf_msg);
                        let _ = chat_tx_ws.send(baf_msg);
                        let flag = bazaar_flips_paused_ws.clone();
                        flag.store(true, Ordering::Relaxed);
                        let ws = ws_client_clone.clone();
                        let enable_bz = enable_bazaar_flips_ws.clone();
                        let chat_tx_resume = chat_tx_ws.clone();
                        let command_queue_resume = command_queue_clone.clone();
                        tokio::spawn(async move {
                            sleep(Duration::from_secs(20)).await;
                            flag.store(false, Ordering::Relaxed);
                            // Notify user that bazaar flips are resuming (matching TypeScript bazaarFlipPauser.ts)
                            let baf_msg = "§f[§4BAF§f]: §aBazaar flips resumed, requesting new recommendations...".to_string();
                            print_mc_chat(&baf_msg);
                            let _ = chat_tx_resume.send(baf_msg);
                            info!("[BazaarFlips] Bazaar flips resumed after AH flip window");
                            // Queue a ManageOrders run to handle any deferred order
                            // management (filled orders that need collecting, etc.).
                            if !command_queue_resume.has_manage_orders() {
                                info!("[BazaarFlips] Queuing deferred ManageOrders after AH flip window");
                                command_queue_resume.enqueue(
                                    CommandType::ManageOrders { cancel_open: false },
                                    CommandPriority::Normal,
                                    false,
                                );
                            }
                            // Re-request bazaar flips to get fresh recommendations after the pause
                            if enable_bz.load(Ordering::Relaxed) {
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
                CoflEvent::LicenseList { entries, page: _ } => {
                    // Auto-detect the license index for the current account's IGN.
                    // We searched by `/cofl licenses list <current_ign>`, so the
                    // response contains entries with global license indices.
                    // Look for any non-NONE license matching the current IGN first,
                    // then fall back to any known IGN.
                    let current_ign = &ingame_name_ws;
                    if let Some((found_ign, global_idx, tier)) = entries.iter().find(|(name, _, tier)| {
                        name.eq_ignore_ascii_case(current_ign) && !tier.eq_ignore_ascii_case("NONE")
                    }) {
                        info!("[LicenseDetect] Found {} license index {} for '{}' ", tier, global_idx, found_ign);
                        detected_cofl_license_ws.store(*global_idx, Ordering::Relaxed);
                    } else if let Some((found_ign, global_idx, tier)) = entries.iter().find(|(name, _, tier)| {
                        // Check all configured IGNs as a fallback
                        ingame_names_ws.iter().any(|ign| name.eq_ignore_ascii_case(ign))
                            && !tier.eq_ignore_ascii_case("NONE")
                    }) {
                        info!("[LicenseDetect] Found {} license index {} for '{}' (other account)", tier, global_idx, found_ign);
                        detected_cofl_license_ws.store(*global_idx, Ordering::Relaxed);
                    } else if let Some((found_ign, _, _)) = entries.iter().find(|(name, _, _)| name.eq_ignore_ascii_case(current_ign)) {
                        info!("[LicenseDetect] Found '{}' but only has NONE licenses — no transfer needed", found_ign);
                    } else {
                        // No active license found for any configured IGN — set the
                        // default account so the user's subscription tier is applied.
                        if !license_default_sent_ws.load(Ordering::Relaxed) {
                            license_default_sent_ws.store(true, Ordering::Relaxed);
                            let ws = ws_client_clone.clone();
                            let ign = ingame_name_ws.clone();
                            let chat_tx = chat_tx_ws.clone();
                            info!("[LicenseDetect] No active license for any configured IGN — sending /cofl license default {}", ign);
                            let baf_msg = format!(
                                "§f[§4BAF§f]: §eNo license found — setting §b{}§e as default account...",
                                ign
                            );
                            print_mc_chat(&baf_msg);
                            let _ = chat_tx.send(baf_msg);
                            tokio::spawn(async move {
                                if let Err(e) = ws.set_default_license(&ign).await {
                                    warn!("[LicenseDefault] Failed to set default license: {}", e);
                                }
                            });
                        } else {
                            info!("[LicenseDetect] No license entries matched but license already handled");
                        }
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
    let macro_paused_proc = macro_paused.clone();
    let command_delay_ms = config.command_delay_ms;
    let auction_listing_delay_ms = config.auction_listing_delay_ms;
    let chat_tx_proc = chat_tx.clone();
    tokio::spawn(async move {
        use frikadellen_baf::types::BotState;
        loop {
            // When macro is paused via web panel, skip command processing entirely.
            if macro_paused_proc.load(Ordering::Relaxed) {
                sleep(Duration::from_millis(500)).await;
                continue;
            }

            // Register for notification BEFORE checking the queue.  This
            // prevents a race where a command is enqueued between
            // start_current() returning None and the await: the stored
            // permit from notify_one() will make `notified` resolve
            // immediately.
            let notified = command_queue_processor.notified();
            tokio::pin!(notified);

            // Process commands from queue
            if let Some(cmd) = command_queue_processor.start_current() {
                debug!("Processing command: {:?}", cmd.command_type);

                // During AH pause, drop incoming bazaar buy orders and defer
                // ManageOrders — they will be re-queued when bazaar flips resume.
                if should_drop_bazaar_command_during_ah_pause(
                    &cmd.command_type,
                    bazaar_flips_paused_proc.load(Ordering::Relaxed),
                ) {
                    if matches!(cmd.command_type, frikadellen_baf::types::CommandType::ManageOrders { .. }) {
                        info!("[Queue] Deferring ManageOrders — AH flip window active, will re-queue on resume");
                        let baf_msg = "§f[§4BAF§f]: §e⏸ Order management deferred — AH flips incoming, will resume after".to_string();
                        print_mc_chat(&baf_msg);
                        let _ = chat_tx_proc.send(baf_msg);
                    } else {
                        debug!("[Queue] Dropping bazaar command {:?} — AH flip window active", cmd.command_type);
                    }
                    command_queue_processor.complete_current();
                    sleep(Duration::from_millis(50)).await;
                    continue;
                }

                // Skip SellToAuction commands when the auction house is at the
                // listing limit — avoids the repeated /ah → "Maximum auction count
                // reached" → idle → next SellToAuction spam loop.
                if matches!(cmd.command_type, frikadellen_baf::types::CommandType::SellToAuction { .. })
                    && bot_client_clone.is_auction_at_limit()
                {
                    debug!("[Queue] Dropping SellToAuction — auction limit reached: {:?}", cmd.command_type);
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
                    | frikadellen_baf::types::CommandType::CheckCookie => 60,
                    // ManageOrders processes ONE order per cycle with a 10s
                    // internal deadline; keep external timeout just above.
                    frikadellen_baf::types::CommandType::ManageOrders { .. } => 15,
                    frikadellen_baf::types::CommandType::BazaarBuyOrder { .. }
                    | frikadellen_baf::types::CommandType::BazaarSellOrder { .. } => 20,
                    frikadellen_baf::types::CommandType::SellToAuction { .. } => 15,
                    _ => 10,
                };

                // Poll until the bot returns to an allows_commands() state or we hit the
                // per-type timeout. A single loop replaces the previous per-type if/else chain.
                let deadline = std::time::Instant::now()
                    + std::time::Duration::from_secs(timeout_secs);
                let mut interrupted = false;
                loop {
                    sleep(Duration::from_millis(250)).await;
                    if bot_client_clone.state().allows_commands()
                        || std::time::Instant::now() >= deadline
                    {
                        break;
                    }

                    // Check if a higher-priority command is waiting and the
                    // current command is interruptible.  This lets AH flips
                    // (Critical priority) preempt bazaar operations.
                    if cmd.interruptible {
                        if let Some(next) = command_queue_processor.peek_queued() {
                            if next.priority < cmd.priority {
                                warn!(
                                    "[Queue] Interrupting {:?} ({:?}) for higher-priority {:?} ({:?})",
                                    cmd.command_type, cmd.priority,
                                    next.command_type, next.priority,
                                );
                                bot_client_clone.set_state(BotState::Idle);
                                interrupted = true;
                                break;
                            }
                        }
                    }
                }

                // Safety reset: if the bot is still in a busy state after the timeout,
                // force it back to Idle so the queue can continue.
                if !interrupted && !bot_client_clone.state().allows_commands() {
                    warn!(
                        "[Queue] Command {:?} timed out after {}s — forcing Idle",
                        cmd.command_type, timeout_secs
                    );
                    bot_client_clone.set_state(BotState::Idle);
                }

                command_queue_processor.complete_current();

                // Always wait the configurable inter-command delay so Hypixel interactions
                // don't run back-to-back.  Skip the delay when we interrupted for an
                // AH flip so it is picked up immediately.
                // Use a longer delay after auction listings to prevent "Sending packets too fast" kicks.
                if !interrupted {
                    let delay = if matches!(cmd.command_type, frikadellen_baf::types::CommandType::SellToAuction { .. }) {
                        std::cmp::max(command_delay_ms, auction_listing_delay_ms)
                    } else {
                        command_delay_ms
                    };
                    sleep(Duration::from_millis(delay)).await;
                }
            } else {
                // Queue is empty — wait for a notification instead of busy-polling.
                // Times out after 500 ms so paused-state and other periodic checks
                // still run promptly even when no commands arrive.
                let _ = tokio::time::timeout(
                    Duration::from_millis(500),
                    &mut notified,
                ).await;
            }
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
        // Rustyline provides readline with history (up/down arrow key navigation) and
        // proper terminal handling. Since it's a blocking API we drive it in a
        // dedicated blocking task and send each line over an mpsc channel.
        let (line_tx, mut line_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        tokio::task::spawn_blocking(move || {
            let mut rl = match rustyline::DefaultEditor::new() {
                Ok(ed) => ed,
                Err(e) => {
                    eprintln!("[Console] Failed to initialize readline: {}", e);
                    return;
                }
            };
            loop {
                match rl.readline("") {
                    Ok(line) => {
                        let _ = rl.add_history_entry(line.as_str());
                        if line_tx.send(line).is_err() {
                            break;
                        }
                    }
                    Err(rustyline::error::ReadlineError::Interrupted) => {
                        // Ctrl-C in readline: forward as a shutdown signal
                        let _ = line_tx.send("__SHUTDOWN__".to_string());
                        break;
                    }
                    Err(rustyline::error::ReadlineError::Eof) => {
                        // Ctrl-D / end of stdin
                        break;
                    }
                    Err(e) => {
                        eprintln!("[Console] Readline error: {}", e);
                        break;
                    }
                }
            }
        });

        while let Some(line) = line_rx.recv().await {
            let input = line.trim();
            if input == "__SHUTDOWN__" {
                info!("Received Ctrl+C — shutting down BAF...");
                std::process::exit(0);
            }
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
        let bazaar_flips_paused_orders = bazaar_flips_paused.clone();
        let bazaar_tracker_orders = bazaar_tracker.clone();
        let order_interval = config.bazaar_order_check_interval_seconds;
        tokio::spawn(async move {
            use frikadellen_baf::types::{CommandType, CommandPriority};
            // Give startup workflow time to complete before starting periodic checks
            sleep(Duration::from_secs(120)).await;
            loop {
                sleep(Duration::from_secs(order_interval)).await;
                // Skip during AH pause — ManageOrders would be deferred anyway
                // and will be re-queued when bazaar flips resume.
                if bazaar_flips_paused_orders.load(Ordering::Relaxed) {
                    debug!("[BazaarOrders] Skipping periodic order check — AH flips incoming");
                    continue;
                }
                // Skip when the tracker has no filled orders to collect.
                // The BazaarOrderFilled event already triggers an immediate
                // ManageOrders run, so the periodic timer only needs to fire
                // when a fill notification was missed or on startup.  This
                // avoids constant GUI cycling that generates packet spam and
                // slows down the macro.
                if !bazaar_tracker_orders.has_filled_orders() {
                    debug!("[BazaarOrders] No filled orders in tracker — skipping periodic ManageOrders");
                    continue;
                }
                // When inventory is full, ManageOrders can't collect buy orders
                // and repeatedly opening/closing the bazaar GUI generates packet
                // spam that risks a kick.  Wait 90 s to give the player (or
                // InstaSell) time to free space, then clear the flag so
                // ManageOrders can retry BUY collection once.  If the inventory
                // is still full, the flag will be re-set on the next failed
                // claim attempt.
                if bot_client_orders.is_inventory_full() {
                    debug!("[BazaarOrders] Inventory full — waiting extra 90s before next order check");
                    sleep(Duration::from_secs(90)).await;
                    bot_client_orders.clear_inventory_full();
                    debug!("[BazaarOrders] Inventory full cooldown elapsed — clearing flag for retry");
                }
                if bot_client_orders.state().allows_commands() && !command_queue_orders.has_manage_orders() {
                    debug!("[BazaarOrders] Periodic order check triggered (every {}s)", order_interval);
                    command_queue_orders.enqueue(
                        CommandType::ManageOrders { cancel_open: false },
                        CommandPriority::Normal,
                        false,
                    );
                }
            }
        });
    }

    // Periodic stale bazaar order cleanup — remove tracked orders that are older
    // than the cancel timeout so the web panel doesn't accumulate stale entries
    // from orders that were cancelled/collected without emitting events.
    {
        let bazaar_tracker_cleanup = bazaar_tracker.clone();
        let cancel_minutes_per_million = config.bazaar_order_cancel_minutes_per_million;
        tokio::spawn(async move {
            // Max age = 2 × cancel_timeout or at least 30 minutes (in seconds)
            let max_age_secs = std::cmp::max(cancel_minutes_per_million * 2, 30) * 60;
            loop {
                sleep(Duration::from_secs(60)).await;
                let removed = bazaar_tracker_cleanup.remove_stale_orders(max_age_secs);
                if removed > 0 {
                    info!("[BazaarTracker] Cleaned up {} stale order(s) older than {}m", removed, max_age_secs / 60);
                }
            }
        });
    }

    // --- Periodic chatBatch upload to Coflnet ---
    // Sends accumulated Hypixel chat messages as a JSON array so Coflnet's
    // ChatBatchCommand can process purchases, collections, and other events.
    {
        let bot_client_chat_batch = bot_client.clone();
        let ws_client_chat_batch = ws_client.clone();
        tokio::spawn(async move {
            // Wait a bit for the bot to connect before starting uploads.
            sleep(Duration::from_secs(30)).await;
            loop {
                sleep(Duration::from_secs(2)).await;
                let batch = bot_client_chat_batch.drain_chat_batch();
                if batch.is_empty() {
                    continue;
                }
                let data_json = serde_json::to_string(&batch).unwrap_or_else(|_| "[]".to_string());
                let msg = serde_json::json!({
                    "type": "chatBatch",
                    "data": data_json
                }).to_string();
                if let Err(e) = ws_client_chat_batch.send_message(&msg).await {
                    debug!("[ChatBatch] Failed to send chatBatch to Coflnet: {}", e);
                } else {
                    debug!("[ChatBatch] Sent {} message(s) to Coflnet", batch.len());
                }
            }
        });
    }

    // Periodic "My Auctions" check to claim sold/expired auctions that don't emit chat events.
    if config.enable_ah_flips {
        let bot_client_ah_claim = bot_client.clone();
        let command_queue_ah_claim = command_queue.clone();
        tokio::spawn(async move {
            use frikadellen_baf::types::{CommandPriority, CommandType};
            // Give startup workflow time to complete before periodic checks.
            sleep(Duration::from_secs(120)).await;
            loop {
                sleep(Duration::from_secs(PERIODIC_AH_CLAIM_CHECK_INTERVAL_SECS)).await;
                let bot_state = bot_client_ah_claim.state();
                let queue_empty = command_queue_ah_claim.is_empty();
                if should_enqueue_periodic_auction_claim(bot_state, queue_empty) {
                    debug!(
                        "[ClaimSold] Periodic My Auctions check triggered (every {}s)",
                        PERIODIC_AH_CLAIM_CHECK_INTERVAL_SECS
                    );
                    command_queue_ah_claim.enqueue(
                        CommandType::ClaimSoldItem,
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
        let chat_tx_island = chat_tx.clone();
        tokio::spawn(async move {
            use frikadellen_baf::types::{CommandType, CommandPriority, BotState};

            // Give the startup workflow time to complete before we start checking.
            sleep(Duration::from_secs(60)).await;

            // Track consecutive rejoin attempts to add cooldown when kicked from SkyBlock.
            let mut consecutive_rejoin_attempts: u32 = 0;

            loop {
                sleep(Duration::from_secs(10)).await;

                // Don't interfere while the bot is actively doing work.
                // Any non-Idle state means the bot is in a GUI workflow (bazaar,
                // purchasing, selling, claiming, …) and may have navigated away
                // from the island — that is NOT a reason to rejoin.
                if bot_client_island.state() != BotState::Idle {
                    consecutive_rejoin_attempts = 0;
                    continue;
                }

                let lines = bot_client_island.get_scoreboard_lines();

                // Scoreboard not yet populated — skip until it has data.
                if lines.is_empty() {
                    continue;
                }

                // If "Your Island" is in the sidebar we are home — nothing to do.
                if lines.iter().any(|l| l.contains("Your Island")) {
                    consecutive_rejoin_attempts = 0;
                    continue;
                }

                consecutive_rejoin_attempts += 1;

                // Safety cap: after REJOIN_MAX_ATTEMPTS consecutive failures,
                // reset the counter so the backoff does not grow unbounded.
                if consecutive_rejoin_attempts >= REJOIN_MAX_ATTEMPTS {
                    warn!(
                        "[AFKHandler] {} consecutive rejoin attempts failed — resetting backoff",
                        REJOIN_MAX_ATTEMPTS
                    );
                    consecutive_rejoin_attempts = 1;
                }

                // Exponential backoff: after repeated failures, wait longer to avoid
                // infinite transfer cooldown when kicked from SkyBlock.
                if consecutive_rejoin_attempts > 1 {
                    let backoff_secs = std::cmp::min(REJOIN_BACKOFF_BASE_SECS * consecutive_rejoin_attempts as u64, REJOIN_MAX_BACKOFF_SECS);
                    let baf_msg = format!(
                        "§f[§4BAF§f]: §cRejoin attempt #{} — waiting {}s before retry...",
                        consecutive_rejoin_attempts, backoff_secs
                    );
                    print_mc_chat(&baf_msg);
                    let _ = chat_tx_island.send(baf_msg);
                    warn!("[AFKHandler] Consecutive rejoin attempt #{} — backing off {}s", consecutive_rejoin_attempts, backoff_secs);
                    sleep(Duration::from_secs(backoff_secs)).await;
                }

                // Not on island — send the return sequence.
                let baf_msg = "§f[§4BAF§f]: §eNot detected on island — returning to island...".to_string();
                print_mc_chat(&baf_msg);
                let _ = chat_tx_island.send(baf_msg);
                info!("[AFKHandler] Not on island — sending /lobby → /play sb → /is");

                // Send commands with delays between them so each server
                // transfer has time to complete before the next fires.
                // Check bot state between steps: if the bot left Idle (e.g.
                // a flip arrived), abort the sequence so we don't interfere.
                command_queue_island.enqueue(
                    CommandType::SendChat { message: "/lobby".to_string() },
                    CommandPriority::High,
                    false,
                );
                sleep(Duration::from_secs(5)).await;

                if bot_client_island.state() != BotState::Idle {
                    continue;
                }

                command_queue_island.enqueue(
                    CommandType::SendChat { message: "/play sb".to_string() },
                    CommandPriority::High,
                    false,
                );
                sleep(Duration::from_secs(10)).await;

                if bot_client_island.state() != BotState::Idle {
                    continue;
                }

                command_queue_island.enqueue(
                    CommandType::SendChat { message: "/is".to_string() },
                    CommandPriority::High,
                    false,
                );

                // Wait for the island teleport to finish before checking again.
                sleep(Duration::from_secs(15)).await;
            }
        });
    }

    // Automatic account switching timer.
    // When multiple accounts are configured and `multi_switch_time` is set, switch to the
    // next account after the specified number of hours by persisting the next account index
    // and restarting the process.
    if ingame_names.len() > 1 {
        if let Some(switch_hours) = config.multi_switch_time {
            let switch_secs = (switch_hours * 3600.0) as u64;
            let next_index = (current_account_index + 1) % ingame_names.len();
            let next_name = ingame_names[next_index].clone();
            let index_path = account_index_path.clone();
            let chat_tx_switch = chat_tx.clone();
            let detected_license_switch = detected_cofl_license.clone();
            let ws_switch = ws_client.clone();
            info!(
                "[AccountSwitch] Will switch from {} to {} in {:.1}h",
                ingame_name, next_name, switch_hours
            );
            tokio::spawn(async move {
                sleep(Duration::from_secs(switch_secs)).await;
                info!(
                    "[AccountSwitch] Switch time reached — switching to account {} ({})",
                    next_index + 1, next_name
                );
                // Transfer the COFL license to the next account before restarting.
                let license_index = detected_license_switch.load(Ordering::Relaxed);
                if license_index > 0 {
                    if let Err(e) = ws_switch.transfer_license(license_index, &next_name).await {
                        warn!("[AccountSwitch] Failed to transfer license: {}", e);
                    }
                    // Give COFL time to process the license transfer before restarting.
                    sleep(Duration::from_secs(3)).await;
                }
                // Persist the next account index so the next process invocation picks it up.
                if let Err(e) = std::fs::write(&index_path, next_index.to_string()) {
                    warn!("[AccountSwitch] Failed to write account index: {}", e);
                }
                let baf_msg = format!(
                    "§f[§4BAF§f]: §eSwitching to account §b{}§e...",
                    next_name
                );
                print_mc_chat(&baf_msg);
                let _ = chat_tx_switch.send(baf_msg);
                info!("[AccountSwitch] Restarting process with next account...");
                restart_process();
            });
        }
    }

    // Periodic profit summary webhook (configurable interval, 0 = disabled)
    let profit_interval_mins = config.profit_summary_interval_minutes;
    if profit_interval_mins > 0 {
        if let Some(webhook_url) = config.active_webhook_url() {
            let profit_tracker_webhook = profit_tracker.clone();
            let webhook_url = webhook_url.to_string();
            let name = ingame_name.clone();
            let started = std::time::Instant::now();
            let interval_secs = profit_interval_mins.saturating_mul(60);
            tokio::spawn(async move {
                loop {
                    sleep(Duration::from_secs(interval_secs)).await;
                    let (ah, bz) = profit_tracker_webhook.totals();
                    let uptime = started.elapsed().as_secs();
                    frikadellen_baf::webhook::send_webhook_profit_summary(
                        &name, ah, bz, uptime, &webhook_url,
                    )
                    .await;
                }
            });
        }
    }

    // ── VPS Socket (Coflnet hosted instance management) ──────────────────
    if config.vps_enabled {
        if let Some(secret) = config.vps_secret.as_deref().filter(|s| !s.is_empty()) {
            info!("[VPS] VPS mode enabled — connecting to {}", config.vps_url);
            frikadellen_baf::vps::spawn_vps_socket(
                config.vps_url.clone(),
                secret.to_string(),
                std::sync::Arc::new(|update: frikadellen_baf::vps::VpsStateUpdate| {
                    let turned_off = update.instance.context.contains_key("turnedOff");
                    if turned_off {
                        info!(
                            "[VPS] Instance {} marked as turned off",
                            update.instance.id
                        );
                    } else {
                        info!(
                            "[VPS] Instance {} active (owner={}, kind={})",
                            update.instance.id,
                            update.instance.owner_id,
                            update.instance.app_kind
                        );
                    }
                }),
            );
        } else {
            warn!("[VPS] vps_enabled is true but vps_secret is empty — skipping VPS socket");
        }
    }

    // Keep the application running
    info!("BAF is now running. Type commands below or press Ctrl+C to exit.");
    
    // Wait until Ctrl+C (SIGINT) is received
    tokio::signal::ctrl_c().await?;
    info!("Received Ctrl+C — shutting down BAF...");
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::{is_ban_disconnect, parse_cofl_profit_response, parse_short_number, parse_bz_list_flip_detail, should_drop_bazaar_command_during_ah_pause, should_enqueue_periodic_auction_claim};
    use frikadellen_baf::types::{BotState, CommandType};

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

    #[test]
    fn periodic_auction_claim_requires_idle_and_empty_queue() {
        assert!(should_enqueue_periodic_auction_claim(BotState::Idle, true));
        assert!(!should_enqueue_periodic_auction_claim(BotState::ClaimingSold, true));
        assert!(!should_enqueue_periodic_auction_claim(BotState::Idle, false));
    }

    #[test]
    fn ah_pause_drops_bazaar_and_manage_orders_commands() {
        let paused = true;
        assert!(should_drop_bazaar_command_during_ah_pause(
            &CommandType::BazaarBuyOrder {
                item_name: "Booster Cookie".into(),
                item_tag: None,
                amount: 1,
                price_per_unit: 1.0,
            },
            paused,
        ));
        // BazaarSellOrder should NOT be dropped during AH pause (only buy orders are dropped)
        assert!(!should_drop_bazaar_command_during_ah_pause(
            &CommandType::BazaarSellOrder {
                item_name: "Booster Cookie".into(),
                item_tag: None,
                amount: 1,
                price_per_unit: 1.0,
            },
            paused,
        ));
        assert!(!should_drop_bazaar_command_during_ah_pause(
            &CommandType::ClaimSoldItem,
            paused,
        ));
        // ManageOrders IS deferred during AH pause — it would block the AH
        // flip purchase.  A new ManageOrders is re-queued when flips resume.
        assert!(should_drop_bazaar_command_during_ah_pause(
            &CommandType::ManageOrders { cancel_open: false },
            paused,
        ));
        assert!(should_drop_bazaar_command_during_ah_pause(
            &CommandType::ManageOrders { cancel_open: true },
            paused,
        ));
    }

    #[test]
    fn parse_cofl_profit_response_82m() {
        let msg = "According to our data TestUser made 82.7M in the last 0.05 days across 6 auctions";
        assert_eq!(parse_cofl_profit_response(msg), Some(82_700_000));
    }

    #[test]
    fn parse_cofl_profit_response_1b() {
        let msg = "According to our data Player123 made 1.5B in the last 2.3 days across 142 auctions";
        assert_eq!(parse_cofl_profit_response(msg), Some(1_500_000_000));
    }

    #[test]
    fn parse_cofl_profit_response_plain() {
        let msg = "According to our data SomeIGN made 500 in the last 0.01 days across 1 auctions";
        assert_eq!(parse_cofl_profit_response(msg), Some(500));
    }

    #[test]
    fn parse_cofl_profit_response_250k() {
        let msg = "According to our data IGN made 250K in the last 0.1 days across 3 auctions";
        assert_eq!(parse_cofl_profit_response(msg), Some(250_000));
    }

    #[test]
    fn parse_cofl_profit_response_no_match() {
        assert_eq!(parse_cofl_profit_response("Some random chat message"), None);
    }

    #[test]
    fn parse_short_number_values() {
        assert_eq!(parse_short_number("82.7M"), Some(82_700_000));
        assert_eq!(parse_short_number("1.5B"), Some(1_500_000_000));
        assert_eq!(parse_short_number("250K"), Some(250_000));
        assert_eq!(parse_short_number("500"), Some(500));
        assert_eq!(parse_short_number("1,500,000"), Some(1_500_000));
        assert_eq!(parse_short_number("abc"), None);
    }

    #[test]
    fn parse_bz_list_flip_detail_profit() {
        let line = "2xJungle Key: 1.05M -> 287K => -768K(1)";
        let (name, profit, count) = parse_bz_list_flip_detail(line).unwrap();
        assert_eq!(name, "Jungle Key");
        assert_eq!(profit, -768_000);
        assert_eq!(count, 1);
    }

    #[test]
    fn parse_bz_list_flip_detail_multiple_flips() {
        let line = "128xWorm Membrane: 7.16M -> 7.91M => 741K(7)";
        let (name, profit, count) = parse_bz_list_flip_detail(line).unwrap();
        assert_eq!(name, "Worm Membrane");
        assert_eq!(profit, 741_000);
        assert_eq!(count, 7);
    }

    #[test]
    fn parse_bz_list_flip_detail_no_match() {
        assert!(parse_bz_list_flip_detail("Some random text").is_none());
        assert!(parse_bz_list_flip_detail("Last Completed Bazaar Flips").is_none());
    }
}
