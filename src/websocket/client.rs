use super::messages::{parse_message_data, inject_referral_id, ChatMessage, WebSocketMessage};
use crate::types::{BazaarFlipRecommendation, Flip};
use anyhow::{Context, Result};
use futures::{stream::SplitSink, StreamExt, SinkExt};
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};
use std::sync::Arc;
use tracing::{debug, error, info, warn};

pub enum CoflEvent {
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
        let (tx, rx) = mpsc::unbounded_channel();
        let tx_clone = tx.clone();

        // Spawn task to handle incoming messages
        tokio::spawn(async move {
            while let Some(msg) = read.next().await {
                match msg {
                    Ok(Message::Text(text)) => {
                        if let Err(e) = Self::handle_message(&text, &tx_clone) {
                            error!("Error handling WebSocket message: {}", e);
                        }
                    }
                    Ok(Message::Close(_)) => {
                        warn!("WebSocket closed by server");
                        break;
                    }
                    Ok(Message::Ping(_data)) => {
                        debug!("Received ping, sending pong");
                        // Pong is handled automatically by tungstenite
                    }
                    Err(e) => {
                        error!("WebSocket error: {}", e);
                        break;
                    }
                    _ => {}
                }
            }
            info!("WebSocket connection closed");
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
        let msg: WebSocketMessage = serde_json::from_str(text)
            .context("Failed to parse WebSocket message")?;

        debug!("Received message type: {}", msg.msg_type);

        match msg.msg_type.as_str() {
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
                info!("Received getInventory request - will upload inventory");
                let _ = tx.send(CoflEvent::GetInventory);
            }
            "tradeResponse" => {
                info!("Received tradeResponse - will click slot 39");
                let _ = tx.send(CoflEvent::TradeResponse);
            }
            "privacySettings" => {
                info!("Received privacySettings");
                let _ = tx.send(CoflEvent::PrivacySettings(msg.data.clone()));
            }
            "swapProfile" => {
                info!("Received swapProfile request");
                if let Ok(profile_name) = parse_message_data::<String>(&msg.data) {
                    let _ = tx.send(CoflEvent::SwapProfile(profile_name));
                } else {
                    warn!("Failed to parse swapProfile data");
                }
            }
            "createAuction" => {
                info!("Received createAuction request");
                let _ = tx.send(CoflEvent::CreateAuction(msg.data.clone()));
            }
            "trade" => {
                info!("Received trade request");
                let _ = tx.send(CoflEvent::Trade(msg.data.clone()));
            }
            "runSequence" => {
                info!("Received runSequence request");
                let _ = tx.send(CoflEvent::RunSequence(msg.data.clone()));
            }
            "countdown" => {
                // COFL sends this ~10 seconds before AH flips arrive.
                // Matches TypeScript: used by bazaarFlipPauser to pause bazaar flips.
                info!("[COFL] Flips in 10 seconds (countdown)");
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
        let mut write = self.write.lock().await;
        write.send(Message::Text(message.to_string())).await
            .context("Failed to send message to WebSocket")?;
        info!("Sent message to COFL WebSocket: {}", message);
        Ok(())
    }
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
}
