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
    // ═══════════════════════════ Account ═══════════════════════════
    /// Ingame Minecraft username(s). Supports multiple comma-separated accounts:
    /// `ingame_name = "Account1"` for a single account, or
    /// `ingame_name = "Account1,Account2"` for automatic switching.
    #[serde(default)]
    pub ingame_name: Option<String>,

    /// Time in hours after which the bot switches to the next account in `ingame_name`.
    /// Only used when multiple accounts are specified. E.g. `multi_switch_time = 12.0`
    /// means switch accounts every 12 hours. Set to `0` to disable automatic switching.
    #[serde(default, with = "opt_f64_as_zero")]
    pub multi_switch_time: Option<f64>,

    // ═══════════════════════ Coflnet connection ════════════════════
    #[serde(default = "default_websocket_url")]
    pub websocket_url: String,

    /// Multisocket: extra Coflnet modsocket URLs to connect in parallel with
    /// `websocket_url` (e.g. regional servers like "us-sky.coflnet.com/modsocket").
    /// Each extra socket uses the same player/session; auction flips from all
    /// sockets are merged and deduped by UUID, so whichever socket delivers a
    /// flip first wins. Secondary sockets only contribute auction flips — chat,
    /// commands and bazaar flips still come from the primary socket alone.
    /// Empty (the default) = classic single-socket behaviour.
    #[serde(default)]
    pub multisocket_urls: Vec<String>,

    // ═══════════════════ Private finder (baf-flip-finder) ══════════
    /// baf-flip-finder websocket feed (e.g. "ws://192.168.0.250:15101").
    /// When set, flips found by the private finder are bought through the
    /// same pipeline as COFL flips (deduped by auction UUID, first source
    /// wins). The flip's `target` is used for listing, like COFL targets.
    #[serde(default)]
    pub finder_ws_url: Option<String>,

    /// Token from the finder's data/ws-config.json.
    #[serde(default)]
    pub finder_ws_token: Option<String>,

    /// Report this account's live purse to the FINDER sockets (finder_ws_url /
    /// multisocket_urls) so it can size flips to the account — a low-purse
    /// account is fed small, fast, safe flips to grind up, a grown one gets the
    /// bigger fish. Sent ONLY to finder sockets, NEVER to the COFL websocket, so
    /// no purse ever leaves the machine to a third party. Default OFF: enable it
    /// only on your own accounts pointed at your own finder.
    #[serde(default)]
    pub finder_report_purse: bool,

    /// In finder-only mode COFL never sends createAuction, so nothing lists the
    /// items the bot buys or reclaims from expired auctions. When enabled the
    /// bot uploads its inventory to the finder every minute and lists what the
    /// finder prices (its listing recommendation). Safe: only items the finder
    /// can value as sellable AH items are listed; blacklist ids in the finder
    /// config to exclude any. Default on.
    #[serde(default = "default_true")]
    pub finder_auto_list: bool,

    // ═══════════════════════ Auto-relist blocklist ═════════════════
    /// SkyBlock item IDs which the private finder must never automatically
    /// relist (for example, `JUJU_SHORTBOW`). This only affects finder-driven
    /// listing instructions; manual and ordinary COFL listings still work.
    /// Values are normalized to uppercase, sorted, and deduplicated on load.
    /// SkyBlock item IDs that must never be automatically relisted after a
    /// buy — for either COFL or finder flips (e.g. `HYPERION`, `TERMINATOR`).
    /// The item stays in inventory for manual handling. Values are normalized
    /// to uppercase, sorted, and deduplicated on load.
    #[serde(default = "default_do_not_relist_ids")]
    pub do_not_relist_ids: Vec<String>,

    /// Flip finders whose buys must never be automatically relisted. Matches
    /// the `finder` shown on the purchase webhook (the COFL finder, e.g.
    /// `craftcost`). Matching is punctuation/case-insensitive, so `craftcost`,
    /// `CRAFT_COST`, and `CraftCost` are equivalent.
    #[serde(default = "default_do_not_relist_finders")]
    pub do_not_relist_finders: Vec<String>,

    /// Profit ceiling (coins) for automatic relisting. When above 0, any flip
    /// whose expected profit (target − buy − AH fee) is at or above this value
    /// is NOT auto-relisted — big-ticket items are held for manual handling
    /// instead of being auto-dumped. Applies to both COFL and finder flips.
    #[serde(default = "default_do_not_relist_over_profit")]
    pub do_not_relist_over_profit: u64,

    // ═══════════════════════ Flip behaviour toggles ════════════════
    /// **Deprecated**: COFL now handles flip type selection automatically.
    /// Master switch for bazaar flipping. Defaults to true. Persisted so the web
    /// panel's Bazaar-flips toggle survives restarts (it was previously
    /// `skip_serializing`, which silently reset it to true on every load/save —
    /// the "config keeps going back to default" bug).
    #[serde(default = "default_true")]
    pub enable_bazaar_flips: bool,

    /// Master switch for auction-house flipping. Defaults to true. Persisted for
    /// the same reason as `enable_bazaar_flips` above.
    #[serde(default = "default_true")]
    pub enable_ah_flips: bool,

    /// When a purchased drill has parts installed (Fuel Tank / Drill Engine /
    /// Upgrade Module), call Jotraeline Greatforge via the Abiphone and pull the
    /// parts out before listing, so the parts and the stripped drill sell
    /// separately (often worth more than the assembled drill). Off by default.
    /// The workflow is deliberately slow and safe, waits for every GUI to load,
    /// opens no other menus, and only acts when the bought item is a drill that
    /// actually has removable parts. In finder-primary mode the periodic inventory
    /// upload re-lists the parts + stripped drill automatically afterwards.
    #[serde(default)]
    pub remove_drill_parts: bool,

    /// Enable fast-buy skip-click on predicted Confirm Purchase window.
    /// When true, the bot pre-clicks slot 11 (confirm) in the same TCP burst as
    /// the buy-click, saving one round-trip to the server.
    #[serde(default, alias = "fastbuy")]
    pub skip: bool,

    /// Bed-timing mode for grace-period (bed) auctions.  When enabled the bot uses
    /// the COFL `purchaseAt` timestamp to start pre-clicking the bed slot
    /// `bed_pre_click_ms` before the grace period ends, instead of waiting for the
    /// item to become buyable.  Defaults to `true`.  (Formerly `freemoney`; the old
    /// name is still accepted via the serde alias for backward compatibility.)
    #[serde(default = "default_true", alias = "freemoney")]
    pub bedtiming: bool,

    // ═══════════════════════ Timing / delays ═══════════════════════
    /// Minimum delay between consecutive queued commands in milliseconds.
    /// Prevents back-to-back Hypixel interactions from overlapping.
    /// Default: 500ms.
    #[serde(default = "default_command_delay_ms")]
    pub command_delay_ms: u64,

    #[serde(default = "default_bed_spam_click_delay")]
    pub bed_spam_click_delay: u64,

    /// How many ms before the COFL `purchaseAt` deadline to start clicking (default: 30).
    /// Only used when `bedtiming = true`. Without bedtiming, bed spam starts immediately
    /// using `bed_spam_click_delay` and this value is ignored.
    #[serde(default = "default_bed_pre_click_ms")]
    pub bed_pre_click_ms: u64,

    /// Delay in milliseconds between consecutive auction listing commands
    /// (SellToAuction). Prevents Hypixel from kicking the bot with
    /// "Sending packets too fast!" during bulk listings. Default: 1500ms.
    #[serde(default = "default_auction_listing_delay_ms")]
    pub auction_listing_delay_ms: u64,

    // ═══════════════════════ Bazaar settings ═══════════════════════
    #[serde(default = "default_bazaar_order_check_interval_seconds")]
    pub bazaar_order_check_interval_seconds: u64,

    #[serde(default = "default_bazaar_order_cancel_minutes_per_million", alias = "bazaar_order_cancel_minutes")]
    pub bazaar_order_cancel_minutes_per_million: u64,

    /// Bazaar sell tax rate as a percentage (e.g. 1.25 = 1.25%).
    /// Hypixel applies 1.25% by default. The Bazaar Flipper perk from the
    /// Community Shop reduces it by up to 0.25% (two levels × 0.125%).
    /// Set to 1.0 if you have the max perk level.
    #[serde(default = "default_bazaar_tax_rate")]
    pub bazaar_tax_rate: f64,

    // ═══════════════════ Auction / inventory / runtime ═════════════
    #[serde(default = "default_auction_duration_hours")]
    pub auction_duration_hours: u64,

    /// Maximum number of flip items allowed in inventory at once.
    /// Sent to COFL on startup via `/cofl set maxitemsininventory`.
    /// Default: 12.
    #[serde(default = "default_max_items_in_inventory")]
    pub max_items_in_inventory: u64,

    #[serde(default)]
    pub auto_cookie: u64,

    /// Internal: set once the first-run wizard has asked about auto-cookie, so
    /// the prompt does not reappear on every launch. Not a user-facing setting
    /// (edit `auto_cookie` directly to change the threshold afterwards).
    #[serde(default)]
    pub auto_cookie_prompted: bool,

    #[serde(default = "default_true")]
    pub use_cofl_chat: bool,

    #[serde(default = "default_true")]
    pub enable_console_input: bool,

    // ═══════════════════════════ Proxy ═════════════════════════════
    /// Enable proxy for both the Minecraft and WebSocket connections.
    #[serde(default)]
    pub proxy_enabled: bool,

    /// Proxy server address in `host:port` format, e.g. `"121.124.241.211:3313"`.
    /// Only used when `proxy_enabled = true`. Leave empty to disable.
    #[serde(default, with = "opt_string_as_empty")]
    pub proxy_address: Option<String>,

    /// Proxy credentials in `username:password` format, e.g. `"myuser:mypassword"`.
    /// Leave empty if the proxy requires no authentication.
    #[serde(default, with = "opt_string_as_empty")]
    pub proxy_credentials: Option<String>,

    // ═══════════════════════ Web control panel ═════════════════════
    #[serde(default = "default_web_gui_port")]
    pub web_gui_port: u16,

    /// Password required to open the web control panel. A strong random one is
    /// generated the first time this file is written, so a panel is never
    /// reachable without a password. Change it to whatever you like — but it
    /// cannot be left empty: blanking it just generates a fresh random one.
    ///
    /// The panel always serves HTTPS and manages its own certificate, so this
    /// password is never sent over the wire in the clear.
    #[serde(default, with = "opt_string_as_empty")]
    pub web_gui_password: Option<String>,

    // ═══════════════════════ External API keys ═════════════════════
    /// Hypixel API key for fetching active auctions. Obtain one from https://developer.hypixel.net/
    /// Leave empty to use the Coflnet API as a fallback.
    #[serde(default, with = "opt_string_as_empty")]
    pub hypixel_api_key: Option<String>,

    // ═══════════════════ Discord notifications ═════════════════════
    #[serde(default)]
    /// Discord webhook URL for notifications.
    /// `None` = not yet configured (prompts on next startup).
    /// `Some("")` = explicitly disabled (no further prompts).
    /// `Some(url)` = active webhook.
    pub webhook_url: Option<String>,

    /// Separate Discord webhook URL for bazaar-specific notifications
    /// (order placed, collected, cancelled). Leave empty to use the regular
    /// `webhook_url` for all notifications.
    #[serde(default, with = "opt_string_as_empty")]
    pub bazaar_webhook_url: Option<String>,

    /// Discord user ID for pinging on legendary/divine flips and bans.
    /// Leave empty to disable pings.
    #[serde(default, with = "opt_string_as_empty")]
    pub discord_id: Option<String>,

    /// Whether to share legendary/divine flip purchases to the public Discord channel.
    /// Defaults to true. Set to false to opt out.
    #[serde(default = "default_true")]
    pub share_legendary_flips: bool,

    /// Ping the owner (via `discord_id`) when another player visits the bot's
    /// island ("[RANK] Name is visiting your island!"). Defaults to true.
    #[serde(default = "default_true")]
    pub notify_island_visitors: bool,

    /// Flip on a FRIEND's island instead of the bot's own. When set to a
    /// friend's IGN the bot goes to their island via `/visit <ign>` (clicking
    /// the "Visit player island" ender-eye at slot 11) wherever it would
    /// otherwise `/is` home, and treats that island as its default location so
    /// the AFK guard doesn't teleport it back. If the friend has guest visits
    /// disabled, the option is ignored for the rest of the session (the bot
    /// flips on its own island) and a webhook is sent. Leave empty to flip on
    /// the bot's own island (the default).
    #[serde(default, with = "opt_string_as_empty")]
    pub visitfriend: Option<String>,

    /// Ping the owner (via `discord_id`) when another player tries to reach the
    /// bot in chat: an incoming whisper/DM, or a guild/party/public line that
    /// directly addresses the bot (name at the start of the message, or
    /// `@name`). A name merely appearing mid-sentence does NOT trigger it.
    /// Defaults to true.
    #[serde(default = "default_true")]
    pub notify_name_mentions: bool,

    // ═══════════════════ Central backend (baf-backend) ═════════════
    /// Stable, per-installation identifier used by the central backend to
    /// recognise this bot across reconnects. Auto-generated on first run and
    /// persisted; do not change it or the backend will treat it as a new bot.
    #[serde(default, with = "opt_string_as_empty")]
    pub instance_id: Option<String>,

    /// Connect to the central backend gateway for remote control + profit
    /// tracking. Defaults to true so every client joins the central server.
    #[serde(default = "default_true")]
    pub backend_enabled: bool,

    /// Central backend gateway WebSocket URL. Defaults to the shared server.
    #[serde(default = "default_backend_url")]
    pub backend_url: String,

    /// Extra Discord user IDs (comma-separated) allowed to control this bot from
    /// Discord, in addition to `discord_id` (the owner). Leave empty for none.
    #[serde(default, with = "opt_string_as_empty")]
    pub backend_allowed_ids: Option<String>,

    // ═══════════════════ Humanization / Rest Breaks ═══════════════
    /// Enable periodic "human-like" rest breaks where the macro disconnects
    /// for a randomized period before reconnecting. Does NOT reset the
    /// account-switching session timer. Default: false.
    #[serde(default)]
    pub humanization_enabled: bool,

    /// Minimum time between rest breaks in minutes. Default: 45.
    #[serde(default = "default_humanization_min_interval_minutes")]
    pub humanization_min_interval_minutes: u64,

    /// Maximum time between rest breaks in minutes. Default: 120.
    #[serde(default = "default_humanization_max_interval_minutes")]
    pub humanization_max_interval_minutes: u64,

    /// Minimum rest break duration in minutes. Default: 2.
    #[serde(default = "default_humanization_min_break_minutes")]
    pub humanization_min_break_minutes: u64,

    /// Maximum rest break duration in minutes. Default: 10.
    #[serde(default = "default_humanization_max_break_minutes")]
    pub humanization_max_break_minutes: u64,

    // ═══════════════════════ Persisted session state ═══════════════
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub sessions: HashMap<String, CoflSession>,
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

fn default_backend_url() -> String {
    "wss://backend.auctionflipper.bz/ws".to_string()
}

fn default_web_gui_port() -> u16 {
    8080
}

/// Generate a random panel password.
///
/// The alphabet deliberately drops the characters people mis-transcribe
/// (`0/O`, `1/l/I`) because this password is read off a console banner or out of
/// config.toml and typed by hand. 20 characters of a 58-symbol alphabet is ~117
/// bits, far past anything the 500 ms-per-attempt login endpoint can be walked
/// through.
pub fn generate_web_password() -> String {
    use rand::Rng;
    const ALPHABET: &[u8] = b"abcdefghijkmnopqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut rng = rand::rng();
    (0..20)
        .map(|_| ALPHABET[rng.random_range(0..ALPHABET.len())] as char)
        .collect()
}

fn default_do_not_relist_ids() -> Vec<String> {
    vec!["HYPERION".to_string(), "TERMINATOR".to_string()]
}

fn default_do_not_relist_finders() -> Vec<String> {
    vec!["craftcost".to_string()]
}

fn default_do_not_relist_over_profit() -> u64 {
    200_000_000
}

/// Canonical form of a finder name for blocklist matching: keep only ASCII
/// alphanumerics, uppercased. So `craftcost`, `CRAFT_COST`, and `CraftCost`
/// all canonicalize to `CRAFTCOST`.
fn canon_finder(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect()
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

fn default_max_items_in_inventory() -> u64 {
    12
}

fn default_true() -> bool {
    true
}

fn default_humanization_min_interval_minutes() -> u64 {
    45
}

fn default_humanization_max_interval_minutes() -> u64 {
    120
}

fn default_humanization_min_break_minutes() -> u64 {
    2
}

fn default_humanization_max_break_minutes() -> u64 {
    10
}

impl Default for Config {
    fn default() -> Self {
        Self {
            // Account
            ingame_name: None,
            multi_switch_time: None,
            // Coflnet connection
            websocket_url: default_websocket_url(),
            multisocket_urls: Vec::new(),
            // Private finder
            finder_ws_url: None,
            finder_ws_token: None,
            finder_report_purse: false,
            finder_auto_list: true,
            // Auto-relist blocklist
            do_not_relist_ids: default_do_not_relist_ids(),
            do_not_relist_finders: default_do_not_relist_finders(),
            do_not_relist_over_profit: default_do_not_relist_over_profit(),
            // Flip behaviour toggles
            enable_bazaar_flips: true,
            enable_ah_flips: true,
            remove_drill_parts: false,
            skip: false,
            bedtiming: true,
            // Timing / delays
            command_delay_ms: default_command_delay_ms(),
            bed_spam_click_delay: default_bed_spam_click_delay(),
            bed_pre_click_ms: default_bed_pre_click_ms(),
            auction_listing_delay_ms: default_auction_listing_delay_ms(),
            // Bazaar settings
            bazaar_order_check_interval_seconds: default_bazaar_order_check_interval_seconds(),
            bazaar_order_cancel_minutes_per_million: default_bazaar_order_cancel_minutes_per_million(),
            bazaar_tax_rate: default_bazaar_tax_rate(),
            // Auction / inventory / runtime
            auction_duration_hours: default_auction_duration_hours(),
            max_items_in_inventory: default_max_items_in_inventory(),
            auto_cookie: 0,
            auto_cookie_prompted: false,
            use_cofl_chat: true,
            enable_console_input: true,
            // Proxy
            proxy_enabled: false,
            proxy_address: None,
            proxy_credentials: None,
            // Web control panel
            web_gui_port: default_web_gui_port(),
            web_gui_password: None,
            // External API keys
            hypixel_api_key: None,
            // Discord notifications
            webhook_url: None,
            bazaar_webhook_url: None,
            discord_id: None,
            share_legendary_flips: true,
            notify_island_visitors: true,
            visitfriend: None,
            notify_name_mentions: true,
            // Central backend
            instance_id: None,
            backend_enabled: true,
            backend_url: default_backend_url(),
            backend_allowed_ids: None,
            // Humanization / rest breaks
            humanization_enabled: false,
            humanization_min_interval_minutes: default_humanization_min_interval_minutes(),
            humanization_max_interval_minutes: default_humanization_max_interval_minutes(),
            humanization_min_break_minutes: default_humanization_min_break_minutes(),
            humanization_max_break_minutes: default_humanization_max_break_minutes(),
            // Persisted session state
            sessions: HashMap::new(),
        }
    }
}

impl Config {
    /// Guarantee the control panel has a password, generating one if it does not.
    ///
    /// Returns the password when it had to invent one, so the caller can shout
    /// about it — a password nobody is told about is the same as a lockout.
    ///
    /// This runs on every load, not just on first write, because the panel
    /// exposes full bot control (chat, config, account switching) and an
    /// unauthenticated one on a public VPS is exactly how instances get taken
    /// over. Blanking the field is therefore not a way to disable auth.
    pub fn ensure_web_gui_password(&mut self) -> Option<String> {
        if self.web_gui_password.as_deref().is_some_and(|p| !p.is_empty()) {
            return None;
        }
        let generated = generate_web_password();
        self.web_gui_password = Some(generated.clone());
        Some(generated)
    }

    /// Normalize the finder relist blocklist before it is persisted so the
    /// generated config stays easy to read and comparisons are reliable.
    pub fn normalize_do_not_relist_ids(&mut self) {
        self.do_not_relist_ids = self
            .do_not_relist_ids
            .iter()
            .map(|id| id.trim().to_ascii_uppercase())
            .filter(|id| !id.is_empty())
            .collect();
        self.do_not_relist_ids.sort();
        self.do_not_relist_ids.dedup();

        // Finders: keep the user's spelling but drop blanks/dupes (matching is
        // punctuation/case-insensitive via canon_finder, so we don't rewrite).
        self.do_not_relist_finders = self
            .do_not_relist_finders
            .iter()
            .map(|f| f.trim().to_string())
            .filter(|f| !canon_finder(f).is_empty())
            .collect();
        self.do_not_relist_finders
            .dedup_by(|a, b| canon_finder(a) == canon_finder(b));
    }

    /// True when this SkyBlock item ID is excluded from automatic finder
    /// relisting. Input is intentionally normalized here too, so callers do
    /// not depend on where the id came from (NBT, JSON, or UI).
    pub fn should_not_relist_id(&self, item_id: &str) -> bool {
        self.do_not_relist_ids
            .binary_search(&item_id.trim().to_ascii_uppercase())
            .is_ok()
    }

    /// True when a flip found by `finder` must not be auto-relisted. Matching
    /// is punctuation/case-insensitive (`craftcost` == `CRAFT_COST`).
    pub fn should_not_relist_finder(&self, finder: &str) -> bool {
        let target = canon_finder(finder);
        !target.is_empty()
            && self
                .do_not_relist_finders
                .iter()
                .any(|f| canon_finder(f) == target)
    }

    /// True when a flip's expected profit (coins) is at or above the relist
    /// ceiling. A ceiling of 0 disables the check.
    pub fn should_not_relist_profit(&self, profit: i64) -> bool {
        self.do_not_relist_over_profit > 0 && profit >= self.do_not_relist_over_profit as i64
    }

    /// Central relist blocklist gate for both COFL and finder flips. Returns
    /// `Some(reason)` (for the chat/log line) when the item must be held rather
    /// than auto-relisted, or `None` when it may be listed. Each argument is
    /// optional so callers pass whatever they know at the relist site.
    pub fn relist_block_reason(
        &self,
        item_id: Option<&str>,
        finder: Option<&str>,
        profit: Option<i64>,
    ) -> Option<String> {
        if let Some(id) = item_id {
            if !id.is_empty() && self.should_not_relist_id(id) {
                return Some(format!(
                    "item id {} is in do_not_relist_ids",
                    id.trim().to_ascii_uppercase()
                ));
            }
        }
        if let Some(f) = finder {
            if self.should_not_relist_finder(f) {
                return Some(format!("finder {} is in do_not_relist_finders", f));
            }
        }
        if let Some(p) = profit {
            if self.should_not_relist_profit(p) {
                return Some(format!(
                    "profit {} ≥ do_not_relist_over_profit {}",
                    p, self.do_not_relist_over_profit
                ));
            }
        }
        None
    }

    pub fn bedtiming_enabled(&self) -> bool {
        self.bedtiming
    }

    pub fn skip_enabled(&self) -> bool {
        self.skip
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

    /// Parse `backend_allowed_ids` (comma-separated) into a list of Discord IDs.
    pub fn backend_allowed_ids_list(&self) -> Vec<String> {
        match &self.backend_allowed_ids {
            None => vec![],
            Some(s) => s
                .split(',')
                .map(|id| id.trim().to_string())
                .filter(|id| !id.is_empty())
                .collect(),
        }
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
    use super::{default_web_gui_port, generate_web_password, Config};

    #[test]
    fn default_config_enables_bedtiming() {
        let config = Config::default();
        assert!(config.bedtiming_enabled());
    }

    #[test]
    fn default_config_includes_bedtiming() {
        let toml = toml::to_string_pretty(&Config::default()).expect("default config should serialize");
        assert!(toml.contains("bedtiming = true"), "bedtiming should appear in default config");
    }

    #[test]
    fn manual_bedtiming_false_disables_flag() {
        let config: Config = toml::from_str("bedtiming = false").expect("config should parse");
        assert!(!config.bedtiming_enabled());
    }

    #[test]
    fn legacy_freemoney_alias_still_works() {
        // Old configs using `freemoney = true` must keep enabling bed timing.
        let config: Config = toml::from_str("freemoney = true").expect("config should parse");
        assert!(config.bedtiming_enabled());
        let config: Config = toml::from_str("freemoney = false").expect("config should parse");
        assert!(!config.bedtiming_enabled());
    }

    #[test]
    fn bedtiming_defaults_true_when_absent() {
        let config: Config = toml::from_str("").expect("config should parse");
        assert!(config.bedtiming_enabled());
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
    fn ensure_web_gui_password_fills_in_a_missing_one() {
        let mut config: Config = toml::from_str(r#"web_gui_password = """#).expect("config should parse");
        let generated = config.ensure_web_gui_password().expect("should generate one");
        assert_eq!(config.web_gui_password.as_deref(), Some(generated.as_str()));
        assert_eq!(generated.len(), 20);
        assert!(generated.chars().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn ensure_web_gui_password_keeps_a_configured_one() {
        let mut config: Config =
            toml::from_str(r#"web_gui_password = "hunter2""#).expect("config should parse");
        assert_eq!(config.ensure_web_gui_password(), None);
        assert_eq!(config.web_gui_password.as_deref(), Some("hunter2"));
    }

    #[test]
    fn generated_web_passwords_differ() {
        // A constant "random" default would be worse than none at all: every
        // install would ship the same publicly known password.
        assert_ne!(generate_web_password(), generate_web_password());
    }

    #[test]
    fn generated_web_passwords_avoid_ambiguous_characters() {
        let pw = generate_web_password();
        for c in ['0', 'O', '1', 'l', 'I'] {
            assert!(!pw.contains(c), "{pw} contains hand-transcription trap {c}");
        }
    }

    #[test]
    fn removed_tls_settings_do_not_break_existing_configs() {
        // Configs written by older builds still carry these keys. Parsing must
        // ignore them rather than refusing to start.
        let config: Config = toml::from_str(
            "web_https = true\nweb_tls_cert_path = \"/etc/cert.pem\"\nweb_tls_key_path = \"/etc/key.pem\"",
        )
        .expect("stale TLS keys should be ignored, not fatal");
        assert_eq!(config.web_gui_port, default_web_gui_port());
    }

    #[test]
    fn optional_fields_appear_in_default_config() {
        let toml = toml::to_string_pretty(&Config::default()).expect("default config should serialize");
        assert!(toml.contains("web_gui_password"), "web_gui_password should appear in default config");
        assert!(toml.contains("proxy_address"), "proxy_address should appear in default config");
        assert!(toml.contains("proxy_credentials"), "proxy_credentials should appear in default config");
        assert!(toml.contains("multi_switch_time"), "multi_switch_time should appear in default config");
        assert!(toml.contains("discord_id"), "discord_id should appear in default config");
        assert!(toml.contains("do_not_relist_ids"), "do_not_relist_ids should appear in default config");
        assert!(toml.contains("do_not_relist_finders"), "do_not_relist_finders should appear in default config");
        assert!(toml.contains("do_not_relist_over_profit"), "do_not_relist_over_profit should appear in default config");
    }

    #[test]
    fn relist_blocklist_defaults() {
        let c = Config::default();
        assert_eq!(c.do_not_relist_ids, vec!["HYPERION", "TERMINATOR"]);
        assert_eq!(c.do_not_relist_finders, vec!["craftcost"]);
        assert_eq!(c.do_not_relist_over_profit, 200_000_000);
    }

    #[test]
    fn relist_block_reason_covers_all_three_axes() {
        let c = Config::default();
        // Item id (case-insensitive)
        assert!(c.relist_block_reason(Some("hyperion"), None, None).is_some());
        assert!(c.relist_block_reason(Some("ASPECT_OF_THE_END"), None, None).is_none());
        // Finder (punctuation/case-insensitive)
        assert!(c.relist_block_reason(None, Some("CRAFT_COST"), None).is_some());
        assert!(c.relist_block_reason(None, Some("CraftCost"), None).is_some());
        assert!(c.relist_block_reason(None, Some("SNIPER"), None).is_none());
        // Profit ceiling (>= 200m held; below still lists)
        assert!(c.relist_block_reason(None, None, Some(200_000_000)).is_some());
        assert!(c.relist_block_reason(None, None, Some(250_000_000)).is_some());
        assert!(c.relist_block_reason(None, None, Some(199_999_999)).is_none());
        // Nothing supplied → never blocks.
        assert!(c.relist_block_reason(None, None, None).is_none());
    }

    #[test]
    fn relist_over_profit_zero_disables() {
        let c: Config = toml::from_str("do_not_relist_over_profit = 0").expect("parse");
        assert!(!c.should_not_relist_profit(i64::MAX), "0 disables the profit gate");
    }

    #[test]
    fn do_not_relist_ids_are_normalized_sorted_and_matched() {
        let mut config: Config = toml::from_str(
            "do_not_relist_ids = [\" juju_shortbow \", \"HYPERION\", \"JUJU_SHORTBOW\", \"\"]",
        )
        .expect("config should parse");
        config.normalize_do_not_relist_ids();
        assert_eq!(config.do_not_relist_ids, vec!["HYPERION", "JUJU_SHORTBOW"]);
        assert!(config.should_not_relist_id("juju_shortbow"));
        assert!(config.should_not_relist_id(" JUJU_SHORTBOW "));
        assert!(!config.should_not_relist_id("TERMINATOR"));
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
    fn skip_defaults_to_false() {
        let config = Config::default();
        assert!(!config.skip_enabled());
    }

    #[test]
    fn parses_skip_true() {
        let config: Config = toml::from_str("skip = true").expect("config should parse");
        assert!(config.skip_enabled());
    }

    #[test]
    fn parses_skip_false() {
        let config: Config = toml::from_str("skip = false").expect("config should parse");
        assert!(!config.skip_enabled());
    }

    #[test]
    fn fastbuy_alias_still_works() {
        let config: Config = toml::from_str("fastbuy = true").expect("config should parse");
        assert!(config.skip_enabled());
    }

    #[test]
    fn skip_appears_in_default_config() {
        let toml = toml::to_string_pretty(&Config::default()).expect("default config should serialize");
        assert!(toml.contains("skip = false"), "skip should appear in default config");
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

}
