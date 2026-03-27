use super::types::{VpsMessage, VpsStateUpdate};
use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::{sleep, Duration};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};

/// Interval (seconds) between heartbeat pings to the Coflnet backend,
/// matching the C# `Task.Delay(60_000)` in `VpsSocket.OnOpen`.
const HEARTBEAT_INTERVAL_SECS: u64 = 60;

/// Delay (seconds) before reconnecting after a disconnect or error.
const RECONNECT_DELAY_SECS: u64 = 10;

/// Callback type invoked when the server sends an instance state update.
pub type OnStateUpdate = Arc<dyn Fn(VpsStateUpdate) + Send + Sync>;

/// A WebSocket client that connects to the Coflnet VPS instance management
/// endpoint (`/instances`).  It mirrors the server-side `VpsSocket` behaviour:
///
/// 1. Connects with `?ip=<IP>&secret=<SECRET>` query parameters.
/// 2. Receives an `"init"` message containing a `Vec<VpsStateUpdate>` of all
///    instances that should be running on this host.
/// 3. Receives `"configUpdate"` messages whenever an instance is
///    created / modified / turned off.
/// 4. Sends periodic heartbeats to signal liveness.
pub struct VpsSocket {
    url: String,
    secret: String,
    on_state_update: OnStateUpdate,
}

impl VpsSocket {
    /// Create a new VPS socket client.
    ///
    /// * `url`   – base WebSocket URL (e.g. `wss://sky.coflnet.com/instances`)
    /// * `secret` – shared secret for authentication
    /// * `on_state_update` – callback invoked for every received state update
    pub fn new(url: String, secret: String, on_state_update: OnStateUpdate) -> Self {
        Self {
            url,
            secret,
            on_state_update,
        }
    }

    /// Resolve the IP address of this machine to send as the `ip` query param.
    /// Falls back to `"unknown"` if resolution fails.
    fn resolve_local_ip() -> String {
        // Attempt to discover the outbound IP by opening a UDP socket to a
        // public address (no actual traffic is sent).
        let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok();
        socket
            .and_then(|s| {
                s.connect("8.8.8.8:80").ok()?;
                s.local_addr().ok()
            })
            .map(|addr| addr.ip().to_string())
            .unwrap_or_else(|| "unknown".to_string())
    }

    /// Build the full connection URL with query parameters.
    fn connection_url(&self) -> String {
        let ip = Self::resolve_local_ip();
        format!("{}?ip={}&secret={}", self.url, ip, self.secret)
    }

    /// Start the connection loop.  Automatically reconnects on disconnect.
    /// This method runs forever and should be spawned as a background task.
    pub async fn run(&self) {
        loop {
            match self.connect_and_listen().await {
                Ok(()) => {
                    warn!("[VPS] WebSocket closed normally, reconnecting...");
                }
                Err(e) => {
                    error!("[VPS] WebSocket error: {e:#}, reconnecting in {RECONNECT_DELAY_SECS}s...");
                }
            }
            sleep(Duration::from_secs(RECONNECT_DELAY_SECS)).await;
        }
    }

    /// Single connection lifetime: connect → read loop → return on disconnect.
    async fn connect_and_listen(&self) -> Result<()> {
        let url = self.connection_url();
        info!("[VPS] Connecting to {}", self.url);

        let (ws_stream, _) = connect_async(&url)
            .await
            .context("Failed to connect to VPS WebSocket")?;

        info!("[VPS] Connected successfully");

        let (write, mut read) = ws_stream.split();
        let write = Arc::new(Mutex::new(write));

        // Spawn heartbeat task — sends a ping every HEARTBEAT_INTERVAL_SECS.
        let write_hb = write.clone();
        let heartbeat = tokio::spawn(async move {
            loop {
                sleep(Duration::from_secs(HEARTBEAT_INTERVAL_SECS)).await;
                let mut w = write_hb.lock().await;
                if w.send(Message::Ping(vec![].into())).await.is_err() {
                    break;
                }
                debug!("[VPS] Heartbeat sent");
            }
        });

        // Read loop
        while let Some(msg) = read.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    self.handle_message(&text);
                }
                Ok(Message::Close(_)) => {
                    warn!("[VPS] Server closed the connection");
                    break;
                }
                Ok(Message::Pong(_)) => {
                    debug!("[VPS] Pong received");
                }
                Ok(_) => {} // Binary, Ping, Frame — ignore
                Err(e) => {
                    error!("[VPS] Read error: {e}");
                    break;
                }
            }
        }

        heartbeat.abort();
        Ok(())
    }

    /// Dispatch a single text message from the server.
    fn handle_message(&self, text: &str) {
        let msg: VpsMessage = match serde_json::from_str(text) {
            Ok(m) => m,
            Err(e) => {
                warn!("[VPS] Failed to parse message: {e}");
                return;
            }
        };

        match msg.msg_type.as_str() {
            "init" => self.handle_init(&msg.data),
            "configUpdate" => self.handle_config_update(&msg.data),
            "error" => {
                error!("[VPS] Server error: {}", msg.data);
            }
            other => {
                debug!("[VPS] Unknown message type: {other}");
            }
        }
    }

    /// Process the `"init"` message which contains a list of all instances
    /// currently assigned to this host.
    fn handle_init(&self, data: &str) {
        let updates: Vec<VpsStateUpdate> = match serde_json::from_str(data) {
            Ok(u) => u,
            Err(e) => {
                error!("[VPS] Failed to parse init payload: {e}");
                return;
            }
        };

        info!("[VPS] Received init with {} instance(s)", updates.len());
        for update in updates {
            info!(
                "[VPS]   instance {} (owner={}, kind={})",
                update.instance.id, update.instance.owner_id, update.instance.app_kind
            );
            (self.on_state_update)(update);
        }
    }

    /// Process a single `"configUpdate"` message — an instance was created,
    /// modified, or turned off.
    fn handle_config_update(&self, data: &str) {
        let update: VpsStateUpdate = match serde_json::from_str(data) {
            Ok(u) => u,
            Err(e) => {
                error!("[VPS] Failed to parse configUpdate payload: {e}");
                return;
            }
        };

        let turned_off = update.instance.context.contains_key("turnedOff");
        info!(
            "[VPS] configUpdate: instance {} (owner={}, kind={}, turnedOff={})",
            update.instance.id, update.instance.owner_id, update.instance.app_kind, turned_off
        );
        (self.on_state_update)(update);
    }
}

/// Convenience function to spawn the VPS socket as a background task.
/// Returns immediately. The socket reconnects automatically on failure.
///
/// * `url`    – VPS WebSocket URL from config (e.g. `wss://sky.coflnet.com/instances`)
/// * `secret` – authentication secret from config
/// * `on_state_update` – callback for each received instance state update
pub fn spawn_vps_socket(
    url: String,
    secret: String,
    on_state_update: OnStateUpdate,
) {
    tokio::spawn(async move {
        let socket = VpsSocket::new(url, secret, on_state_update);
        socket.run().await;
    });
}
