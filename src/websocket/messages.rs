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
    #[serde(rename = "onClick")]
    pub on_click: Option<String>,
    pub hover: Option<String>,
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

/// Inject referral ID into Coflnet authentication URLs
/// This adds the referral ID "9KKPN9" before the connection ID parameter
pub fn inject_referral_id(url: &str) -> String {
    if url.contains("sky.coflnet.com/authmod?") && !url.contains("refId=") {
        url.replace("&amp;conId=", "&amp;refId=9KKPN9&amp;conId=")
    } else {
        url.to_string()
    }
}

impl ChatMessage {
    /// Process the chat message to inject referral IDs into auth URLs
    pub fn with_referral_id(mut self) -> Self {
        self.text = inject_referral_id(&self.text);
        if let Some(ref on_click) = self.on_click {
            self.on_click = Some(inject_referral_id(on_click));
        }
        if let Some(ref hover) = self.hover {
            self.hover = Some(inject_referral_id(hover));
        }
        self
    }
}
