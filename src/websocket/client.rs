use super::messages::{parse_message_data, ChatMessage, WebSocketMessage};
use crate::types::{BazaarFlipRecommendation, Flip};
use anyhow::{Context, Result};
use futures::StreamExt;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};

pub enum CoflEvent {
    AuctionFlip(Flip),
    BazaarFlip(BazaarFlipRecommendation),
    ChatMessage(String),
    Command(String),
}

pub struct CoflWebSocket {
    #[allow(dead_code)]
    tx: mpsc::UnboundedSender<CoflEvent>,
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

        let (_write, mut read) = ws_stream.split();
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

        Ok((Self { tx }, rx))
    }

    fn handle_message(text: &str, tx: &mpsc::UnboundedSender<CoflEvent>) -> Result<()> {
        let msg: WebSocketMessage = serde_json::from_str(text)
            .context("Failed to parse WebSocket message")?;

        debug!("Received message type: {}", msg.msg_type);

        match msg.msg_type.as_str() {
            "flip" => {
                if let Ok(flip) = parse_message_data::<Flip>(&msg.data) {
                    debug!("Parsed auction flip: {:?}", flip.item_name);
                    let _ = tx.send(CoflEvent::AuctionFlip(flip));
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
                if let Ok(chat) = parse_message_data::<ChatMessage>(&msg.data) {
                    let _ = tx.send(CoflEvent::ChatMessage(chat.text));
                } else if let Ok(text) = parse_message_data::<String>(&msg.data) {
                    let _ = tx.send(CoflEvent::ChatMessage(text));
                }
            }
            "execute" => {
                if let Ok(command) = parse_message_data::<String>(&msg.data) {
                    let _ = tx.send(CoflEvent::Command(command));
                }
            }
            // Handle additional message types for 100% compatibility
            "swapProfile" | "createAuction" | "trade" | "tradeResponse" | 
            "getInventory" | "runSequence" | "privacySettings" => {
                // Log these message types but don't process them yet
                // These are advanced features not required for basic flipping
                info!("Received {} message (not yet implemented)", msg.msg_type);
                debug!("Message data: {}", msg.data);
            }
            _ => {
                // Log any unknown message types for debugging
                warn!("Unknown websocket message type: {}", msg.msg_type);
                debug!("Message data: {}", msg.data);
            }
        }

        Ok(())
    }
}
