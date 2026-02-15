use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebSocketMessage {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub data: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub text: String,
}

/// Parse websocket message data (handles double-JSON encoding)
pub fn parse_message_data<T: for<'de> Deserialize<'de>>(data: &str) -> Result<T, serde_json::Error> {
    // First parse as Value to handle potential double encoding
    let value: Value = serde_json::from_str(data)?;
    
    // If it's a string, parse it again
    if let Some(string_data) = value.as_str() {
        serde_json::from_str(string_data)
    } else {
        serde_json::from_value(value)
    }
}
