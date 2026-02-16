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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_inject_referral_id() {
        // Test basic injection
        let url = "https://sky.coflnet.com/authmod?userId=123&amp;conId=abc123";
        let result = inject_referral_id(url);
        assert!(result.contains("refId=9KKPN9"));
        assert!(result.contains("&amp;conId=abc123"));
        assert_eq!(result, "https://sky.coflnet.com/authmod?userId=123&amp;refId=9KKPN9&amp;conId=abc123");

        // Test already has refId - should not inject again
        let url_with_ref = "https://sky.coflnet.com/authmod?userId=123&amp;refId=existing&amp;conId=abc123";
        let result = inject_referral_id(url_with_ref);
        assert_eq!(result, url_with_ref);

        // Test non-auth URL - should remain unchanged
        let normal_url = "https://example.com/page?param=value";
        let result = inject_referral_id(normal_url);
        assert_eq!(result, normal_url);
    }

    #[test]
    fn test_chat_message_with_referral_id() {
        let msg = ChatMessage {
            text: "Click here to authenticate".to_string(),
            on_click: Some("https://sky.coflnet.com/authmod?userId=123&amp;conId=abc123".to_string()),
            hover: Some("Hover text with https://sky.coflnet.com/authmod?test=1&amp;conId=xyz".to_string()),
        };

        let processed = msg.with_referral_id();
        
        // Check that refId was injected into onClick
        assert!(processed.on_click.as_ref().unwrap().contains("refId=9KKPN9"));
        
        // Check that refId was injected into hover
        assert!(processed.hover.as_ref().unwrap().contains("refId=9KKPN9"));
    }

    #[test]
    fn test_parse_chat_message_array() {
        let json = r#"[{"text":"Hello","onClick":"https://example.com"},{"text":"World"}]"#;
        let messages: Result<Vec<ChatMessage>, _> = serde_json::from_str(json);
        assert!(messages.is_ok());
        let messages = messages.unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].text, "Hello");
        assert_eq!(messages[0].on_click, Some("https://example.com".to_string()));
        assert_eq!(messages[1].text, "World");
        assert_eq!(messages[1].on_click, None);
    }
}
