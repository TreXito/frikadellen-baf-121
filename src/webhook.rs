use tracing::warn;

async fn post_embed(webhook_url: &str, payload: serde_json::Value) {
    let client = match reqwest::Client::builder().build() {
        Ok(c) => c,
        Err(e) => {
            warn!("[Webhook] Failed to build HTTP client: {}", e);
            return;
        }
    };
    if let Err(e) = client.post(webhook_url).json(&payload).send().await {
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
