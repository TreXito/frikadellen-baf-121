# Bot Module

This module implements the core Azalea bot client for Frikadellen BAF (Bazaar Auction Flipper).

## ⚠️ Implementation Status

**IMPORTANT**: This is a **skeleton implementation** that compiles but requires completion for full functionality.

The current implementation provides:
- ✅ Core structure and types
- ✅ Event handling framework
- ✅ Window title parsing (with tests)
- ✅ Chat message filtering
- ✅ NBT data parsing utilities
- ✅ State management
- ❌ Actual bot connection (requires azalea 0.15 plugin integration)
- ❌ Window clicking (requires packet sending implementation)
- ❌ Chat sending (requires client instance access)

## Structure

- **client.rs** - Main bot client wrapper
  - Provides API structure for bot operations
  - Manages state and event channels
  - **Stub methods** for connect(), chat(), click_window()
  
- **handlers.rs** - Event handlers (FULLY IMPLEMENTED)
  - Parses window titles from JSON format ✅
  - Handles chat messages and filters Coflnet messages ✅
  - Tracks current window state ✅
  - Provides utilities for NBT parsing and item identification ✅

## Key Implementation Details

### From TypeScript Version

The implementation preserves all critical logic from `/tmp/frikadellen-baf/src/BAF.ts`:

1. **Window Click Mechanics** (from `fastWindowClick.ts`):
   ```typescript
   // Slot 31: Purchase button in BIN Auction View
   // Slot 11: Confirm button in Confirm Purchase
   // Action counter increments with each click (anti-cheat)
   ```

2. **Window Title Parsing**:
   ```json
   {"text":"","extra":[{"text":"Bazaar"}]}
   ```
   Extracts "Bazaar" from the JSON structure.

3. **Coflnet Message Filtering**:
   Messages starting with `[Chat]` are filtered out.

4. **NBT Parsing for SkyBlock Items**:
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

### Required Implementation

To complete the bot client, you need to:

1. **Implement Bot Connection** (client.rs:104-111):
   ```rust
   // Use azalea 0.15 API:
   let account = Account::microsoft(&username).await?;
   azalea::ClientBuilder::new()
       .set_handler(|bot, event, state| {
           // Event handling
       })
       .start(account, "mc.hypixel.net")
       .await?;
   ```

2. **Implement Window Clicking** (client.rs:172-178):
   ```rust
   // Send window_click packet:
   client.write_packet(ServerboundContainerClickPacket {
       container_id: window_id,
       slot_num: slot,
       button_num: button,
       state_id: action_counter,
       click_type: mode,
       changed_slots: vec![],
       carried_item: None,
   });
   ```

3. **Implement Chat Sending** (client.rs:164-169):
   ```rust
   // Through azalea Client instance:
   client.chat(message);
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

## Next Steps

1. Study azalea 0.15 examples to understand plugin architecture
2. Implement event handler with proper packet inspection
3. Add window state tracking through azalea's inventory system
4. Implement packet sending for window clicks
5. Add integration tests with mock server
