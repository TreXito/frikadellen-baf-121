# Bot Module

This module implements the core Azalea bot client for Frikadellen BAF (Bazaar Auction Flipper).

## ✅ Implementation Complete

The bot client now provides full integration with Azalea 0.15:

### Implemented Features

- ✅ Microsoft authentication (azalea::Account::microsoft)
- ✅ Connection to Hypixel (mc.hypixel.net)
- ✅ Event handler integration with azalea's bevy_ecs system
- ✅ Window packet handling (OpenScreen, ContainerClose)
- ✅ Chat message reception and filtering
- ✅ Window title parsing from JSON format
- ✅ Action counter for window clicks (anti-cheat protection)
- ✅ Packet sending for chat and window clicks
- ✅ State management and event emission
- ✅ NBT data parsing utilities (in handlers.rs)

### Architecture

The implementation follows azalea 0.15's plugin architecture:

1. **BotClient** - Main wrapper providing high-level API
2. **BotClientState** - ECS Component holding shared state across event handlers
3. **event_handler** - Function that processes azalea Events
4. **BotEventHandlers** - Utility methods for parsing and handling game data

## Structure

- **client.rs** - Main bot client with azalea integration
  - `connect()` - Authenticates and connects to Hypixel ✅
  - `chat()` - Sends chat messages ✅
  - `click_window()` - Sends container click packets ✅
  - `click_purchase()` and `click_confirm()` - Specific slot clicks ✅
  - Event handler for window open/close, chat, inventory ✅
  
- **handlers.rs** - Event handlers and utilities (FULLY IMPLEMENTED)
  - Parses window titles from JSON format ✅
  - Handles chat messages and filters Coflnet messages ✅
  - Tracks current window state ✅
  - Provides utilities for NBT parsing and item identification ✅

## Implementation Details

### From TypeScript Version

The implementation preserves all critical logic from `/tmp/frikadellen-baf/src/BAF.ts`:

1. **Connection & Authentication**:
   - Uses `Account::microsoft()` for authentication
   - Connects to `mc.hypixel.net` with azalea's ClientBuilder
   - Runs in separate thread with own tokio runtime

2. **Window Click Mechanics** (from `fastWindowClick.ts`):
   - Slot 31: Purchase button in BIN Auction View
   - Slot 11: Confirm button in Confirm Purchase
   - Action counter increments with each click (anti-cheat)
   - Uses `ServerboundContainerClick` packet

3. **Chat Message Sending**:
   - Uses azalea's `SendChatEvent` via ECS message system
   - Commands start with '/' and are automatically handled
   - Messages sent through `bot.ecs.lock().write_message()`

4. **Window Title Parsing**:
   ```json
   {"text":"","extra":[{"text":"Bazaar"}]}
   ```
   Extracts "Bazaar" from the JSON structure.

5. **Event Handling**:
   - Login event: Sets bot state to Idle
   - Chat event: Filters and logs messages
   - OpenScreen packet: Tracks window opens and parses titles
   - ContainerClose packet: Tracks window closes
   - Disconnect event: Logs disconnection reason

6. **Coflnet Message Filtering**:
   Messages starting with `[Chat]` are filtered out.

7. **NBT Parsing for SkyBlock Items**:
   - Extracts `ExtraAttributes.id` for item IDs
   - Parses `display.Name` for custom names
   - Handles both JSON and plain text formats

### Anti-Cheat Protection

```rust
pub fn action_counter(&self) -> i16 {
    *self.action_counter.read()
}

pub fn increment_action_counter(&self) {
    *self.action_counter.write() += 1;
}
```

Each window click must increment this counter to avoid detection.

### Usage Example

```rust
use frikadellen_baf::bot::client::BotClient;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut bot = BotClient::new();
    
    // Connect to Hypixel
    bot.connect("email@example.com".to_string()).await?;
    
    // Wait for events
    while let Some(event) = bot.next_event().await {
        match event {
            BotEvent::Login => println!("Bot logged in!"),
            BotEvent::WindowOpen(id, window_type, title) => {
                println!("Window opened: {} ({})", title, window_type);
            }
            BotEvent::ChatMessage(msg) => println!("Chat: {}", msg),
            _ => {}
        }
    }
    
    Ok(())
}
```

### Packet Sending from Event Handlers

For operations that require the Client instance (chat, window clicks), 
access is available within event handlers:

```rust
fn event_handler(bot: Client, event: Event, state: BotClientState) 
    -> impl std::future::Future<Output = Result<()>> + Send 
{
    async move {
        match event {
            Event::Init => {
                // Send a chat message
                bot.ecs.lock().write_message(SendChatEvent {
                    entity: bot.entity,
                    content: "/bz".to_string(),
                });
                
                // Click a window slot
                let packet = ServerboundContainerClick {
                    container_id: 0,
                    state_id: 0,
                    slot_num: 31,
                    button_num: 0,
                    click_type: ClickType::Pickup,
                    changed_slots: Default::default(),
                    carried_item: HashedStack(None),
                };
                bot.write_packet(packet);
            }
            _ => {}
        }
        Ok(())
    }
}
```

## Dependencies

Requires Rust **nightly** toolchain:

```bash
rustup install nightly
rustup default nightly
cargo build
```

## Testing

```bash
# Run tests (all pass)
cargo test --lib

# Run specific test
cargo test test_parse_window_title
```

Current test coverage:
- ✅ Window title parsing (3 tests)
- ✅ Color code removal (1 test)
- ✅ Coflnet message detection (1 test)
- ✅ Display name parsing (2 tests)
- ✅ Price parsing from lore (2 tests)

## References

- **Original TypeScript**: `/tmp/frikadellen-baf/src/BAF.ts`
- **FastWindowClick**: `/tmp/frikadellen-baf/src/fastWindowClick.ts`
- **Bazaar Handler**: `/tmp/frikadellen-baf/src/bazaarFlipHandler.ts`
- **Azalea Examples**: https://github.com/azalea-rs/azalea/tree/main/azalea/examples
- **Azalea 0.15 Docs**: Generated with `cargo doc --open`

## Notes

- The bot runs in a separate thread with its own tokio runtime
- Window clicks and chat messages should be sent from within event handlers where the Client is accessible
- The action counter is incremented automatically for each window click to prevent server-side detection
- Window IDs are tracked automatically when windows are opened
- All stub implementations have been replaced with full azalea 0.15 integration
