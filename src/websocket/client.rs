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
                // Try to parse as array of chat messages (most common for chatMessage)
                if let Ok(messages) = parse_message_data::<Vec<ChatMessage>>(&msg.data) {
                    for msg in messages {
                        let msg_with_ref = msg.with_referral_id();
                        
                        // If there's an onClick URL with authmod, this is an authentication prompt
                        if let Some(ref on_click) = msg_with_ref.on_click {
                            if on_click.contains("sky.coflnet.com/authmod") {
                                // Format authentication prompt with BAF colors
                                let auth_prompt = format!(
                                    "§f[§4BAF§f]: §c========================================\n\
                                     §f[§4BAF§f]: §c§lCOFL Authentication Required!\n\
                                     §f[§4BAF§f]: §e{}\n\
                                     §f[§4BAF§f]: §bAuthentication URL: §f{}\n\
                                     §f[§4BAF§f]: §c========================================",
                                    msg_with_ref.text,
                                    on_click
                                );
                                let _ = tx.send(CoflEvent::ChatMessage(auth_prompt));
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
                            // Format authentication prompt with BAF colors
                            let auth_prompt = format!(
                                "§f[§4BAF§f]: §c========================================\n\
                                 §f[§4BAF§f]: §c§lCOFL Authentication Required!\n\
                                 §f[§4BAF§f]: §e{}\n\
                                 §f[§4BAF§f]: §bAuthentication URL: §f{}\n\
                                 §f[§4BAF§f]: §c========================================",
                                msg_with_ref.text,
                                on_click
                            );
                            let _ = tx.send(CoflEvent::ChatMessage(auth_prompt));
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

    /// Send a message to the COFL WebSocket
    pub async fn send_message(&self, message: &str) -> Result<()> {
        let mut write = self.write.lock().await;
        write.send(Message::Text(message.to_string())).await
            .context("Failed to send message to WebSocket")?;
        info!("Sent message to COFL WebSocket: {}", message);
        Ok(())
    }
}
