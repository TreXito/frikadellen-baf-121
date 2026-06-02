use once_cell::sync::Lazy;
use tracing::warn;

// Shared HTTP client - reqwest clients are designed to be cloned/reused
static HTTP_CLIENT: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .build()
        .expect("Failed to build reqwest client")
});

/// Return the relay endpoint URL.
///
/// The value is first looked up at **compile time** via `option_env!`.  When the
/// release CI sets `BAF_NOTIFY_RELAY_URL` as a build environment variable the URL
/// is baked directly into the binary — users never need to configure anything.
/// During local development the runtime environment variable of the same name is
/// used as a fallback, so you can test without a full rebuild.  If neither is
/// set, public-channel notifications are silently skipped.
fn notify_relay_url() -> Option<String> {
    // `option_env!` is evaluated at compile time; returns None when the var is absent.
    const COMPILE_TIME: Option<&str> = option_env!("BAF_NOTIFY_RELAY_URL");
    COMPILE_TIME
        .filter(|s| !s.is_empty())
        .map(|s| s.to_owned())
        .or_else(|| std::env::var("BAF_NOTIFY_RELAY_URL").ok().filter(|s| !s.is_empty()))
}

/// Return the HMAC-SHA256 signing secret.
///
/// Like `notify_relay_url`, the value is baked in at compile time when the CI
/// sets `BAF_NOTIFY_SECRET` during the build.  Falls back to the runtime
/// environment variable for local development.  When present, every relay
/// request is signed so the relay server can reject spoofed requests.
fn notify_relay_secret() -> Option<String> {
    const COMPILE_TIME: Option<&str> = option_env!("BAF_NOTIFY_SECRET");
    COMPILE_TIME
        .filter(|s| !s.is_empty())
        .map(|s| s.to_owned())
        .or_else(|| std::env::var("BAF_NOTIFY_SECRET").ok().filter(|s| !s.is_empty()))
}

/// Compute an HMAC-SHA256 hex digest over `message` using `key`.
fn hmac_sha256_hex(key: &str, message: &str) -> String {
    use hmac::{Hmac, Mac};
    type HmacSha256 = Hmac<sha2::Sha256>;
    let mut mac = HmacSha256::new_from_slice(key.as_bytes())
        .expect("HMAC accepts any key length");
    mac.update(message.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// Send an HMAC-signed POST request to the relay endpoint.
///
/// The body is a JSON object:
/// ```json
/// { "event": "<event>", "timestamp": <unix_secs>, "payload": { ... }, "signature": "<hmac_hex>" }
/// ```
///
/// The signature covers `"<event>:<timestamp>:<payload_json>"` using the
/// `BAF_NOTIFY_SECRET` env-var as the key.  If `BAF_NOTIFY_SECRET` is not set,
/// the signature field is omitted so the relay can choose whether to accept
/// unsigned requests (useful during local development).
async fn post_relay(event: &str, payload: serde_json::Value) {
    let Some(relay_url) = notify_relay_url() else {
        tracing::debug!(
            "[Relay] BAF_NOTIFY_RELAY_URL not set — skipping {} notification",
            event
        );
        return;
    };

    let timestamp = now_unix();
    let payload_json = payload.to_string();

    let mut body = serde_json::json!({
        "event": event,
        "timestamp": timestamp,
        "payload": payload,
    });

    if let Some(secret) = notify_relay_secret() {
        let message = format!("{}:{}:{}", event, timestamp, payload_json);
        let sig = hmac_sha256_hex(&secret, &message);
        body.as_object_mut()
            .expect("body is a JSON object")
            .insert("signature".to_string(), serde_json::Value::String(sig));
    }

    if let Err(e) = HTTP_CLIENT.post(&relay_url).json(&body).send().await {
        warn!("[Relay] Failed to send {} notification: {}", event, e);
    }
}

async fn post_embed(webhook_url: &str, payload: serde_json::Value) {
    if let Err(e) = HTTP_CLIENT.post(webhook_url).json(&payload).send().await {
        warn!("[Webhook] Failed to send webhook: {}", e);
    }
}

/// Post an embed with optional text content (used for Discord pings).
async fn post_embed_with_content(webhook_url: &str, content: Option<&str>, payload: serde_json::Value) {
    let mut body = payload;
    if let Some(text) = content {
        if let Some(obj) = body.as_object_mut() {
            obj.insert("content".to_string(), serde_json::Value::String(text.to_string()));
        }
    }
    if let Err(e) = HTTP_CLIENT.post(webhook_url).json(&body).send().await {
        warn!("[Webhook] Failed to send webhook: {}", e);
    }
}

/// Format a number with M/K suffixes matching TypeScript formatNumber()
fn format_number(n: f64) -> String {
    if n >= 1_000_000.0 {
        format!("{:.2}M", n / 1_000_000.0)
    } else if n >= 1_000.0 {
        format!("{:.2}K", n / 1_000.0)
    } else {
        format!("{:.0}", n)
    }
}

/// Sanitize an item name into an uppercase tag for use as a Coflnet icon URL path component.
/// Converts "Meteor Magma Lord Helmet Skin" → "METEOR_MAGMA_LORD_HELMET_SKIN".
fn sanitize_item_name(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_alphanumeric() { c.to_ascii_uppercase() } else { '_' })
        .collect::<String>()
        .trim_matches('_')
        .to_string()
}

/// Unix timestamp seconds for Discord relative timestamps
fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Format seconds as a human-readable duration ("2h 5m 30s" etc.)
fn format_duration(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{}h {}m", h, m)
    } else if m > 0 {
        format!("{}m {}s", m, s)
    } else {
        format!("{}s", s)
    }
}

/// Format purse amount with b/m/k suffixes matching TypeScript formatPurse()
/// Examples: 1_404_040_000 → "1.40b", 96_532_000 → "96.53m", 590_278 → "590.3k"
fn format_purse(n: u64) -> String {
    if n >= 1_000_000_000 {
        format!("{:.2}b", n as f64 / 1_000_000_000.0)
    } else if n >= 1_000_000 {
        format!("{:.2}m", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

pub async fn send_webhook_auth_failed(
    ingame_name: &str,
    attempt: u32,
    max_retries: u32,
    error: &str,
    discord_id: Option<&str>,
    webhook_url: &str,
) {
    let description = if attempt >= max_retries {
        format!(
            "**{}** — all {} authentication attempts failed.\nThe process will restart automatically.",
            ingame_name, max_retries
        )
    } else {
        format!(
            "**{}** — authentication failed (attempt {}/{}).\n```\n{}\n```",
            ingame_name, attempt, max_retries, error
        )
    };

    let payload = serde_json::json!({
        "embeds": [{
            "title": "🔒 Authentication Failed",
            "description": description,
            "color": 0xe74c3cu32,
            "footer": {
                "text": format!("BAF - {}", ingame_name),
                "icon_url": format!("https://mc-heads.net/avatar/{}/32.png", ingame_name)
            },
            "timestamp": chrono::Utc::now().to_rfc3339()
        }]
    });
    let ping = discord_id.map(|id| format!("<@{}>", id));
    post_embed_with_content(webhook_url, ping.as_deref(), payload).await;
}


pub async fn send_webhook_initialized(
    ingame_name: &str,
    ah_enabled: bool,
    bazaar_enabled: bool,
    connection_id: Option<&str>,
    premium: Option<(&str, &str)>, // (tier, expires)
    webhook_url: &str,
) {
    let mut description = format!(
        "AH Flips: {} | Bazaar Flips: {}\n<t:{}:R>",
        if ah_enabled { "✅" } else { "❌" },
        if bazaar_enabled { "✅" } else { "❌" },
        now_unix()
    );
    if let Some((tier, expires)) = premium {
        description.push_str(&format!("\n\n**Coflnet {}** expires {}", tier, expires));
    }

    let mut fields: Vec<serde_json::Value> = Vec::new();
    if let Some(conn_id) = connection_id {
        fields.push(serde_json::json!({
            "name": "Connection ID",
            "value": format!("`{}`", conn_id),
            "inline": false
        }));
    }

    let embed = if fields.is_empty() {
        serde_json::json!({
            "title": "✓ Started BAF",
            "description": description,
            "color": 0x00ff88u32,
            "footer": {
                "text": format!("BAF - {}", ingame_name),
                "icon_url": format!("https://mc-heads.net/avatar/{}/32.png", ingame_name)
            }
        })
    } else {
        serde_json::json!({
            "title": "✓ Started BAF",
            "description": description,
            "color": 0x00ff88u32,
            "fields": fields,
            "footer": {
                "text": format!("BAF - {}", ingame_name),
                "icon_url": format!("https://mc-heads.net/avatar/{}/32.png", ingame_name)
            }
        })
    };
    let payload = serde_json::json!({ "embeds": [embed] });
    post_embed(webhook_url, payload).await;
}

pub async fn send_webhook_startup_complete(
    ingame_name: &str,
    orders_found: u64,
    ah_enabled: bool,
    bazaar_enabled: bool,
    connection_id: Option<&str>,
    premium: Option<(&str, &str)>, // (tier, expires)
    webhook_url: &str,
) {
    let mut description = format!(
        "Ready to accept flips!\n\nAH Flips: {}\nBazaar Flips: {}",
        if ah_enabled { "✅ Enabled" } else { "❌ Disabled" },
        if bazaar_enabled { "✅ Enabled" } else { "❌ Disabled" }
    );
    if let Some((tier, expires)) = premium {
        description.push_str(&format!("\n\n**Coflnet {}** expires {}", tier, expires));
    }

    let mut fields = vec![
        serde_json::json!({"name": "1️⃣ Cookie Check", "value": "```✓ Complete```", "inline": true}),
        serde_json::json!({
            "name": "2️⃣ Order Discovery",
            "value": if bazaar_enabled {
                format!("```✓ Found {} order(s)```", orders_found)
            } else {
                "```- Skipped (Bazaar disabled)```".to_string()
            },
            "inline": true
        }),
        serde_json::json!({"name": "3️⃣ Claim Items", "value": "```✓ Complete```", "inline": true}),
    ];
    if let Some(conn_id) = connection_id {
        fields.push(serde_json::json!({
            "name": "Connection ID",
            "value": format!("`{}`", conn_id),
            "inline": false
        }));
    }

    let payload = serde_json::json!({
        "embeds": [{
            "title": "🚀 Startup Workflow Complete",
            "description": description,
            "color": 0x2ecc71u32,
            "fields": fields,
            "footer": {
                "text": format!("BAF - {}", ingame_name),
                "icon_url": format!("https://mc-heads.net/avatar/{}/32.png", ingame_name)
            },
            "timestamp": chrono::Utc::now().to_rfc3339()
        }]
    });
    post_embed(webhook_url, payload).await;
}

pub async fn send_webhook_item_purchased(
    ingame_name: &str,
    item_name: &str,
    price: u64,
    target: Option<u64>,
    profit: Option<i64>,
    purse: Option<u64>,
    buy_speed_ms: Option<u64>,
    auction_uuid: Option<&str>,
    finder: Option<&str>,
    webhook_url: &str,
) {
    let fields = build_purchase_fields(price, target, profit, buy_speed_ms, finder, auction_uuid);
    let safe_item = sanitize_item_name(item_name);
    let payload = serde_json::json!({
        "embeds": [{
            "title": "🛒 Item Purchased Successfully",
            "description": format!("**{}** • <t:{}:R>", item_name, now_unix()),
            "color": 0x00ff00,
            "fields": fields,
            "thumbnail": {"url": format!("https://sky.coflnet.com/static/icon/{}", safe_item)},
            "footer": {
                "text": format!("BAF • {}{}", ingame_name,
                    purse.map(|p| format!(" • Purse: {} coins", format_purse(p))).unwrap_or_default()),
                "icon_url": format!("https://mc-heads.net/avatar/{}/32.png", ingame_name)
            }
        }]
    });
    post_embed(webhook_url, payload).await;
}

pub async fn send_webhook_item_sold(
    ingame_name: &str,
    item_name: &str,
    price: u64,
    buyer: &str,
    profit: Option<i64>,
    buy_price: Option<u64>,
    time_to_sell_secs: Option<u64>,
    purse: Option<u64>,
    auction_uuid: Option<&str>,
    webhook_url: &str,
) {
    let safe_item = sanitize_item_name(item_name);
    let status_emoji = match profit {
        Some(p) if p >= 0 => "✅",
        Some(_) => "❌",
        None => "✅",
    };
    let title = match profit {
        Some(p) if p >= 0 => "Item Sold (Profit)",
        Some(_) => "Item Sold (Loss)",
        None => "Item Sold",
    };
    let mut fields = vec![
        serde_json::json!({
            "name": "👤 Buyer",
            "value": format!("```\n{}\n```", buyer),
            "inline": true
        }),
        serde_json::json!({
            "name": "💵 Sale Price",
            "value": format!("```fix\n{} coins\n```", format_number(price as f64)),
            "inline": true
        }),
    ];
    if let Some(p) = profit {
        let sign = if p >= 0 { "+" } else { "-" };
        let abs_profit = if p >= 0 { p as f64 } else { (-p) as f64 };
        fields.push(serde_json::json!({
            "name": "💰 Net Profit",
            "value": format!("```diff\n{}{} coins\n```", sign, format_number(abs_profit)),
            "inline": true
        }));
        // ROI percentage (matching TypeScript sendWebhookItemSold)
        if let Some(bp) = buy_price {
            if bp > 0 {
                let roi = (p as f64 / bp as f64) * 100.0;
                fields.push(serde_json::json!({
                    "name": "📊 ROI",
                    "value": format!("```{:.1}%```", roi),
                    "inline": true
                }));
            }
        }
    }
    if let Some(secs) = time_to_sell_secs {
        fields.push(serde_json::json!({
            "name": "⏱️ Time to Sell",
            "value": format!("```\n{}\n```", format_duration(secs)),
            "inline": true
        }));
    }
    if let Some(uuid) = auction_uuid {
        if !uuid.is_empty() {
            fields.push(serde_json::json!({
                "name": "🔗 Auction Link",
                "value": format!("[View on Coflnet](https://sky.coflnet.com/auction/{}?refId=9KKPN9)", uuid),
                "inline": false
            }));
        }
    }
    let payload = serde_json::json!({
        "embeds": [{
            "title": format!("{} {}", status_emoji, title),
            "description": format!("**{}** • <t:{}:R>", item_name, now_unix()),
            "color": 0x0099ff,
            "fields": fields,
            "thumbnail": {"url": format!("https://sky.coflnet.com/static/icon/{}", safe_item)},
            "footer": {
                "text": format!("BAF • {}{}", ingame_name,
                    purse.map(|p| format!(" • Purse: {} coins", format_purse(p))).unwrap_or_default()),
                "icon_url": format!("https://mc-heads.net/avatar/{}/32.png", ingame_name)
            }
        }]
    });
    post_embed(webhook_url, payload).await;
}

pub async fn send_webhook_bazaar_order_placed(
    ingame_name: &str,
    item_name: &str,
    amount: u64,
    price_per_unit: f64,
    total_price: f64,
    is_buy_order: bool,
    purse: Option<u64>,
    active_orders: usize,
    webhook_url: &str,
) {
    let order_type = if is_buy_order { "Buy Order" } else { "Sell Offer" };
    let order_emoji = if is_buy_order { "🛒" } else { "🏷️" };
    let color: u32 = if is_buy_order { 0x00cccc } else { 0xff9900 };
    let safe_item = sanitize_item_name(item_name);
    let payload = serde_json::json!({
        "embeds": [{
            "title": format!("{} Bazaar {} Placed", order_emoji, order_type),
            "description": format!("**{}** • <t:{}:R>", item_name, now_unix()),
            "color": color,
            "fields": [
                {"name": "📦 Amount",       "value": format!("```fix\n{}x\n```", amount),                     "inline": true},
                {"name": "💵 Price/Unit",   "value": format!("```fix\n{} coins\n```", format_number(price_per_unit)), "inline": true},
                {"name": "💰 Total Price",  "value": format!("```fix\n{} coins\n```", format_number(total_price)),    "inline": true},
                {"name": "📊 Order Type",   "value": format!("```\n{}\n```", order_type),                     "inline": true},
                {"name": "📋 Active Orders", "value": format!("```fix\n{}\n```", active_orders),              "inline": true},
            ],
            "thumbnail": {"url": format!("https://sky.coflnet.com/static/icon/{}", safe_item)},
            "footer": {
                "text": format!("BAF • {}{}", ingame_name,
                    purse.map(|p| format!(" • Purse: {} coins", format_purse(p))).unwrap_or_default()),
                "icon_url": format!("https://mc-heads.net/avatar/{}/32.png", ingame_name)
            }
        }]
    });
    post_embed(webhook_url, payload).await;
}

pub async fn send_webhook_bazaar_order_collected(
    ingame_name: &str,
    item_name: &str,
    is_buy_order: bool,
    amount: Option<u64>,
    price_per_unit: Option<f64>,
    profit: Option<i64>,
    purse: Option<u64>,
    remaining_orders: usize,
    webhook_url: &str,
) {
    let order_type = if is_buy_order { "Buy Order" } else { "Sell Offer" };
    let color: u32 = if is_buy_order {
        0x66FF66
    } else {
        match profit {
            Some(p) if p < 0 => 0xFF4444,
            _ => 0xFFCC00,
        }
    };
    let order_emoji = match profit {
        Some(p) if p < 0 => "❌",
        _ => "✅",
    };
    let title_suffix = if !is_buy_order {
        match profit {
            Some(p) if p >= 0 => " (Profit)",
            Some(_) => " (Loss)",
            None => "",
        }
    } else {
        ""
    };
    let safe_item = sanitize_item_name(item_name);

    let mut fields = vec![
        serde_json::json!({"name": "📊 Order Type", "value": format!("```\n{}\n```", order_type), "inline": false}),
    ];
    if let Some(amt) = amount {
        fields.push(serde_json::json!({"name": "📦 Amount", "value": format!("```fix\n{}x\n```", amt), "inline": true}));
    }
    if let Some(ppu) = price_per_unit {
        fields.push(serde_json::json!({"name": "💵 Price/Unit", "value": format!("```fix\n{} coins\n```", format_number(ppu)), "inline": true}));
        if let Some(amt) = amount {
            let total = ppu * amt as f64;
            fields.push(serde_json::json!({"name": "💰 Total", "value": format!("```fix\n{} coins\n```", format_number(total)), "inline": true}));
        }
    }
    if let Some(p) = profit {
        let sign = if p >= 0 { "+" } else { "-" };
        let abs_profit = if p >= 0 { p as f64 } else { (-p) as f64 };
        fields.push(serde_json::json!({
            "name": if p >= 0 { "💰 Profit" } else { "💸 Loss" },
            "value": format!("```diff\n{}{} coins\n```", sign, format_number(abs_profit)),
            "inline": true
        }));
    }
    fields.push(serde_json::json!({"name": "📋 Remaining Orders", "value": format!("```fix\n{}\n```", remaining_orders), "inline": true}));

    let payload = serde_json::json!({
        "embeds": [{
            "title": format!("{} Bazaar {} Collected{}", order_emoji, order_type, title_suffix),
            "description": format!("**{}** • <t:{}:R>", item_name, now_unix()),
            "color": color,
            "fields": fields,
            "thumbnail": {"url": format!("https://sky.coflnet.com/static/icon/{}", safe_item)},
            "footer": {
                "text": format!("BAF • {}{}", ingame_name,
                    purse.map(|p| format!(" • Purse: {} coins", format_purse(p))).unwrap_or_default()),
                "icon_url": format!("https://mc-heads.net/avatar/{}/32.png", ingame_name)
            }
        }]
    });
    post_embed(webhook_url, payload).await;
}

pub async fn send_webhook_bazaar_order_cancelled(
    ingame_name: &str,
    item_name: &str,
    is_buy_order: bool,
    amount: Option<u64>,
    price_per_unit: Option<f64>,
    purse: Option<u64>,
    remaining_orders: usize,
    webhook_url: &str,
) {
    let order_type = if is_buy_order { "Buy Order" } else { "Sell Offer" };
    let order_emoji = "🚫";
    let color: u32 = 0x808080; // Gray for cancellation
    let safe_item = sanitize_item_name(item_name);

    let mut fields = vec![
        serde_json::json!({"name": "📊 Order Type", "value": format!("```\n{}\n```", order_type), "inline": false}),
    ];
    if let Some(amt) = amount {
        fields.push(serde_json::json!({"name": "📦 Amount", "value": format!("```fix\n{}x\n```", amt), "inline": true}));
    }
    if let Some(ppu) = price_per_unit {
        fields.push(serde_json::json!({"name": "💵 Price/Unit", "value": format!("```fix\n{} coins\n```", format_number(ppu)), "inline": true}));
        if let Some(amt) = amount {
            let total = ppu * amt as f64;
            fields.push(serde_json::json!({"name": "💰 Total", "value": format!("```fix\n{} coins\n```", format_number(total)), "inline": true}));
        }
    }
    fields.push(serde_json::json!({"name": "📋 Remaining Orders", "value": format!("```fix\n{}\n```", remaining_orders), "inline": true}));

    let payload = serde_json::json!({
        "embeds": [{
            "title": format!("{} Bazaar {} Cancelled", order_emoji, order_type),
            "description": format!("**{}** • <t:{}:R>", item_name, now_unix()),
            "color": color,
            "fields": fields,
            "thumbnail": {"url": format!("https://sky.coflnet.com/static/icon/{}", safe_item)},
            "footer": {
                "text": format!("BAF • {}{}", ingame_name,
                    purse.map(|p| format!(" • Purse: {} coins", format_purse(p))).unwrap_or_default()),
                "icon_url": format!("https://mc-heads.net/avatar/{}/32.png", ingame_name)
            }
        }]
    });
    post_embed(webhook_url, payload).await;
}

/// Webhook sent when the bazaar daily sell value limit is reached.
pub async fn send_webhook_bazaar_daily_limit(
    ingame_name: &str,
    webhook_url: &str,
) {
    let payload = serde_json::json!({
        "embeds": [{
            "title": "⚠️ Bazaar Daily Limit Reached",
            "description": format!("Bazaar flips disabled for **{}** until 0:00 UTC daily reset.", ingame_name),
            "color": 0xFF0000u32,
            "fields": [
                {"name": "⏰ Resets At", "value": format!("<t:{}:R>", next_utc_midnight_unix()), "inline": true},
            ],
            "footer": {
                "text": format!("BAF • {}", ingame_name),
                "icon_url": format!("https://mc-heads.net/avatar/{}/32.png", ingame_name)
            }
        }]
    });
    post_embed(webhook_url, payload).await;
}

/// Unix timestamp of the next 0:00 UTC.
pub fn next_utc_midnight_unix() -> u64 {
    let now = now_unix();
    let secs_since_midnight = now % 86400;
    now + (86400 - secs_since_midnight)
}

pub async fn send_webhook_auction_listed(
    ingame_name: &str,
    item_name: &str,
    starting_bid: u64,
    duration_hours: u64,
    purse: Option<u64>,
    active_listings: usize,
    webhook_url: &str,
) {
    let safe_item = sanitize_item_name(item_name);
    let expires_unix = now_unix() + duration_hours * 3600;
    let payload = serde_json::json!({
        "embeds": [{
            "title": "🏷️ BIN Auction Listed",
            "description": format!("**{}** • <t:{}:R>", item_name, now_unix()),
            "color": 0xe67e22u32,
            "fields": [
                {"name": "💵 BIN Price",  "value": format!("```fix\n{} coins\n```", format_number(starting_bid as f64)), "inline": true},
                {"name": "⏳ Duration",   "value": format!("```\n{}h\n```", duration_hours),                             "inline": true},
                {"name": "📅 Expires",    "value": format!("<t:{}:R>", expires_unix),                                    "inline": true},
                {"name": "📋 Active Listings", "value": format!("```fix\n{}\n```", active_listings),                     "inline": true},
            ],
            "thumbnail": {"url": format!("https://sky.coflnet.com/static/icon/{}", safe_item)},
            "footer": {
                "text": format!("BAF • {}{}", ingame_name,
                    purse.map(|p| format!(" • Purse: {} coins", format_purse(p))).unwrap_or_default()),
                "icon_url": format!("https://mc-heads.net/avatar/{}/32.png", ingame_name)
            }
        }]
    });
    post_embed(webhook_url, payload).await;
}

pub async fn send_webhook_banned(
    ingame_name: &str,
    reason: &str,
    discord_id: Option<&str>,
    webhook_url: &str,
) {
    let parsed = parse_ban_reason(reason);

    let mut fields: Vec<serde_json::Value> = Vec::new();
    if let Some(duration) = &parsed.duration {
        fields.push(serde_json::json!({
            "name": "⏱️ Duration",
            "value": format!("```\n{}\n```", duration),
            "inline": true
        }));
    }
    if let Some(ban_reason) = &parsed.reason {
        fields.push(serde_json::json!({
            "name": "📋 Reason",
            "value": format!("```\n{}\n```", ban_reason),
            "inline": false
        }));
    }
    if let Some(ban_id) = &parsed.ban_id {
        let id_label = if parsed.is_security_ban { "🔖 Block ID" } else { "🔖 Ban ID" };
        fields.push(serde_json::json!({
            "name": id_label,
            "value": format!("`{}`", ban_id),
            "inline": true
        }));
    }
    if let Some(appeal_url) = &parsed.appeal_url {
        fields.push(serde_json::json!({
            "name": "🔗 Appeal",
            "value": appeal_url,
            "inline": true
        }));
    }

    let title = if parsed.is_security_ban {
        "🛡️ Security Ban"
    } else if parsed.is_permanent {
        "⛔ Permanently Banned"
    } else if parsed.duration.is_some() {
        "⛔ Temporarily Banned"
    } else {
        "⛔ Bot Banned / Disconnected"
    };

    let description = if parsed.is_security_ban {
        if parsed.clean_text.is_empty() {
            format!("**{}** has been security blocked.\nCheck <https://www.hypixel.net/security-block> for details.", ingame_name)
        } else {
            format!("**{}** has been security blocked.\n\n{}", ingame_name, parsed.clean_text)
        }
    } else if parsed.clean_text.is_empty() {
        format!("**{}** has been banned.", ingame_name)
    } else {
        format!("**{}** has been banned.\n\n{}", ingame_name, parsed.clean_text)
    };

    let mut embed = serde_json::json!({
        "title": title,
        "description": description,
        "color": 0xe74c3cu32,
        "footer": {
            "text": format!("BAF - {}", ingame_name),
            "icon_url": format!("https://mc-heads.net/avatar/{}/32.png", ingame_name)
        },
        "timestamp": chrono::Utc::now().to_rfc3339()
    });
    if !fields.is_empty() {
        embed.as_object_mut().expect("embed is a JSON object").insert("fields".to_string(), serde_json::json!(fields));
    }

    let payload = serde_json::json!({ "embeds": [embed] });
    // Triple ping so ban notifications are easily differentiated from other alerts
    let ping = discord_id.map(|id| format!("<@{}> <@{}> <@{}>", id, id, id));
    post_embed_with_content(webhook_url, ping.as_deref(), payload).await;
}

/// Send a public ban notification via the configured relay endpoint.
/// Anonymized — no IGN or user-identifying information.
///
/// The relay endpoint and signing secret are read from the `BAF_NOTIFY_RELAY_URL`
/// and `BAF_NOTIFY_SECRET` environment variables.  If not configured, this is a
/// no-op.
pub async fn send_webhook_banned_public() {
    let payload = serde_json::json!({
        "message": "A user of this macro just got banned"
    });
    post_relay("ban_notify", payload).await;
}

/// Send a webhook when "You cannot view this auction!" is detected (no booster cookie).
/// Pings the user so they can manually log in and buy a booster cookie.
pub async fn send_webhook_no_cookie(
    ingame_name: &str,
    discord_id: Option<&str>,
    webhook_url: &str,
) {
    let payload = serde_json::json!({
        "embeds": [{
            "title": "🍪 No Booster Cookie",
            "description": format!(
                "**{}** received \"You cannot view this auction!\" — this usually means the account has no active booster cookie.\n\nPlease log in manually and buy a booster cookie, then start the bot again.",
                ingame_name
            ),
            "color": 0xe67e22u32,
            "footer": {
                "text": format!("BAF - {}", ingame_name),
                "icon_url": format!("https://mc-heads.net/avatar/{}/32.png", ingame_name)
            },
            "timestamp": chrono::Utc::now().to_rfc3339()
        }]
    });
    // Triple ping so the user notices immediately
    let ping = discord_id.map(|id| format!("<@{}> <@{}> <@{}>", id, id, id));
    post_embed_with_content(webhook_url, ping.as_deref(), payload).await;
}

pub async fn send_webhook_auction_cancelled(
    ingame_name: &str,
    item_name: &str,
    starting_bid: u64,
    purse: Option<u64>,
    remaining_listings: usize,
    webhook_url: &str,
) {
    let safe_item = sanitize_item_name(item_name);
    let payload = serde_json::json!({
        "embeds": [{
            "title": "❌ Auction Cancelled",
            "description": format!("**{}** • <t:{}:R>", item_name, now_unix()),
            "color": 0xe74c3cu32,
            "fields": [
                {"name": "💵 Starting Bid", "value": format!("```fix\n{} coins\n```", format_number(starting_bid as f64)), "inline": true},
                {"name": "📋 Remaining Listings", "value": format!("```fix\n{}\n```", remaining_listings), "inline": true},
            ],
            "thumbnail": {"url": format!("https://sky.coflnet.com/static/icon/{}", safe_item)},
            "footer": {
                "text": format!("BAF • {}{}", ingame_name,
                    purse.map(|p| format!(" • Purse: {} coins", format_purse(p))).unwrap_or_default()),
                "icon_url": format!("https://mc-heads.net/avatar/{}/32.png", ingame_name)
            }
        }]
    });
    post_embed(webhook_url, payload).await;
}

/// Profit threshold for a "Legendary" flip (100M coins).
pub const LEGENDARY_PROFIT_THRESHOLD: u64 = 100_000_000;

/// Profit threshold for a "Divine" flip (1B coins).
pub const DIVINE_PROFIT_THRESHOLD: u64 = 1_000_000_000;

/// Send a legendary flip (100M+ profit) notification to the user's webhook.
/// Like a normal purchase webhook but with yellow color, legendary title, and optional Discord ping.
/// Also always sends an anonymized notification to the shared public channel.
pub async fn send_webhook_legendary_flip(
    ingame_name: &str,
    item_name: &str,
    price: u64,
    target: Option<u64>,
    profit: i64,
    purse: Option<u64>,
    buy_speed_ms: Option<u64>,
    auction_uuid: Option<&str>,
    finder: Option<&str>,
    discord_id: Option<&str>,
    webhook_url: &str,
) {
    let fields = build_purchase_fields(price, target, Some(profit), buy_speed_ms, finder, auction_uuid);
    let safe_item = sanitize_item_name(item_name);
    let payload = serde_json::json!({
        "embeds": [{
            "title": "🌟 Legendary Flip!",
            "description": format!("**{}** • <t:{}:R>", item_name, now_unix()),
            "color": 0xFFD700u32,
            "fields": fields,
            "thumbnail": {"url": format!("https://sky.coflnet.com/static/icon/{}", safe_item)},
            "footer": {
                "text": format!("BAF • {}{}", ingame_name,
                    purse.map(|p| format!(" • Purse: {} coins", format_purse(p))).unwrap_or_default()),
                "icon_url": format!("https://mc-heads.net/avatar/{}/32.png", ingame_name)
            }
        }]
    });
    let ping = discord_id.map(|id| format!("<@{}>", id));
    post_embed_with_content(webhook_url, ping.as_deref(), payload).await;
    send_webhook_flip_channel(item_name, price, target, profit, buy_speed_ms, finder).await;
}

/// Send a divine flip (1B+ profit) notification to the user's webhook.
/// Like a normal purchase webhook but with cyan color, divine title, and optional Discord ping.
/// Also always sends an anonymized notification to the shared public channel.
pub async fn send_webhook_divine_flip(
    ingame_name: &str,
    item_name: &str,
    price: u64,
    target: Option<u64>,
    profit: i64,
    purse: Option<u64>,
    buy_speed_ms: Option<u64>,
    auction_uuid: Option<&str>,
    finder: Option<&str>,
    discord_id: Option<&str>,
    webhook_url: &str,
) {
    let fields = build_purchase_fields(price, target, Some(profit), buy_speed_ms, finder, auction_uuid);
    let safe_item = sanitize_item_name(item_name);
    let payload = serde_json::json!({
        "embeds": [{
            "title": "💎 Divine Flip!",
            "description": format!("**{}** • <t:{}:R>", item_name, now_unix()),
            "color": 0x00FFFFu32,
            "fields": fields,
            "thumbnail": {"url": format!("https://sky.coflnet.com/static/icon/{}", safe_item)},
            "footer": {
                "text": format!("BAF • {}{}", ingame_name,
                    purse.map(|p| format!(" • Purse: {} coins", format_purse(p))).unwrap_or_default()),
                "icon_url": format!("https://mc-heads.net/avatar/{}/32.png", ingame_name)
            }
        }]
    });
    let ping = discord_id.map(|id| format!("<@{}>", id));
    post_embed_with_content(webhook_url, ping.as_deref(), payload).await;
    send_webhook_flip_channel(item_name, price, target, profit, buy_speed_ms, finder).await;
}

/// Send an anonymized legendary/divine flip notification to the shared channel
/// via the configured relay endpoint.
///
/// No IGN, purse, auction link, or other identifying info is included.
/// The relay endpoint and signing secret are read from the `BAF_NOTIFY_RELAY_URL`
/// and `BAF_NOTIFY_SECRET` environment variables — no webhook URL is stored in
/// the source code.  If the relay is not configured, this is a no-op.
pub async fn send_webhook_flip_channel(
    item_name: &str,
    price: u64,
    target: Option<u64>,
    profit: i64,
    buy_speed_ms: Option<u64>,
    finder: Option<&str>,
) {
    let event_type = if profit >= DIVINE_PROFIT_THRESHOLD as i64 {
        "divine_flip"
    } else {
        "legendary_flip"
    };
    let payload = serde_json::json!({
        "item_name": item_name,
        "price": price,
        "target": target,
        "profit": profit,
        "buy_speed_ms": buy_speed_ms,
        "finder": finder,
    });
    post_relay(event_type, payload).await;
}

/// Send a bazaar legendary/divine flip notification to the shared channel via
/// the configured relay endpoint.  Anonymized: no IGN, purse, or identifying info.
pub async fn send_webhook_bazaar_flip_channel(
    item_name: &str,
    amount: u64,
    price_per_unit: f64,
    profit: i64,
) {
    let event_type = if profit >= DIVINE_PROFIT_THRESHOLD as i64 {
        "divine_bazaar_flip"
    } else {
        "legendary_bazaar_flip"
    };
    let total = price_per_unit * amount as f64;
    let payload = serde_json::json!({
        "item_name": item_name,
        "amount": amount,
        "price_per_unit": price_per_unit,
        "total": total,
        "profit": profit,
    });
    post_relay(event_type, payload).await;
}

/// Build embed fields for purchase-style webhooks (purchase price, target, profit/ROI, buy speed, finder, auction link).
fn build_purchase_fields(
    price: u64,
    target: Option<u64>,
    profit: Option<i64>,
    buy_speed_ms: Option<u64>,
    finder: Option<&str>,
    auction_uuid: Option<&str>,
) -> Vec<serde_json::Value> {
    let mut fields = vec![
        serde_json::json!({
            "name": "💰 Purchase Price",
            "value": format!("```fix\n{} coins\n```", format_number(price as f64)),
            "inline": true
        }),
    ];
    if let Some(t) = target {
        fields.push(serde_json::json!({
            "name": "🎯 Target Price",
            "value": format!("```fix\n{} coins\n```", format_number(t as f64)),
            "inline": true
        }));
    }
    if let Some(p) = profit {
        let sign = if p >= 0 { "+" } else { "" };
        let roi_str = if let Some(t) = target {
            if t > 0 && price > 0 {
                format!(" ({:.1}%)", (p as f64 / price as f64) * 100.0)
            } else {
                String::new()
            }
        } else {
            String::new()
        };
        fields.push(serde_json::json!({
            "name": "📈 Expected Profit",
            "value": format!("```diff\n{}{} coins{}\n```", sign, format_number(p as f64), roi_str),
            "inline": true
        }));
    }
    if let Some(ms) = buy_speed_ms {
        fields.push(serde_json::json!({
            "name": "⚡ Buy Speed",
            "value": format!("```\n{}ms\n```", ms),
            "inline": true
        }));
    }
    if let Some(f) = finder {
        if !f.is_empty() {
            // Convert COFL snake_case finder names to readable form
            // e.g. "SNIPER_MEDIAN" → "Sniper Median"
            let readable = f
                .split('_')
                .map(|w| {
                    let mut c = w.chars();
                    match c.next() {
                        None => String::new(),
                        Some(first) => first.to_uppercase().collect::<String>() + &c.as_str().to_lowercase(),
                    }
                })
                .collect::<Vec<_>>()
                .join(" ");
            fields.push(serde_json::json!({
                "name": "🔍 Finder",
                "value": format!("```\n{}\n```", readable),
                "inline": true
            }));
        }
    }
    if let Some(uuid) = auction_uuid {
        if !uuid.is_empty() {
            fields.push(serde_json::json!({
                "name": "🔗 Auction Link",
                "value": format!("[View on Coflnet](https://sky.coflnet.com/auction/{}?refId=9KKPN9)", uuid),
                "inline": false
            }));
        }
    }
    fields
}

/// Parsed ban information extracted from the debug-formatted disconnect reason.
pub struct ParsedBan {
    pub is_permanent: bool,
    pub is_security_ban: bool,
    pub duration: Option<String>,
    pub reason: Option<String>,
    pub ban_id: Option<String>,
    pub appeal_url: Option<String>,
    pub clean_text: String,
}

/// Parse a ban disconnect reason string (Debug-formatted TextComponent) into structured fields.
pub fn parse_ban_reason(reason: &str) -> ParsedBan {
    // Extract all `text: "..."` values from the Debug-formatted TextComponent.
    // The Debug format uses ASCII-safe escaping (\n, \\, \", \u{xxxx}) for
    // non-printable / non-ASCII chars, so byte-level matching on the prefix
    // is safe. Content bytes within the quoted value are valid UTF-8 because
    // any non-ASCII is escaped by Rust's Debug trait.
    let mut texts: Vec<String> = Vec::new();
    let prefix = "text: \"";
    let mut search_start = 0;
    while let Some(pos) = reason[search_start..].find(prefix) {
        let content_start = search_start + pos + prefix.len();
        let mut s = String::new();
        let mut i = content_start;
        let bytes = reason.as_bytes();
        while i < bytes.len() && bytes[i] != b'"' {
            if bytes[i] == b'\\' && i + 1 < bytes.len() {
                match bytes[i + 1] {
                    b'n' => { s.push('\n'); i += 2; continue; }
                    b'"' => { s.push('"'); i += 2; continue; }
                    b'\\' => { s.push('\\'); i += 2; continue; }
                    _ => {}
                }
            }
            // Safe: Rust Debug format escapes non-ASCII to \u{xxxx}, so
            // unescaped bytes here are always valid single-byte ASCII chars.
            s.push(bytes[i] as char);
            i += 1;
        }
        if !s.is_empty() {
            texts.push(s);
        }
        search_start = if i < bytes.len() { i + 1 } else { bytes.len() };
    }

    let full_text = texts.join("");
    let lower = full_text.to_ascii_lowercase();
    let is_permanent = lower.contains("permanently banned");
    let is_security_ban = lower.contains("account has been blocked")
        || lower.contains("security-block")
        || lower.contains("block id:");

    // Extract duration (e.g. "29d 23h 59m 58s")
    let duration = texts.iter().find(|t| {
        let t = t.trim();
        !t.is_empty() && t.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false)
            && (t.contains('d') || t.contains('h') || t.contains('m') || t.contains('s'))
    }).map(|s| s.trim().to_string());

    // Extract ban reason
    let reason_text = {
        let mut found = false;
        let mut result = None;
        for t in &texts {
            if found {
                let trimmed = t.trim().trim_end_matches('\n');
                if !trimmed.is_empty() && !trimmed.starts_with("Find out more") && !trimmed.starts_with("Ban ID") {
                    result = Some(trimmed.to_string());
                }
                break;
            }
            if t.trim().starts_with("Reason:") || t.trim() == "Reason: " {
                found = true;
            }
        }
        result
    };

    // Extract ban ID (e.g. "#AF4CD6A8") or Block ID (security bans use "Block ID:")
    let ban_id = {
        let mut found = false;
        let mut result = None;
        for t in &texts {
            if found {
                let trimmed = t.trim().trim_end_matches('\n');
                if !trimmed.is_empty() {
                    result = Some(trimmed.to_string());
                }
                break;
            }
            let tt = t.trim();
            if tt.starts_with("Ban ID:") || tt == "Ban ID: "
                || tt.starts_with("Block ID:") || tt == "Block ID: "
            {
                found = true;
            }
        }
        result
    };

    // Extract appeal URL (regular bans use /appeal, security bans use /security-block)
    let appeal_url = texts.iter().find(|t| t.contains("hypixel.net/appeal") || t.contains("hypixel.net/security-block"))
        .map(|s| s.trim().trim_end_matches('\n').to_string());

    // Build clean text summary (no raw debug output)
    let clean_text = if !texts.is_empty() {
        full_text.trim().replace("\n\n\n", "\n\n").to_string()
    } else {
        // Fallback: if no text: fields were found, the reason might be plain text
        reason.to_string()
    };

    ParsedBan {
        is_permanent,
        is_security_ban,
        duration,
        reason: reason_text,
        ban_id,
        appeal_url,
        clean_text,
    }
}

/// Send a periodic profit summary embed.
/// Always uses the real IGN — this goes to the user's personal webhook.
pub async fn send_webhook_profit_summary(
    ingame_name: &str,
    ah_profit: i64,
    bz_profit: i64,
    uptime_secs: u64,
    webhook_url: &str,
) {
    let total = ah_profit + bz_profit;
    let hours = uptime_secs as f64 / 3600.0;
    let per_hour = if hours > 0.0 {
        total as f64 / hours
    } else {
        0.0
    };

    let payload = serde_json::json!({
        "embeds": [{
            "title": "📊 Profit Summary",
            "description": format!("<t:{}:R>", now_unix()),
            "color": 0x2ecc71u32,
            "fields": [
                {"name": "🏛️ Auction House Profit", "value": format!("```{}```", format_number(ah_profit as f64)), "inline": true},
                {"name": "📦 Bazaar Profit", "value": format!("```{}```", format_number(bz_profit as f64)), "inline": true},
                {"name": "💰 Total Profit", "value": format!("```{}```", format_number(total as f64)), "inline": false},
                {"name": "⏱️ Profit per Hour", "value": format!("```{}```", format_number(per_hour)), "inline": true}
            ],
            "footer": {
                "text": format!("BAF • {} • Uptime: {}", ingame_name, format_duration(uptime_secs))
            }
        }]
    });
    post_embed(webhook_url, payload).await;
}

/// Send a webhook when the bot takes a human-like rest break.
pub async fn send_webhook_rest_break_start(
    ingame_name: &str,
    break_duration_secs: u64,
    webhook_url: &str,
) {
    let payload = serde_json::json!({
        "embeds": [{
            "title": "😴 Rest Break",
            "description": format!(
                "Taking a human-like break for **{}**.\nWill reconnect <t:{}:R>.",
                format_duration(break_duration_secs),
                now_unix() + break_duration_secs,
            ),
            "color": 0xf39c12u32,
            "footer": {
                "text": format!("BAF • {}", ingame_name)
            }
        }]
    });
    post_embed(webhook_url, payload).await;
}

/// Send a webhook when the bot reconnects after a rest break.
pub async fn send_webhook_rest_break_end(
    ingame_name: &str,
    webhook_url: &str,
) {
    let payload = serde_json::json!({
        "embeds": [{
            "title": "☀️ Break Over",
            "description": "Reconnecting and resuming operations.",
            "color": 0x2ecc71u32,
            "footer": {
                "text": format!("BAF • {}", ingame_name)
            }
        }]
    });
    post_embed(webhook_url, payload).await;
}

#[cfg(test)]
mod tests {
    use super::parse_ban_reason;

    #[test]
    fn parse_temporary_ban_message() {
        let reason = r##"Some(Text(TextComponent { base: BaseComponent { siblings: [Text(TextComponent { base: BaseComponent { siblings: [], style: Style { color: Some(TextColor { value: 16733525, name: Some("red") }) } }, text: "You are temporarily banned for " }), Text(TextComponent { base: BaseComponent { siblings: [], style: Style { color: Some(TextColor { value: 16777215, name: Some("white") }) } }, text: "29d 23h 59m 58s" }), Text(TextComponent { base: BaseComponent { siblings: [], style: Style { color: Some(TextColor { value: 16733525, name: Some("red") }) } }, text: " from this server!\n\n" }), Text(TextComponent { base: BaseComponent { siblings: [], style: Style { color: Some(TextColor { value: 11184810, name: Some("gray") }) } }, text: "Reason: " }), Text(TextComponent { base: BaseComponent { siblings: [], style: Style { color: Some(TextColor { value: 16777215, name: Some("white") }) } }, text: "Cheating through the use of unfair game advantages.\n" }), Text(TextComponent { base: BaseComponent { siblings: [], style: Style { color: Some(TextColor { value: 11184810, name: Some("gray") }) } }, text: "Find out more: " }), Text(TextComponent { base: BaseComponent { siblings: [], style: Style { color: Some(TextColor { value: 5636095, name: Some("aqua") }) } }, text: "https://www.hypixel.net/appeal\n\n" }), Text(TextComponent { base: BaseComponent { siblings: [], style: Style { color: Some(TextColor { value: 11184810, name: Some("gray") }) } }, text: "Ban ID: " }), Text(TextComponent { base: BaseComponent { siblings: [], style: Style { color: Some(TextColor { value: 16777215, name: Some("white") }) } }, text: "#AF4CD6A8\n" }), Text(TextComponent { base: BaseComponent { siblings: [], style: Style { color: Some(TextColor { value: 11184810, name: Some("gray") }) } }, text: "Sharing your Ban ID may affect the processing of your appeal!" })], style: Style { color: None } }, text: "" }))"##;

        let parsed = parse_ban_reason(reason);
        assert!(!parsed.is_permanent);
        assert_eq!(parsed.duration.as_deref(), Some("29d 23h 59m 58s"));
        assert_eq!(parsed.reason.as_deref(), Some("Cheating through the use of unfair game advantages."));
        assert_eq!(parsed.ban_id.as_deref(), Some("#AF4CD6A8"));
        assert_eq!(parsed.appeal_url.as_deref(), Some("https://www.hypixel.net/appeal"));
    }

    #[test]
    fn parse_permanent_ban_message() {
        let reason = r#"Some(Text(TextComponent { base: BaseComponent { siblings: [Text(TextComponent { text: "You are permanently banned from this server!" })], style: Style {} }, text: "" }))"#;

        let parsed = parse_ban_reason(reason);
        assert!(parsed.is_permanent);
    }

    #[test]
    fn parse_plain_text_ban_fallback() {
        let reason = "You are temporarily banned for 5d from this server!";
        let parsed = parse_ban_reason(reason);
        // Falls back to raw text since there are no `text: "..."` fields
        assert_eq!(parsed.clean_text, reason);
    }

    #[test]
    fn parse_security_ban_message() {
        // Simulated security ban disconnect reason with "account has been blocked" and Block ID
        let reason = r##"Some(Text(TextComponent { base: BaseComponent { siblings: [Text(TextComponent { text: "Your account has been blocked." }), Text(TextComponent { text: "\n\nReason: " }), Text(TextComponent { text: "Suspicious activity has been detected on your account.\n" }), Text(TextComponent { text: "Find out more: " }), Text(TextComponent { text: "https://www.hypixel.net/security-block\n\n" }), Text(TextComponent { text: "Block ID: " }), Text(TextComponent { text: "#ABC12345\n" }), Text(TextComponent { text: "Sharing your Block ID may affect the processing of your appeal!" })], style: Style {} }, text: "" }))"##;

        let parsed = parse_ban_reason(reason);
        assert!(parsed.is_security_ban);
        assert!(!parsed.is_permanent);
        assert_eq!(parsed.reason.as_deref(), Some("Suspicious activity has been detected on your account."));
        assert_eq!(parsed.ban_id.as_deref(), Some("#ABC12345"));
        assert_eq!(parsed.appeal_url.as_deref(), Some("https://www.hypixel.net/security-block"));
    }

    #[test]
    fn regular_ban_is_not_security_ban() {
        let reason = r##"Some(Text(TextComponent { base: BaseComponent { siblings: [Text(TextComponent { text: "You are temporarily banned for " }), Text(TextComponent { text: "29d 23h 59m 58s" }), Text(TextComponent { text: " from this server!\n\n" }), Text(TextComponent { text: "Reason: " }), Text(TextComponent { text: "Cheating through the use of unfair game advantages.\n" }), Text(TextComponent { text: "Ban ID: " }), Text(TextComponent { text: "#AF4CD6A8\n" })], style: Style {} }, text: "" }))"##;

        let parsed = parse_ban_reason(reason);
        assert!(!parsed.is_security_ban);
    }
}
