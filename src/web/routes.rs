use axum::{
    extract::State,
    http::StatusCode,
    response::{Html, Json},
};
use serde::Deserialize;
use std::sync::Arc;
use tracing::{error, info};

use super::state::WebState;

/// Serve the single-page dashboard.
pub async fn index() -> Html<&'static str> {
    Html(include_str!("static/index.html"))
}

/// `GET /api/status` — core bot telemetry for the dashboard header cards.
pub async fn status(State(state): State<Arc<WebState>>) -> Json<serde_json::Value> {
    let uptime_secs = state.start_time.elapsed().as_secs();
    let bot_state = state.bot_client.state();
    let purse = state.bot_client.get_purse();
    let scoreboard = state.bot_client.get_scoreboard_lines();
    let queue_depth = state.command_queue.len();

    Json(serde_json::json!({
        "state": format!("{:?}", bot_state),
        "allowsCommands": bot_state.allows_commands(),
        "purse": purse,
        "queueDepth": queue_depth,
        "scoreboard": scoreboard,
        "uptimeSecs": uptime_secs,
        "player": state.ingame_name,
        "ahFlips": state.config.enable_ah_flips,
        "bazaarFlips": state.config.enable_bazaar_flips,
    }))
}

/// `GET /api/inventory` — cached player inventory JSON.
pub async fn inventory(State(state): State<Arc<WebState>>) -> Json<serde_json::Value> {
    let inv = state
        .bot_client
        .get_cached_inventory_json()
        .unwrap_or_else(|| "null".to_string());
    // inv is already a JSON string from the bot — parse it so we don't double-encode.
    let parsed: serde_json::Value =
        serde_json::from_str(&inv).unwrap_or(serde_json::Value::Null);
    Json(serde_json::json!({ "inventory": parsed }))
}

/// `GET /api/events` — return the recent event log snapshot.
pub async fn events(State(state): State<Arc<WebState>>) -> Json<serde_json::Value> {
    let events = state.event_log.snapshot();
    Json(serde_json::json!({ "events": events }))
}

/// Body for `POST /api/command`.
#[derive(Deserialize)]
pub struct CommandBody {
    pub command: String,
}

/// `POST /api/command` — send a command (same logic as the console handler).
pub async fn send_command(
    State(state): State<Arc<WebState>>,
    Json(body): Json<CommandBody>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let input = body.command.trim().to_string();
    if input.is_empty() {
        return Ok(Json(serde_json::json!({ "ok": false, "error": "empty command" })));
    }

    let lowercase = input.to_lowercase();

    if lowercase.starts_with("/cofl") || lowercase.starts_with("/baf") {
        let parts: Vec<&str> = input.split_whitespace().collect();
        if parts.len() > 1 {
            let command = parts[1].to_string();
            let args = parts[2..].join(" ");
            let data_json =
                serde_json::to_string(&args).unwrap_or_else(|_| "\"\"".to_string());
            let message = serde_json::json!({
                "type": command,
                "data": data_json
            })
            .to_string();
            if let Err(e) = state.ws_client.send_message(&message).await {
                error!("Web GUI: failed to send /cofl command: {}", e);
                return Ok(Json(
                    serde_json::json!({ "ok": false, "error": format!("{}", e) }),
                ));
            }
            info!("Web GUI sent /cofl {} {}", command, args);
        }
    } else if input.starts_with('/') {
        state.command_queue.enqueue(
            crate::types::CommandType::SendChat {
                message: input.clone(),
            },
            crate::types::CommandPriority::High,
            false,
        );
        info!("Web GUI queued Minecraft command: {}", input);
    } else {
        // Non-slash → send as COFL chat
        let data_json =
            serde_json::to_string(&input).unwrap_or_else(|_| "\"\"".to_string());
        let message = serde_json::json!({
            "type": "chat",
            "data": data_json
        })
        .to_string();
        if let Err(e) = state.ws_client.send_message(&message).await {
            error!("Web GUI: failed to send chat: {}", e);
            return Ok(Json(
                serde_json::json!({ "ok": false, "error": format!("{}", e) }),
            ));
        }
    }

    state.event_log.push("command", format!("[Web] {}", input));
    Ok(Json(serde_json::json!({ "ok": true })))
}
