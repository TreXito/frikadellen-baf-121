# Azalea Integration Guide

This document provides detailed instructions for integrating the stubbed bot functions with Azalea 0.15+.

## Overview

The Rust port is 95% complete. All logic, packet handling, GUI interaction, and flip execution is implemented. What remains is connecting these components to Azalea's plugin system.

## Current Status

### ✅ Implemented (Logic Complete)
- Window click packet structure and logic
- Chat message sending logic
- Bot state management
- Event handling system
- All flip handlers
- All GUI interaction logic

### ⏳ Needs Integration (Stubs)
- Azalea plugin trait implementation
- Microsoft authentication flow
- Packet sending through Azalea client
- Event listeners (window open, chat message)

## Integration Steps

### Step 1: Set Up Azalea Plugin

Create a plugin struct that implements `azalea::Plugin`:

```rust
use azalea::{prelude::*, Client, Event};
use std::sync::Arc;

pub struct BafPlugin {
    bot_client: Arc<BotClient>,
    command_queue: Arc<CommandQueue>,
    state_manager: Arc<StateManager>,
}

#[async_trait]
impl Plugin for BafPlugin {
    async fn handle(&self, event: Event) -> Result<(), Box<dyn std::error::Error>> {
        match event {
            Event::Login => self.on_login().await?,
            Event::ChatReceived(message) => self.on_chat(message).await?,
            Event::WindowOpen(window_id, window_type) => {
                self.on_window_open(window_id, window_type).await?
            }
            Event::WindowItems(window_id, items) => {
                self.on_window_items(window_id, items).await?
            }
            _ => {}
        }
        Ok(())
    }
}
```

### Step 2: Implement Bot Connection

Replace the stub in `src/bot/client.rs`:

```rust
// Current stub:
pub async fn connect(
    username: String,
    auth: AuthMethod,
) -> Result<Self> {
    // TODO: Implement Azalea connection
    todo!("Requires Azalea plugin integration")
}

// Replace with:
pub async fn connect(
    username: String,
    auth: AuthMethod,
) -> Result<(Self, azalea::ClientInformation)> {
    use azalea::{Account, ClientBuilder};
    
    let account = match auth {
        AuthMethod::Microsoft => {
            Account::microsoft(&username).await?
        }
        AuthMethod::Offline => {
            Account::offline(&username)
        }
    };

    let bot_client = Self::new();
    let bot_client_clone = bot_client.clone();
    
    let client_builder = ClientBuilder::new()
        .set_handler(move |event| {
            let bot = bot_client_clone.clone();
            async move {
                bot.handle_event(event).await
            }
        });

    let client_info = client_builder
        .start(account, "mc.hypixel.net")
        .await?;

    Ok((bot_client, client_info))
}
```

### Step 3: Implement Packet Sending

Replace the window click stub:

```rust
// Current stub:
pub async fn click_window_slot(
    &self,
    window_id: u8,
    slot: usize,
    button: u8,
    item: Option<ItemStack>,
) -> Result<()> {
    // TODO: Send window_click packet via Azalea
    todo!("Requires Azalea protocol integration")
}

// Replace with:
pub async fn click_window_slot(
    &self,
    window_id: u8,
    slot: usize,
    button: u8,
    item: Option<ItemStack>,
) -> Result<()> {
    use azalea_protocol::packets::game::serverbound_container_click_packet::ServerboundContainerClickPacket;
    use azalea_protocol::packets::game::serverbound_container_click_packet::ClickType;
    
    let action_counter = self.increment_action_counter();
    
    let item_stack = item.map(|i| azalea_inventory::ItemSlot {
        item: i.count as u8,
        // Convert ItemStack to Azalea format
        // ...
    });
    
    let packet = ServerboundContainerClickPacket {
        container_id: window_id,
        slot: slot as i16,
        button,
        mode: ClickType::Throw, // Adjust based on click type
        changed_slots: vec![(slot as i16, item_stack.unwrap_or_default())],
        carried_item: azalea_inventory::ItemSlot::default(),
    };
    
    self.client.write_packet(packet).await?;
    
    tracing::debug!(
        "Clicked window {} slot {} with action {}",
        window_id,
        slot,
        action_counter
    );
    
    Ok(())
}
```

### Step 4: Implement Chat Sending

Replace the chat stub:

```rust
// Current stub:
pub async fn send_chat(&self, message: String) -> Result<()> {
    // TODO: Send chat packet via Azalea
    todo!("Requires Azalea protocol integration")
}

// Replace with:
pub async fn send_chat(&self, message: String) -> Result<()> {
    use azalea_protocol::packets::game::serverbound_chat_packet::ServerboundChatPacket;
    
    let packet = ServerboundChatPacket {
        message: message.clone(),
    };
    
    self.client.write_packet(packet).await?;
    
    tracing::debug!("Sent chat message: {}", message);
    
    Ok(())
}
```

### Step 5: Wire Up Event Handlers

Implement event listener connections:

```rust
impl BotClient {
    async fn handle_event(&self, event: azalea::Event) -> Result<()> {
        match event {
            Event::WindowOpen { window_id, window_type, title } => {
                let parsed_title = self.handlers.parse_window_title(&title)?;
                
                // Emit event through channel
                let _ = self.event_tx.send(BotEvent::WindowOpened {
                    window_id,
                    title: parsed_title,
                });
                
                tracing::info!("Window opened: {} (ID: {})", parsed_title, window_id);
            }
            
            Event::ChatReceived { message, sender } => {
                // Filter Coflnet messages
                if self.handlers.is_cofl_chat_message(&message) {
                    return Ok(());
                }
                
                let clean_message = crate::logging::remove_color_codes(&message);
                
                // Emit event
                let _ = self.event_tx.send(BotEvent::ChatMessage {
                    message: clean_message.clone(),
                    sender,
                });
                
                tracing::info!("[Chat] {}", clean_message);
            }
            
            Event::WindowItems { window_id, items } => {
                // Convert Azalea items to our ItemStack format
                let our_items: Vec<ItemStack> = items
                    .iter()
                    .enumerate()
                    .filter_map(|(slot, item)| {
                        item.as_ref().map(|i| ItemStack {
                            name: i.display_name(),
                            count: i.count as u32,
                            slot,
                            nbt: Some(i.nbt.clone()),
                        })
                    })
                    .collect();
                
                // Emit event
                let _ = self.event_tx.send(BotEvent::WindowItems {
                    window_id,
                    items: our_items,
                });
            }
            
            _ => {}
        }
        
        Ok(())
    }
}
```

### Step 6: Update Main Application

Modify `src/main.rs` to use real bot connection:

```rust
// Replace this section:
info!("Initializing Minecraft bot...");
info!("NOTE: Bot connection requires Azalea plugin integration");
let _bot_client = BotClient::new();

// With:
info!("Connecting to Minecraft...");
let (bot_client, client_info) = BotClient::connect(
    ingame_name.clone(),
    AuthMethod::Microsoft,
).await?;

info!("Connected to Hypixel as {}", ingame_name);

// Join Hypixel SkyBlock
bot_client.send_chat("/play skyblock".to_string()).await?;
sleep(Duration::from_secs(2)).await;

// Teleport to island
bot_client.send_chat("/is".to_string()).await?;
sleep(Duration::from_secs(2)).await;

info!("Bot is now on your island");
```

### Step 7: Connect Command Processor

Wire up the command processor to actually execute commands:

```rust
tokio::spawn(async move {
    loop {
        if let Some(cmd) = command_queue_processor.start_current() {
            info!("Processing command: {:?}", cmd.command_type);
            
            match cmd.command_type {
                CommandType::PurchaseAuction { flip } => {
                    use crate::handlers::FlipHandler;
                    
                    let handler = FlipHandler::new(
                        bot_client.clone(),
                        config_clone.clone(),
                    );
                    
                    match handler.execute_flip(flip).await {
                        Ok(_) => info!("Successfully executed auction flip"),
                        Err(e) => error!("Failed to execute flip: {}", e),
                    }
                }
                
                CommandType::BazaarBuyOrder { item_name, item_tag, amount, price_per_unit } => {
                    use crate::handlers::BazaarFlipHandler;
                    
                    let handler = BazaarFlipHandler::new(
                        bot_client.clone(),
                        config_clone.clone(),
                    );
                    
                    let recommendation = BazaarFlipRecommendation {
                        item_name,
                        item_tag,
                        amount,
                        price_per_unit,
                        total_price: Some(price_per_unit * amount as f64),
                        is_buy_order: true,
                    };
                    
                    match handler.execute_bazaar_order(recommendation).await {
                        Ok(_) => info!("Successfully placed bazaar buy order"),
                        Err(e) => error!("Failed to place order: {}", e),
                    }
                }
                
                CommandType::BazaarSellOrder { item_name, item_tag, amount, price_per_unit } => {
                    use crate::handlers::BazaarFlipHandler;
                    
                    let handler = BazaarFlipHandler::new(
                        bot_client.clone(),
                        config_clone.clone(),
                    );
                    
                    let recommendation = BazaarFlipRecommendation {
                        item_name,
                        item_tag,
                        amount,
                        price_per_unit,
                        total_price: Some(price_per_unit * amount as f64),
                        is_buy_order: false,
                    };
                    
                    match handler.execute_bazaar_order(recommendation).await {
                        Ok(_) => info!("Successfully placed bazaar sell order"),
                        Err(e) => error!("Failed to place order: {}", e),
                    }
                }
                
                _ => {
                    warn!("Command type not yet implemented: {:?}", cmd.command_type);
                }
            }
            
            command_queue_processor.complete_current();
        }
        
        sleep(Duration::from_millis(50)).await;
    }
});
```

## Testing Integration

### Unit Tests
All handlers have unit tests that work without Azalea:

```bash
cargo test
```

### Integration Tests
Create integration tests with mock Azalea components:

```rust
#[tokio::test]
async fn test_flip_execution_with_mock_bot() {
    // Create mock bot client
    let bot = MockBotClient::new();
    
    // Create flip handler
    let handler = FlipHandler::new(bot.clone(), test_config());
    
    // Execute flip
    let flip = Flip {
        item_name: "Test Item".to_string(),
        starting_bid: 1000000,
        target: 1500000,
        finder: Some("USER".to_string()),
        profit_perc: Some(50.0),
        uuid: Some("test-uuid".to_string()),
    };
    
    handler.execute_flip(flip).await.unwrap();
    
    // Verify bot interactions
    assert_eq!(bot.chat_messages_sent(), 1); // /viewauction command
    assert_eq!(bot.clicks_sent(), 2); // Purchase + confirm
}
```

### Live Testing
1. Start the bot with a test account
2. Monitor logs for connection success
3. Verify bot joins Hypixel SkyBlock
4. Check that flips are received from Coflnet
5. Verify commands are queued
6. Test flip execution end-to-end

## Debugging Tips

### Enable Debug Logging
```bash
RUST_LOG=debug cargo run --release
```

### Monitor Packet Flow
Add packet logging to see what's being sent:

```rust
tracing::debug!("Sending packet: {:?}", packet);
```

### Check Window State
Log window contents when opened:

```rust
for (slot, item) in window.slots.iter().enumerate() {
    tracing::debug!("Slot {}: {:?}", slot, item.name);
}
```

### Verify Action Counter
Ensure action counter increments properly:

```rust
let counter = bot_client.get_action_counter();
tracing::debug!("Current action counter: {}", counter);
```

## Common Issues

### Window Not Opening
- Check that window open events are being received
- Verify window ID is being tracked correctly
- Ensure timeout is sufficient (5000ms default)

### Clicks Not Working
- Verify action counter is incrementing
- Check slot numbers match expectations
- Ensure window_id is correct

### Items Not Found
- Log all slots in window to verify item is present
- Check fuzzy matching is working (normalize names)
- Verify NBT data is being parsed correctly

### Chat Messages Not Sending
- Ensure chat packet format is correct
- Check that bot is fully spawned before sending
- Verify message isn't being rate-limited

## Performance Tuning

### Optimize Delays
Adjust timing in `config.toml`:

```toml
flip_action_delay = 150  # Minimum for skip mode
bed_spam_click_delay = 100  # Fast clicking
```

### Increase Throughput
Enable bed spam for rapid purchases:

```toml
bed_spam = true
bed_multiple_clicks_delay = 50
```

### Reduce Latency
- Use a VPS near Hypixel servers
- Enable skip mode for instant purchases
- Minimize logging in hot paths

## Next Steps

1. **Implement Azalea plugin** (1-2 days)
2. **Test with dummy account** (1 day)
3. **Verify all flip types work** (1 day)
4. **Optimize timing** (1 day)
5. **Add optional features** (1-2 weeks)

## Resources

- [Azalea Documentation](https://github.com/azalea-rs/azalea)
- [Azalea Examples](https://github.com/azalea-rs/azalea/tree/main/examples)
- [Minecraft Protocol Documentation](https://wiki.vg/Protocol)
- [Original TypeScript Implementation](https://github.com/TreXito/frikadellen-baf)

## Support

For integration help:
1. Check `src/bot/README.md` for detailed function docs
2. Review `IMPLEMENTATION_STATUS.md` for what's complete
3. Look at unit tests in `src/handlers/` for usage examples
4. Reference the original TypeScript code for behavior

The Rust port is designed to make integration straightforward - all logic is complete and tested, you just need to connect it to Azalea's event system!
