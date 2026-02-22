use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use chrono::{DateTime, Utc};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub ingame_name: Option<String>,
    
    #[serde(default = "default_websocket_url")]
    pub websocket_url: String,
    
    #[serde(default = "default_web_gui_port")]
    pub web_gui_port: u16,
    
    #[serde(default = "default_flip_action_delay")]
    pub flip_action_delay: u64,
    
    #[serde(default = "default_bed_spam_click_delay")]
    pub bed_spam_click_delay: u64,
    
    #[serde(default)]
    pub bed_multiple_clicks_delay: u64,
    
    #[serde(default = "default_bazaar_order_check_interval_seconds")]
    pub bazaar_order_check_interval_seconds: u64,
    
    #[serde(default = "default_bazaar_order_cancel_minutes")]
    pub bazaar_order_cancel_minutes: u64,
    
    #[serde(default = "default_true")]
    pub enable_bazaar_flips: bool,
    
    #[serde(default = "default_true")]
    pub enable_ah_flips: bool,
    
    #[serde(default)]
    pub bed_spam: bool,
    
    #[serde(default = "default_true")]
    pub use_cofl_chat: bool,
    
    #[serde(default)]
    pub auto_cookie: u64,
    
    #[serde(default = "default_true")]
    pub enable_console_input: bool,
    
    #[serde(default = "default_auction_duration_hours")]
    pub auction_duration_hours: u64,
    
    #[serde(default)]
    pub skip: SkipConfig,
    
    #[serde(default)]
    pub proxy_enabled: bool,
    
    #[serde(default)]
    pub proxy: Option<String>,
    
    #[serde(default)]
    pub proxy_username: Option<String>,
    
    #[serde(default)]
    pub proxy_password: Option<String>,
    
    #[serde(default)]
    pub webhook_url: Option<String>,
    
    #[serde(default)]
    pub web_gui_password: Option<String>,
    
    #[serde(default)]
    pub accounts: Option<String>,
    
    #[serde(default)]
    pub auto_switching: Option<String>,
    
    #[serde(default)]
    pub sessions: HashMap<String, CoflSession>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SkipConfig {
    #[serde(default)]
    pub always: bool,
    
    #[serde(default = "default_min_profit")]
    pub min_profit: u64,
    
    #[serde(default)]
    pub user_finder: bool,
    
    #[serde(default)]
    pub skins: bool,
    
    #[serde(default = "default_profit_percentage")]
    pub profit_percentage: f64,
    
    #[serde(default = "default_min_price")]
    pub min_price: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoflSession {
    pub id: String,
    pub expires: DateTime<Utc>,
}

// Default values
fn default_websocket_url() -> String {
    "wss://sky.coflnet.com/modsocket".to_string()
}

fn default_web_gui_port() -> u16 {
    8080
}

fn default_flip_action_delay() -> u64 {
    150 // TypeScript FLIP_ACTION_DELAY
}

fn default_bed_spam_click_delay() -> u64 {
    100
}

fn default_bazaar_order_check_interval_seconds() -> u64 {
    30
}

fn default_bazaar_order_cancel_minutes() -> u64 {
    5
}

fn default_auction_duration_hours() -> u64 {
    24
}

fn default_true() -> bool {
    true
}

fn default_min_profit() -> u64 {
    1_000_000
}

fn default_profit_percentage() -> f64 {
    50.0
}

fn default_min_price() -> u64 {
    10_000_000
}

impl Default for Config {
    fn default() -> Self {
        Self {
            ingame_name: None,
            websocket_url: default_websocket_url(),
            web_gui_port: default_web_gui_port(),
            flip_action_delay: default_flip_action_delay(),
            bed_spam_click_delay: default_bed_spam_click_delay(),
            bed_multiple_clicks_delay: 0,
            bazaar_order_check_interval_seconds: default_bazaar_order_check_interval_seconds(),
            bazaar_order_cancel_minutes: default_bazaar_order_cancel_minutes(),
            enable_bazaar_flips: true,
            enable_ah_flips: true,
            bed_spam: false,
            use_cofl_chat: true,
            auto_cookie: 0,
            enable_console_input: true,
            auction_duration_hours: default_auction_duration_hours(),
            skip: SkipConfig::default(),
            proxy_enabled: false,
            proxy: None,
            proxy_username: None,
            proxy_password: None,
            webhook_url: Some(String::new()),
            web_gui_password: None,
            accounts: None,
            auto_switching: None,
            sessions: HashMap::new(),
        }
    }
}

impl Config {
    /// Returns the webhook URL only if it is non-empty.
    pub fn active_webhook_url(&self) -> Option<&str> {
        self.webhook_url.as_deref().filter(|u| !u.is_empty())
    }
}
