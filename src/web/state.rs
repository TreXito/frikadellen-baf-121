use parking_lot::RwLock;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;

use crate::bot::BotClient;
use crate::config::types::Config;
use crate::state::CommandQueue;
use crate::websocket::CoflWebSocket;

/// Maximum number of events to keep in the ring buffer.
const MAX_EVENTS: usize = 200;

/// A single event entry displayed in the web dashboard feed.
#[derive(Clone, serde::Serialize)]
pub struct WebEvent {
    /// Unix-millis timestamp
    pub timestamp: u64,
    /// Category tag used for colour coding in the UI
    pub kind: &'static str,
    /// Human-readable message (may contain Minecraft § colour codes — the
    /// frontend strips or converts them).
    pub message: String,
}

/// Thread-safe ring buffer of recent events for the dashboard feed.
#[derive(Clone)]
pub struct WebEventLog {
    inner: Arc<RwLock<VecDeque<WebEvent>>>,
}

impl WebEventLog {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(VecDeque::with_capacity(MAX_EVENTS + 1))),
        }
    }

    /// Push a new event. Oldest events are evicted when the buffer is full.
    pub fn push(&self, kind: &'static str, message: String) {
        let event = WebEvent {
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            kind,
            message,
        };
        let mut buf = self.inner.write();
        if buf.len() >= MAX_EVENTS {
            buf.pop_front();
        }
        buf.push_back(event);
    }

    /// Return a snapshot of all stored events (oldest first).
    pub fn snapshot(&self) -> Vec<WebEvent> {
        self.inner.read().iter().cloned().collect()
    }
}

/// Shared application state handed to every Axum handler via `State<Arc<WebState>>`.
pub struct WebState {
    pub bot_client: BotClient,
    pub command_queue: CommandQueue,
    pub ws_client: CoflWebSocket,
    pub event_log: WebEventLog,
    pub config: Config,
    pub start_time: Instant,
    pub ingame_name: String,
}
