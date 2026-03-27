use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use chrono::{DateTime, Utc};

/// Serde helpers that serialize `None` as `""` and deserialize `""` as `None`.
/// This ensures optional string config fields always appear in the saved TOML file
/// so users can see and edit them without needing to know the field names.
mod opt_string_as_empty {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(value: &Option<String>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(value.as_deref().unwrap_or(""))
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(if s.is_empty() { None } else { Some(s) })
    }
}

/// Serde helpers that serialize `None` as `0.0` and deserialize `0.0` (or any non-positive value) as `None`.
/// Used for `multi_switch_time` so the field appears in config.toml with a clear "disabled" value.
/// Note: negative values are also treated as `None` (disabled) since negative hours make no sense.
mod opt_f64_as_zero {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(value: &Option<f64>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_f64(value.unwrap_or(0.0))
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<f64>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let f = f64::deserialize(deserializer)?;
        Ok(if f <= 0.0 { None } else { Some(f) })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    // ─── Account ──────────────────────────────────────────────────────────

    /// Minecraft username(s). Use a single name or comma-separated names for
    /// automatic account switching: `"Account1"` or `"Account1,Account2"`.
    #[serde(default, with = "opt_string_as_empty")]
    pub ingame_name: Option<String>,

    /// Hours between automatic account switches when multiple names are set
    /// in `ingame_name`. For example, `12.0` switches every 12 hours.
    /// Set to `0` to disable automatic switching.
    #[serde(default, with = "opt_f64_as_zero")]
    pub multi_switch_time: Option<f64>,

    // ─── Flip Modes ───────────────────────────────────────────────────────

    /// Enable auction house (AH) flip buying & selling.
    #[serde(default = "default_true")]
    pub enable_ah_flips: bool,

    /// Enable bazaar flip buying & selling.
    #[serde(default = "default_true")]
    pub enable_bazaar_flips: bool,

    // ─── Auction House ────────────────────────────────────────────────────

    /// Duration in hours for new auction listings (default: 24).
    #[serde(default = "default_auction_duration_hours")]
    pub auction_duration_hours: u64,

    /// Delay in milliseconds between consecutive auction listing commands.
    /// Prevents Hypixel "Sending packets too fast!" kicks during bulk
    /// listings (default: 1500).
    #[serde(default = "default_auction_listing_delay_ms")]
    pub auction_listing_delay_ms: u64,

    // ─── Bazaar ───────────────────────────────────────────────────────────

    /// How often (seconds) to check and manage open bazaar orders (default: 60).
    #[serde(default = "default_bazaar_order_check_interval_seconds")]
    pub bazaar_order_check_interval_seconds: u64,

    /// Minutes to wait per million coins of order value before cancelling a
    /// stale bazaar order. E.g. a 5M order waits 5 × this value (default: 1).
    #[serde(default = "default_bazaar_order_cancel_minutes_per_million", alias = "bazaar_order_cancel_minutes")]
    pub bazaar_order_cancel_minutes_per_million: u64,

    /// Bazaar sell tax rate as a percentage (default: 1.25 = 1.25%).
    /// The Bazaar Flipper perk in the Community Shop reduces it by up to
    /// 0.25% (two levels × 0.125%). Set to `1.0` for max perk level.
    #[serde(default = "default_bazaar_tax_rate")]
    pub bazaar_tax_rate: f64,

    // ─── Buy Speed / Click Timings ────────────────────────────────────────

    /// Use bed-spam buying instead of the default nugget method.
    #[serde(default)]
    pub bed_spam: bool,

    /// Delay in milliseconds between clicks when bed-spam is active
    /// (default: 100). Recommended range: 100–125.
    #[serde(default = "default_bed_spam_click_delay")]
    pub bed_spam_click_delay: u64,

    /// Extra delay in milliseconds for secondary bed-spam clicks (default: 0).
    #[serde(default)]
    pub bed_multiple_clicks_delay: u64,

    /// Milliseconds before the COFL `purchaseAt` deadline to start clicking
    /// (default: 30). Only used when `freemoney = true`; ignored otherwise.
    #[serde(default = "default_bed_pre_click_ms")]
    pub bed_pre_click_ms: u64,

    /// Advanced: enable fast-buy skip-click on predicted Confirm Purchase
    /// window. Faster but riskier. Hidden unless explicitly set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fastbuy: Option<bool>,

    /// Advanced: enable auto-buy mode with timed clicks. Hidden unless
    /// explicitly set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub freemoney: Option<bool>,

    // ─── Commands & Bot Behaviour ─────────────────────────────────────────

    /// Minimum delay in milliseconds between consecutive queued commands.
    /// Prevents back-to-back Hypixel interactions from overlapping
    /// (default: 500).
    #[serde(default = "default_command_delay_ms")]
    pub command_delay_ms: u64,

    /// Show COFL chat messages in the Minecraft chat window (default: true).
    #[serde(default = "default_true")]
    pub use_cofl_chat: bool,

    /// Automatically buy a booster cookie when it expires (hours).
    /// Set to `0` to disable automatic cookie purchase.
    #[serde(default)]
    pub auto_cookie: u64,

    /// Allow typing commands in the terminal console (default: true).
    #[serde(default = "default_true")]
    pub enable_console_input: bool,

    // ─── Connection ───────────────────────────────────────────────────────

    /// Coflnet mod WebSocket URL. Only change if you run a custom server.
    #[serde(default = "default_websocket_url")]
    pub websocket_url: String,

    /// Port for the local web control panel (default: 8080).
    #[serde(default = "default_web_gui_port")]
    pub web_gui_port: u16,

    /// Password to protect the web control panel. Leave empty for no auth.
    #[serde(default, with = "opt_string_as_empty")]
    pub web_gui_password: Option<String>,

    // ─── Proxy ────────────────────────────────────────────────────────────

    /// Enable a SOCKS/HTTP proxy for both Minecraft and WebSocket connections.
    #[serde(default)]
    pub proxy_enabled: bool,

    /// Proxy server address in `host:port` format, e.g. `"121.124.241.211:3313"`.
    #[serde(default, with = "opt_string_as_empty")]
    pub proxy_address: Option<String>,

    /// Proxy credentials in `username:password` format. Leave empty if the
    /// proxy requires no authentication.
    #[serde(default, with = "opt_string_as_empty")]
    pub proxy_credentials: Option<String>,

    // ─── Discord / Webhooks ───────────────────────────────────────────────

    /// Discord webhook URL for flip notifications and profit summaries.
    /// Leave empty to disable. On first run you will be prompted.
    #[serde(default, with = "opt_string_as_empty")]
    pub webhook_url: Option<String>,

    /// Separate Discord webhook for bazaar-specific notifications (orders
    /// placed / collected / cancelled). Falls back to `webhook_url` if empty.
    #[serde(default, with = "opt_string_as_empty")]
    pub bazaar_webhook_url: Option<String>,

    /// Your Discord user ID for @-mention pings on legendary/divine flips
    /// and bans. Leave empty to disable pings.
    #[serde(default, with = "opt_string_as_empty")]
    pub discord_id: Option<String>,

    /// How often (in minutes) to send a profit summary to the webhook.
    /// Set to `0` to disable periodic profit summaries (default: 30).
    #[serde(default = "default_profit_summary_interval_minutes")]
    pub profit_summary_interval_minutes: u64,

    // ─── API Keys ─────────────────────────────────────────────────────────

    /// Hypixel API key for fetching active auctions. Get one at
    /// https://developer.hypixel.net/. Leave empty to use the Coflnet API
    /// as a fallback.
    #[serde(default, with = "opt_string_as_empty")]
    pub hypixel_api_key: Option<String>,

    // ─── Miscellaneous ────────────────────────────────────────────────────

    /// Share legendary/divine flip purchases (>100M) to the public Discord
    /// channel (default: true). Set to `false` to opt out.
    #[serde(default = "default_true")]
    pub share_legendary_flips: bool,

    /// **Deprecated** — this field is ignored. Anonymization is now a
    /// session-only toggle in the web panel.
    #[serde(default)]
    pub anonymize_webhook_name: bool,

    // ─── VPS (Coflnet Hosted) ─────────────────────────────────────────────

    /// Enable the Coflnet VPS socket to receive instance management commands
    /// from the Coflnet backend. Only enable this if running on a Coflnet VPS.
    #[serde(default)]
    pub vps_enabled: bool,

    /// WebSocket URL for the Coflnet VPS instance management endpoint.
    #[serde(default = "default_vps_url")]
    pub vps_url: String,

    /// Shared secret for authenticating with the Coflnet VPS service.
    /// Provided by Coflnet when provisioning a VPS host.
    #[serde(default, with = "opt_string_as_empty")]
    pub vps_secret: Option<String>,

    // ─── Internal (do not edit) ───────────────────────────────────────────

    /// Persisted Coflnet session tokens. Managed automatically.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub sessions: HashMap<String, CoflSession>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoflSession {
    pub id: String,
    pub expires: DateTime<Utc>,
}

// ── Default value functions ────────────────────────────────────────────────

fn default_websocket_url() -> String {
    "wss://sky.coflnet.com/modsocket".to_string()
}

fn default_web_gui_port() -> u16 {
    8080
}

fn default_command_delay_ms() -> u64 {
    500
}

fn default_bed_spam_click_delay() -> u64 {
    100
}

fn default_bed_pre_click_ms() -> u64 {
    30
}

fn default_bazaar_order_check_interval_seconds() -> u64 {
    60
}

fn default_bazaar_order_cancel_minutes_per_million() -> u64 {
    1
}

fn default_bazaar_tax_rate() -> f64 {
    1.25
}

fn default_auction_listing_delay_ms() -> u64 {
    1500
}

fn default_auction_duration_hours() -> u64 {
    24
}

fn default_profit_summary_interval_minutes() -> u64 {
    30
}

fn default_vps_url() -> String {
    "wss://sky.coflnet.com/instances".to_string()
}

fn default_true() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Self {
            // Account
            ingame_name: None,
            multi_switch_time: None,
            // Flip modes
            enable_ah_flips: true,
            enable_bazaar_flips: true,
            // Auction house
            auction_duration_hours: default_auction_duration_hours(),
            auction_listing_delay_ms: default_auction_listing_delay_ms(),
            // Bazaar
            bazaar_order_check_interval_seconds: default_bazaar_order_check_interval_seconds(),
            bazaar_order_cancel_minutes_per_million: default_bazaar_order_cancel_minutes_per_million(),
            bazaar_tax_rate: default_bazaar_tax_rate(),
            // Buy speed / click timings
            bed_spam: false,
            bed_spam_click_delay: default_bed_spam_click_delay(),
            bed_multiple_clicks_delay: 0,
            bed_pre_click_ms: default_bed_pre_click_ms(),
            fastbuy: None,
            freemoney: None,
            // Commands & bot behaviour
            command_delay_ms: default_command_delay_ms(),
            use_cofl_chat: true,
            auto_cookie: 0,
            enable_console_input: true,
            // Connection
            websocket_url: default_websocket_url(),
            web_gui_port: default_web_gui_port(),
            web_gui_password: None,
            // Proxy
            proxy_enabled: false,
            proxy_address: None,
            proxy_credentials: None,
            // Discord / webhooks
            webhook_url: None,
            bazaar_webhook_url: None,
            discord_id: None,
            profit_summary_interval_minutes: default_profit_summary_interval_minutes(),
            // API keys
            hypixel_api_key: None,
            // Miscellaneous
            share_legendary_flips: true,
            anonymize_webhook_name: false,
            // VPS
            vps_enabled: false,
            vps_url: default_vps_url(),
            vps_secret: None,
            // Internal
            sessions: HashMap::new(),
        }
    }
}

impl Config {
    pub fn freemoney_enabled(&self) -> bool {
        self.freemoney.unwrap_or(false)
    }

    pub fn fastbuy_enabled(&self) -> bool {
        self.fastbuy.unwrap_or(false)
    }

    /// Returns the webhook URL only if it is non-empty.
    pub fn active_webhook_url(&self) -> Option<&str> {
        self.webhook_url.as_deref().filter(|u| !u.is_empty())
    }

    /// Returns the bazaar-specific webhook URL if set, otherwise falls back
    /// to the regular `webhook_url`. Returns `None` if neither is configured.
    pub fn active_bazaar_webhook_url(&self) -> Option<&str> {
        self.bazaar_webhook_url
            .as_deref()
            .filter(|u| !u.is_empty())
            .or_else(|| self.active_webhook_url())
    }

    /// Returns the Discord user ID only if it is non-empty.
    pub fn active_discord_id(&self) -> Option<&str> {
        self.discord_id.as_deref().filter(|id| !id.is_empty())
    }

    /// Returns all ingame names parsed from the (comma-separated) `ingame_name` field.
    /// `"Account1,Account2"` → `["Account1", "Account2"]`
    pub fn ingame_names(&self) -> Vec<String> {
        match &self.ingame_name {
            None => vec![],
            Some(s) => s
                .split(',')
                .map(|n| n.trim().to_string())
                .filter(|n| !n.is_empty())
                .collect(),
        }
    }

    /// Returns the proxy username parsed from `proxy_credentials` (`"user:pass"` → `"user"`).
    pub fn proxy_username(&self) -> Option<&str> {
        let creds = self.proxy_credentials.as_deref()?;
        // splitn(2, ':').next() always returns Some for non-empty iterators
        Some(creds.splitn(2, ':').next().unwrap())
    }

    /// Returns the proxy password parsed from `proxy_credentials` (`"user:pass"` → `"pass"`).
    pub fn proxy_password(&self) -> Option<&str> {
        let creds = self.proxy_credentials.as_deref()?;
        let colon_pos = creds.find(':')?;
        Some(&creds[colon_pos + 1..])
    }
}

#[cfg(test)]
mod tests {
    use super::Config;

    #[test]
    fn default_config_omits_freemoney() {
        let toml = toml::to_string_pretty(&Config::default()).expect("default config should serialize");
        assert!(!toml.contains("freemoney"));
    }

    #[test]
    fn manual_freemoney_true_enables_flag() {
        let config: Config = toml::from_str("freemoney = true").expect("config should parse");
        assert!(config.freemoney_enabled());
    }

    #[test]
    fn parses_bed_spam_click_delay() {
        let config: Config = toml::from_str("bed_spam_click_delay = 125").expect("config should parse");
        assert_eq!(config.bed_spam_click_delay, 125);
    }

    #[test]
    fn default_bed_pre_click_ms() {
        let config = Config::default();
        assert_eq!(config.bed_pre_click_ms, 30);
    }

    #[test]
    fn parses_bed_pre_click_ms() {
        let config: Config = toml::from_str("bed_pre_click_ms = 300").expect("config should parse");
        assert_eq!(config.bed_pre_click_ms, 300);
    }

    #[test]
    fn single_ingame_name() {
        let config: Config = toml::from_str(r#"ingame_name = "Player1""#).expect("config should parse");
        assert_eq!(config.ingame_names(), vec!["Player1"]);
    }

    #[test]
    fn multiple_ingame_names() {
        let config: Config = toml::from_str(r#"ingame_name = "Player1,Player2,Player3""#)
            .expect("config should parse");
        assert_eq!(config.ingame_names(), vec!["Player1", "Player2", "Player3"]);
    }

    #[test]
    fn multiple_ingame_names_with_spaces() {
        let config: Config = toml::from_str(r#"ingame_name = "Player1, Player2 , Player3""#)
            .expect("config should parse");
        assert_eq!(config.ingame_names(), vec!["Player1", "Player2", "Player3"]);
    }

    #[test]
    fn no_ingame_name() {
        let config = Config::default();
        assert!(config.ingame_names().is_empty());
    }

    #[test]
    fn parses_multi_switch_time() {
        let config: Config = toml::from_str("multi_switch_time = 12.0").expect("config should parse");
        assert_eq!(config.multi_switch_time, Some(12.0));
    }

    #[test]
    fn multi_switch_time_zero_is_none() {
        let config: Config = toml::from_str("multi_switch_time = 0.0").expect("config should parse");
        assert_eq!(config.multi_switch_time, None);
    }

    #[test]
    fn multi_switch_time_default_serializes_as_zero() {
        let toml = toml::to_string_pretty(&Config::default()).expect("default config should serialize");
        assert!(toml.contains("multi_switch_time = 0.0"));
    }

    #[test]
    fn proxy_credentials_parsing() {
        let config: Config =
            toml::from_str(r#"proxy_credentials = "myuser:mypassword""#).expect("config should parse");
        assert_eq!(config.proxy_username(), Some("myuser"));
        assert_eq!(config.proxy_password(), Some("mypassword"));
    }

    #[test]
    fn proxy_credentials_password_with_colon() {
        let config: Config =
            toml::from_str(r#"proxy_credentials = "user:pass:word""#).expect("config should parse");
        assert_eq!(config.proxy_username(), Some("user"));
        assert_eq!(config.proxy_password(), Some("pass:word"));
    }

    #[test]
    fn proxy_empty_string_is_none() {
        let config: Config = toml::from_str(r#"proxy_address = """#).expect("config should parse");
        assert_eq!(config.proxy_address, None);
    }

    #[test]
    fn web_gui_password_empty_string_is_none() {
        let config: Config = toml::from_str(r#"web_gui_password = """#).expect("config should parse");
        assert_eq!(config.web_gui_password, None);
    }

    #[test]
    fn optional_fields_appear_in_default_config() {
        let toml = toml::to_string_pretty(&Config::default()).expect("default config should serialize");
        assert!(toml.contains("web_gui_password"), "web_gui_password should appear in default config");
        assert!(toml.contains("proxy_address"), "proxy_address should appear in default config");
        assert!(toml.contains("proxy_credentials"), "proxy_credentials should appear in default config");
        assert!(toml.contains("multi_switch_time"), "multi_switch_time should appear in default config");
        assert!(toml.contains("discord_id"), "discord_id should appear in default config");
    }

    #[test]
    fn proxy_fields_use_new_names() {
        let config: Config = toml::from_str(
            r#"
proxy_enabled = true
proxy_address = "121.124.241.211:3313"
proxy_credentials = "myuser:mypassword"
"#,
        )
        .expect("config should parse");
        assert!(config.proxy_enabled);
        assert_eq!(config.proxy_address.as_deref(), Some("121.124.241.211:3313"));
        assert_eq!(config.proxy_username(), Some("myuser"));
        assert_eq!(config.proxy_password(), Some("mypassword"));
    }

    #[test]
    fn default_config_has_no_skip_field() {
        let toml = toml::to_string_pretty(&Config::default()).expect("default config should serialize");
        assert!(!toml.contains("[skip]"));
        assert!(!toml.contains("min_profit"));
    }

    #[test]
    fn discord_id_empty_string_is_none() {
        let config: Config = toml::from_str(r#"discord_id = """#).expect("config should parse");
        assert_eq!(config.discord_id, None);
        assert_eq!(config.active_discord_id(), None);
    }

    #[test]
    fn discord_id_parses_and_returns_active() {
        let config: Config = toml::from_str(r#"discord_id = "123456789012345678""#).expect("config should parse");
        assert_eq!(config.active_discord_id(), Some("123456789012345678"));
    }

    #[test]
    fn fastbuy_defaults_to_false() {
        let config = Config::default();
        assert!(!config.fastbuy_enabled());
    }

    #[test]
    fn parses_fastbuy_true() {
        let config: Config = toml::from_str("fastbuy = true").expect("config should parse");
        assert!(config.fastbuy_enabled());
    }

    #[test]
    fn parses_fastbuy_false() {
        let config: Config = toml::from_str("fastbuy = false").expect("config should parse");
        assert!(!config.fastbuy_enabled());
    }

    #[test]
    fn bazaar_webhook_url_defaults_to_none() {
        let config: Config = toml::from_str("").expect("config should parse");
        assert_eq!(config.bazaar_webhook_url, None);
        assert_eq!(config.active_bazaar_webhook_url(), None);
    }

    #[test]
    fn bazaar_webhook_url_falls_back_to_regular() {
        let config: Config = toml::from_str(r#"webhook_url = "https://discord.com/api/webhooks/main""#)
            .expect("config should parse");
        assert_eq!(config.active_bazaar_webhook_url(), Some("https://discord.com/api/webhooks/main"));
    }

    #[test]
    fn bazaar_webhook_url_overrides_regular() {
        let config: Config = toml::from_str(
            r#"webhook_url = "https://discord.com/api/webhooks/main"
bazaar_webhook_url = "https://discord.com/api/webhooks/bazaar""#
        ).expect("config should parse");
        assert_eq!(config.active_bazaar_webhook_url(), Some("https://discord.com/api/webhooks/bazaar"));
        // Regular webhook is unchanged
        assert_eq!(config.active_webhook_url(), Some("https://discord.com/api/webhooks/main"));
    }

    #[test]
    fn bazaar_webhook_url_empty_string_falls_back() {
        let config: Config = toml::from_str(
            r#"webhook_url = "https://discord.com/api/webhooks/main"
bazaar_webhook_url = """#
        ).expect("config should parse");
        assert_eq!(config.active_bazaar_webhook_url(), Some("https://discord.com/api/webhooks/main"));
    }

    #[test]
    fn profit_summary_interval_defaults_to_30() {
        let config = Config::default();
        assert_eq!(config.profit_summary_interval_minutes, 30);
    }

    #[test]
    fn profit_summary_interval_zero_parses() {
        let config: Config = toml::from_str("profit_summary_interval_minutes = 0").expect("config should parse");
        assert_eq!(config.profit_summary_interval_minutes, 0);
    }

    #[test]
    fn profit_summary_interval_custom_value() {
        let config: Config = toml::from_str("profit_summary_interval_minutes = 15").expect("config should parse");
        assert_eq!(config.profit_summary_interval_minutes, 15);
    }

    #[test]
    fn vps_defaults() {
        let config = Config::default();
        assert!(!config.vps_enabled);
        assert_eq!(config.vps_url, "wss://sky.coflnet.com/instances");
        assert_eq!(config.vps_secret, None);
    }

    #[test]
    fn vps_fields_parse() {
        let config: Config = toml::from_str(
            r#"vps_enabled = true
vps_url = "wss://custom.server.com/instances"
vps_secret = "my-secret""#,
        ).expect("config should parse");
        assert!(config.vps_enabled);
        assert_eq!(config.vps_url, "wss://custom.server.com/instances");
        assert_eq!(config.vps_secret.as_deref(), Some("my-secret"));
    }

    #[test]
    fn vps_secret_empty_is_none() {
        let config: Config = toml::from_str(r#"vps_secret = """#).expect("config should parse");
        assert_eq!(config.vps_secret, None);
    }

}
