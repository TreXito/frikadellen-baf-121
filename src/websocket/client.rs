use super::messages::{parse_message_data, inject_referral_id, ChatMessage, WebSocketMessage};
use crate::types::{BazaarFlipRecommendation, Flip};
use anyhow::{Context, Result};
use futures::{stream::SplitSink, StreamExt, SinkExt};
use tokio::sync::{mpsc, watch, Mutex};
use tokio_tungstenite::{
    connect_async_tls_with_config, tungstenite::Message, Connector, MaybeTlsStream, WebSocketStream,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::{debug, error, info, warn};

/// Set to `true` the instant COFL confirms the mod session is authenticated
/// (`loggedIn` with `verified: true`). Written by the socket read task, which
/// runs from the moment of connect — independent of the main event loop
/// draining `CoflEvent`s. The startup sequence polls this to hold the
/// Minecraft/Microsoft login until the user has finished signing into COFL, so
/// the COFL sign-in link is dealt with FIRST and never buried behind (or
/// scrolled away by) the Hypixel auth output.
pub static COFL_LOGGED_IN: AtomicBool = AtomicBool::new(false);

/// True once COFL has actually pushed a sign-in link and it has been printed
/// (see `CoflWebSocket::send_auth_prompt`). The startup gate reads this so it
/// never tells the user to "open the link above" when nothing was printed —
/// COFL only sends the prompt for a session id it considers unauthenticated,
/// so on every other path the instruction pointed at empty terminal.
pub static COFL_AUTH_LINK_SHOWN: AtomicBool = AtomicBool::new(false);

/// Build the COFL sign-in URL for a session id. `conId` in COFL's own authmod
/// link is exactly the `SId` we generate locally and pass on the connect URL,
/// so the bot can always print a working link itself instead of depending on
/// COFL pushing one.
pub fn cofl_auth_url(session_id: &str) -> String {
    format!(
        "https://sky.coflnet.com/authmod?refId=9KKPN9&conId={}",
        session_id
    )
}

/// True when a websocket URL points at Coflnet rather than the private
/// baf-flip-finder, identified by "coflnet" in the host or a "/modsocket" path.
/// Anything else is the finder (e.g. ws://127.0.0.1:15101), which speaks its own
/// protocol and has no sign-in of any kind. Scheme-insensitive, so it gives the
/// same answer before or after `normalize_ws_url`.
pub fn is_cofl_url(url: &str) -> bool {
    let host = url
        .strip_prefix("wss://")
        .or_else(|| url.strip_prefix("ws://"))
        .unwrap_or(url);
    host.contains("coflnet") || host.contains("/modsocket")
}

/// Normalize a websocket URL. Coflnet hosts are force-upgraded to `wss://`
/// (their regional servers are TLS-only; old configs may still say `ws://`).
/// NON-Coflnet endpoints — e.g. the local baf-flip-finder on
/// `ws://127.0.0.1:15101`, which speaks plaintext on loopback — keep their
/// explicit scheme: forcing TLS there produced
/// "ssl3_get_record:wrong version number" and killed startup.
fn normalize_ws_url(url: &str) -> String {
    let host = url
        .strip_prefix("wss://")
        .or_else(|| url.strip_prefix("ws://"))
        .unwrap_or(url);
    if !is_cofl_url(url) && url.starts_with("ws://") {
        // Bare-authority URLs ("ws://127.0.0.1:15101") need an explicit "/":
        // tungstenite passes the empty path through and the handshake becomes
        // "GET ?player=… HTTP/1.1", which strict HTTP parsers (Node) 400.
        if !host.contains('/') {
            return format!("{}/", url);
        }
        return url.to_string();
    }
    format!("wss://{}", host)
}

/// Build a TLS connector for the Coflnet modsocket that tolerates self-signed /
/// untrusted certificates. Coflnet's regional servers (e.g. us-sky.coflnet.com)
/// present self-signed certs that the system trust store rejects; this connector
/// is used ONLY for the Coflnet websocket (which the bot already authenticates
/// to), so the relaxed verification is scoped to that single endpoint. `ws://`
/// (non-TLS) URLs ignore the connector entirely.
fn cofl_tls_connector() -> Option<Connector> {
    native_tls::TlsConnector::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .ok()
        .map(Connector::NativeTls)
}

pub enum CoflEvent {
    AuctionFlip(Flip),
    BazaarFlip(BazaarFlipRecommendation),
    /// COFL `cancelOrder` message – cancel a specific open bazaar order.
    /// The order is identified by item name + side (isBuyOrder/isSell) and,
    /// when multiple same-side orders exist for the item, disambiguated by
    /// `pricePerUnit`. Reuses the same payload shape as `placeOrder`.
    CancelBazaarOrder(BazaarFlipRecommendation),
    /// COFL confirmed the mod session is authenticated (`loggedIn` with
    /// `verified: true`). Used as a reliable signal to enable flip/order buying,
    /// independent of the textual "Hello <ign> (<email>)" chat greeting (which
    /// COFL does not always send).
    Authenticated,
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
    /// COFL "collectAuctions" message – the server has detected sold/expired
    /// auctions to collect, so the bot should run a claim-sold cycle now instead
    /// of waiting. Makes claiming proactive (frees AH slots → can list → frees
    /// inventory → can keep buying).
    CollectAuctions,
    /// Parsed license list from `/cofl licenses list` response.
    /// Fields: `(entries, page_number)` where entries are `(ign, 1-based page-local index, tier)` tuples
    /// and `page_number` is the 1-based page that was returned.
    LicenseList {
        entries: Vec<(String, u32, String)>,
        page: u32,
    },
    /// Finder listInstructions: items the finder priced for listing.
    /// Contains an array of {name, listAt, volumePerDay, confidence, slot} objects.
    ListInstructions(serde_json::Value),
}

#[derive(Clone)]
pub struct CoflWebSocket {
    #[allow(dead_code)]
    tx: mpsc::UnboundedSender<CoflEvent>,
    write: Arc<Mutex<SplitSink<WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>, Message>>>,
    /// True when this socket points at the private baf-flip-finder rather than
    /// Coflnet. Only the finder speaks the `{type:"inventory"}` pricing/listing
    /// protocol; Coflnet can't parse it, so sending inventory there just spams
    /// the modsocket with a message it drops. `send_inventory` is a no-op unless
    /// this is set — on a COFL-primary deployment the finder is reached through
    /// its own dedicated feed/lister sockets instead.
    is_finder: bool,
    /// The URL the read task reconnects to, query string included. Shared so a
    /// COFL region switch can repoint the socket at a regional host WITHOUT
    /// touching the user's config (see [`Self::switch_region`]).
    full_url: Arc<Mutex<String>>,
    /// Bumped to kick the read task off the current connection so it picks the
    /// new [`Self::full_url`] up immediately.
    switch_tx: watch::Sender<u64>,
}

impl CoflWebSocket {
    pub async fn connect(
        url: String,
        username: String,
        version: String,
        session_id: String,
    ) -> Result<(Self, mpsc::UnboundedReceiver<CoflEvent>)> {
        // Coflnet fully switched to TLS. Upgrade any plaintext `ws://` URL left in
        // an older persisted config to `wss://` so the bot never tries (and fails)
        // a plaintext connection to a regional server that only speaks TLS.
        let url = normalize_ws_url(&url);
        // Anything that isn't Coflnet is the private finder (e.g.
        // ws://127.0.0.1:15101), which is the only endpoint that understands
        // inventory-pricing messages and the only one with no COFL sign-in.
        let is_finder = !is_cofl_url(&url);
        let full_url = format!(
            "{}?player={}&version={}&SId={}",
            url, username, version, session_id
        );

        info!("Connecting to Coflnet WebSocket: {}", url);

        let (ws_stream, _) =
            connect_async_tls_with_config(&full_url, None, false, cofl_tls_connector())
                .await
                .context("Failed to connect to WebSocket")?;

        info!("WebSocket connected successfully");

        let (write, mut read) = ws_stream.split();
        let write = Arc::new(Mutex::new(write));
        let write_for_task = write.clone();
        let (tx, rx) = mpsc::unbounded_channel();
        let tx_clone = tx.clone();
        let full_url = Arc::new(Mutex::new(full_url));
        let full_url_for_task = full_url.clone();
        let (switch_tx, mut switch_rx) = watch::channel(0u64);

        // Spawn task to handle incoming messages, with automatic reconnection
        tokio::spawn(async move {
            loop {
                // A region switch reconnects immediately; a dropped connection
                // backs off so a server-side outage isn't hammered.
                let mut switched = false;
                // ── inner read loop ───────────────────────────────────────────
                loop {
                    let message = tokio::select! {
                        // `switch_region` bumped the URL. Abandon this connection
                        // so the reconnect below picks the new host up. Cancelling
                        // `read.next()` here is safe: the stream buffers partial
                        // frames internally, so nothing is lost mid-message.
                        _ = switch_rx.changed() => {
                            info!("[WS] Region switch requested — dropping current connection");
                            switched = true;
                            break;
                        }
                        message = read.next() => message,
                    };
                    match message {
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
                if !switched {
                    let _ = tx_clone.send(CoflEvent::ChatMessage(
                        "§f[§4BAF§f]: §cWebSocket disconnected — reconnecting...".to_string(),
                    ));
                }

                let mut backoff_secs = if switched { 0 } else { 5u64 };
                loop {
                    tokio::time::sleep(tokio::time::Duration::from_secs(backoff_secs)).await;
                    // Re-read every attempt: a switch may land while we are backing off.
                    let full_url = full_url_for_task.lock().await.clone();
                    match connect_async_tls_with_config(&full_url, None, false, cofl_tls_connector()).await {
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
                            backoff_secs = (backoff_secs * 2).max(5).min(60);
                            error!("[WS] Reconnection failed (retry in {}s): {}", backoff_secs, e);
                        }
                    }
                }
                // Resume outer loop → inner read loop continues on new connection
            }
        });

        Ok((Self { tx, write, is_finder, full_url, switch_tx }, rx))
    }

    /// Repoint this socket at `new_url` and reconnect, WITHOUT persisting
    /// anything.
    ///
    /// COFL routes a user to their nearest modsocket by pushing
    /// `connect <host>` (its `/cofl switchregion`), so the configured URL is
    /// only ever the entry point — the plain `sky.coflnet.com` default is
    /// correct for every region and users never need to set a regional host
    /// themselves. Writing the redirect target back into `config.toml` used to
    /// pin the bot to one region permanently, which is exactly how a config
    /// ends up stuck on a regional host that later stops resolving.
    ///
    /// The query string (player / version / SId) is carried over so the session
    /// survives the move.
    pub async fn switch_region(&self, new_url: &str) {
        let normalized = normalize_ws_url(new_url);
        let mut full_url = self.full_url.lock().await;
        let query = full_url
            .split_once('?')
            .map(|(_, q)| format!("?{}", q))
            .unwrap_or_default();
        *full_url = format!("{}{}", normalized, query);
        drop(full_url);
        // Wakes the read task, which drops the current connection and dials the
        // URL just stored.
        let _ = self.switch_tx.send_modify(|n| *n = n.wrapping_add(1));
    }

    /// True when this socket points at the private baf-flip-finder rather than
    /// Coflnet. Callers use this to skip COFL-only startup steps: the finder has
    /// no accounts and no sign-in, so it never sends an `authmod` link and never
    /// sends `loggedIn`. Waiting on COFL auth against a finder socket can only
    /// ever time out.
    pub fn is_finder(&self) -> bool {
        self.is_finder
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
        // Print to the terminal the instant COFL sends the link. The socket read
        // task runs from connect (before the Minecraft/Microsoft login), so this
        // guarantees the sign-in link is seen FIRST and is never lost in the
        // Hypixel auth output. Also mirror it into the event stream for the web
        // panel.
        crate::logging::print_mc_chat(&auth_prompt);
        COFL_AUTH_LINK_SHOWN.store(true, Ordering::Relaxed);
        let _ = tx.send(CoflEvent::ChatMessage(auth_prompt));
    }

    /// Treat an authenticated-only COFL push (flips, bazaar recommendations) as
    /// proof the session is authenticated.
    ///
    /// COFL sends `loggedIn` when a session is established, but a session the
    /// user signed into on an EARLIER run resumes silently: COFL pushes no
    /// sign-in link and does not re-send `loggedIn`. The explicit auth signal
    /// therefore never arrived for returning users, `cofl_authenticated` stayed
    /// false for the whole run, and every COFL flip was dropped with
    /// "Coflnet is not authenticated yet" — on accounts that had been flipping
    /// fine for weeks. COFL only routes flips/bazaar recommendations to a
    /// session it has already authenticated, so receiving one IS the signal.
    ///
    /// Guarded by [`COFL_AUTH_LINK_SHOWN`]: if COFL asked us to sign in, the
    /// session really is unauthenticated and the gate must keep holding.
    fn note_authenticated_traffic(tx: &mpsc::UnboundedSender<CoflEvent>) {
        if COFL_AUTH_LINK_SHOWN.load(Ordering::Relaxed) {
            return;
        }
        if !COFL_LOGGED_IN.swap(true, Ordering::Relaxed) {
            info!(
                "[Coflnet] Session resumed — treating it as authenticated \
                 (COFL is pushing flips and never asked us to sign in)"
            );
            let _ = tx.send(CoflEvent::Authenticated);
        }
    }

    fn handle_message(text: &str, tx: &mpsc::UnboundedSender<CoflEvent>) -> Result<()> {
        info!("[COFL <-] {}", text);

        // baf-flip-finder protocol: when this socket points at the private
        // finder instead of Coflnet (finder-only mode), its messages are
        // {type:'flip', flip:{…}} / welcome / filters / pong — translate flips
        // into the normal pipeline and ignore the rest quietly.
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(text) {
            match v.get("type").and_then(|t| t.as_str()) {
                Some("flip") if v.get("flip").is_some() => {
                    let f = &v["flip"];
                    let flip = Flip {
                        item_name: f.get("itemName").and_then(|x| x.as_str()).unwrap_or("?").to_string(),
                        starting_bid: f.get("price").and_then(|x| x.as_u64()).unwrap_or(0),
                        target: f.get("target").and_then(|x| x.as_u64()).unwrap_or(0),
                        finder: Some("BAF_FINDER".to_string()),
                        profit_perc: f.get("roiPct").and_then(|x| x.as_f64()),
                        purchase_at_ms: crate::types::purchase_at_from_json(f),
                        uuid: f.get("uuid").and_then(|x| x.as_str()).map(String::from),
                        list_at: f.get("listAt").and_then(|x| x.as_u64()),
                    };
                    if flip.starting_bid > 0 && flip.target > 0 && flip.uuid.is_some() {
                        debug!("Parsed finder flip: {}", flip.item_name);
                        let _ = tx.send(CoflEvent::AuctionFlip(flip));
                    }
                    return Ok(());
                }
                Some("welcome") | Some("filters") | Some("pong") => {
                    return Ok(());
                }
                Some("listInstructions") => {
                    let _ = tx.send(CoflEvent::ListInstructions(v));
                    return Ok(());
                }
                _ => {}
            }
        }

        let msg: WebSocketMessage = serde_json::from_str(text)
            .context("Failed to parse WebSocket message")?;

        info!("[COFL <-] type={} data={}", msg.msg_type, msg.data);

        match msg.msg_type.as_str() {
            "loggedIn" => {
                // COFL confirms the mod session is authenticated. Treat this as
                // the auth signal that enables flip/order buying — the previous
                // logic relied solely on a "Hello <ign> (<email>)" chat greeting
                // which COFL does not reliably send, leaving the bot unable to
                // ever buy flips or place orders.
                let verified = parse_message_data::<serde_json::Value>(&msg.data)
                    .ok()
                    .and_then(|v| v.get("verified").and_then(|b| b.as_bool()))
                    // If the field is missing, receiving `loggedIn` at all still
                    // means the session is established, so default to true.
                    .unwrap_or(true);
                if verified {
                    // Publish the auth state for the startup sequence's
                    // "sign into COFL first" gate, before the event even reaches
                    // the main loop.
                    COFL_LOGGED_IN.store(true, Ordering::Relaxed);
                    let _ = tx.send(CoflEvent::Authenticated);
                }
            }
            "flip" => {
                // Sent BEFORE the flip so the main loop latches auth first and
                // this very flip clears the gate (same ordered channel).
                Self::note_authenticated_traffic(tx);
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
                Self::note_authenticated_traffic(tx);
                if let Ok(bazaar_flip) = parse_message_data::<BazaarFlipRecommendation>(&msg.data) {
                    debug!("Parsed bazaar flip: {:?}", bazaar_flip.item_name);
                    let _ = tx.send(CoflEvent::BazaarFlip(bazaar_flip));
                } else {
                    warn!("Failed to parse bazaar flip from '{}' message (data length: {} bytes)", msg.msg_type, msg.data.len());
                }
            }
            "cancelOrder" | "cancelorder" => {
                // COFL tells the bot to cancel a specific open bazaar order.
                // Same payload shape as placeOrder (itemName/pricePerUnit/isBuyOrder|isSell).
                if let Ok(order) = parse_message_data::<BazaarFlipRecommendation>(&msg.data) {
                    debug!("Parsed cancel-order request: {:?}", order.item_name);
                    let _ = tx.send(CoflEvent::CancelBazaarOrder(order));
                } else {
                    warn!("Failed to parse cancelOrder from data (length: {} bytes)", msg.data.len());
                }
            }
            "getbazaarflips" => {
                Self::note_authenticated_traffic(tx);
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
                // COFL's execute payload is a raw command string (e.g.
                // "/cofl connect <url>" or "/cofl ping <sid> <ticks>"), which is NOT
                // valid JSON — parse_message_data would fail and silently drop it,
                // breaking /cofl ping reflection and /cofl connect (region switch).
                // The execute payload is a command STRING that COFL JSON-encodes
                // (e.g. data = "\"/tip x cnc\"" → after serde, msg.data = "/tip x cnc"
                // WITH quotes). Decode exactly ONE JSON-string level to strip those
                // quotes; fall back to the raw value when it isn't a quoted string.
                // (Using parse_message_data here double-decodes, fails on the inner
                // non-JSON command, and left the quotes on — so the bot typed
                // `"/tip x cnc"` literally instead of running it.)
                let command = serde_json::from_str::<String>(&msg.data)
                    .unwrap_or_else(|_| msg.data.clone());
                if !command.trim().is_empty() {
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
            "collectAuctions" => {
                // COFL tells the bot it has sold/expired auctions to collect.
                // Trigger a claim-sold cycle proactively.
                debug!("Received collectAuctions");
                let _ = tx.send(CoflEvent::CollectAuctions);
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

    /// Send inventory to the finder for pricing/listing via the primary WS.
    /// `force: true` skips the finder's confidence gate so all items get priced.
    ///
    /// No-op when this socket is Coflnet rather than the finder: COFL doesn't
    /// understand the `{type:"inventory"}` pricing protocol and just drops it,
    /// so pushing it there is pure modsocket spam. On a COFL-primary deployment
    /// the finder is fed through its own dedicated feed/lister sockets instead.
    pub async fn send_inventory(&self, items: &serde_json::Value, force: bool) -> Result<()> {
        if !self.is_finder {
            debug!("[Inventory] Skipping inventory upload — primary socket is COFL, not the finder (force={})", force);
            return Ok(());
        }
        let msg = serde_json::json!({
            "type": "inventory",
            "items": items,
            "force": force,
        }).to_string();
        self.send_message(&msg).await
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

    /// Close the COFL WebSocket connection gracefully.
    pub async fn close(&self) -> Result<()> {
        let mut write = self.write.lock().await;
        write.close().await
            .context("Failed to close COFL WebSocket")?;
        info!("[COFL] WebSocket closed");
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
    fn test_is_cofl_url_classifies_finder_vs_cofl() {
        // Coflnet: the sign-in gate must still apply to these.
        assert!(is_cofl_url("wss://sky.coflnet.com/modsocket"));
        assert!(is_cofl_url("ws://sky.coflnet.com/modsocket"));
        assert!(is_cofl_url("wss://us-sky.coflnet.com/modsocket"));

        // Own finder: no sign-in exists, so the gate must never wait on these.
        assert!(!is_cofl_url("ws://127.0.0.1:15101/"));
        assert!(!is_cofl_url("ws://127.0.0.1:15101"));
        assert!(!is_cofl_url("ws://192.168.0.250:15101/"));
    }

    #[test]
    fn test_is_cofl_url_agrees_before_and_after_normalize() {
        // is_finder is derived from the normalized URL; the startup gate asks the
        // same question. Both must agree or the gate desyncs from the socket.
        for url in [
            "wss://sky.coflnet.com/modsocket",
            "ws://sky.coflnet.com/modsocket",
            "ws://127.0.0.1:15101/",
            "ws://127.0.0.1:15101",
        ] {
            assert_eq!(
                is_cofl_url(url),
                is_cofl_url(&normalize_ws_url(url)),
                "classification flipped across normalize for {}",
                url
            );
        }
    }

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

    /// A resumed COFL session sends no sign-in link and no `loggedIn`, so the
    /// only evidence it is authenticated is that COFL keeps pushing flips.
    /// Both halves live in ONE test because they share the process-global
    /// `COFL_*` latches, which parallel tests would race over.
    #[test]
    fn test_cofl_flip_confirms_auth_only_when_no_signin_was_requested() {
        let cofl_flip = serde_json::json!({
            "type": "flip",
            "data": serde_json::json!({
                "id": "4f1d2446974e43dbaf644fb13cd8af62",
                "auction": { "itemName": "Rod of the Sea", "startingBid": 15000000 },
                "target": 29314940,
                "finder": "SNIPER_MEDIAN"
            }).to_string()
        })
        .to_string();

        // ── resumed session: no sign-in link was ever shown ──────────────
        COFL_AUTH_LINK_SHOWN.store(false, Ordering::Relaxed);
        COFL_LOGGED_IN.store(false, Ordering::Relaxed);
        let (tx, mut rx) = mpsc::unbounded_channel();
        handle_message_for_test(&cofl_flip, &tx);

        // Auth must be published BEFORE the flip, so the flip that proved it
        // clears the gate instead of being dropped as "not authenticated yet".
        assert!(
            matches!(rx.try_recv(), Ok(CoflEvent::Authenticated)),
            "a COFL flip on a link-free session must confirm auth first"
        );
        assert!(matches!(rx.try_recv(), Ok(CoflEvent::AuctionFlip(_))));
        assert!(COFL_LOGGED_IN.load(Ordering::Relaxed));

        // A second flip must not re-announce auth.
        handle_message_for_test(&cofl_flip, &tx);
        assert!(matches!(rx.try_recv(), Ok(CoflEvent::AuctionFlip(_))));

        // ── COFL asked us to sign in: the gate must keep holding ─────────
        COFL_AUTH_LINK_SHOWN.store(true, Ordering::Relaxed);
        COFL_LOGGED_IN.store(false, Ordering::Relaxed);
        let (tx2, mut rx2) = mpsc::unbounded_channel();
        handle_message_for_test(&cofl_flip, &tx2);
        assert!(
            matches!(rx2.try_recv(), Ok(CoflEvent::AuctionFlip(_))),
            "the flip still flows; only the auth claim is withheld"
        );
        assert!(
            !COFL_LOGGED_IN.load(Ordering::Relaxed),
            "an unauthenticated session must not be latched as authenticated"
        );

        COFL_AUTH_LINK_SHOWN.store(false, Ordering::Relaxed);
        COFL_LOGGED_IN.store(false, Ordering::Relaxed);
    }

    fn handle_message_for_test(text: &str, tx: &mpsc::UnboundedSender<CoflEvent>) {
        CoflWebSocket::handle_message(text, tx).expect("message parses");
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
}
