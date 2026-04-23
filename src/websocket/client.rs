use super::messages::{parse_message_data, inject_referral_id, ChatMessage, WebSocketMessage};
use crate::types::{BazaarFlipRecommendation, Flip};
use anyhow::{Context, Result};
use futures::{stream::SplitSink, StreamExt, SinkExt};
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};
use std::sync::Arc;
use tracing::{debug, error, info, warn};

pub enum CoflEvent {
    LoggedIn,
    AuctionFlip(Flip),
    BazaarFlip(BazaarFlipRecommendation),
    ChatMessage(String),
    Command(String),
    GetInventory,
    TradeResponse,
    PrivacySettings(String), // Store raw JSON for now
    SwapProfile(String),     // Profile name
    CreateAuction(String),   // Auction data as JSON
    Trade(String),           // Trade data as JSON
    RunSequence(String),     // Sequence data as JSON
    /// COFL "countdown" message – AH flips arriving in ~10 seconds.
    /// Used to pause bazaar flips while the AH flip window is active.
    Countdown,
    /// Parsed license list from `/cofl licenses list` response.
    /// Fields: `(entries, page_number)` where entries are `(ign, 1-based page-local index, tier)` tuples
    /// and `page_number` is the 1-based page that was returned.
    LicenseList {
        entries: Vec<(String, u32, String)>,
        page: u32,
    },
}

#[derive(Clone)]
pub struct CoflWebSocket {
    #[allow(dead_code)]
    tx: mpsc::UnboundedSender<CoflEvent>,
    write: Arc<Mutex<SplitSink<WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>, Message>>>,
}

impl CoflWebSocket {
    pub async fn connect(
        url: String,
        username: String,
        version: String,
        session_id: String,
    ) -> Result<(Self, mpsc::UnboundedReceiver<CoflEvent>)> {
        let full_url = format!(
            "{}?player={}&version={}&SId={}",
            url, username, version, session_id
        );

        info!("Connecting to Coflnet WebSocket: {}", url);

        let (ws_stream, _) = connect_async(&full_url)
            .await
            .context("Failed to connect to WebSocket")?;

        info!("WebSocket connected successfully");

        let (write, mut read) = ws_stream.split();
        let write = Arc::new(Mutex::new(write));
        let write_for_task = write.clone();
        let (tx, rx) = mpsc::unbounded_channel();
        let tx_clone = tx.clone();

        // Spawn task to handle incoming messages, with automatic reconnection
        tokio::spawn(async move {
            loop {
                // ── inner read loop ───────────────────────────────────────────
                loop {
                    match read.next().await {
                        Some(Ok(Message::Text(text))) => {
                            if let Err(e) = Self::handle_message(&text, &tx_clone) {
                                error!("Error handling WebSocket message: {}", e);
                            }
                        }
                        Some(Ok(Message::Close(_))) => {
                            warn!("WebSocket closed by server");
                            break;
                        }
                        Some(Ok(Message::Ping(_data))) => {
                            debug!("Received ping, sending pong");
                            // Pong is handled automatically by tungstenite
                        }
                        Some(Err(e)) => {
                            error!("WebSocket error: {}", e);
                            break;
                        }
                        None => {
                            warn!("WebSocket stream ended");
                            break;
                        }
                        Some(Ok(_)) => {}
                    }
                }

                // ── reconnection loop ─────────────────────────────────────────
                let _ = tx_clone.send(CoflEvent::ChatMessage(
                    "§f[§4BAF§f]: §cWebSocket disconnected — reconnecting...".to_string(),
                ));

                let mut backoff_secs = 5u64;
                loop {
                    tokio::time::sleep(tokio::time::Duration::from_secs(backoff_secs)).await;
                    match connect_async(&full_url).await {
                        Ok((new_stream, _)) => {
                            let (new_write, new_read) = new_stream.split();
                            *write_for_task.lock().await = new_write;
                            read = new_read;
                            info!("[WS] Reconnected to COFL WebSocket");
                            let _ = tx_clone.send(CoflEvent::ChatMessage(
                                "§f[§4BAF§f]: §aWebSocket reconnected!".to_string(),
                            ));
                            break;
                        }
                        Err(e) => {
                            error!("[WS] Reconnection failed (retry in {}s): {}", backoff_secs, e);
                            backoff_secs = (backoff_secs * 2).min(60);
                        }
                    }
                }
                // Resume outer loop → inner read loop continues on new connection
            }
        });

        Ok((Self { tx, write }, rx))
    }

    /// Format and send an authentication prompt to the user
    fn send_auth_prompt(tx: &mpsc::UnboundedSender<CoflEvent>, text: &str, url: &str) {
        let auth_prompt = format!(
            "§f[§4BAF§f]: §c========================================\n\
             §f[§4BAF§f]: §c§lCOFL Authentication Required!\n\
             §f[§4BAF§f]: §e{}\n\
             §f[§4BAF§f]: §bAuthentication URL: §f{}\n\
             §f[§4BAF§f]: §c========================================",
            text, url
        );
        let _ = tx.send(CoflEvent::ChatMessage(auth_prompt));
    }

    fn handle_message(text: &str, tx: &mpsc::UnboundedSender<CoflEvent>) -> Result<()> {
        info!("[COFL <-] {}", text);
        let msg: WebSocketMessage = serde_json::from_str(text)
            .context("Failed to parse WebSocket message")?;

        info!("[COFL <-] type={} data={}", msg.msg_type, msg.data);

        match msg.msg_type.as_str() {
            "loggedIn" => {
                debug!("Received loggedIn");
                let _ = tx.send(CoflEvent::LoggedIn);
            }
            "flip" => {
                if let Ok(value) = parse_message_data::<serde_json::Value>(&msg.data) {
                    // Normalize: COFL sends itemName/startingBid nested inside "auction"
                    // but also provides "id" at the top level as the auction UUID.
                    // Promote auction sub-fields to the top level when missing there.
                    let normalized = normalize_flip_value(value);
                    if let Ok(flip) = serde_json::from_value::<Flip>(normalized) {
                        debug!("Parsed auction flip: {:?}", flip.item_name);
                        let _ = tx.send(CoflEvent::AuctionFlip(flip));
                    }
                }
            }
            "bazaarFlip" | "bzRecommend" | "placeOrder" => {
                if let Ok(bazaar_flip) = parse_message_data::<BazaarFlipRecommendation>(&msg.data) {
                    debug!("Parsed bazaar flip: {:?}", bazaar_flip.item_name);
                    let _ = tx.send(CoflEvent::BazaarFlip(bazaar_flip));
                } else {
                    warn!("Failed to parse bazaar flip from '{}' message (data length: {} bytes)", msg.msg_type, msg.data.len());
                }
            }
            "getbazaarflips" => {
                // Handle array of bazaar flips
                if let Ok(flips) = parse_message_data::<Vec<BazaarFlipRecommendation>>(&msg.data) {
                    debug!("Parsed {} bazaar flips", flips.len());
                    for flip in flips {
                        let _ = tx.send(CoflEvent::BazaarFlip(flip));
                    }
                }
            }
            "chatMessage" | "writeToChat" => {
                // Try to parse as array of chat messages (most common for chatMessage)
                if let Ok(messages) = parse_message_data::<Vec<ChatMessage>>(&msg.data) {
                    // Check if this looks like a licenses list response and emit a
                    // LicenseList event so the main loop can auto-detect the license
                    // index for the current IGN.
                    let license_entries = parse_license_entries(&messages);
                    if !license_entries.is_empty() {
                        let page = parse_license_page_number(&messages);
                        let _ = tx.send(CoflEvent::LicenseList { entries: license_entries, page });
                    }

                    for msg in messages {
                        let msg_with_ref = msg.with_referral_id();
                        
                        // If there's an onClick URL with authmod, this is an authentication prompt
                        if let Some(ref on_click) = msg_with_ref.on_click {
                            if on_click.contains("sky.coflnet.com/authmod") {
                                Self::send_auth_prompt(tx, &msg_with_ref.text, on_click);
                                continue;
                            }
                        }
                        
                        let _ = tx.send(CoflEvent::ChatMessage(msg_with_ref.text));
                    }
                } else if let Ok(chat) = parse_message_data::<ChatMessage>(&msg.data) {
                    // Single chat message (common for writeToChat)
                    // Also check for license entries in single-message responses
                    let single = [chat.clone()];
                    let license_entries = parse_license_entries(&single);
                    if !license_entries.is_empty() {
                        let page = parse_license_page_number(&single);
                        let _ = tx.send(CoflEvent::LicenseList { entries: license_entries, page });
                    }

                    let msg_with_ref = chat.with_referral_id();
                    
                    // Check for authentication URL
                    if let Some(ref on_click) = msg_with_ref.on_click {
                        if on_click.contains("sky.coflnet.com/authmod") {
                            Self::send_auth_prompt(tx, &msg_with_ref.text, on_click);
                            return Ok(());
                        }
                    }
                    
                    let _ = tx.send(CoflEvent::ChatMessage(msg_with_ref.text));
                } else if let Ok(text) = parse_message_data::<String>(&msg.data) {
                    // Fallback: plain text string
                    let text_with_ref = inject_referral_id(&text);
                    let _ = tx.send(CoflEvent::ChatMessage(text_with_ref));
                }
            }
            "execute" => {
                if let Ok(command) = parse_message_data::<String>(&msg.data) {
                    let _ = tx.send(CoflEvent::Command(command));
                }
            }
            // Handle ALL message types for 100% compatibility (matching TypeScript BAF.ts)
            "getInventory" => {
                debug!("Received getInventory request");
                let _ = tx.send(CoflEvent::GetInventory);
            }
            "tradeResponse" => {
                debug!("Received tradeResponse");
                let _ = tx.send(CoflEvent::TradeResponse);
            }
            "privacySettings" => {
                debug!("Received privacySettings");
                let _ = tx.send(CoflEvent::PrivacySettings(msg.data.clone()));
            }
            "swapProfile" => {
                debug!("Received swapProfile request");
                if let Ok(profile_name) = parse_message_data::<String>(&msg.data) {
                    let _ = tx.send(CoflEvent::SwapProfile(profile_name));
                } else {
                    warn!("Failed to parse swapProfile data");
                }
            }
            "createAuction" => {
                debug!("Received createAuction request");
                let _ = tx.send(CoflEvent::CreateAuction(msg.data.clone()));
            }
            "trade" => {
                debug!("Received trade request");
                let _ = tx.send(CoflEvent::Trade(msg.data.clone()));
            }
            "runSequence" => {
                debug!("Received runSequence request");
                let _ = tx.send(CoflEvent::RunSequence(msg.data.clone()));
            }
            "countdown" => {
                // COFL sends this ~10 seconds before AH flips arrive.
                // Matches TypeScript: used by bazaarFlipPauser to pause bazaar flips.
                debug!("Received countdown");
                let _ = tx.send(CoflEvent::Countdown);
            }
            _ => {
                // Log any unknown message types for debugging
                warn!("Unknown websocket message type: {}", msg.msg_type);
                debug!("Message data: {}", msg.data);
            }
        }

        Ok(())
    }

    /// Send a message to the COFL WebSocket
    pub async fn send_message(&self, message: &str) -> Result<()> {
        if let Some(payload) = extract_upload_inventory_payload(message) {
            info!("[Inventory] uploadInventory payload: {}", payload);
            info!("[Inventory] uploadInventory ws message: {}", message);
        }
        let mut write = self.write.lock().await;
        write.send(Message::Text(message.to_string())).await
            .context("Failed to send message to WebSocket")?;
        info!("[COFL ->] {}", message);
        debug!("Sent WS message ({} bytes)", message.len());
        Ok(())
    }

    /// Transfer a COFL license to a different IGN.
    ///
    /// Sends `/cofl license use <license_index> <target_ign>` via the WebSocket.
    /// Used before account switching to move the license to the next account.
    pub async fn transfer_license(&self, license_index: u32, target_ign: &str) -> Result<()> {
        let args = format!("use {} {}", license_index, target_ign);
        let data_json = serde_json::json!(args).to_string();
        let message = serde_json::json!({
            "type": "license",
            "data": data_json
        }).to_string();
        self.send_message(&message).await?;
        info!("[LicenseTransfer] Sent /cofl license use {} {}", license_index, target_ign);
        Ok(())
    }

    /// Set the default license account to the given IGN.
    ///
    /// Sends `/cofl license default <ign>` via the WebSocket so that a new IGN
    /// inherits the subscription tier from the user's default account.
    pub async fn set_default_license(&self, ign: &str) -> Result<()> {
        let args = format!("default {}", ign);
        let data_json = serde_json::json!(args).to_string();
        let message = serde_json::json!({
            "type": "license",
            "data": data_json
        }).to_string();
        self.send_message(&message).await?;
        info!("[LicenseDefault] Sent /cofl license default {}", ign);
        Ok(())
    }
}

/// Prefix for license entry text lines in COFL's licenses list response: `§7> §a`
/// When searching by IGN, the format includes a global index: `§7N> §a` where N is the number.
const LICENSE_ENTRY_PREFIX: &str = "\u{00a7}7> \u{00a7}a";

/// Suffix after the digits in a numbered license entry: `> §a`
const LICENSE_NUMBERED_SUFFIX: &str = "> \u{00a7}a";

/// License tier value indicating no active license (default/expired).
const LICENSE_TIER_NONE: &str = "NONE";

/// Parse the page number from a COFL licenses list response.
///
/// COFL includes a line like `"Content (page 1):"` or `"Content (page 2):"`.
/// Returns the parsed page number, defaulting to 1 if not found.
pub fn parse_license_page_number(messages: &[ChatMessage]) -> u32 {
    for msg in messages {
        // Look for "Content (page N):" pattern
        if let Some(start) = msg.text.find("(page ") {
            let rest = &msg.text[start + 6..]; // skip "(page "
            let num_str: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            match num_str.parse::<u32>() {
                Ok(n) => return n,
                Err(_) => {
                    tracing::debug!("[LicenseDetect] Found page indicator but failed to parse number from '{}'", num_str);
                }
            }
        }
    }
    1 // Default to page 1 if not found
}

/// Parse license entries from a COFL licenses list chatMessage response.
///
/// Supports two formats:
///   **Page listing** — no global index prefix:
///     `§7> §aIGN_NAME §2§mNONE§c expired`  (expired license)
///     `§7> §aIGN_NAME §2TIER`               (active license)
///
///   **Search results** — global index prefix:
///     `§7N> §aIGN_NAME §2TIER Xd`           (e.g. `§716> §aTreXitooo §2PREMIUM 29.9d`)
///
/// Returns `(ign, index, tier)` tuples.  For page listings the index is a
/// 1-based page-local counter; for search results it is the global license
/// index parsed from the `N>` prefix.
pub fn parse_license_entries(messages: &[ChatMessage]) -> Vec<(String, u32, String)> {
    let mut entries = Vec::new();
    let mut counter: u32 = 0;

    for msg in messages {
        // Split by newlines so entries embedded in multi-line ChatMessages
        // (e.g. the COFL server sending the whole response in one text field)
        // are still detected.
        for line in msg.text.split('\n') {
            // Match the exact old format: `§7> §a...`
            if line.starts_with(LICENSE_ENTRY_PREFIX) {
                counter += 1;
                let rest = &line[LICENSE_ENTRY_PREFIX.len()..];
                let ign: String = rest
                    .chars()
                    .take_while(|&c| c != ' ' && c != '\u{00a7}')
                    .collect();
                if !ign.is_empty() {
                    let tier = extract_license_tier(&rest[ign.len()..]);
                    entries.push((ign, counter, tier));
                }
            }
            // Match search result format: `§7N> §a...` where N is digits
            else if line.starts_with("\u{00a7}7") {
                let after_color = &line["\u{00a7}7".len()..];
                // Try to read digits followed by "> §a"
                let num_str: String = after_color
                    .chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect();
                if !num_str.is_empty() {
                    let rest_after_num = &after_color[num_str.len()..];
                    if rest_after_num.starts_with(LICENSE_NUMBERED_SUFFIX) {
                        if let Ok(global_idx) = num_str.parse::<u32>() {
                            let ign_start = &rest_after_num[LICENSE_NUMBERED_SUFFIX.len()..];
                            let ign: String = ign_start
                                .chars()
                                .take_while(|&c| c != ' ' && c != '\u{00a7}')
                                .collect();
                            if !ign.is_empty() {
                                let tier = extract_license_tier(&ign_start[ign.len()..]);
                                entries.push((ign, global_idx, tier));
                            }
                        }
                    }
                }
            }
        }
    }

    entries
}

/// Extract the license tier from the text following an IGN in a COFL license entry.
///
/// Input examples:
///   ` §2§mNONE§c expired`  →  `"NONE"`
///   ` §2PREMIUM 9.9d`      →  `"PREMIUM"`
fn extract_license_tier(text: &str) -> String {
    // Find §2 color code (§ = U+00A7, 2 bytes in UTF-8, plus '2')
    let marker = "\u{00a7}2";
    if let Some(pos) = text.find(marker) {
        let after = &text[pos + marker.len()..];
        // Skip optional §m (strikethrough for expired)
        let tier_start = if after.starts_with("\u{00a7}m") {
            &after["\u{00a7}m".len()..]
        } else {
            after
        };
        // Read tier name until space or §
        let tier: String = tier_start
            .chars()
            .take_while(|&c| c != ' ' && c != '\u{00a7}')
            .collect();
        if !tier.is_empty() {
            return tier;
        }
    }
    LICENSE_TIER_NONE.to_string()
}

fn extract_upload_inventory_payload(message: &str) -> Option<String> {
    if !message.contains("\"uploadInventory\"") {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(message).ok()?;
    if value.get("type")?.as_str()? != "uploadInventory" {
        return None;
    }
    let data = value.get("data")?;
    Some(match data {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    })
}

/// Normalize a flip JSON value so that `itemName` and `startingBid` are always
/// at the top level, even when the COFL server nests them inside an `auction`
/// sub-object.  The `id` field (auction UUID) is already at the top level in
/// the new format and is picked up by the `alias = "id"` on the `Flip.uuid`
/// field.
pub fn normalize_flip_value(mut value: serde_json::Value) -> serde_json::Value {
    if let Some(auction) = value.get("auction").cloned() {
        if let Some(obj) = value.as_object_mut() {
            if obj.get("itemName").map(|v| v.is_null()).unwrap_or(true) {
                if let Some(name) = auction.get("itemName") {
                    obj.insert("itemName".to_string(), name.clone());
                }
            }
            if obj.get("startingBid").map(|v| v.is_null()).unwrap_or(true) {
                if let Some(bid) = auction.get("startingBid") {
                    obj.insert("startingBid".to_string(), bid.clone());
                }
            }
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Flip;

    #[test]
    fn test_normalize_flip_value_nested_auction() {
        // New COFL format: id at top level, itemName/startingBid nested in auction
        let json = serde_json::json!({
            "id": "4f1d2446974e43dbaf644fb13cd8af62",
            "auction": {
                "itemName": "§dTreacherous Rod of the Sea",
                "startingBid": 15000000
            },
            "target": 29314940,
            "finder": "SNIPER_MEDIAN"
        });

        let normalized = normalize_flip_value(json);
        let flip: Flip = serde_json::from_value(normalized).expect("should parse");

        assert_eq!(flip.item_name, "§dTreacherous Rod of the Sea");
        assert_eq!(flip.starting_bid, 15000000);
        assert_eq!(flip.target, 29314940);
        assert_eq!(flip.uuid.as_deref(), Some("4f1d2446974e43dbaf644fb13cd8af62"));
    }

    #[test]
    fn test_normalize_flip_value_flat_format() {
        // Old COFL format: itemName/startingBid already at top level (no auction nesting)
        let json = serde_json::json!({
            "itemName": "§dWithered Giant's Sword §6✪✪✪✪✪",
            "startingBid": 100000000,
            "target": 111164880,
            "finder": "SNIPER_MEDIAN",
            "profitPerc": 7.0
        });

        let normalized = normalize_flip_value(json);
        let flip: Flip = serde_json::from_value(normalized).expect("should parse");

        assert_eq!(flip.item_name, "§dWithered Giant's Sword §6✪✪✪✪✪");
        assert_eq!(flip.starting_bid, 100000000);
        assert_eq!(flip.uuid, None);
    }

    #[test]
    fn test_normalize_flip_value_does_not_overwrite_top_level() {
        // When itemName already exists at top level, auction.itemName should not overwrite it
        let json = serde_json::json!({
            "id": "abc123",
            "itemName": "Top Level Item",
            "startingBid": 5000000,
            "auction": {
                "itemName": "Nested Item",
                "startingBid": 9999999
            },
            "target": 10000000,
            "finder": "SNIPER"
        });

        let normalized = normalize_flip_value(json);
        let flip: Flip = serde_json::from_value(normalized).expect("should parse");

        assert_eq!(flip.item_name, "Top Level Item");
        assert_eq!(flip.starting_bid, 5000000);
        assert_eq!(flip.uuid.as_deref(), Some("abc123"));
    }

    #[test]
    fn test_extract_upload_inventory_payload_for_upload_inventory() {
        let message = serde_json::json!({
            "type": "uploadInventory",
            "data": "[{\"name\":\"minecraft:stone\"}]"
        })
        .to_string();

        let payload = extract_upload_inventory_payload(&message);
        assert_eq!(payload.as_deref(), Some("[{\"name\":\"minecraft:stone\"}]"));
    }

    #[test]
    fn test_extract_upload_inventory_payload_ignores_other_messages() {
        let message = serde_json::json!({
            "type": "uploadScoreboard",
            "data": "[\"www.hypixel.net\"]"
        })
        .to_string();

        assert!(extract_upload_inventory_payload(&message).is_none());
    }

    #[test]
    fn test_extract_upload_inventory_payload_handles_non_string_data() {
        let message = serde_json::json!({
            "type": "uploadInventory",
            "data": [{"name":"minecraft:stone","count":1}]
        })
        .to_string();

        let payload = extract_upload_inventory_payload(&message);
        assert_eq!(
            payload.as_deref(),
            Some("[{\"count\":1,\"name\":\"minecraft:stone\"}]")
        );
    }

    #[test]
    fn test_parse_license_entries_from_cofl_response() {
        use crate::websocket::messages::ChatMessage;
        // Simulate a COFL licenses list response (simplified from real output)
        let messages = vec![
            ChatMessage { text: "[§1C§6oflnet§f]§7: ".to_string(), on_click: None, hover: None },
            ChatMessage { text: "Content (page 1):§3(1)".to_string(), on_click: Some("/cofl licenses ls 2".to_string()), hover: None },
            ChatMessage { text: "\n".to_string(), on_click: None, hover: None },
            ChatMessage { text: "§7> §azShadowReaper_ §2§mNONE§c expired".to_string(), on_click: None, hover: None },
            ChatMessage { text: " §a[RENEW]§7§3(2)".to_string(), on_click: Some("/cofl licenses add 651c NONE".to_string()), hover: None },
            ChatMessage { text: "\n".to_string(), on_click: None, hover: None },
            ChatMessage { text: "§7> §ausaiddd §2§mNONE§c expired".to_string(), on_click: None, hover: None },
            ChatMessage { text: " §a[RENEW]§7§3(3)".to_string(), on_click: Some("/cofl licenses add 58f1 NONE".to_string()), hover: None },
            ChatMessage { text: "\n".to_string(), on_click: None, hover: None },
            ChatMessage { text: "§7> §aoBlanky_ §2§mNONE§c expired".to_string(), on_click: None, hover: None },
            ChatMessage { text: " §a[RENEW]§7§3(4)".to_string(), on_click: None, hover: None },
        ];

        let entries = parse_license_entries(&messages);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0], ("zShadowReaper_".to_string(), 1, "NONE".to_string()));
        assert_eq!(entries[1], ("usaiddd".to_string(), 2, "NONE".to_string()));
        assert_eq!(entries[2], ("oBlanky_".to_string(), 3, "NONE".to_string()));
    }

    #[test]
    fn test_parse_license_entries_empty_array() {
        let entries = parse_license_entries(&[]);
        assert!(entries.is_empty());
    }

    #[test]
    fn test_parse_license_entries_no_license_entries() {
        use crate::websocket::messages::ChatMessage;
        // A non-license chatMessage should return empty
        let messages = vec![
            ChatMessage { text: "[§1C§6oflnet§f]§7: ".to_string(), on_click: None, hover: None },
            ChatMessage { text: "Some other message".to_string(), on_click: None, hover: None },
        ];
        let entries = parse_license_entries(&messages);
        assert!(entries.is_empty());
    }

    #[test]
    fn test_parse_license_entries_case_insensitive_lookup() {
        use crate::websocket::messages::ChatMessage;
        let messages = vec![
            ChatMessage { text: "§7> §aPlayerOne §2NONE".to_string(), on_click: None, hover: None },
            ChatMessage { text: "§7> §aPlayerTwo §2NONE".to_string(), on_click: None, hover: None },
        ];
        let entries = parse_license_entries(&messages);
        assert_eq!(entries.len(), 2);
        // The parser returns the IGN as-is; case-insensitive matching is done at lookup time
        assert_eq!(entries[0].0, "PlayerOne");
        assert_eq!(entries[1].0, "PlayerTwo");
    }

    #[test]
    fn test_parse_license_page_number_from_response() {
        use crate::websocket::messages::ChatMessage;
        let messages = vec![
            ChatMessage { text: "[§1C§6oflnet§f]§7: ".to_string(), on_click: None, hover: None },
            ChatMessage { text: "Content (page 1):§3(1)".to_string(), on_click: None, hover: None },
            ChatMessage { text: "§7> §aPlayer1 §2NONE".to_string(), on_click: None, hover: None },
        ];
        assert_eq!(parse_license_page_number(&messages), 1);
    }

    #[test]
    fn test_parse_license_page_number_page_2() {
        use crate::websocket::messages::ChatMessage;
        let messages = vec![
            ChatMessage { text: "[§1C§6oflnet§f]§7: ".to_string(), on_click: None, hover: None },
            ChatMessage { text: "Content (page 2):§3(5)".to_string(), on_click: None, hover: None },
            ChatMessage { text: "§7> §aPlayer4 §2NONE".to_string(), on_click: None, hover: None },
        ];
        assert_eq!(parse_license_page_number(&messages), 2);
    }

    #[test]
    fn test_parse_license_page_number_defaults_to_1() {
        use crate::websocket::messages::ChatMessage;
        let messages = vec![
            ChatMessage { text: "some other message".to_string(), on_click: None, hover: None },
        ];
        assert_eq!(parse_license_page_number(&messages), 1);
    }

    #[test]
    fn test_license_entries_page_local_indices() {
        use crate::websocket::messages::ChatMessage;
        // Entries on any page always start from 1 (page-local indexing).
        // The caller adds the cumulative offset from previous pages.
        let page2_messages = vec![
            ChatMessage { text: "Content (page 2):§3(5)".to_string(), on_click: None, hover: None },
            ChatMessage { text: "§7> §aPlayer4 §2NONE".to_string(), on_click: None, hover: None },
            ChatMessage { text: "§7> §aPlayer5 §2NONE".to_string(), on_click: None, hover: None },
        ];
        let entries = parse_license_entries(&page2_messages);
        // Page-local indices: 1, 2 (caller must add offset from page 1's entry count)
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0], ("Player4".to_string(), 1, "NONE".to_string()));
        assert_eq!(entries[1], ("Player5".to_string(), 2, "NONE".to_string()));
    }

    #[test]
    fn test_parse_license_entries_with_premium_tier() {
        use crate::websocket::messages::ChatMessage;
        // Simulate a response with mixed tiers (NONE and PREMIUM for the same IGN)
        let messages = vec![
            ChatMessage { text: "[§1C§6oflnet§f]§7: ".to_string(), on_click: None, hover: None },
            ChatMessage { text: "Content (page 1):§3(1)".to_string(), on_click: None, hover: None },
            ChatMessage { text: "§7> §aargamer1014 §2§mNONE§c expired".to_string(), on_click: None, hover: None },
            ChatMessage { text: "§7> §aargamer1014 §2PREMIUM 9.9d".to_string(), on_click: None, hover: None },
        ];
        let entries = parse_license_entries(&messages);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0], ("argamer1014".to_string(), 1, "NONE".to_string()));
        assert_eq!(entries[1], ("argamer1014".to_string(), 2, "PREMIUM".to_string()));
    }

    #[test]
    fn test_extract_license_tier_from_entry_text() {
        // Test the tier extraction helper directly
        assert_eq!(extract_license_tier(" §2§mNONE§c expired"), "NONE");
        assert_eq!(extract_license_tier(" §2PREMIUM 9.9d"), "PREMIUM");
        assert_eq!(extract_license_tier(" §2NONE"), "NONE");
        assert_eq!(extract_license_tier(" §2STARTER_PREMIUM 2.1d"), "STARTER_PREMIUM");
        // Fallback when no §2 found
        assert_eq!(extract_license_tier(""), "NONE");
    }

    #[test]
    fn test_parse_license_entries_search_result_with_global_index() {
        use crate::websocket::messages::ChatMessage;
        // Simulate a COFL `/cofl licenses list trexitooo` search response
        // which returns entries with global index prefixes like `§716> §a`
        let messages = vec![
            ChatMessage { text: "[§1C§6oflnet§f]§7: ".to_string(), on_click: None, hover: None },
            ChatMessage { text: "Search for trexitooo resulted in:".to_string(), on_click: None, hover: None },
            ChatMessage { text: "\n".to_string(), on_click: None, hover: None },
            ChatMessage { text: "§716> §aTreXitooo §2PREMIUM 29.9d".to_string(), on_click: None, hover: None },
            ChatMessage { text: " §a[EXTEND]§7".to_string(), on_click: None, hover: None },
        ];
        let entries = parse_license_entries(&messages);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0], ("TreXitooo".to_string(), 16, "PREMIUM".to_string()));
    }

    #[test]
    fn test_parse_license_entries_search_result_multiple() {
        use crate::websocket::messages::ChatMessage;
        // Search result with multiple entries at different global indices
        let messages = vec![
            ChatMessage { text: "§716> §aTreXitooo §2PREMIUM 29.9d".to_string(), on_click: None, hover: None },
            ChatMessage { text: "§742> §aTreXitooo §2§mNONE§c expired".to_string(), on_click: None, hover: None },
        ];
        let entries = parse_license_entries(&messages);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0], ("TreXitooo".to_string(), 16, "PREMIUM".to_string()));
        assert_eq!(entries[1], ("TreXitooo".to_string(), 42, "NONE".to_string()));
    }

    #[test]
    fn test_parse_license_entries_multiline_chat_message() {
        use crate::websocket::messages::ChatMessage;
        // COFL may send the entire search response as a single multi-line ChatMessage
        // instead of separate ChatMessage objects per line. The parser must handle
        // entries embedded within newline-separated text.
        let messages = vec![
            ChatMessage {
                text: "[§1C§6oflnet§f]§7: \nSearch for xtytextorial resulted in:\n\n§719> §aXtyTextorial §2PREMIUM 29.9d\n §a[EXTEND]§7".to_string(),
                on_click: None,
                hover: None,
            },
        ];
        let entries = parse_license_entries(&messages);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0], ("XtyTextorial".to_string(), 19, "PREMIUM".to_string()));
    }

    #[test]
    fn test_parse_license_entries_multiline_page_format() {
        use crate::websocket::messages::ChatMessage;
        // Page listing entries embedded in a single multi-line ChatMessage
        let messages = vec![
            ChatMessage {
                text: "Content (page 1):§3(1)\n§7> §aPlayer1 §2NONE\n§7> §aPlayer2 §2PREMIUM 9.9d".to_string(),
                on_click: None,
                hover: None,
            },
        ];
        let entries = parse_license_entries(&messages);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0], ("Player1".to_string(), 1, "NONE".to_string()));
        assert_eq!(entries[1], ("Player2".to_string(), 2, "PREMIUM".to_string()));
    }

    #[test]
    fn test_handle_logged_in_message() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let message = r#"{"type":"loggedIn","data":"true"}"#;

        CoflWebSocket::handle_message(message, &tx).expect("loggedIn message should parse");

        assert!(matches!(rx.try_recv(), Ok(CoflEvent::LoggedIn)));
    }
}
