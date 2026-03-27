use super::types::Config;
use anyhow::{Context, Result};
use std::fs;
use std::path::PathBuf;
use tracing::info;

pub struct ConfigLoader {
    config_path: PathBuf,
}

impl ConfigLoader {
    pub fn new() -> Self {
        let config_path = Self::get_config_path();
        Self { config_path }
    }

    fn get_config_path() -> PathBuf {
        // Use executable directory for config file
        // This allows multiple instances to run with different configs
        let exe_dir = match std::env::current_exe() {
            Ok(exe_path) => {
                exe_path.parent()
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|| {
                        eprintln!("Warning: Could not get parent directory of executable, using current directory");
                        PathBuf::from(".")
                    })
            }
            Err(e) => {
                eprintln!("Warning: Could not get executable path ({}), using current directory", e);
                PathBuf::from(".")
            }
        };
        
        exe_dir.join("config.toml")
    }

    pub fn load(&self) -> Result<Config> {
        if !self.config_path.exists() {
            info!("Config file not found, creating default config at {:?}", self.config_path);
            let config = Config::default();
            self.save(&config)?;
            return Ok(config);
        }

        let contents = fs::read_to_string(&self.config_path)
            .context("Failed to read config file")?;
        
        let config = Self::parse_config(&contents)?;
        
        // Re-save after every load so that newly added config fields
        // appear in the file with their default values (matches TypeScript
        // initConfigHelper: "add new default values to existing config").
        self.save(&config)?;
        
        info!("Loaded configuration from {:?}", self.config_path);
        Ok(config)
    }

    fn parse_config(contents: &str) -> Result<Config> {
        let value: toml::Value = toml::from_str(contents)
            .context("Failed to parse config file")?;

        value.try_into().context("Failed to deserialize config file")
    }

    pub fn save(&self, config: &Config) -> Result<()> {
        // Ensure parent directory exists
        if let Some(parent) = self.config_path.parent() {
            fs::create_dir_all(parent)
                .context("Failed to create config directory")?;
        }

        let toml_string = toml::to_string_pretty(config)
            .context("Failed to serialize config")?;

        let commented = inject_config_comments(&toml_string);
        
        fs::write(&self.config_path, commented)
            .context("Failed to write config file")?;
        
        info!("Saved configuration to {:?}", self.config_path);
        Ok(())
    }

    pub fn update_property<F>(&self, mut updater: F) -> Result<()>
    where
        F: FnMut(&mut Config),
    {
        let mut config = self.load()?;
        updater(&mut config);
        self.save(&config)?;
        Ok(())
    }
}

// ── TOML comment injection ──────────────────────────────────────────────────
//
// `toml::to_string_pretty` produces valid TOML but strips all comments.
// We post-process the output to insert section headers and per-field comments
// so that users can understand every option without reading the docs.

/// A mapping of TOML field names to their human-readable comments.
/// Comments are injected *above* the first occurrence of each key.
/// Section headers are inserted before designated "anchor" fields.
fn inject_config_comments(toml: &str) -> String {
    use std::fmt::Write;

    // (field_name, comment_lines) — order does not matter; we match on key.
    // Leading `# ` is added automatically.
    let field_comments: &[(&str, &[&str])] = &[
        // Account
        ("ingame_name", &[
            "Minecraft username(s). Comma-separate for multi-account switching:",
            "  ingame_name = \"Account1\"  or  ingame_name = \"Account1,Account2\"",
        ]),
        ("multi_switch_time", &[
            "Hours between automatic account switches (0 = disabled).",
        ]),
        // Flip modes
        ("enable_ah_flips", &[
            "Enable auction house (AH) flip buying & selling.",
        ]),
        ("enable_bazaar_flips", &[
            "Enable bazaar flip buying & selling.",
        ]),
        // Auction house
        ("auction_duration_hours", &[
            "Duration in hours for new auction listings.",
        ]),
        ("auction_listing_delay_ms", &[
            "Delay (ms) between consecutive listing commands to avoid kicks.",
        ]),
        // Bazaar
        ("bazaar_order_check_interval_seconds", &[
            "How often (seconds) to check and manage open bazaar orders.",
        ]),
        ("bazaar_order_cancel_minutes_per_million", &[
            "Minutes to wait per 1M coins before cancelling a stale bazaar order.",
        ]),
        ("bazaar_tax_rate", &[
            "Bazaar sell tax as a percentage (1.25 = 1.25%).",
            "Community Shop Bazaar Flipper perk reduces by up to 0.25%.",
        ]),
        // Buy speed
        ("bed_spam", &[
            "Use bed-spam buying instead of the default nugget method.",
        ]),
        ("bed_spam_click_delay", &[
            "Delay (ms) between clicks when bed-spam is active (recommended: 100-125).",
        ]),
        ("bed_multiple_clicks_delay", &[
            "Extra delay (ms) for secondary bed-spam clicks.",
        ]),
        ("bed_pre_click_ms", &[
            "Milliseconds before COFL purchaseAt deadline to start clicking.",
            "Only used when freemoney = true.",
        ]),
        // Commands
        ("command_delay_ms", &[
            "Minimum delay (ms) between consecutive queued commands.",
        ]),
        ("use_cofl_chat", &[
            "Show COFL chat messages in the Minecraft chat window.",
        ]),
        ("auto_cookie", &[
            "Auto-buy a booster cookie when it expires (hours, 0 = disabled).",
        ]),
        ("enable_console_input", &[
            "Allow typing commands in the terminal console.",
        ]),
        // Connection
        ("websocket_url", &[
            "Coflnet mod WebSocket URL. Only change for a custom server.",
        ]),
        ("web_gui_port", &[
            "Port for the local web control panel.",
        ]),
        ("web_gui_password", &[
            "Password for web panel authentication (empty = no auth).",
        ]),
        // Proxy
        ("proxy_enabled", &[
            "Enable a proxy for Minecraft and WebSocket connections.",
        ]),
        ("proxy_address", &[
            "Proxy address in host:port format, e.g. \"121.124.241.211:3313\".",
        ]),
        ("proxy_credentials", &[
            "Proxy credentials in username:password format (empty = no auth).",
        ]),
        // Discord / webhooks
        ("webhook_url", &[
            "Discord webhook URL for flip notifications and profit summaries.",
            "Leave empty to disable.",
        ]),
        ("bazaar_webhook_url", &[
            "Separate Discord webhook for bazaar notifications (falls back to webhook_url).",
        ]),
        ("discord_id", &[
            "Your Discord user ID for @-mention pings on big flips and bans.",
        ]),
        ("profit_summary_interval_minutes", &[
            "How often (minutes) to send a profit summary webhook (0 = disabled).",
        ]),
        // API keys
        ("hypixel_api_key", &[
            "Hypixel API key for auction data (empty = use Coflnet API fallback).",
            "Get one at https://developer.hypixel.net/",
        ]),
        // Misc
        ("share_legendary_flips", &[
            "Share legendary/divine flip purchases (>100M) to the public Discord.",
        ]),
        ("anonymize_webhook_name", &[
            "DEPRECATED — ignored. Anonymization is a web panel toggle now.",
        ]),
        // VPS
        ("vps_enabled", &[
            "Enable the Coflnet VPS socket for remote instance management.",
            "Only enable this if running on a Coflnet-provisioned VPS host.",
        ]),
        ("vps_url", &[
            "WebSocket URL for the Coflnet VPS instance management endpoint.",
        ]),
        ("vps_secret", &[
            "Shared secret for authenticating with the Coflnet VPS service.",
        ]),
    ];

    // Section headers. Each entry lists fallback anchor fields — the header
    // is inserted before whichever field appears first in the TOML output.
    // This handles fields like `ingame_name` or `webhook_url` that may be
    // absent when their `Option` value is `None`.
    let section_headers: &[(&[&str], &str)] = &[
        (&["ingame_name", "multi_switch_time"],      "═══════════════════════  Account  ═══════════════════════"),
        (&["enable_ah_flips"],                        "═══════════════════════  Flip Modes  ════════════════════"),
        (&["auction_duration_hours"],                 "═══════════════════════  Auction House  ═════════════════"),
        (&["bazaar_order_check_interval_seconds"],    "═══════════════════════  Bazaar  ════════════════════════"),
        (&["bed_spam"],                               "═══════════════════  Buy Speed / Click Timings  ═════════"),
        (&["command_delay_ms"],                       "══════════════════  Commands & Bot Behaviour  ═══════════"),
        (&["websocket_url"],                          "═══════════════════════  Connection  ════════════════════"),
        (&["proxy_enabled"],                          "═══════════════════════  Proxy  ═════════════════════════"),
        (&["webhook_url", "bazaar_webhook_url"],      "═══════════════════  Discord / Webhooks  ════════════════"),
        (&["hypixel_api_key"],                        "═══════════════════════  API Keys  ══════════════════════"),
        (&["share_legendary_flips"],                  "══════════════════════  Miscellaneous  ═══════════════════"),
        (&["vps_enabled"],                            "═══════════════════  VPS (Coflnet Hosted)  ══════════════"),
    ];

    // Collect all keys present in the TOML to resolve fallback anchors.
    let present_keys: std::collections::HashSet<&str> = toml
        .lines()
        .filter_map(|line| {
            let key = line.split('=').next()?.trim_end();
            if key.is_empty() || key.starts_with('#') || key.starts_with('[') {
                None
            } else {
                Some(key)
            }
        })
        .collect();

    // Resolve each section header to its first-present anchor field.
    let mut sections_map: std::collections::HashMap<&str, &str> =
        std::collections::HashMap::new();
    for (anchors, header) in section_headers {
        if let Some(anchor) = anchors.iter().find(|a| present_keys.contains(**a)) {
            sections_map.insert(*anchor, *header);
        }
    }

    let comments_map: std::collections::HashMap<&str, &[&str]> =
        field_comments.iter().cloned().collect();

    let mut out = String::with_capacity(toml.len() * 2);

    for line in toml.lines() {
        // Detect `key = …` lines (TOML keys never start with whitespace at
        // top level in the pretty output).
        if let Some(key) = line.split('=').next().map(str::trim_end) {
            if !key.is_empty() && !key.starts_with('#') && !key.starts_with('[') {
                // Section header?
                if let Some(header) = sections_map.get(key) {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    let _ = writeln!(out, "# {header}");
                    out.push('\n');
                }
                // Per-field comment?
                if let Some(comments) = comments_map.get(key) {
                    for c in *comments {
                        let _ = writeln!(out, "# {c}");
                    }
                }
            }
        }

        out.push_str(line);
        out.push('\n');
    }

    out
}

#[cfg(test)]
mod tests {
    use super::ConfigLoader;

    #[test]
    fn parse_config_ignores_unknown_fields() {
        // confirm_skip is an unknown field — parsing must succeed (not panic/error).
        let config = ConfigLoader::parse_config("confirm_skip = true")
            .expect("config with unknown field should still parse");
        // Known defaults still apply
        assert!(!config.freemoney_enabled());
    }

    #[test]
    fn saved_config_contains_section_comments() {
        let toml = toml::to_string_pretty(&super::super::types::Config::default())
            .expect("serialize");
        let commented = super::inject_config_comments(&toml);
        // Account section falls back to multi_switch_time when ingame_name is None
        assert!(commented.contains("Account"), "Account section header should be present");
        assert!(commented.contains("Bazaar"), "Bazaar section header should be present");
        assert!(commented.contains("VPS (Coflnet Hosted)"), "VPS section header should be present");
    }

    #[test]
    fn saved_config_contains_field_comments() {
        let toml = toml::to_string_pretty(&super::super::types::Config::default())
            .expect("serialize");
        let commented = super::inject_config_comments(&toml);
        // multi_switch_time always appears (opt_f64_as_zero)
        assert!(commented.contains("# Hours between automatic account switches"));
        assert!(commented.contains("# How often (minutes) to send a profit summary webhook"));
    }
}

impl Default for ConfigLoader {
    fn default() -> Self {
        Self::new()
    }
}
