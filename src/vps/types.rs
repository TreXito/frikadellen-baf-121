use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Top-level envelope for all VPS WebSocket messages (both directions).
/// Mirrors the C# `Response` type used by the Coflnet backend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VpsMessage {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub data: String,
}

impl VpsMessage {
    pub fn new(msg_type: &str, data: impl Serialize) -> Self {
        let data = match serde_json::to_string(&data) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("[VPS] Failed to serialize message data: {e}");
                String::new()
            }
        };
        Self {
            msg_type: msg_type.to_string(),
            data,
        }
    }
}

/// Payload sent by the server for `init` (list) and `configUpdate` (single).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct VpsStateUpdate {
    /// The instance configuration (TPM/BAF config blob).  Stored as opaque
    /// JSON since the actual structure varies by `AppKind`.
    pub config: Option<serde_json::Value>,

    /// Instance metadata (host, owner, expiry, …).
    pub instance: VpsInstance,

    /// Optional extra configuration string persisted per-user.
    #[serde(default)]
    pub extra_config: Option<String>,
}

/// A single VPS instance record, matching the C# `Instance` type.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct VpsInstance {
    /// IP address of the host machine this instance runs on.
    #[serde(default)]
    pub host_machine_ip: String,

    /// Coflnet user ID that owns this instance.
    #[serde(default)]
    pub owner_id: String,

    /// Unique instance identifier.
    #[serde(default)]
    pub id: String,

    /// Application kind: `"tpm"`, `"tpm+"`, `"baf"`, etc.
    #[serde(default)]
    pub app_kind: String,

    /// When the instance was created (ISO 8601).
    #[serde(default)]
    pub created_at: Option<String>,

    /// When payment expires (ISO 8601).
    #[serde(default)]
    pub paid_until: Option<String>,

    /// Arbitrary key/value context (e.g. `turnedOff`, `sessionId`).
    #[serde(default)]
    pub context: HashMap<String, String>,

    /// Public IP assigned to this instance (if dedicated).
    #[serde(default)]
    pub public_ip: Option<String>,
}

/// Payload the client sends back for an `extraUpdate` message.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtraConfigUpdate {
    pub user_id: String,
    pub extra_config: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialise_init_payload() {
        let json = r#"[{
            "Config": {"igns": ["TestUser"]},
            "Instance": {
                "HostMachineIp": "10.0.0.1",
                "OwnerId": "user123",
                "Id": "aaaabbbb-cccc-dddd-eeee-ffffffffffff",
                "AppKind": "tpm+",
                "Context": {"sessionId": "s3cr3t"}
            },
            "ExtraConfig": ""
        }]"#;
        let updates: Vec<VpsStateUpdate> = serde_json::from_str(json).expect("parse init");
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].instance.owner_id, "user123");
        assert_eq!(updates[0].instance.context.get("sessionId").unwrap(), "s3cr3t");
    }

    #[test]
    fn deserialise_config_update() {
        let json = r#"{
            "Config": null,
            "Instance": {
                "HostMachineIp": "10.0.0.1",
                "OwnerId": "user456",
                "Id": "11112222-3333-4444-5555-666677778888",
                "AppKind": "baf",
                "Context": {"turnedOff": "true"}
            }
        }"#;
        let update: VpsStateUpdate = serde_json::from_str(json).expect("parse update");
        assert!(update.instance.context.contains_key("turnedOff"));
    }

    #[test]
    fn serialise_extra_update() {
        let msg = VpsMessage::new("extraUpdate", ExtraConfigUpdate {
            user_id: "user1".into(),
            extra_config: "{\"key\":\"val\"}".into(),
        });
        assert_eq!(msg.msg_type, "extraUpdate");
        assert!(msg.data.contains("user1"));
    }
}
