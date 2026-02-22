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

pub async fn send_webhook_initialized(
    ingame_name: &str,
    ah_enabled: bool,
    bazaar_enabled: bool,
    webhook_url: &str,
) {
    let payload = serde_json::json!({
        "embeds": [{
            "title": "✓ Started BAF",
            "color": 0x00ff00,
            "fields": [
                {"name": "Player", "value": ingame_name, "inline": true},
                {"name": "AH Flips", "value": if ah_enabled { "✓" } else { "✗" }, "inline": true},
                {"name": "Bazaar Flips", "value": if bazaar_enabled { "✓" } else { "✗" }, "inline": true},
            ]
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
    let payload = serde_json::json!({
        "embeds": [{
            "title": "🚀 Startup Workflow Complete",
            "color": 0x0099ff,
            "fields": [
                {"name": "Player", "value": ingame_name, "inline": true},
                {"name": "Orders Found", "value": orders_found.to_string(), "inline": true},
                {"name": "AH Flips", "value": if ah_enabled { "✓" } else { "✗" }, "inline": true},
                {"name": "Bazaar Flips", "value": if bazaar_enabled { "✓" } else { "✗" }, "inline": true},
            ]
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
        serde_json::json!({"name": "Player", "value": ingame_name, "inline": true}),
        serde_json::json!({"name": "Item", "value": item_name, "inline": true}),
        serde_json::json!({"name": "Price", "value": format!("{} coins", price), "inline": true}),
    ];
    if let Some(t) = target {
        fields.push(serde_json::json!({"name": "Target", "value": format!("{} coins", t), "inline": true}));
    }
    if let Some(p) = profit {
        fields.push(serde_json::json!({"name": "Profit", "value": format!("{} coins", p), "inline": true}));
    }
    let payload = serde_json::json!({
        "embeds": [{
            "title": "🛒 Item Purchased",
            "color": 0xffaa00,
            "fields": fields
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
    let payload = serde_json::json!({
        "embeds": [{
            "title": "✅ Item Sold",
            "color": 0x00ff99,
            "fields": [
                {"name": "Player", "value": ingame_name, "inline": true},
                {"name": "Item", "value": item_name, "inline": true},
                {"name": "Price", "value": format!("{} coins", price), "inline": true},
                {"name": "Buyer", "value": buyer, "inline": true},
            ]
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
    let color: u32 = if is_buy_order { 0x55ff55 } else { 0xff5555 };
    let payload = serde_json::json!({
        "embeds": [{
            "title": format!("📦 Bazaar {} Placed", order_type),
            "color": color,
            "fields": [
                {"name": "Player",      "value": ingame_name,                          "inline": true},
                {"name": "Item",        "value": item_name,                            "inline": true},
                {"name": "Type",        "value": order_type,                           "inline": true},
                {"name": "Amount",      "value": format!("{}x", amount),               "inline": true},
                {"name": "Price/unit",  "value": format!("{:.1} coins", price_per_unit),"inline": true},
                {"name": "Total",       "value": format!("{:.1} coins", total_price),   "inline": true},
            ]
        }]
    });
    post_embed(webhook_url, payload).await;
}
