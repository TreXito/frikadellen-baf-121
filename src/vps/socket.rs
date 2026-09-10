use super::types::{Instance, VpsMessage, VpsStateUpdate};
use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};

/// Interval between keepAlive pings (30 seconds to avoid Cloudflare timeout).
const KEEPALIVE_INTERVAL_SECS: u64 = 30;

/// Grace period after `paidUntil` before an instance is force-stopped (seconds).
const PAID_UNTIL_GRACE_SECS: i64 = 60;

/// Interval at which expiry is checked for running instances (seconds).
const EXPIRY_CHECK_INTERVAL_SECS: u64 = 60;

/// Manages the WebSocket connection to `wss://sky.coflnet.com/instances` and
/// the lifecycle of managed instances running on this host.
pub struct VpsSocket {
    /// `vps_secret` read from environment.
    secret: String,
    /// Currently known instances keyed by instance ID.
    instances: Arc<RwLock<HashMap<String, ManagedInstance>>>,
}

/// Runtime state for a single managed instance.
#[derive(Debug, Clone)]
struct ManagedInstance {
    instance: Instance,
    config: Option<serde_json::Value>,
    extra_config: Option<String>,
    /// Set to `true` while the instance is considered "running".
    running: bool,
}

impl VpsSocket {
    /// Create a new `VpsSocket` reading `vps_secret` from the `VPS_SECRET` env
    /// variable.  Returns `None` if the variable is not set.
    pub fn from_env() -> Option<Self> {
        let secret = std::env::var("VPS_SECRET").ok()?;
        if secret.is_empty() {
            return None;
        }
        Some(Self {
            secret,
            instances: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    /// Connect to the VPS backend and handle messages indefinitely.
    /// Reconnects automatically on disconnection with exponential backoff.
    pub async fn run(&self) {
        let url = format!("wss://sky.coflnet.com/instances?secret={}", self.secret);

        loop {
            match self.connect_and_handle(&url).await {
                Ok(()) => {
                    info!("[VPS] Connection closed normally, reconnecting...");
                }
                Err(e) => {
                    error!("[VPS] Connection error: {}", e);
                }
            }

            // Exponential backoff on reconnect
            let mut backoff_secs = 5u64;
            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(backoff_secs)).await;
                info!("[VPS] Reconnecting to backend...");
                match self.connect_and_handle(&url).await {
                    Ok(()) => break,
                    Err(e) => {
                        error!("[VPS] Reconnection failed: {}", e);
                        backoff_secs = (backoff_secs * 2).min(60);
                    }
                }
            }
        }
    }

    async fn connect_and_handle(&self, url: &str) -> Result<()> {
        info!("[VPS] Connecting to {}", url);
        let (ws_stream, _) = connect_async(url)
            .await
            .context("Failed to connect to VPS WebSocket")?;

        info!("[VPS] Connected to backend");

        let (write, mut read) = ws_stream.split();
        let write = Arc::new(Mutex::new(write));

        // Spawn keepAlive sender
        let keepalive_write = write.clone();
        let keepalive_handle = tokio::spawn(async move {
            let payload = serde_json::json!({
                "type": "keepAlive",
                "data": "{}"
            })
            .to_string();
            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(KEEPALIVE_INTERVAL_SECS)).await;
                let mut w = keepalive_write.lock().await;
                if w.send(Message::Text(payload.clone())).await.is_err() {
                    debug!("[VPS] keepAlive send failed — connection likely closed");
                    break;
                }
            }
        });

        // Spawn expiry checker
        let instances_expiry = self.instances.clone();
        let expiry_handle = tokio::spawn(async move {
            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(EXPIRY_CHECK_INTERVAL_SECS))
                    .await;
                check_and_stop_expired(&instances_expiry).await;
            }
        });

        // Read loop
        loop {
            match read.next().await {
                Some(Ok(Message::Text(text))) => {
                    if let Err(e) = self.handle_message(&text).await {
                        error!("[VPS] Error handling message: {}", e);
                    }
                }
                Some(Ok(Message::Close(_))) => {
                    warn!("[VPS] WebSocket closed by server");
                    break;
                }
                Some(Ok(Message::Ping(_))) => {
                    debug!("[VPS] Received ping");
                }
                Some(Err(e)) => {
                    error!("[VPS] WebSocket error: {}", e);
                    break;
                }
                None => {
                    warn!("[VPS] WebSocket stream ended");
                    break;
                }
                Some(Ok(_)) => {}
            }
        }

        keepalive_handle.abort();
        expiry_handle.abort();
        Ok(())
    }

    async fn handle_message(&self, text: &str) -> Result<()> {
        let msg: VpsMessage = serde_json::from_str(text).context("Failed to parse VPS message")?;

        info!("[VPS] Received message type: {}", msg.msg_type);

        match msg.msg_type.as_str() {
            "init" => {
                let updates: Vec<VpsStateUpdate> =
                    serde_json::from_str(&msg.data).context("Failed to parse init data")?;
                self.handle_init(updates).await;
            }
            "configUpdate" => {
                let update: VpsStateUpdate =
                    serde_json::from_str(&msg.data).context("Failed to parse configUpdate data")?;
                self.handle_config_update(update).await;
            }
            "error" => {
                error!("[VPS] Backend error: {}", msg.data);
            }
            other => {
                warn!("[VPS] Unknown message type: {}", other);
            }
        }

        Ok(())
    }

    /// Handle `init` — full state sync from the backend.
    ///
    /// All instances present in the update should be started (unless turned off
    /// or expired).  Instances that were previously running but are NOT in this
    /// update must be stopped (the backend restarted and they are no longer
    /// assigned to this host).
    async fn handle_init(&self, updates: Vec<VpsStateUpdate>) {
        info!("[VPS] init: received {} instance(s)", updates.len());

        let new_ids: std::collections::HashSet<String> =
            updates.iter().map(|u| u.instance.id.clone()).collect();

        let mut map = self.instances.write().await;

        // Stop instances that are no longer present
        let stale_ids: Vec<String> = map
            .keys()
            .filter(|id| !new_ids.contains(id.as_str()))
            .cloned()
            .collect();

        for id in &stale_ids {
            if let Some(mi) = map.get(id) {
                if mi.running {
                    info!(
                        "[VPS] Stopping instance {} (owner: {}) — no longer in init update",
                        id, mi.instance.owner_id
                    );
                    stop_instance(&mi.instance);
                }
            }
            map.remove(id);
        }

        // Upsert all instances from the init payload
        for update in updates {
            let inst = &update.instance;

            // Plausibility checks
            if let Err(e) = inst.validate() {
                warn!("[VPS] Skipping invalid instance: {}", e);
                continue;
            }

            let should_run = !inst.is_turned_off() && !inst.is_expired();
            let instance_id = inst.id.clone();
            let owner_id = inst.owner_id.clone();

            if should_run {
                info!(
                    "[VPS] Starting instance {} (owner: {}, app: {}, paidUntil: {})",
                    instance_id, owner_id, inst.app_kind, inst.paid_until
                );
                start_instance(inst, update.config.as_ref(), update.extra_config.as_deref());
            } else {
                let reason = if inst.is_turned_off() {
                    "turned off"
                } else {
                    "expired"
                };
                info!(
                    "[VPS] Not starting instance {} (owner: {}) — {}",
                    instance_id, owner_id, reason
                );
            }

            map.insert(
                instance_id,
                ManagedInstance {
                    instance: inst.clone(),
                    config: update.config.clone(),
                    extra_config: update.extra_config.clone(),
                    running: should_run,
                },
            );
        }

        info!("[VPS] init complete: {} instance(s) tracked", map.len());
    }

    /// Handle `configUpdate` — a single instance was created or updated.
    async fn handle_config_update(&self, update: VpsStateUpdate) {
        let inst = &update.instance;

        if let Err(e) = inst.validate() {
            warn!("[VPS] Skipping invalid configUpdate: {}", e);
            return;
        }

        let instance_id = inst.id.clone();
        let owner_id = inst.owner_id.clone();
        let should_run = !inst.is_turned_off() && !inst.is_expired();

        let mut map = self.instances.write().await;
        let was_running = map.get(&instance_id).map(|mi| mi.running).unwrap_or(false);

        if was_running && !should_run {
            info!(
                "[VPS] Stopping instance {} (owner: {}) — configUpdate",
                instance_id, owner_id
            );
            stop_instance(inst);
        } else if !was_running && should_run {
            info!(
                "[VPS] Starting instance {} (owner: {}, app: {}, paidUntil: {})",
                instance_id, owner_id, inst.app_kind, inst.paid_until
            );
            start_instance(inst, update.config.as_ref(), update.extra_config.as_deref());
        } else if was_running && should_run {
            info!(
                "[VPS] Updating running instance {} (owner: {})",
                instance_id, owner_id
            );
            // Restart with new config
            stop_instance(inst);
            start_instance(inst, update.config.as_ref(), update.extra_config.as_deref());
        } else {
            debug!(
                "[VPS] Instance {} (owner: {}) remains stopped",
                instance_id, owner_id
            );
        }

        map.insert(
            instance_id,
            ManagedInstance {
                instance: inst.clone(),
                config: update.config.clone(),
                extra_config: update.extra_config.clone(),
                running: should_run,
            },
        );
    }

    /// Returns a snapshot of currently tracked instances.
    pub async fn get_instances(&self) -> Vec<Instance> {
        self.instances
            .read()
            .await
            .values()
            .map(|mi| mi.instance.clone())
            .collect()
    }
}

/// Check all tracked instances and stop any whose `paidUntil` has expired.
async fn check_and_stop_expired(instances: &RwLock<HashMap<String, ManagedInstance>>) {
    let mut map = instances.write().await;
    for mi in map.values_mut() {
        if mi.running && mi.instance.is_expired() {
            let grace_expired = mi
                .instance
                .paid_until_dt()
                .map(|dt| {
                    let grace = chrono::Duration::seconds(PAID_UNTIL_GRACE_SECS);
                    chrono::Utc::now() > dt + grace
                })
                .unwrap_or(true);

            if grace_expired {
                warn!(
                    "[VPS] Stopping expired instance {} (owner: {}, paidUntil: {})",
                    mi.instance.id, mi.instance.owner_id, mi.instance.paid_until
                );
                stop_instance(&mi.instance);
                mi.running = false;
            }
        }
    }
}

/// Start a managed instance.
///
/// In a full deployment this would launch a child process or container.
/// Currently logs the action for integration with the orchestration layer.
fn start_instance(
    instance: &Instance,
    config: Option<&serde_json::Value>,
    extra_config: Option<&str>,
) {
    info!(
        "[VPS] → START instance {} | owner={} | app={} | paidUntil={} | config={} | extra={}",
        instance.id,
        instance.owner_id,
        instance.app_kind,
        instance.paid_until,
        config
            .map(|c| c.to_string())
            .unwrap_or_else(|| "none".into()),
        extra_config.unwrap_or("none"),
    );
    // TODO: Actual process/container launch will be added here based on
    // app_kind and config.  For now this is a hook for the orchestration layer.
}

/// Stop a managed instance.
fn stop_instance(instance: &Instance) {
    info!(
        "[VPS] → STOP instance {} | owner={}",
        instance.id, instance.owner_id
    );
    // TODO: Actual process/container termination will be added here.
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn make_instance(id: &str, owner: &str, paid_until: &str, turned_off: bool) -> Instance {
        let mut context = HashMap::new();
        if turned_off {
            context.insert("turnedOff".to_string(), "true".to_string());
        }
        Instance {
            host_machine_ip: "127.0.0.1".to_string(),
            owner_id: owner.to_string(),
            id: id.to_string(),
            app_kind: "FBAF".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            paid_until: paid_until.to_string(),
            context,
            public_ip: String::new(),
        }
    }

    #[test]
    fn test_instance_turned_off() {
        let on = make_instance("1", "u1", "2099-01-01T00:00:00Z", false);
        assert!(!on.is_turned_off());

        let off = make_instance("2", "u1", "2099-01-01T00:00:00Z", true);
        assert!(off.is_turned_off());
    }

    #[test]
    fn test_instance_expired() {
        let future = make_instance("1", "u1", "2099-01-01T00:00:00Z", false);
        assert!(!future.is_expired());

        let past = make_instance("2", "u1", "2020-01-01T00:00:00Z", false);
        assert!(past.is_expired());

        let empty = make_instance("3", "u1", "", false);
        assert!(empty.is_expired());
    }

    #[test]
    fn test_instance_validate() {
        let valid = make_instance("1", "u1", "2099-01-01T00:00:00Z", false);
        assert!(valid.validate().is_ok());

        let no_id = make_instance("", "u1", "2099-01-01T00:00:00Z", false);
        assert!(no_id.validate().is_err());

        let no_owner = make_instance("1", "", "2099-01-01T00:00:00Z", false);
        assert!(no_owner.validate().is_err());

        let bad_date = make_instance("1", "u1", "not-a-date", false);
        assert!(bad_date.validate().is_err());
    }
}
