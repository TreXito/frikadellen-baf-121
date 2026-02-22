use anyhow::Result;
use dialoguer::{Input, Confirm};
use frikadellen_baf::{
    config::ConfigLoader,
    logging::{init_logger, print_mc_chat},
    state::CommandQueue,
    websocket::CoflWebSocket,
    bot::BotClient,
};
use tracing::{debug, error, info, warn};
use tokio::time::{sleep, Duration};
use serde_json;
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};

const VERSION: &str = "af-3.0";

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
        tokio::spawn(async move {
            frikadellen_baf::webhook::send_webhook_initialized(&name, ah, bz, &url).await;
        });
    }

    // Initialize and connect bot client
    info!("Initializing Minecraft bot...");
    info!("Authenticating with Microsoft account...");
    info!("A browser window will open for you to log in");
    
    let mut bot_client = BotClient::new();
    
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

    // Spawn bot event handler
    let bot_client_clone = bot_client.clone();
    let ws_client_for_events = ws_client.clone();
    let config_for_events = config.clone();
    let command_queue_clone = command_queue.clone();
    let ingame_name_for_events = ingame_name.clone();
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
                    info!("[Minecraft] {}", msg);
                }
                frikadellen_baf::bot::BotEvent::WindowOpen(id, window_type, title) => {
                    info!("Window opened: {} (ID: {}, Type: {})", title, id, window_type);
                }
                frikadellen_baf::bot::BotEvent::WindowClose => {
                    info!("Window closed");
                }
                frikadellen_baf::bot::BotEvent::Disconnected(reason) => {
                    warn!("Bot disconnected: {}", reason);
                }
                frikadellen_baf::bot::BotEvent::Kicked(reason) => {
                    warn!("Bot kicked: {}", reason);
                }
                frikadellen_baf::bot::BotEvent::StartupComplete => {
                    info!("[Startup] Startup complete - bot is ready to flip!");
                    // Upload scoreboard to COFL (with real data matching TypeScript runStartupWorkflow)
                    {
                        let scoreboard_lines = bot_client_clone.get_scoreboard_lines();
                        let ws = ws_client_for_events.clone();
                        tokio::spawn(async move {
                            let data_json = serde_json::to_string(&scoreboard_lines).unwrap_or_else(|_| "[]".to_string());
                            let scoreboard_msg = serde_json::json!({"type": "uploadScoreboard", "data": data_json}).to_string();
                            let tab_msg = serde_json::json!({"type": "uploadTab", "data": "[]"}).to_string();
                            let _ = ws.send_message(&scoreboard_msg).await;
                            let _ = ws.send_message(&tab_msg).await;
                            info!("[Startup] Uploaded scoreboard ({} lines)", scoreboard_lines.len());
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
                        tokio::spawn(async move {
                            frikadellen_baf::webhook::send_webhook_startup_complete(&name, 0, ah, bz, &url).await;
                        });
                    }
                }
                frikadellen_baf::bot::BotEvent::ItemPurchased { item_name, price } => {
                    info!("[Minecraft] You purchased {} for {} coins!", item_name, price);
                    // Send uploadScoreboard (with real data) and uploadTab to COFL
                    let ws = ws_client_for_events.clone();
                    let scoreboard_lines = bot_client_clone.get_scoreboard_lines();
                    tokio::spawn(async move {
                        let data_json = serde_json::to_string(&scoreboard_lines).unwrap_or_else(|_| "[]".to_string());
                        let scoreboard_msg = serde_json::json!({"type": "uploadScoreboard", "data": data_json}).to_string();
                        let tab_msg = serde_json::json!({"type": "uploadTab", "data": "[]"}).to_string();
                        let _ = ws.send_message(&scoreboard_msg).await;
                        let _ = ws.send_message(&tab_msg).await;
                    });
                    // Queue claim
                    command_queue_clone.enqueue(
                        frikadellen_baf::types::CommandType::ClaimPurchasedItem,
                        frikadellen_baf::types::CommandPriority::High,
                        false,
                    );
                    // Send webhook
                    if let Some(webhook_url) = config_for_events.active_webhook_url() {
                        let url = webhook_url.to_string();
                        let name = ingame_name_for_events.clone();
                        let item = item_name.clone();
                        tokio::spawn(async move {
                            frikadellen_baf::webhook::send_webhook_item_purchased(&name, &item, price, None, None, &url).await;
                        });
                    }
                }
                frikadellen_baf::bot::BotEvent::ItemSold { item_name, price, buyer } => {
                    info!("[Minecraft] {} bought {} for {} coins!", buyer, item_name, price);
                    command_queue_clone.enqueue(
                        frikadellen_baf::types::CommandType::ClaimSoldItem,
                        frikadellen_baf::types::CommandPriority::High,
                        true,
                    );
                    if let Some(webhook_url) = config_for_events.active_webhook_url() {
                        let url = webhook_url.to_string();
                        let name = ingame_name_for_events.clone();
                        let item = item_name.clone();
                        let b = buyer.clone();
                        tokio::spawn(async move {
                            frikadellen_baf::webhook::send_webhook_item_sold(&name, &item, price, &b, &url).await;
                        });
                    }
                }
                frikadellen_baf::bot::BotEvent::BazaarOrderPlaced { item_name, amount, price_per_unit, is_buy_order } => {
                    let order_type = if is_buy_order { "BUY" } else { "SELL" };
                    info!("[Bazaar] {} order placed: {}x {} @ {:.1} coins/unit",
                        order_type, amount, item_name, price_per_unit);
                    if let Some(webhook_url) = config_for_events.active_webhook_url() {
                        let url = webhook_url.to_string();
                        let name = ingame_name_for_events.clone();
                        let item = item_name.clone();
                        let total = price_per_unit * amount as f64;
                        tokio::spawn(async move {
                            frikadellen_baf::webhook::send_webhook_bazaar_order_placed(
                                &name, &item, amount, price_per_unit, total, is_buy_order, &url,
                            ).await;
                        });
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

                    // Skip if in startup state - use bot_client state (authoritative source)
                    if !bot_client_for_ws.state().allows_commands() {
                        warn!("Skipping flip during startup: {}", flip.item_name);
                        continue;
                    }

                    info!("Received auction flip: {} (profit: {})", 
                        flip.item_name, 
                        flip.target.saturating_sub(flip.starting_bid)
                    );

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

                    // Skip if in startup state - use bot_client state (authoritative source)
                    if !bot_client_for_ws.state().allows_commands() {
                        warn!("Skipping bazaar flip during startup: {}", bazaar_flip.item_name);
                        continue;
                    }

                    // Skip if bazaar flips are paused due to incoming AH flip (matching bazaarFlipPauser.ts)
                    if bazaar_flips_paused_ws.load(Ordering::Relaxed) {
                        info!("Bazaar flips paused (AH flip incoming), skipping: {}", bazaar_flip.item_name);
                        continue;
                    }

                    info!("Received bazaar flip: {} x{} @ {} coins/unit ({})", 
                        bazaar_flip.item_name,
                        bazaar_flip.amount,
                        bazaar_flip.price_per_unit,
                        if bazaar_flip.is_buy_order { "BUY" } else { "SELL" }
                    );

                    // Queue the bazaar command.
                    // Matching TypeScript: SELL orders use HIGH priority (free up inventory),
                    // BUY orders use NORMAL priority. Both are interruptible by AH flips.
                    let priority = if bazaar_flip.is_buy_order {
                        CommandPriority::Normal
                    } else {
                        CommandPriority::High
                    };
                    let command_type = if bazaar_flip.is_buy_order {
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
                    // Display COFL chat messages with proper color formatting
                    // These are informational messages and should NOT be sent to Hypixel server
                    if config_clone.use_cofl_chat {
                        // Print with color codes if the message contains them
                        print_mc_chat(&msg);
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
                    info!("Processing getInventory request");
                    // Queue command to upload inventory from event handler where bot is accessible
                    command_queue_clone.enqueue(
                        CommandType::UploadInventory,
                        CommandPriority::Normal,
                        false,
                    );
                }
                CoflEvent::TradeResponse => {
                    info!("Processing tradeResponse - clicking accept button");
                    // TypeScript: clicks slot 39 after checking for "Deal!" or "Warning!"
                    // Sleep is handled in TypeScript before clicking - we'll do the same
                    command_queue_clone.enqueue(
                        CommandType::ClickSlot { slot: 39 },
                        CommandPriority::High,
                        false,
                    );
                }
                CoflEvent::PrivacySettings(data) => {
                    info!("Received privacySettings: {}", data);
                    // TypeScript stores this in bot.privacySettings
                    // For now, just log it - can be enhanced later
                    debug!("Privacy settings data: {}", data);
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
                            match (item_raw, price, duration) {
                                (Some(item_raw), Some(price), Some(duration)) => {
                                    // Strip Minecraft color codes (§X) from item name
                                    let item_name = frikadellen_baf::utils::remove_minecraft_colors(item_raw);
                                    command_queue_clone.enqueue(
                                        CommandType::SellToAuction {
                                            item_name,
                                            starting_bid: price,
                                            duration_hours: duration,
                                        },
                                        CommandPriority::High,
                                        false,
                                    );
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
                    info!("Processing trade request");
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
                    info!("Received runSequence request");
                    // TypeScript has a runSequence handler but it's not fully implemented
                    // For now, just log it
                    debug!("Sequence data: {}", data);
                    warn!("runSequence is not yet fully implemented");
                }
                CoflEvent::Countdown => {
                    // COFL sends this ~10 seconds before AH flips arrive.
                    // Matching TypeScript bazaarFlipPauser.ts: pause bazaar flips for 20 seconds
                    // when both AH flips and bazaar flips are enabled.
                    info!("[COFL] Flips in 10 seconds");
                    if config_clone.enable_bazaar_flips && config_clone.enable_ah_flips {
                        info!("AH flip incoming – pausing bazaar flips for 20 seconds");
                        let flag = bazaar_flips_paused_ws.clone();
                        flag.store(true, Ordering::Relaxed);
                        tokio::spawn(async move {
                            sleep(Duration::from_secs(20)).await;
                            flag.store(false, Ordering::Relaxed);
                            info!("Bazaar flips resumed after AH flip window");
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
    tokio::spawn(async move {
        loop {
            // Process commands from queue
            if let Some(cmd) = command_queue_processor.start_current() {
                info!("Processing command: {:?}", cmd.command_type);
                
                // Send command to bot for execution
                if let Err(e) = bot_client_clone.send_command(cmd.clone()) {
                    warn!("Failed to send command to bot: {}", e);
                }
                
                // Wait for command to be processed.
                // For claim commands, poll until the bot leaves the claiming state (up to 30s).
                // For bazaar commands, poll until the bot leaves the Bazaar state (up to 20s).
                // For other commands, wait a fixed 5 seconds.
                let is_claim = matches!(
                    cmd.command_type,
                    frikadellen_baf::types::CommandType::ClaimPurchasedItem
                    | frikadellen_baf::types::CommandType::ClaimSoldItem
                );
                let is_bazaar = matches!(
                    cmd.command_type,
                    frikadellen_baf::types::CommandType::BazaarBuyOrder { .. }
                    | frikadellen_baf::types::CommandType::BazaarSellOrder { .. }
                );
                if is_claim {
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
                    loop {
                        sleep(Duration::from_millis(250)).await;
                        let s = bot_client_clone.state();
                        if !matches!(s,
                            frikadellen_baf::types::BotState::ClaimingPurchased
                            | frikadellen_baf::types::BotState::ClaimingSold
                        ) || std::time::Instant::now() >= deadline {
                            break;
                        }
                    }
                } else if is_bazaar {
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
                    loop {
                        sleep(Duration::from_millis(250)).await;
                        let s = bot_client_clone.state();
                        if s != frikadellen_baf::types::BotState::Bazaar
                            || std::time::Instant::now() >= deadline
                        {
                            break;
                        }
                    }
                } else {
                    sleep(Duration::from_secs(5)).await;
                }
                
                command_queue_processor.complete_current();
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
                        }
                    }
                }
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

