use anyhow::Result;
use dialoguer::{Input, Confirm};
use frikadellen_baf::{
    config::ConfigLoader,
    logging::{init_logger, print_mc_chat},
    state::{StateManager, CommandQueue},
    websocket::CoflWebSocket,
    bot::BotClient,
    types::BotState,
};
use tracing::{debug, error, info, warn};
use tokio::time::{sleep, Duration};

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

    // Initialize state management
    let state_manager = StateManager::new();
    let command_queue = CommandQueue::new();

    // Set initial state to startup (prevents commands during initialization)
    state_manager.set(BotState::Startup);

    // Get or generate session ID for Coflnet
    let session_id = config.sessions
        .get(&ingame_name)
        .map(|s| s.id.clone())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    info!("Connecting to Coflnet WebSocket...");
    
    // Connect to Coflnet WebSocket
    let (ws_client, mut ws_rx) = CoflWebSocket::connect(
        config.websocket_url.clone(),
        ingame_name.clone(),
        VERSION.to_string(),
        session_id.clone(),
    ).await?;

    info!("WebSocket connected successfully");

    // Initialize and connect bot client
    info!("Initializing Minecraft bot...");
    info!("Authenticating with Microsoft account...");
    info!("A browser window will open for you to log in");
    
    let mut bot_client = BotClient::new();
    
    // Connect to Hypixel - Azalea will handle Microsoft OAuth in browser
    match bot_client.connect(ingame_name.clone()).await {
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
            }
        }
    });

    // Spawn WebSocket message handler
    let state_manager_clone = state_manager.clone();
    let command_queue_clone = command_queue.clone();
    let config_clone = config.clone();
    let ws_client_clone = ws_client.clone();
    
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

                    // Skip if in startup state
                    if !state_manager_clone.allows_commands() {
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

                    // Skip if in startup state
                    if !state_manager_clone.allows_commands() {
                        warn!("Skipping bazaar flip during startup: {}", bazaar_flip.item_name);
                        continue;
                    }

                    info!("Received bazaar flip: {} x{} @ {} coins/unit ({})", 
                        bazaar_flip.item_name,
                        bazaar_flip.amount,
                        bazaar_flip.price_per_unit,
                        if bazaar_flip.is_buy_order { "BUY" } else { "SELL" }
                    );

                    // Queue the bazaar command
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
                        CommandPriority::Normal,
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
                    
                    // Check if this is a /cofl command that should be sent back to websocket
                    if cmd.trim().starts_with("/cofl") {
                        // Send /cofl commands to the websocket
                        let ws = ws_client_clone.clone();
                        tokio::spawn(async move {
                            if let Err(e) = ws.send_message(&cmd).await {
                                error!("Failed to send /cofl command to websocket: {}", e);
                            }
                        });
                    } else {
                        // Execute non-cofl commands sent by Coflnet to Minecraft
                        command_queue_clone.enqueue(
                            CommandType::SendChat { message: cmd },
                            CommandPriority::High,
                            false, // Not interruptible
                        );
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
                
                // Wait for command to be processed
                // TODO: Implement proper completion detection via window events
                sleep(Duration::from_secs(5)).await;
                
                command_queue_processor.complete_current();
            }
            
            // Small delay to prevent busy loop
            sleep(Duration::from_millis(50)).await;
        }
    });

    // Bot will complete its startup sequence automatically
    // The state will transition from Startup -> Idle after initialization
    info!("BAF initialization started - waiting for bot to complete setup...");

    // Keep the application running
    info!("BAF is now running. Press Ctrl+C to exit.");
    
    // Wait indefinitely
    loop {
        sleep(Duration::from_secs(60)).await;
        info!("Status: {} commands in queue", command_queue.len());
    }
}

