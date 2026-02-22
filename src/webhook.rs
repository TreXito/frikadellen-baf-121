use once_cell::sync::Lazy;
use tracing::warn;

// Shared HTTP client - reqwest clients are designed to be cloned/reused
static HTTP_CLIENT: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .build()
        .expect("Failed to build reqwest client")
});

async fn post_embed(webhook_url: &str, payload: serde_json::Value) {
    if let Err(e) = HTTP_CLIENT.post(webhook_url).json(&payload).send().await {
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

/// Sanitize an item name for use as an icon URL path component
fn sanitize_item_name(name: &str) -> String {
    name.replace(|c: char| !c.is_alphanumeric() && c != '_', "_")
}

/// Unix timestamp seconds for Discord relative timestamps
fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub async fn send_webhook_initialized(
    ingame_name: &str,
    ah_enabled: bool,
    bazaar_enabled: bool,
    webhook_url: &str,
) {
    let description = format!(
        "AH Flips: {} | Bazaar Flips: {}\n<t:{}:R>",
        if ah_enabled { "✅" } else { "❌" },
        if bazaar_enabled { "✅" } else { "❌" },
        now_unix()
    );
    let payload = serde_json::json!({
        "embeds": [{
            "title": "✓ Started BAF",
            "description": description,
            "color": 0x00ff88,
            "footer": {
                "text": format!("BAF - {}", ingame_name),
                "icon_url": format!("https://mc-heads.net/avatar/{}/32.png", ingame_name)
            }
        }]
    });
    post_embed(webhook_url, payload).await;
}

pub async fn send_webhook_startup_complete(
    ingame_name: &str,
    orders_found: u64,
    ah_enabled: bool,
    bazaar_enabled: bool,
    webhook_url: &str,
) {
    let description = format!(
        "Ready to accept flips!\n\nAH Flips: {}\nBazaar Flips: {}",
        if ah_enabled { "✅ Enabled" } else { "❌ Disabled" },
        if bazaar_enabled { "✅ Enabled" } else { "❌ Disabled" }
    );
    let payload = serde_json::json!({
        "embeds": [{
            "title": "🚀 Startup Workflow Complete",
            "description": description,
            "color": 0x2ecc71,
            "fields": [
                {"name": "1️⃣ Cookie Check", "value": "```✓ Complete```", "inline": true},
                {
                    "name": "2️⃣ Order Discovery",
                    "value": if bazaar_enabled {
                        format!("```✓ Found {} order(s)```", orders_found)
                    } else {
                        "```- Skipped (Bazaar disabled)```".to_string()
                    },
                    "inline": true
                },
                {"name": "3️⃣ Claim Items", "value": "```✓ Complete```", "inline": true},
            ],
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
    webhook_url: &str,
) {
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
        fields.push(serde_json::json!({
            "name": "📈 Expected Profit",
            "value": format!("```diff\n{}{} coins\n```", sign, format_number(p as f64)),
            "inline": true
        }));
    }
    let safe_item = sanitize_item_name(item_name);
    let payload = serde_json::json!({
        "embeds": [{
            "title": "🛒 Item Purchased Successfully",
            "description": format!("**{}** • <t:{}:R>", item_name, now_unix()),
            "color": 0x00ff00,
            "fields": fields,
            "thumbnail": {"url": format!("https://sky.coflnet.com/static/icon/{}", safe_item)},
            "footer": {
                "text": format!("BAF • {}", ingame_name),
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
    webhook_url: &str,
) {
    let safe_item = sanitize_item_name(item_name);
    let payload = serde_json::json!({
        "embeds": [{
            "title": "✅ Item Sold",
            "description": format!("**{}** • <t:{}:R>", item_name, now_unix()),
            "color": 0x0099ff,
            "fields": [
                {"name": "👤 Buyer", "value": format!("```\n{}\n```", buyer), "inline": true},
                {"name": "💵 Sale Price", "value": format!("```fix\n{} coins\n```", format_number(price as f64)), "inline": true},
            ],
            "thumbnail": {"url": format!("https://sky.coflnet.com/static/icon/{}", safe_item)},
            "footer": {
                "text": format!("BAF • {}", ingame_name),
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
                {"name": "📊 Order Type",   "value": format!("```\n{}\n```", order_type),                     "inline": false},
            ],
            "thumbnail": {"url": format!("https://sky.coflnet.com/static/icon/{}", safe_item)},
            "footer": {
                "text": format!("BAF • {}", ingame_name),
                "icon_url": format!("https://mc-heads.net/avatar/{}/32.png", ingame_name)
            }
        }]
    });
    post_embed(webhook_url, payload).await;
}
