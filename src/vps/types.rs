use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Top-level WebSocket message exchanged with the VPS backend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VpsMessage {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub data: String,
}

/// A single managed VPS instance as returned by the SkyModCommands backend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Instance {
    /// IP address of the host machine running the instance.
    #[serde(rename = "HostMachineIp", default)]
    pub host_machine_ip: String,
    /// User ID of the owner (used for logging and identification).
    #[serde(rename = "OwnerId", default)]
    pub owner_id: String,
    /// Unique ID for this instance.
    #[serde(rename = "Id")]
    pub id: String,
    /// Application kind (e.g. "FBAF", "TPM").
    #[serde(rename = "AppKind", default)]
    pub app_kind: String,
    /// When this instance was created.
    #[serde(rename = "CreatedAt", default)]
    pub created_at: String,
    /// Billing expiry timestamp.  The instance MUST be stopped when this
    /// time has passed. The format is ISO-8601 / RFC-3339.
    #[serde(rename = "PaidUntil", default)]
    pub paid_until: String,
    /// Arbitrary key-value context.  The presence of `"turnedOff"` means the
    /// instance should not be running.
    #[serde(rename = "Context", default)]
    pub context: HashMap<String, String>,
    /// Public IP assigned to the instance (if any).
    #[serde(rename = "PublicIp", default)]
    pub public_ip: String,
}

impl Instance {
    /// Returns `true` if the instance should NOT be running (the `"turnedOff"`
    /// key is present in `context`).
    pub fn is_turned_off(&self) -> bool {
        self.context.contains_key("turnedOff")
    }

    /// Parse `paid_until` into a `chrono::DateTime<Utc>`.
    /// Returns `None` if parsing fails.
    pub fn paid_until_dt(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        chrono::DateTime::parse_from_rfc3339(&self.paid_until)
            .ok()
            .map(|dt| dt.with_timezone(&chrono::Utc))
    }

    /// Returns `true` if `paidUntil` is in the past.
    pub fn is_expired(&self) -> bool {
        match self.paid_until_dt() {
            Some(dt) => dt < chrono::Utc::now(),
            None => true, // treat unparseable as expired
        }
    }

    /// Quick plausibility check.  Returns an error string if the instance
    /// looks invalid.
    pub fn validate(&self) -> Result<(), String> {
        if self.id.is_empty() {
            return Err("Instance has empty Id".into());
        }
        if self.owner_id.is_empty() {
            return Err(format!("Instance {} has empty OwnerId", self.id));
        }
        if self.paid_until.is_empty() {
            return Err(format!("Instance {} has empty PaidUntil", self.id));
        }
        if self.paid_until_dt().is_none() {
            return Err(format!(
                "Instance {} has unparseable PaidUntil: {}",
                self.id, self.paid_until
            ));
        }
        Ok(())
    }
}

/// State update pushed by the backend for a single instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VpsStateUpdate {
    /// Opaque config object forwarded to the instance.
    #[serde(rename = "Config", default)]
    pub config: Option<serde_json::Value>,
    /// Instance metadata.
    #[serde(rename = "Instance")]
    pub instance: Instance,
    /// Optional extra configuration string.
    #[serde(rename = "ExtraConfig", default)]
    pub extra_config: Option<String>,
}

/// FBAF default settings — analogous to TPM's `NormalDefault` in VpsSocket.cs.
/// These settings are auto-generated for the managed backend UI when no custom
/// config is provided.  Field names and defaults match the `Config` struct in
/// `src/config/types.rs`.
pub const FBAF_DEFAULT_SETTINGS: &str = r#"{
    "ingame_name": "",
    "multi_switch_time": 0,
    "web_gui_port": 8080,
    "command_delay_ms": 500,
    "bed_spam_click_delay": 100,
    "bed_multiple_clicks_delay": 0,
    "bed_pre_click_ms": 30,
    "bazaar_order_check_interval_seconds": 60,
    "bazaar_order_cancel_minutes_per_million": 1,
    "bazaar_tax_rate": 1.25,
    "auction_listing_delay_ms": 1500,
    "bed_spam": false,
    "skip": false,
    "use_cofl_chat": true,
    "auto_cookie": 0,
    "auction_duration_hours": 24,
    "max_items_in_inventory": 12,
    "proxy_enabled": false,
    "proxy_address": "",
    "proxy_credentials": "",
    "webhook_url": "",
    "bazaar_webhook_url": "",
    "discord_id": "",
    "share_legendary_flips": true,
    "humanization_enabled": false,
    "humanization_min_interval_minutes": 45,
    "humanization_max_interval_minutes": 120,
    "humanization_min_break_minutes": 2,
    "humanization_max_break_minutes": 10
}"#;
