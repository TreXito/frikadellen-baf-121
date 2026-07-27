use std::collections::HashSet;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

use axum::{
    extract::{
        ws::{Message, WebSocket},
        Request, State, WebSocketUpgrade,
    },
    http::StatusCode,
    middleware::Next,
    response::{Html, IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};

use crate::bot::BotClient;
use crate::bazaar_tracker::BazaarOrderTracker;
use crate::logging::print_mc_chat;
use crate::state::CommandQueue;
use crate::types::{CommandPriority, CommandType};
use crate::websocket::CoflWebSocket;

// ── Shared state passed to every handler ─────────────────────

/// Holds references to all bot state that the web UI needs.
#[derive(Clone)]
pub struct WebSharedState {
    pub bot_client: BotClient,
    pub command_queue: CommandQueue,
    pub ws_client: CoflWebSocket,
    pub bazaar_flips_paused: Arc<AtomicBool>,
    /// Master macro pause — when true the command-processor loop skips work.
    pub macro_paused: Arc<AtomicBool>,
    pub enable_ah_flips: Arc<AtomicBool>,
    pub enable_bazaar_flips: Arc<AtomicBool>,
    /// Transient pause flag set by the Disconnect button.  While `true`, the
    /// COFL WS event loop in `main.rs` drops incoming AH/Bazaar flips instead
    /// of queueing them.  This is intentionally separate from the config
    /// `enable_*_flips` atomics (which represent the user's persistent config
    /// preference and are expected to stay `true`).  Cleared by the Connect
    /// button and reset by a full process restart.
    pub flip_intake_paused: Arc<AtomicBool>,
    /// Account names from config (may be single or multi).
    pub ingame_names: Vec<String>,
    pub current_account_index: usize,
    pub account_index_path: std::path::PathBuf,
    /// Broadcast channel for chat messages flowing to web clients.
    pub chat_tx: broadcast::Sender<String>,
    /// Password required to access the web panel (`None` = no auth).
    pub web_gui_password: Option<String>,
    /// PEM certificate (full chain) to present for the panel. `None` = use the
    /// self-signed certificate the bot issues itself.
    pub web_tls_cert_path: Option<String>,
    /// PEM private key matching `web_tls_cert_path`.
    pub web_tls_key_path: Option<String>,
    /// Set of valid session tokens for authenticated clients.
    pub valid_sessions: Arc<Mutex<HashSet<String>>>,
    /// Cached Minecraft UUID for the current account (dashes format).
    /// Resolved lazily from the Mojang API on first `/api/auctions` request.
    pub player_uuid: Arc<tokio::sync::RwLock<Option<String>>>,
    /// Timestamp when the bot process started (for uptime tracking).
    pub started_at: std::time::Instant,
    /// Accumulated running time from previous sessions (seconds).
    /// Added to `started_at.elapsed()` to get total uptime across restarts.
    pub previous_session_secs: u64,
    /// Hypixel API key for fetching active auctions (optional).
    pub hypixel_api_key: Option<String>,
    /// Auto-detected COFL license index for the current IGN (0 = none detected).
    pub detected_cofl_license: Arc<std::sync::atomic::AtomicU32>,
    /// Shared profit tracker for AH and Bazaar realized profits.
    pub profit_tracker: Arc<crate::profit::ProfitTracker>,
    /// Session-only anonymize toggle for the web panel (defaults to OFF).
    /// Not persisted to config — resets to OFF on each process start.
    pub anonymize_webhook_name: Arc<AtomicBool>,
    /// Tracks active bazaar orders for the web panel and profit calculation.
    pub bazaar_tracker: Arc<BazaarOrderTracker>,
    /// Config loader for persisting changes to config.toml.
    pub config_loader: Arc<crate::config::ConfigLoader>,
    /// Flip-intake diagnostics — surfaces why incoming flips are being dropped.
    pub flip_diag: Arc<crate::state::FlipDiagnostics>,
}

// ── JSON payloads ────────────────────────────────────────────

#[derive(Serialize)]
struct StatusResponse {
    state: String,
    macro_paused: bool,
    enable_ah_flips: bool,
    enable_bazaar_flips: bool,
    anonymize_webhook_name: bool,
    queue_depth: usize,
    current_account: String,
    current_account_index: usize,
    accounts: Vec<String>,
    purse: Option<u64>,
    uptime_seconds: u64,
    bazaar_at_limit: bool,
    auction_at_limit: bool,
    inventory_full: bool,
    /// Flips queued for purchase this session (intake health).
    flips_accepted: u64,
    /// Flips dropped this session across all reasons.
    flips_dropped: u64,
    /// Human-readable reason the most recent flip was dropped, if any.
    flip_drop_reason: Option<String>,
}

#[derive(Deserialize)]
struct ChatMessage {
    message: String,
}

#[derive(Deserialize)]
struct TogglePayload {
    enabled: bool,
}

#[derive(Deserialize)]
struct SwitchPayload {
    index: usize,
}

#[derive(Deserialize)]
struct CancelAuctionPayload {
    item_name: String,
    starting_bid: i64,
}

#[derive(Deserialize)]
struct CancelBzOrderPayload {
    item_name: String,
    is_buy_order: bool,
}

#[derive(Deserialize)]
struct ListItemPayload {
    /// Display name of the item (for logging / confirmation message).
    item_name: String,
    /// Mineflayer inventory slot index (9–44).
    item_slot: u64,
    /// Desired BIN price in coins.
    starting_bid: u64,
    /// Auction duration in hours (1–168).
    #[serde(default = "default_auction_duration")]
    duration_hours: u64,
}

#[derive(Deserialize)]
struct LoginPayload {
    password: String,
}

#[derive(Serialize)]
struct LoginResponse {
    success: bool,
}

#[derive(Serialize)]
struct ProfitResponse {
    ah_points: Vec<(u64, i64)>,
    bz_points: Vec<(u64, i64)>,
    ah_total: i64,
    bz_total: i64,
    uptime_seconds: u64,
}

/// Default auction duration used when the client doesn't provide one.
fn default_auction_duration() -> u64 { 24 }

/// Public (unauthenticated) profit summary — no IGN, no account info.
/// Used by the login page and OpenGraph embeds.
#[derive(Serialize)]
struct PublicProfitResponse {
    ah_total: i64,
    bz_total: i64,
    total: i64,
    per_hour: f64,
    uptime_seconds: u64,
    ah_points: Vec<(u64, i64)>,
    bz_points: Vec<(u64, i64)>,
}

#[derive(Serialize)]
struct AuctionEntry {
    uuid: String,
    item_name: String,
    /// SkyBlock item tag for icon lookup (e.g. "MITHRIL_DRILL_2")
    tag: Option<String>,
    highest_bid: i64,
    starting_bid: i64,
    bin: bool,
    /// ISO 8601 end timestamp
    end: String,
    /// Seconds remaining until the auction expires (negative = expired).
    /// `None` when the time is genuinely UNKNOWN — a listing still inside its
    /// grace period shows no "Ends in:" line yet. Reporting that as `0` made the
    /// panel label brand-new listings "Expired".
    time_remaining_seconds: Option<i64>,
    /// Seconds until a freshly listed auction leaves Hypixel's ~20s grace period
    /// and becomes buyable. `None` once it is buyable (the normal case).
    #[serde(skip_serializing_if = "Option::is_none")]
    buyable_in_seconds: Option<i64>,
    /// Lore lines from the in-game item tooltip (only present for GUI-sourced entries)
    #[serde(skip_serializing_if = "Option::is_none")]
    lore: Option<Vec<String>>,
}

// ── Authentication middleware ─────────────────────────────────

/// Extract the `baf_session` cookie value from a request.
fn extract_session_cookie(req: &Request) -> Option<String> {
    req.headers()
        .get("cookie")?
        .to_str()
        .ok()?
        .split(';')
        .find_map(|c| {
            let c = c.trim();
            c.strip_prefix("baf_session=").map(|v| v.to_string())
        })
}

/// Paths served without a session: the panel shell itself (which is just the
/// login form until you authenticate) and the two endpoints link previews fetch.
fn is_public_path(path: &str) -> bool {
    matches!(
        path,
        "/" | "/api/login" | "/api/profit/public" | "/api/og-image.png"
    )
}

/// The authorization decision, separated from axum so it can be tested directly.
///
/// `password_set` is passed rather than the password itself because the request
/// never carries one — only a session token minted by `/api/login`.
fn request_is_authorized(
    password_set: bool,
    sessions: &HashSet<String>,
    path: &str,
    presented: &[String],
) -> bool {
    if !password_set || is_public_path(path) {
        return true;
    }
    presented.iter().any(|t| sessions.contains(t))
}

/// Every session token a request presents: cookie, bearer header, or `?token=`
/// (the last one exists because browsers cannot set headers on a WebSocket).
fn presented_tokens(req: &Request) -> Vec<String> {
    let mut tokens: Vec<String> = Vec::new();

    if let Some(token) = extract_session_cookie(req) {
        tokens.push(token);
    }

    if let Some(auth) = req.headers().get("authorization") {
        if let Ok(auth_str) = auth.to_str() {
            if let Some(token) = auth_str.strip_prefix("Bearer ") {
                tokens.push(token.to_string());
            }
        }
    }

    if let Some(query) = req.uri().query() {
        for pair in query.split('&') {
            if let Some(token) = pair.strip_prefix("token=") {
                tokens.push(token.to_string());
            }
        }
    }

    tokens
}

/// Middleware logic that enforces authentication when a password is configured.
/// Allows unauthenticated access to `GET /` (panel HTML) and `POST /api/login`.
async fn check_auth(
    s: WebSharedState,
    req: Request,
    next: Next,
) -> Response {
    let password_set = s.web_gui_password.as_deref().is_some_and(|p| !p.is_empty());
    let path = req.uri().path().to_string();
    let presented = presented_tokens(&req);

    // Lock and release before awaiting the inner service.
    let allowed = {
        let sessions = s.valid_sessions.lock().unwrap();
        request_is_authorized(password_set, &sessions, &path, &presented)
    };

    if allowed {
        return next.run(req).await;
    }

    StatusCode::UNAUTHORIZED.into_response()
}

// ── Start the web server ─────────────────────────────────────

/// Whether the panel is currently served over TLS. Read by the login handler so
/// the session cookie is marked `Secure` exactly when that will not lock the
/// user out (a `Secure` cookie is dropped by the browser on plain HTTP).
static WEB_TLS_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Escape hatch for people who terminate TLS in front of the bot (nginx, Caddy,
/// a Cloudflare tunnel). Everyone else gets HTTPS with no configuration at all.
fn plain_http_requested() -> bool {
    std::env::var("BAF_WEB_PLAIN_HTTP")
        .map(|v| matches!(v.trim(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

/// Best-effort local address this machine uses to reach the internet.
///
/// Connecting a UDP socket sends no packets — it only asks the routing table
/// which interface would be used — so this is instant and works offline. On a
/// VPS with a public IP bound directly to the NIC this is the address users
/// actually type, so putting it in the certificate keeps the browser's warning
/// down to "unknown issuer" instead of also "wrong host".
fn primary_local_ip() -> Option<std::net::IpAddr> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("1.1.1.1:80").ok()?;
    sock.local_addr().ok().map(|a| a.ip()).filter(|ip| !ip.is_loopback())
}

/// Return the panel's certificate and key, generating them on first use.
///
/// The certificate is persisted and reused across restarts on purpose: the
/// browser then only warns once, and a certificate that changes every boot is
/// indistinguishable from someone swapping it out mid-session.
fn ensure_panel_cert(dir: &std::path::Path) -> anyhow::Result<(std::path::PathBuf, std::path::PathBuf)> {
    use anyhow::Context;
    let _ = std::fs::create_dir_all(dir);
    let cert_file = dir.join("web-cert.pem");
    let key_file = dir.join("web-key.pem");
    if cert_file.exists() && key_file.exists() {
        return Ok((cert_file, key_file));
    }
    let mut sans = vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        "::1".to_string(),
    ];
    if let Some(ip) = primary_local_ip() {
        sans.push(ip.to_string());
    }
    info!("[WebTLS] Generating panel certificate for {}", sans.join(", "));
    let signed = rcgen::generate_simple_self_signed(sans)
        .context("failed to generate self-signed certificate")?;
    std::fs::write(&cert_file, signed.cert.pem()).context("write panel cert")?;
    // The key authenticates the panel; on a shared box it must not be readable
    // by other accounts.
    write_private_key(&key_file, &signed.key_pair.serialize_pem()).context("write panel key")?;
    Ok((cert_file, key_file))
}

/// Write a private key with owner-only permissions where the platform has them.
fn write_private_key(path: &std::path::Path, pem: &str) -> std::io::Result<()> {
    std::fs::write(path, pem)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// Which certificate the panel should present.
///
/// The old `web_https` flag is gone for good — TLS is unconditional, and a
/// switch that could turn it off was the real footgun. But removing the cert
/// PATHS along with it meant a user who had installed a real certificate got it
/// silently ignored, saw "rcgen self signed cert" in the browser, and had
/// nothing in the log pointing at why. Choosing a certificate is not the same
/// decision as choosing whether to encrypt.
#[derive(Debug, PartialEq)]
enum PanelCert {
    /// Both paths configured — present the user's certificate.
    Configured { cert: String, key: String },
    /// Nothing configured: keep issuing our own.
    SelfSigned,
    /// Exactly one of the two paths set, which cannot work.
    Incomplete { have: &'static str, missing: &'static str },
}

/// How often to check whether the certificate on disk has been replaced.
const CERT_RELOAD_POLL: std::time::Duration = std::time::Duration::from_secs(60);

/// Watch a configured certificate and swap it in when it changes on disk.
///
/// Let's Encrypt only issues IP-address certificates under the mandatory
/// `shortlived` profile — about 160 hours — so an IP certificate is REPLACED
/// every few days. TLS is otherwise loaded once at startup, which would leave
/// the panel serving an expired certificate from the first renewal until the
/// whole bot was restarted. Restarting a flip bot to pick up a certificate is a
/// real cost (lost session, lost uptime), so the renewal is picked up in place.
///
/// A failed reload keeps the certificate already in memory: a half-written file
/// (the renewal is not atomic across two files) must not take the panel down —
/// the next poll picks it up once the writer has finished.
fn spawn_cert_reloader(
    config: axum_server::tls_rustls::RustlsConfig,
    cert: std::path::PathBuf,
    key: std::path::PathBuf,
) {
    let stamp = |p: &std::path::Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    tokio::spawn(async move {
        let mut seen = (stamp(&cert), stamp(&key));
        loop {
            tokio::time::sleep(CERT_RELOAD_POLL).await;
            let now = (stamp(&cert), stamp(&key));
            if now == seen || now.0.is_none() {
                continue;
            }
            seen = now;
            match config.reload_from_pem_file(&cert, &key).await {
                Ok(()) => info!(
                    "[WebTLS] Certificate changed on disk — reloaded {} without restarting",
                    cert.display()
                ),
                Err(e) => warn!(
                    "[WebTLS] Certificate at {} changed but could not be reloaded (still serving the \
                     previous one, will retry): {e}",
                    cert.display()
                ),
            }
        }
    });
}

/// Decide from the configured paths, without touching the filesystem.
fn choose_panel_cert(cert_path: Option<&str>, key_path: Option<&str>) -> PanelCert {
    let cert = cert_path.map(str::trim).filter(|s| !s.is_empty());
    let key = key_path.map(str::trim).filter(|s| !s.is_empty());
    match (cert, key) {
        (Some(c), Some(k)) => PanelCert::Configured { cert: c.to_string(), key: k.to_string() },
        (None, None) => PanelCert::SelfSigned,
        (Some(_), None) => PanelCert::Incomplete { have: "web_tls_cert_path", missing: "web_tls_key_path" },
        (None, Some(_)) => PanelCert::Incomplete { have: "web_tls_key_path", missing: "web_tls_cert_path" },
    }
}

/// Load the panel's certificate: the configured one when there is one, and the
/// bot's own self-signed certificate otherwise.
///
/// A configured certificate that fails to load falls back to self-signed so the
/// panel still comes up — locking someone out of their own bot over a bad path
/// is worse than a browser warning — but it says so LOUDLY. Falling back in
/// silence is exactly what made this look like "TLS certs don't work".
async fn build_web_tls(
    cert_path: Option<&str>,
    key_path: Option<&str>,
) -> anyhow::Result<axum_server::tls_rustls::RustlsConfig> {
    use anyhow::Context;
    // Ensure a process-level crypto provider is installed (no-op if another
    // component already installed one).
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    match choose_panel_cert(cert_path, key_path) {
        PanelCert::Configured { cert, key } => {
            match axum_server::tls_rustls::RustlsConfig::from_pem_file(&cert, &key).await {
                Ok(config) => {
                    info!("[WebTLS] Using the configured certificate: {} (key {})", cert, key);
                    // Renewals replace this file; pick them up without a restart.
                    spawn_cert_reloader(config.clone(), cert.into(), key.into());
                    return Ok(config);
                }
                Err(e) => {
                    error!(
                        "[WebTLS] Could NOT load the certificate configured in web_tls_cert_path — \
                         falling back to the bot's self-signed one, so the browser will keep warning. \
                         cert={cert} key={key} error={e}"
                    );
                    error!(
                        "[WebTLS] Check that both files exist, are readable by this process, and are PEM \
                         (the cert should be the FULL chain, e.g. fullchain.pem, and the key the matching \
                         private key, e.g. privkey.pem)."
                    );
                }
            }
        }
        PanelCert::Incomplete { have, missing } => {
            error!(
                "[WebTLS] {have} is set but {missing} is empty — a certificate needs BOTH. \
                 Using the bot's self-signed certificate instead."
            );
        }
        PanelCert::SelfSigned => {}
    }

    let (cert_file, key_file) = ensure_panel_cert(&crate::logging::get_logs_dir())?;
    info!(
        "[WebTLS] Using the bot's own self-signed certificate ({}). Browsers will show a warning; \
         set web_tls_cert_path and web_tls_key_path to a real certificate to remove it.",
        cert_file.display()
    );
    axum_server::tls_rustls::RustlsConfig::from_pem_file(&cert_file, &key_file)
        .await
        .context("failed to load panel certificate")
}

pub async fn start_web_server(state: WebSharedState, port: u16) {
    let use_tls = !plain_http_requested();
    WEB_TLS_ACTIVE.store(use_tls, Ordering::Relaxed);

    // Taken before `state` is moved into the router below.
    let tls_cert_path = state.web_tls_cert_path.clone();
    let tls_key_path = state.web_tls_key_path.clone();

    let has_password = state
        .web_gui_password
        .as_ref()
        .map(|p| !p.is_empty())
        .unwrap_or(false);

    let auth_state = state.clone();
    let app = Router::new()
        .route("/", get(index_page))
        .route("/api/login", axum::routing::post(login))
        .route("/api/profit/public", get(get_profit_public))
        .route("/api/og-image.png", get(get_og_image))
        .route("/api/status", get(get_status))
        .route("/api/pause", get(pause_macro).post(pause_macro))
        .route("/api/resume", get(resume_macro).post(resume_macro))
        .route("/api/inventory", get(get_inventory))
        .route("/api/game-view", get(get_game_view))
        .route("/api/toggle_ah", axum::routing::post(toggle_ah))
        .route("/api/toggle_bazaar", axum::routing::post(toggle_bazaar))
        .route("/api/toggle_anonymize", axum::routing::post(toggle_anonymize))
        .route("/api/chat/send", axum::routing::post(send_chat))
        .route("/api/chat/ws", get(chat_ws_handler))
        .route("/api/switch_account", axum::routing::post(switch_account))
        .route("/api/cancel_auction", axum::routing::post(cancel_auction))
        .route("/api/list_item", axum::routing::post(list_item))
        .route("/api/claim_purchases", axum::routing::post(claim_purchases))
        .route("/api/collect_bz_orders", axum::routing::post(collect_bz_orders))
        .route("/api/claim_bz_orders", axum::routing::post(claim_bz_orders))
        .route("/api/cancel_bz_order", axum::routing::post(cancel_bz_order))
        .route("/api/cancel_all_bz_orders", axum::routing::post(cancel_all_bz_orders))
        .route("/api/auctions", get(get_auctions))
        .route("/api/bazaar_orders", get(get_bazaar_orders))
        .route("/api/queue", get(get_queue_status))
        .route("/api/config", get(get_config).post(save_config))
        .route("/api/config.json", get(get_config_json).post(save_config_json))
        .route("/api/logs/latest", get(download_latest_log))
        .route("/api/profit", get(get_profit))
        .route("/api/kill_session", axum::routing::post(kill_session))
        .route("/api/disconnect", axum::routing::post(disconnect_session))
        .route("/api/connect", axum::routing::post(connect_session))
        .route("/api/restart", axum::routing::post(restart_session))
        .route("/api/update", axum::routing::post(update_session))
        .layer(axum::middleware::from_fn(move |req: Request, next: Next| {
            let s = auth_state.clone();
            async move { check_auth(s, req, next).await }
        }))
        .with_state(state);

    let addr = format!("0.0.0.0:{}", port);
    let scheme = if use_tls { "https" } else { "http" };
    if has_password {
        info!("Web control panel starting on {}://{} (password protected)", scheme, addr);
    } else {
        // Unreachable in practice: the config loader generates a password when
        // one is missing. Kept loud in case the panel is ever started directly.
        warn!(
            "Web control panel starting on {}://{} WITHOUT A PASSWORD — anyone who can reach this port controls the bot",
            scheme, addr
        );
    }
    if !use_tls {
        warn!("[WebTLS] BAF_WEB_PLAIN_HTTP is set — the panel password will be sent unencrypted");
    }

    if use_tls {
        let socket: std::net::SocketAddr = match addr.parse() {
            Ok(s) => s,
            Err(e) => {
                error!("Invalid web server address {}: {}", addr, e);
                return;
            }
        };
        let tls_config = match build_web_tls(tls_cert_path.as_deref(), tls_key_path.as_deref()).await {
            Ok(c) => c,
            Err(e) => {
                error!("Failed to set up web TLS (panel will not start): {:#}", e);
                return;
            }
        };
        if let Err(e) = axum_server::bind_rustls(socket, tls_config)
            .serve(app.into_make_service())
            .await
        {
            error!("Web server (https) error: {}", e);
        }
    } else {
        let listener = match tokio::net::TcpListener::bind(&addr).await {
            Ok(l) => l,
            Err(e) => {
                error!("Failed to bind web server on {}: {}", addr, e);
                return;
            }
        };
        if let Err(e) = axum::serve(listener, app).await {
            error!("Web server error: {}", e);
        }
    }
}

// ── Route handlers ───────────────────────────────────────────

/// Helper to format large numbers for OG tags (e.g. 1.5M, 250K)
fn format_og_number(val: f64) -> String {
    let abs = val.abs();
    let formatted = if abs >= 1e9 {
        format!("{:.1}B", val / 1e9)
    } else if abs >= 1e6 {
        format!("{:.1}M", val / 1e6)
    } else if abs >= 1e3 {
        format!("{:.1}K", val / 1e3)
    } else {
        format!("{:.0}", val)
    };
    formatted
}

/// Helper to format uptime for OG tags
fn format_og_uptime(secs: u64) -> String {
    let d = secs / 86400;
    let h = (secs % 86400) / 3600;
    let m = (secs % 3600) / 60;
    if d > 0 {
        format!("{}d {}h {}m", d, h, m)
    } else if h > 0 {
        format!("{}h {}m", h, m)
    } else {
        format!("{}m", m)
    }
}

async fn index_page(State(s): State<WebSharedState>) -> Html<String> {
    let (ah_total, bz_total) = s.profit_tracker.totals();
    let total = ah_total + bz_total;
    let uptime = s.previous_session_secs + s.started_at.elapsed().as_secs();
    let hours = uptime as f64 / 3600.0;
    let per_hour = if hours > 0.0 { total as f64 / hours } else { 0.0 };

    let og_title = "Frikadellen BAF — Control Panel";
    let og_description = format!(
        "💰 Total Profit: {} coins | ⏱️ P/H: {} coins/h | 🕐 Uptime: {}",
        format_og_number(total as f64),
        format_og_number(per_hour),
        format_og_uptime(uptime),
    );

    // Inject OG meta tags at the designated marker in the HTML template
    let og_tags = format!(
        "<meta property=\"og:title\" content=\"{og_title}\">\n\
         <meta property=\"og:description\" content=\"{og_description}\">\n\
         <meta property=\"og:type\" content=\"website\">\n\
         <meta property=\"og:image\" content=\"/api/og-image.png\">\n\
         <meta property=\"og:image:width\" content=\"1200\">\n\
         <meta property=\"og:image:height\" content=\"630\">\n\
         <meta name=\"twitter:card\" content=\"summary_large_image\">\n\
         <meta name=\"twitter:image\" content=\"/api/og-image.png\">\n\
         <meta name=\"theme-color\" content=\"#6c5ce7\">",
    );

    let html = include_str!("panel.html")
        .replacen("<!-- OG_META_TAGS -->", &og_tags, 1);

    Html(html)
}

async fn login(
    State(s): State<WebSharedState>,
    Json(payload): Json<LoginPayload>,
) -> impl IntoResponse {
    let expected = match &s.web_gui_password {
        Some(p) if !p.is_empty() => p,
        _ => {
            // No password configured — login always succeeds (no cookie needed).
            // The config loader generates one when it is missing, so reaching
            // this arm means the panel was started outside the normal path.
            return (StatusCode::OK, Json(LoginResponse { success: true })).into_response();
        }
    };

    // Constant-time password comparison to prevent timing attacks
    if payload.password.len() != expected.len()
        || payload
            .password
            .bytes()
            .zip(expected.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            != 0
    {
        info!("[WebGUI] Failed login attempt from web panel");
        // Small fixed delay to slow down brute-force attempts against the panel
        // password. Combined with the constant-time comparison above this keeps
        // the login endpoint from being a fast password oracle.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        return (
            StatusCode::UNAUTHORIZED,
            Json(LoginResponse { success: false }),
        )
            .into_response();
    }

    // Generate a random session token and cap the number of active sessions
    let token = uuid::Uuid::new_v4().to_string();
    {
        let mut sessions = s.valid_sessions.lock().unwrap();
        // Limit to 64 active sessions; evict oldest when full
        if sessions.len() >= 64 {
            if let Some(oldest) = sessions.iter().next().cloned() {
                sessions.remove(&oldest);
            }
        }
        sessions.insert(token.clone());
    }

    info!("[WebGUI] Successful login via web panel");

    // `Secure` only when we actually serve TLS: browsers silently drop a Secure
    // cookie sent over plain HTTP, which would look like a login that "works"
    // but never sticks.
    let secure = if WEB_TLS_ACTIVE.load(Ordering::Relaxed) { " Secure;" } else { "" };
    let cookie = format!(
        "baf_session={};{} Path=/; HttpOnly; SameSite=Strict; Max-Age=604800",
        token, secure
    );
    (
        StatusCode::OK,
        [("set-cookie", cookie)],
        Json(LoginResponse { success: true }),
    )
        .into_response()
}

async fn get_status(State(s): State<WebSharedState>) -> Json<StatusResponse> {
    let anonymize = s.anonymize_webhook_name.load(Ordering::Relaxed);

    // When anonymize is enabled, hide account names in the web panel so
    // screenshots don't leak the player's IGN.
    let (current_account, accounts) = if anonymize {
        let hidden = "Hidden".to_string();
        let anon_accounts: Vec<String> = s.ingame_names.iter().map(|_| hidden.clone()).collect();
        let anon_current = anon_accounts.get(s.current_account_index).cloned().unwrap_or_default();
        (anon_current, anon_accounts)
    } else {
        (
            s.ingame_names.get(s.current_account_index).cloned().unwrap_or_default(),
            s.ingame_names.clone(),
        )
    };

    Json(StatusResponse {
        state: format!("{:?}", s.bot_client.state()),
        macro_paused: s.macro_paused.load(Ordering::Relaxed),
        enable_ah_flips: s.enable_ah_flips.load(Ordering::Relaxed),
        enable_bazaar_flips: s.enable_bazaar_flips.load(Ordering::Relaxed),
        anonymize_webhook_name: anonymize,
        queue_depth: s.command_queue.len(),
        current_account,
        current_account_index: s.current_account_index,
        accounts,
        purse: s.bot_client.get_purse(),
        uptime_seconds: s.previous_session_secs + s.started_at.elapsed().as_secs(),
        bazaar_at_limit: s.bot_client.is_bazaar_at_limit(),
        auction_at_limit: s.bot_client.is_auction_at_limit(),
        inventory_full: s.bot_client.is_inventory_full(),
        flips_accepted: s.flip_diag.accepted_total(),
        flips_dropped: s.flip_diag.dropped_total(),
        flip_drop_reason: s.flip_diag.last_drop().map(|(r, secs_ago)| {
            format!("{} ({}) — {}s ago", r.as_str(), r.hint(), secs_ago)
        }),
    })
}

async fn pause_macro(State(s): State<WebSharedState>) -> impl IntoResponse {
    s.macro_paused.store(true, Ordering::Relaxed);
    info!("[WebGUI] Macro paused via web panel");
    let msg = "[BAF Web] Macro paused".to_string();
    print_mc_chat(&msg);
    let _ = s.chat_tx.send(msg);
    StatusCode::OK
}

async fn resume_macro(State(s): State<WebSharedState>) -> impl IntoResponse {
    s.macro_paused.store(false, Ordering::Relaxed);
    info!("[WebGUI] Macro resumed via web panel");
    let msg = "[BAF Web] Macro resumed".to_string();
    print_mc_chat(&msg);
    let _ = s.chat_tx.send(msg);
    StatusCode::OK
}

async fn get_inventory(State(s): State<WebSharedState>) -> impl IntoResponse {
    match s.bot_client.get_cached_inventory_json() {
        Some(json) => (StatusCode::OK, json),
        None => (StatusCode::OK, r#"{"slots":[]}"#.to_string()),
    }
}

async fn get_game_view(State(s): State<WebSharedState>) -> impl IntoResponse {
    match s.bot_client.get_cached_window_json() {
        Some(json) => (StatusCode::OK, json),
        None => (StatusCode::OK, r#"{"open":false,"botState":"Unknown","windowId":null,"title":null,"slots":[]}"#.to_string()),
    }
}

async fn toggle_ah(
    State(s): State<WebSharedState>,
    Json(payload): Json<TogglePayload>,
) -> impl IntoResponse {
    s.enable_ah_flips.store(payload.enabled, Ordering::Relaxed);
    info!("[WebGUI] AH flips set to {} via web panel", payload.enabled);
    let msg = format!("[BAF Web] AH flips {}", if payload.enabled { "enabled" } else { "disabled" });
    print_mc_chat(&msg);
    let _ = s.chat_tx.send(msg);
    // Persist to config file
    let enabled = payload.enabled;
    let loader = s.config_loader.clone();
    tokio::task::spawn_blocking(move || {
        if let Err(e) = loader.update_property(|c| c.enable_ah_flips = enabled) {
            error!("[WebGUI] Failed to persist AH flips toggle to config: {}", e);
        }
    });
    StatusCode::OK
}

async fn toggle_bazaar(
    State(s): State<WebSharedState>,
    Json(payload): Json<TogglePayload>,
) -> impl IntoResponse {
    s.enable_bazaar_flips.store(payload.enabled, Ordering::Relaxed);
    info!("[WebGUI] Bazaar flips set to {} via web panel", payload.enabled);
    let msg = format!("[BAF Web] Bazaar flips {}", if payload.enabled { "enabled" } else { "disabled" });
    print_mc_chat(&msg);
    let _ = s.chat_tx.send(msg);
    // Persist to config file
    let enabled = payload.enabled;
    let loader = s.config_loader.clone();
    tokio::task::spawn_blocking(move || {
        if let Err(e) = loader.update_property(|c| c.enable_bazaar_flips = enabled) {
            error!("[WebGUI] Failed to persist Bazaar flips toggle to config: {}", e);
        }
    });
    StatusCode::OK
}

async fn toggle_anonymize(
    State(s): State<WebSharedState>,
    Json(payload): Json<TogglePayload>,
) -> impl IntoResponse {
    s.anonymize_webhook_name.store(payload.enabled, Ordering::Relaxed);
    info!("[WebGUI] Anonymize set to {} via web panel", payload.enabled);
    let msg = format!("[BAF Web] Anonymize {}", if payload.enabled { "enabled" } else { "disabled" });
    print_mc_chat(&msg);
    let _ = s.chat_tx.send(msg);
    StatusCode::OK
}

// ── Shared chat input processor ───────────────────────────────

/// Process a chat input string the same way the console does:
/// - `/cofl <cmd>` or `/baf <cmd>` → send to Coflnet WebSocket
/// - `/<command>` → queue as Minecraft SendChat command
/// - plain text → send to Coflnet as "chat" type
/// Build the `/ping` report: live Hypixel ping, bot state, purse and a one-line
/// flip-intake health summary (which also reveals *why* flips are being dropped,
/// e.g. Coflnet not authenticated or AH flips disabled).
fn ping_report(state: &WebSharedState) -> String {
    let ping = crate::hypixel_ping::best_ping_ms()
        .map(|ms| format!("{}ms", ms))
        .unwrap_or_else(|| "measuring…".to_string());
    let bot_state = format!("{:?}", state.bot_client.state());
    let purse = state
        .bot_client
        .get_purse()
        .map(crate::utils::format_number_with_separators)
        .unwrap_or_else(|| "?".to_string());
    format!(
        "§f[§4BAF§f]: §b/ping §7→ §fping §a{}§7 | §fstate §b{}§7 | §fpurse §6{}§7 | {}",
        ping, bot_state, purse, state.flip_diag.summary_line(),
    )
}

async fn process_chat_input(input: &str, state: &WebSharedState) {
    let lowercase = input.to_lowercase();

    // `/ping` is answered locally by the panel: it reports the bot's live ping
    // to Hypixel plus a flip-intake health line, instead of spamming Hypixel's
    // own `/ping` in-game. Handled before the generic `/command` forwarder.
    if lowercase == "/ping" {
        let report = ping_report(state);
        print_mc_chat(&report);
        let _ = state.chat_tx.send(report);
        return;
    }

    if lowercase.starts_with("/cofl") || lowercase.starts_with("/baf") {
        let parts: Vec<&str> = input.split_whitespace().collect();
        if parts.len() > 1 {
            let command = parts[1];
            let args = parts[2..].join(" ");
            // `/cofl ping` is handled server-side by Coflnet — forward it normally.
            let data_json = serde_json::to_string(&args).unwrap_or_else(|_| "\"\"".to_string());
            let message = serde_json::json!({
                "type": command,
                "data": data_json
            })
            .to_string();
            if let Err(e) = state.ws_client.send_message(&message).await {
                error!("[WebGUI] Failed to send command to websocket: {}", e);
            }
        }
    } else if input.starts_with('/') {
        state.command_queue.enqueue(
            CommandType::SendChat {
                message: input.to_string(),
            },
            CommandPriority::Critical,
            false,
        );
    } else {
        let data_json = serde_json::to_string(&input).unwrap_or_else(|_| "\"\"".to_string());
        let message = serde_json::json!({
            "type": "chat",
            "data": data_json
        })
        .to_string();
        if let Err(e) = state.ws_client.send_message(&message).await {
            error!("[WebGUI] Failed to send chat to websocket: {}", e);
        }
    }

    let echo = format!("> {}", input);
    print_mc_chat(&echo);
    let _ = state.chat_tx.send(echo);
}

async fn send_chat(
    State(s): State<WebSharedState>,
    Json(payload): Json<ChatMessage>,
) -> impl IntoResponse {
    let input = payload.message.trim().to_string();
    if input.is_empty() {
        return StatusCode::BAD_REQUEST;
    }

    process_chat_input(&input, &s).await;
    StatusCode::OK
}

async fn switch_account(
    State(s): State<WebSharedState>,
    Json(payload): Json<SwitchPayload>,
) -> impl IntoResponse {
    if s.ingame_names.len() <= 1 {
        return (StatusCode::BAD_REQUEST, "Multi-account not active");
    }
    if payload.index >= s.ingame_names.len() {
        return (StatusCode::BAD_REQUEST, "Invalid account index");
    }

    let next_name = &s.ingame_names[payload.index];
    info!(
        "[WebGUI] Switching to account {} ({}) via web panel",
        payload.index + 1,
        next_name
    );

    if let Err(e) = std::fs::write(&s.account_index_path, payload.index.to_string()) {
        warn!("[WebGUI] Failed to write account index: {}", e);
    }

    // Mark the incoming account so the restarted process starts a fresh session
    // (profit + uptime reset to 0) rather than resuming the previous account's
    // stale totals when the restart lands inside the quick-restart window.
    crate::session::write_account_switch_marker(next_name);

    let _ = s
        .chat_tx
        .send(format!("[BAF Web] Switching to account {}...", next_name));

    // Transfer the COFL license to the next account before restarting.
    let license_index = s.detected_cofl_license.load(std::sync::atomic::Ordering::Relaxed);
    let ws = s.ws_client.clone();
    let target_name = next_name.clone();

    // Restart the process with the new account index.
    tokio::spawn(async move {
        if license_index > 0 {
            if let Err(e) = ws.transfer_license(license_index, &target_name).await {
                warn!("[WebGUI] Failed to transfer license: {}", e);
            }
            // Give COFL time to process the license transfer before restarting.
            tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
        } else {
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        }
        crate::utils::restart_process();
    });

    (StatusCode::OK, "Switching account — process will restart")
}

async fn cancel_auction(
    State(s): State<WebSharedState>,
    Json(payload): Json<CancelAuctionPayload>,
) -> impl IntoResponse {
    info!(
        "[WebGUI] Cancel auction requested: '{}' (bid: {})",
        payload.item_name, payload.starting_bid
    );

    let msg = format!(
        "[BAF Web] Cancelling auction: {}...",
        payload.item_name
    );
    print_mc_chat(&msg);
    let _ = s.chat_tx.send(msg);

    s.command_queue.enqueue(
        CommandType::CancelAuction {
            item_name: payload.item_name,
            starting_bid: payload.starting_bid,
        },
        CommandPriority::Critical,
        false,
    );

    (StatusCode::OK, "Cancel auction command queued")
}

async fn list_item(
    State(s): State<WebSharedState>,
    Json(payload): Json<ListItemPayload>,
) -> impl IntoResponse {
    // Basic validation
    if payload.starting_bid == 0 {
        return (StatusCode::BAD_REQUEST, "Starting bid must be greater than 0").into_response();
    }
    // Clamp to Hypixel's maximum auction duration of 7 days (168 hours).
    let duration = payload.duration_hours.clamp(1, 168);

    info!(
        "[WebGUI] Manual AH listing: '{}' slot={} bid={} duration={}h",
        payload.item_name, payload.item_slot, payload.starting_bid, duration
    );

    let msg = format!(
        "[BAF Web] Listing '{}' on AH for {} coins...",
        payload.item_name, payload.starting_bid
    );
    print_mc_chat(&msg);
    let _ = s.chat_tx.send(msg);

    s.command_queue.enqueue(
        CommandType::SellToAuction {
            item_name: payload.item_name,
            starting_bid: payload.starting_bid,
            duration_hours: duration,
            item_slot: Some(payload.item_slot),
            item_id: None,
        },
        CommandPriority::Critical,
        false,
    );

    (StatusCode::OK, "List item command queued").into_response()
}

async fn claim_purchases(
    State(s): State<WebSharedState>,
) -> impl IntoResponse {
    info!("[WebGUI] Claim purchases requested");

    let msg = "[BAF Web] Checking unclaimed purchases...".to_string();
    print_mc_chat(&msg);
    let _ = s.chat_tx.send(msg);

    s.command_queue.enqueue(
        CommandType::ClaimPurchasedItem,
        CommandPriority::Critical,
        false,
    );

    (StatusCode::OK, "Claim purchases command queued")
}

async fn collect_bz_orders(
    State(s): State<WebSharedState>,
) -> impl IntoResponse {
    info!("[WebGUI] Sell inventory instantly on bazaar requested");

    let msg = "[BAF Web] Selling inventory on bazaar...".to_string();
    print_mc_chat(&msg);
    let _ = s.chat_tx.send(msg);

    s.command_queue.enqueue(
        CommandType::SellInventoryBz,
        CommandPriority::Critical,
        false,
    );

    (StatusCode::OK, "Sell inventory on bazaar command queued")
}

async fn claim_bz_orders(
    State(s): State<WebSharedState>,
) -> impl IntoResponse {
    info!("[WebGUI] Force claim bazaar orders requested");

    let msg = "[BAF Web] Checking and claiming bazaar orders...".to_string();
    print_mc_chat(&msg);
    let _ = s.chat_tx.send(msg);

    s.command_queue.enqueue(
        CommandType::ManageOrders { cancel_open: false, target_item: None },
        CommandPriority::Critical,
        false,
    );

    (StatusCode::OK, "Claim bazaar orders command queued")
}

async fn cancel_bz_order(
    State(s): State<WebSharedState>,
    Json(payload): Json<CancelBzOrderPayload>,
) -> impl IntoResponse {
    let order_type = if payload.is_buy_order { "BUY" } else { "SELL" };
    info!(
        "[WebGUI] Cancel bazaar order requested: '{}' ({})",
        payload.item_name, order_type
    );

    let msg = format!(
        "[BAF Web] Cancelling bazaar {} order: {}...",
        order_type, payload.item_name
    );
    print_mc_chat(&msg);
    let _ = s.chat_tx.send(msg);

    // Remove the order from the tracker immediately so the web GUI reflects
    // the intent.  The in-game cancellation happens asynchronously via
    // ManageOrders targeting this specific order.
    //
    // Also mark it pending-cancel: the ManageOrders cycle reads the Bazaar
    // Orders window (emitting a snapshot that still contains this order) BEFORE
    // it cancels it, so without this the reconcile pass would re-add the order
    // and it would flicker back into the panel.
    s.bazaar_tracker.mark_cancelling(&payload.item_name, payload.is_buy_order);
    s.bazaar_tracker.remove_order(&payload.item_name, payload.is_buy_order);

    s.command_queue.enqueue(
        CommandType::ManageOrders {
            cancel_open: true,
            target_item: Some(crate::types::BazaarOrderTarget {
                item_name: payload.item_name,
                is_buy: payload.is_buy_order,
                price_per_unit: None,
            }),
        },
        CommandPriority::Critical,
        false,
    );

    (StatusCode::OK, "Cancel bazaar order command queued")
}

async fn cancel_all_bz_orders(
    State(s): State<WebSharedState>,
) -> impl IntoResponse {
    info!("[WebGUI] Cancel ALL bazaar orders requested");

    let msg = "[BAF Web] Cancelling all bazaar orders...".to_string();
    print_mc_chat(&msg);
    let _ = s.chat_tx.send(msg);

    // Clear the tracker immediately so the web GUI reflects the intent.
    let removed = s.bazaar_tracker.clear_all_orders();
    info!("[WebGUI] Cleared {} order(s) from tracker", removed);

    // Queue a ManageOrders cycle with cancel_open=true to cancel in-game orders.
    s.command_queue.enqueue(
        CommandType::ManageOrders { cancel_open: true, target_item: None },
        CommandPriority::Critical,
        false,
    );

    (StatusCode::OK, "Cancel all bazaar orders command queued")
}

// ── Session control ───────────────────────────────────────────

async fn kill_session() -> impl IntoResponse {
    info!("[WebGUI] Kill session requested — terminating process");
    // Spawn so the HTTP response is sent before exit
    tokio::spawn(async {
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
        std::process::exit(0);
    });
    (StatusCode::OK, "Killing session — process will terminate")
}

async fn disconnect_session(State(s): State<WebSharedState>) -> impl IntoResponse {
    info!("[WebGUI] Disconnect requested");

    // Pause flip intake so the COFL event loop in main.rs drops new flips
    // instead of queueing them. Without this, the bot would keep accepting
    // flips via the COFL WS (which auto-reconnects) while the user thinks
    // it's disconnected.
    //
    // NOTE: We use a dedicated `flip_intake_paused` flag here instead of
    // flipping `enable_ah_flips` / `enable_bazaar_flips`.  Those atomics
    // represent the user's persistent config preference and are expected to
    // remain `true` across the process lifetime (see main.rs).  Previously
    // this code cleared those atomics, which permanently disabled flips
    // after a single Disconnect click because the COFL WS auto-reconnects
    // and nothing restored them until a full process restart.
    s.flip_intake_paused.store(true, Ordering::Relaxed);

    // Clear any already-queued flips/orders so they don't fire after the
    // user pressed Disconnect.
    s.command_queue.clear();

    let msg = "[BAF Web] Disconnect: flip intake paused, queue cleared, COFL closed".to_string();
    print_mc_chat(&msg);
    let _ = s.chat_tx.send(msg);

    // Close the COFL websocket
    let ws = s.ws_client.clone();
    tokio::spawn(async move {
        if let Err(e) = ws.close().await {
            warn!("[WebGUI] Failed to close COFL websocket: {}", e);
        }
    });

    // Disconnect the bot from Hypixel (logs + parks state in Idle)
    s.bot_client.disconnect();

    (StatusCode::OK, "Disconnected: flip intake paused, queue cleared, COFL closed")
}

async fn connect_session(State(s): State<WebSharedState>) -> impl IntoResponse {
    info!("[WebGUI] Reconnect requested — restarting process");

    // Safety net: clear the flip-intake pause in case the restart is skipped
    // or delayed for any reason. The restart itself re-creates the atomic
    // fresh (defaulting to unpaused), which is the authoritative reset.
    s.flip_intake_paused.store(false, Ordering::Relaxed);

    let msg = "[BAF Web] Reconnecting — restarting process...".to_string();
    let _ = s.chat_tx.send(msg);

    // Restart the process to reconnect everything cleanly
    tokio::spawn(async {
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
        crate::utils::restart_process();
    });

    (StatusCode::OK, "Reconnecting — process will restart")
}

/// Restart the bot process in place (re-exec the same binary). Same mechanism as
/// the post-rest-break restart — reconnects Hypixel + COFL cleanly without the
/// user touching the console. Does NOT check for updates (see `update_session`).
async fn restart_session(State(s): State<WebSharedState>) -> impl IntoResponse {
    info!("[WebGUI] Restart requested — restarting process");

    // Clear the flip-intake pause so a restart from a disconnected state comes
    // back flipping. The restart re-creates the atomic fresh anyway.
    s.flip_intake_paused.store(false, Ordering::Relaxed);

    let _ = s.chat_tx.send("[BAF Web] Restarting process...".to_string());

    tokio::spawn(async {
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
        crate::utils::restart_process();
    });

    (StatusCode::OK, "Restarting — process will restart")
}

/// Download the latest release (if newer) and restart into it — the same update
/// the external loader performs, but triggered from the web GUI so the user
/// never has to drop to a console. Returns 200 with a human-readable status:
/// "up to date" (no restart) or "updating to <version>" (process restarts).
async fn update_session(State(s): State<WebSharedState>) -> impl IntoResponse {
    info!("[WebGUI] Update requested — checking GitHub for a newer release");

    match crate::updater::download_latest().await {
        Ok(crate::updater::UpdateStatus::UpToDate { version }) => {
            let msg = format!("Already up to date ({version}) — no restart needed.");
            info!("[WebGUI] Update: {msg}");
            (StatusCode::OK, msg)
        }
        Ok(crate::updater::UpdateStatus::Updated { version }) => {
            let msg = format!("Updated to {version} — restarting...");
            info!("[WebGUI] Update: {msg}");
            let _ = s
                .chat_tx
                .send(format!("[BAF Web] Updated to {version} — restarting..."));
            // Flush the HTTP response, then apply the staged update and restart.
            tokio::spawn(async {
                tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
                crate::updater::finish_update_restart();
            });
            (StatusCode::OK, msg)
        }
        Err(e) => {
            let msg = format!("Update failed: {e}");
            warn!("[WebGUI] {msg}");
            (StatusCode::INTERNAL_SERVER_ERROR, msg)
        }
    }
}

// ── Active auctions ───────────────────────────────────────────

/// Resolve a Minecraft username to a UUID (with dashes) using the Mojang API.
/// Returns `None` if the lookup fails.
async fn fetch_player_uuid(username: &str) -> Option<String> {
    let url = format!(
        "https://api.mojang.com/users/profiles/minecraft/{}",
        username
    );
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .ok()?;
    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let json: serde_json::Value = resp.json().await.ok()?;
    let raw_id = json.get("id")?.as_str()?;
    // Insert dashes into the raw 32-char hex UUID: 8-4-4-4-12
    if raw_id.len() != 32 {
        return None;
    }
    Some(format!(
        "{}-{}-{}-{}-{}",
        &raw_id[0..8],
        &raw_id[8..12],
        &raw_id[12..16],
        &raw_id[16..20],
        &raw_id[20..32]
    ))
}

async fn get_auctions(State(s): State<WebSharedState>) -> impl IntoResponse {
    // Try locally cached "My Auctions" data first (extracted from in-game GUI).
    // This provides immediate, accurate data without external API calls.
    if let Some(cached_json) = s.bot_client.get_cached_my_auctions_json() {
        // Parse the cached array and convert to AuctionEntry format
        if let Ok(cached_arr) = serde_json::from_str::<Vec<serde_json::Value>>(&cached_json) {
            let entries: Vec<AuctionEntry> = cached_arr
                .into_iter()
                .filter(|a| {
                    // Only include active auctions
                    a.get("status").and_then(|s| s.as_str()).unwrap_or("") == "active"
                })
                .map(|a| {
                    AuctionEntry {
                        uuid: String::new(),
                        item_name: a.get("item_name").and_then(|v| v.as_str()).unwrap_or("Unknown").to_string(),
                        tag: a.get("tag").and_then(|v| v.as_str()).map(|s| s.to_string()),
                        highest_bid: a.get("highest_bid").and_then(|v| v.as_i64()).unwrap_or(0),
                        starting_bid: a.get("starting_bid").and_then(|v| v.as_i64()).unwrap_or(0),
                        bin: a.get("bin").and_then(|v| v.as_bool()).unwrap_or(false),
                        end: String::new(),
                        time_remaining_seconds: a.get("time_remaining_seconds").and_then(|v| v.as_i64()),
                        buyable_in_seconds: a.get("buyable_in_seconds").and_then(|v| v.as_i64()),
                        lore: a.get("lore").and_then(|v| v.as_array()).map(|arr| {
                            arr.iter().filter_map(|l| l.as_str().map(|s| s.to_string())).collect()
                        }),
                    }
                })
                .collect();
            if !entries.is_empty() {
                return Json(entries).into_response();
            }
        }
    }

    // Resolve UUID — use cache if available, otherwise fetch from Mojang
    let uuid = {
        let cached = s.player_uuid.read().await.clone();
        if let Some(u) = cached {
            u
        } else {
            let name = s
                .ingame_names
                .get(s.current_account_index)
                .cloned()
                .unwrap_or_default();
            if name.is_empty() {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(serde_json::json!({"error": "No player name configured"})),
                )
                    .into_response();
            }
            match fetch_player_uuid(&name).await {
                Some(u) => {
                    *s.player_uuid.write().await = Some(u.clone());
                    u
                }
                None => {
                    warn!("[WebGUI] Could not resolve UUID for player '{}'", name);
                    return (
                        StatusCode::SERVICE_UNAVAILABLE,
                        Json(serde_json::json!({"error": "Could not resolve player UUID"})),
                    )
                        .into_response();
                }
            }
        }
    };

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            error!("[WebGUI] Failed to build HTTP client for auctions: {}", e);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    // Try Hypixel API first if an API key is configured
    if let Some(ref api_key) = s.hypixel_api_key {
        let uuid_no_dashes = uuid.replace('-', "");
        let url = format!(
            "https://api.hypixel.net/v2/skyblock/auction?player={}",
            uuid_no_dashes
        );
        match client
            .get(&url)
            .header("API-Key", api_key.as_str())
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                match resp.json::<serde_json::Value>().await {
                    Ok(data) => {
                        if data.get("success").and_then(|v| v.as_bool()).unwrap_or(false) {
                            let entries = parse_hypixel_auctions(&data);
                            return Json(entries).into_response();
                        }
                        warn!("[WebGUI] Hypixel API returned success=false, falling back to Coflnet");
                    }
                    Err(e) => {
                        warn!("[WebGUI] Failed to parse Hypixel auction response: {}", e);
                    }
                }
            }
            Ok(resp) => {
                warn!("[WebGUI] Hypixel API returned status {}, falling back to Coflnet", resp.status());
            }
            Err(e) => {
                warn!("[WebGUI] Failed to fetch auctions from Hypixel: {}", e);
            }
        }
    }

    // Fallback: Fetch auctions from Coflnet
    let url = format!(
        "https://sky.coflnet.com/api/player/{}/auctions",
        uuid
    );

    let resp = match client.get(&url).send().await {
        Ok(r) => r,
        Err(e) => {
            warn!("[WebGUI] Failed to fetch auctions from Coflnet: {}", e);
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": "Failed to fetch auctions"})),
            )
                .into_response();
        }
    };

    let raw: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            warn!("[WebGUI] Failed to parse auctions response: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "Failed to parse auction data"})),
            )
                .into_response();
        }
    };

    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_else(|e| {
            warn!("[WebGUI] System clock appears to be before Unix epoch: {}", e);
            0
        });

    let entries: Vec<AuctionEntry> = raw
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .filter_map(|auction| {
            let end_str = auction.get("end")?.as_str()?;
            // Parse ISO 8601 end timestamp into epoch seconds; skip entries with invalid timestamps
            let end_secs = match chrono::DateTime::parse_from_rfc3339(end_str) {
                Ok(dt) => dt.timestamp(),
                Err(e) => {
                    warn!("[WebGUI] Skipping auction with invalid end timestamp '{}': {}", end_str, e);
                    return None;
                }
            };
            let time_remaining = end_secs - now_secs;
            // Only include auctions that are still active
            if time_remaining <= 0 {
                return None;
            }
            let item_name = auction
                .get("itemName")
                .and_then(|v| v.as_str())
                .unwrap_or("Unknown")
                .to_string();
            let tag = auction
                .get("tag")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let highest_bid = auction
                .get("highestBid")
                .or_else(|| auction.get("highestBidAmount"))
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let starting_bid = auction
                .get("startingBid")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let bin = auction
                .get("bin")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let uuid = auction
                .get("uuid")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            Some(AuctionEntry {
                uuid,
                item_name,
                tag,
                highest_bid,
                starting_bid,
                bin,
                end: end_str.to_string(),
                time_remaining_seconds: Some(time_remaining),
                // The Hypixel API exposes no grace-period flag; only the in-game
                // GUI lore shows the countdown.
                buyable_in_seconds: None,
                lore: None,
            })
        })
        .collect();

    Json(entries).into_response()
}

// ── Bazaar orders endpoint ──────────────────────────────────

async fn get_bazaar_orders(State(s): State<WebSharedState>) -> Json<Vec<crate::bazaar_tracker::TrackedBazaarOrder>> {
    Json(s.bazaar_tracker.get_orders())
}

// ── Queue status endpoint ───────────────────────────────────

async fn get_queue_status(State(s): State<WebSharedState>) -> Json<Vec<crate::state::QueueEntry>> {
    Json(s.command_queue.queue_snapshot())
}

// ── Config endpoint ─────────────────────────────────────────

async fn get_config(State(s): State<WebSharedState>) -> impl IntoResponse {
    let loader = s.config_loader.clone();
    match tokio::task::spawn_blocking(move || {
        loader.load()
    }).await {
        Ok(Ok(mut config)) => {
            // Never expose COFL account session tokens to the web client. They
            // are server-managed credentials, not user-editable settings, and
            // leaking them would hand out account access to anyone with panel
            // access. They are preserved on save (see save_config).
            config.sessions.clear();
            match toml::to_string_pretty(&config) {
                Ok(toml_str) => (StatusCode::OK, toml_str).into_response(),
                Err(e) => {
                    error!("[WebGUI] Failed to serialize config: {}", e);
                    (StatusCode::INTERNAL_SERVER_ERROR, "Failed to serialize config").into_response()
                }
            }
        }
        Ok(Err(e)) => {
            error!("[WebGUI] Failed to load config: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Failed to load config").into_response()
        }
        Err(e) => {
            error!("[WebGUI] Config task panicked: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response()
        }
    }
}

#[derive(Deserialize)]
struct SaveConfigPayload {
    config_toml: String,
}

/// Refuse to save a config that leaves the panel unauthenticated.
///
/// Without this the loader would just mint a new random password on the next
/// read, and the user would be locked out of a panel they thought they had
/// opened up. Failing the save says so while they are still looking at it.
fn reject_empty_panel_password(config: &crate::config::Config) -> Result<(), String> {
    if config.web_gui_password.as_deref().is_some_and(|p| !p.is_empty()) {
        return Ok(());
    }
    Err("Panel password cannot be empty — the panel controls the bot, so it always needs one".to_string())
}

async fn save_config(
    State(s): State<WebSharedState>,
    Json(payload): Json<SaveConfigPayload>,
) -> impl IntoResponse {
    let loader = s.config_loader.clone();
    let enable_ah = s.enable_ah_flips.clone();
    let enable_bz = s.enable_bazaar_flips.clone();
    let toml_str = payload.config_toml;
    match tokio::task::spawn_blocking(move || -> Result<(), String> {
        // Parse the TOML to validate it first
        let mut config: crate::config::Config = toml::from_str(&toml_str)
            .map_err(|e| format!("Invalid config TOML: {}", e))?;
        reject_empty_panel_password(&config)?;
        // Preserve server-managed COFL session tokens: get_config strips them
        // before sending to the client, so the incoming TOML never contains
        // them. Restore them from the current on-disk config so saving from the
        // web panel does not wipe the user's authenticated sessions.
        if let Ok(existing) = loader.load() {
            config.sessions = existing.sessions;
        }
        // Update in-memory toggle flags to match the saved config
        enable_ah.store(config.enable_ah_flips, Ordering::Relaxed);
        enable_bz.store(config.enable_bazaar_flips, Ordering::Relaxed);
        // Save validated config
        loader.save(&config).map_err(|e| format!("Failed to save config: {}", e))
    }).await {
        Ok(Ok(())) => {
            info!("[WebGUI] Config saved via web panel");
            let msg = "[BAF Web] Config saved".to_string();
            print_mc_chat(&msg);
            let _ = s.chat_tx.send(msg);
            StatusCode::OK.into_response()
        }
        Ok(Err(msg)) => {
            warn!("[WebGUI] Config save failed: {}", msg);
            (StatusCode::BAD_REQUEST, msg).into_response()
        }
        Err(e) => {
            error!("[WebGUI] Config save task panicked: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal error".to_string()).into_response()
        }
    }
}

// ── JSON config API ─────────────────────────────────────────
//
// The panel used to round-trip the WHOLE config as TOML text: the browser
// hand-parsed it, rebuilt it field by field, and posted the result back. Every
// setting the hand-rolled parser did not know about had to be re-emitted
// blind, so adding a field meant touching three places and any mismatch
// silently rewrote the user's file. These two endpoints move that job to serde,
// which already knows the real schema:
//
//   GET  /api/config.json  → the current config as JSON
//   POST /api/config.json  → a PARTIAL object of changed fields, merged in
//
// A patch only carries what the user actually edited, so unknown, unedited and
// server-managed fields are preserved by construction rather than by the
// client remembering to write them back.

/// Serialize the live config to JSON with server-managed secrets stripped.
fn config_to_json(config: &crate::config::Config) -> Result<serde_json::Value, String> {
    let mut config = config.clone();
    // Same reasoning as get_config: COFL session tokens are account
    // credentials, not settings. Never send them to the browser.
    config.sessions.clear();
    serde_json::to_value(&config).map_err(|e| format!("Failed to serialize config: {e}"))
}

async fn get_config_json(State(s): State<WebSharedState>) -> impl IntoResponse {
    let loader = s.config_loader.clone();
    match tokio::task::spawn_blocking(move || loader.load()).await {
        Ok(Ok(config)) => match config_to_json(&config) {
            Ok(v) => (StatusCode::OK, Json(v)).into_response(),
            Err(e) => {
                error!("[WebGUI] {}", e);
                (StatusCode::INTERNAL_SERVER_ERROR, "Failed to serialize config").into_response()
            }
        },
        Ok(Err(e)) => {
            error!("[WebGUI] Failed to load config: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Failed to load config").into_response()
        }
        Err(e) => {
            error!("[WebGUI] Config task panicked: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response()
        }
    }
}

/// Merge a partial `patch` object into `base`, returning the config it produces.
///
/// Rejects unknown keys rather than dropping them: a typo'd field name from a
/// stale panel would otherwise look like it saved and then silently do nothing.
fn merge_config_patch(
    base: &crate::config::Config,
    patch: &serde_json::Map<String, serde_json::Value>,
) -> Result<crate::config::Config, String> {
    let mut doc = config_to_json(base)?;
    let obj = doc
        .as_object_mut()
        .ok_or_else(|| "config did not serialize to an object".to_string())?;
    for (key, value) in patch {
        if !obj.contains_key(key) {
            return Err(format!("Unknown config field '{key}'"));
        }
        obj.insert(key.clone(), value.clone());
    }
    // Round-tripping through Config is the validation: a wrong type, or a
    // number where a string belongs, fails HERE instead of corrupting the file.
    serde_json::from_value(doc).map_err(|e| format!("Invalid config value: {e}"))
}

async fn save_config_json(
    State(s): State<WebSharedState>,
    Json(patch): Json<serde_json::Map<String, serde_json::Value>>,
) -> impl IntoResponse {
    if patch.is_empty() {
        return (StatusCode::OK, "No changes".to_string()).into_response();
    }
    let loader = s.config_loader.clone();
    let enable_ah = s.enable_ah_flips.clone();
    let enable_bz = s.enable_bazaar_flips.clone();
    let changed: Vec<String> = patch.keys().cloned().collect();
    match tokio::task::spawn_blocking(move || -> Result<(), String> {
        let existing = loader.load().map_err(|e| format!("Failed to load config: {e}"))?;
        let mut config = merge_config_patch(&existing, &patch)?;
        reject_empty_panel_password(&config)?;
        // config_to_json cleared these; restore the real ones so saving from the
        // panel never wipes the user's authenticated COFL sessions.
        config.sessions = existing.sessions;
        config.normalize_do_not_relist_ids();
        enable_ah.store(config.enable_ah_flips, Ordering::Relaxed);
        enable_bz.store(config.enable_bazaar_flips, Ordering::Relaxed);
        loader.save(&config).map_err(|e| format!("Failed to save config: {e}"))
    })
    .await
    {
        Ok(Ok(())) => {
            info!("[WebGUI] Config updated ({} field(s): {})", changed.len(), changed.join(", "));
            (StatusCode::OK, format!("Saved {} setting(s)", changed.len())).into_response()
        }
        Ok(Err(msg)) => {
            warn!("[WebGUI] Config patch rejected: {}", msg);
            (StatusCode::BAD_REQUEST, msg).into_response()
        }
        Err(e) => {
            error!("[WebGUI] Config patch task panicked: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal error".to_string()).into_response()
        }
    }
}

/// Parse auctions from Hypixel API response format.
/// Hypixel uses millisecond timestamps and different field names than Coflnet.
fn parse_hypixel_auctions(data: &serde_json::Value) -> Vec<AuctionEntry> {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    data.get("auctions")
        .and_then(|a| a.as_array())
        .unwrap_or(&vec![])
        .iter()
        .filter_map(|auction| {
            // Skip claimed auctions
            if auction.get("claimed").and_then(|v| v.as_bool()).unwrap_or(false) {
                return None;
            }
            let end_ms = auction.get("end").and_then(|v| v.as_i64()).unwrap_or(0);
            let time_remaining_ms = end_ms - now_ms;
            if time_remaining_ms <= 0 {
                return None;
            }
            let item_name = auction
                .get("item_name")
                .and_then(|v| v.as_str())
                .unwrap_or("Unknown")
                .to_string();
            // Hypixel doesn't return a tag directly; derive from item_name for icon lookup
            let tag = derive_item_tag(&item_name);
            let highest_bid = auction
                .get("highest_bid_amount")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let starting_bid = auction
                .get("starting_bid")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let bin = auction
                .get("bin")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let uuid = auction
                .get("uuid")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            // Convert millisecond end timestamp to ISO 8601
            let nanos = ((end_ms % 1000).unsigned_abs() as u32) * 1_000_000;
            let end_iso = chrono::DateTime::from_timestamp(end_ms / 1000, nanos)
                .map(|dt| dt.to_rfc3339())
                .unwrap_or_default();
            Some(AuctionEntry {
                uuid,
                item_name,
                tag,
                highest_bid,
                starting_bid,
                bin,
                end: end_iso,
                time_remaining_seconds: Some((time_remaining_ms / 1000).max(0)),
                buyable_in_seconds: None,
                lore: None,
            })
        })
        .collect()
}

/// Derive a SkyBlock item tag from an item name for icon lookup.
/// Converts "Aspect of the End" → "ASPECT_OF_THE_END".
fn derive_item_tag(item_name: &str) -> Option<String> {
    if item_name.is_empty() || item_name == "Unknown" {
        return None;
    }
    Some(
        item_name
            .chars()
            .map(|c| if c.is_alphanumeric() { c.to_ascii_uppercase() } else { '_' })
            .collect::<String>()
            .trim_matches('_')
            .to_string(),
    )
}

/// Serve the latest.log file as a downloadable file.
async fn download_latest_log() -> impl IntoResponse {
    let logs_dir = crate::logging::get_logs_dir();
    let log_path = logs_dir.join("latest.log");

    match tokio::fs::read(&log_path).await {
        Ok(contents) => {
            let headers = [
                (axum::http::header::CONTENT_TYPE, "text/plain; charset=utf-8"),
                (
                    axum::http::header::CONTENT_DISPOSITION,
                    "attachment; filename=\"latest.log\"",
                ),
            ];
            (StatusCode::OK, headers, contents).into_response()
        }
        Err(e) => {
            warn!("[WebGUI] Failed to read latest.log: {}", e);
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Log file not found"})),
            )
                .into_response()
        }
    }
}

// ── WebSocket handler for live chat ──────────────────────────

async fn get_profit(State(s): State<WebSharedState>) -> Json<ProfitResponse> {
    let (ah_total, bz_total) = s.profit_tracker.totals();
    Json(ProfitResponse {
        ah_points: s.profit_tracker.ah_points(),
        bz_points: s.profit_tracker.bz_points(),
        ah_total,
        bz_total,
        uptime_seconds: s.previous_session_secs + s.started_at.elapsed().as_secs(),
    })
}

/// Public profit endpoint — no authentication required.
/// Returns anonymized profit data (no IGN, no account info) for the
/// login page display and OpenGraph embeds.
async fn get_profit_public(State(s): State<WebSharedState>) -> Json<PublicProfitResponse> {
    let (ah_total, bz_total) = s.profit_tracker.totals();
    let total = ah_total + bz_total;
    let uptime = s.previous_session_secs + s.started_at.elapsed().as_secs();
    let hours = uptime as f64 / 3600.0;
    let per_hour = if hours > 0.0 { total as f64 / hours } else { 0.0 };
    Json(PublicProfitResponse {
        ah_total,
        bz_total,
        total,
        per_hour,
        uptime_seconds: uptime,
        ah_points: s.profit_tracker.ah_points(),
        bz_points: s.profit_tracker.bz_points(),
    })
}

/// Public OG image endpoint — no authentication required.
/// Generates a 1200×630 PNG stats card for Discord / social media embeds.
async fn get_og_image(State(s): State<WebSharedState>) -> impl IntoResponse {
    let (ah_total, bz_total) = s.profit_tracker.totals();
    let total = ah_total + bz_total;
    let uptime = s.previous_session_secs + s.started_at.elapsed().as_secs();
    let hours = uptime as f64 / 3600.0;
    let per_hour = if hours > 0.0 { total as f64 / hours } else { 0.0 };

    let ah_pts = s.profit_tracker.ah_points();
    let bz_pts = s.profit_tracker.bz_points();
    let png = super::og_image::generate_og_image(total, per_hour, uptime, &ah_pts, &bz_pts);

    (
        StatusCode::OK,
        [
            (axum::http::header::CONTENT_TYPE, "image/png"),
            (
                axum::http::header::CACHE_CONTROL,
                "public, max-age=30",
            ),
        ],
        png,
    )
}

async fn chat_ws_handler(
    ws: WebSocketUpgrade,
    State(s): State<WebSharedState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_chat_ws(socket, s))
}

async fn handle_chat_ws(mut socket: WebSocket, state: WebSharedState) {
    let mut rx = state.chat_tx.subscribe();

    loop {
        tokio::select! {
            // Forward broadcast messages to the WebSocket client
            Ok(msg) = rx.recv() => {
                if socket.send(Message::Text(msg.into())).await.is_err() {
                    break;
                }
            }
            // Handle incoming messages from the WebSocket client (chat input)
            Some(Ok(msg)) = socket.recv() => {
                if let Message::Text(text) = msg {
                    let input = text.trim().to_string();
                    if !input.is_empty() {
                        process_chat_input(&input, &state).await;
                    }
                }
            }
            else => break,
        }
    }
    debug!("[WebGUI] WebSocket client disconnected");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("baf-tls-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn panel_cert_is_generated_and_then_reused() {
        let dir = temp_dir("gen");
        let (cert, key) = ensure_panel_cert(&dir).expect("certificate should be generated");
        let cert_pem = std::fs::read_to_string(&cert).expect("cert written");
        let key_pem = std::fs::read_to_string(&key).expect("key written");
        assert!(cert_pem.contains("BEGIN CERTIFICATE"));
        assert!(key_pem.contains("PRIVATE KEY"));

        // Reused, not regenerated: a certificate that changes on every restart
        // trains users to click through the warning that would catch a swap.
        let (cert2, _) = ensure_panel_cert(&dir).expect("second call should succeed");
        assert_eq!(cert2, cert);
        assert_eq!(std::fs::read_to_string(&cert2).unwrap(), cert_pem);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn panel_key_is_not_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("perms");
        let (_, key) = ensure_panel_cert(&dir).expect("certificate should be generated");
        let mode = std::fs::metadata(&key).expect("key exists").permissions().mode();
        assert_eq!(mode & 0o077, 0, "key is readable by group/other: {:o}", mode);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn generated_panel_cert_loads_into_rustls() {
        // Proves the generated PEMs are actually a usable TLS pair, so the panel
        // cannot start, fail to serve, and leave the user with no panel at all.
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let dir = temp_dir("load");
        let (cert, key) = ensure_panel_cert(&dir).expect("certificate should be generated");
        axum_server::tls_rustls::RustlsConfig::from_pem_file(&cert, &key)
            .await
            .expect("generated certificate should load into rustls");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn sessions_with(token: &str) -> HashSet<String> {
        let mut s = HashSet::new();
        s.insert(token.to_string());
        s
    }

    #[test]
    fn unauthenticated_requests_cannot_reach_control_endpoints() {
        let sessions = sessions_with("good-token");
        for path in ["/api/config", "/api/config.json", "/api/chat/send", "/api/status"] {
            assert!(
                !request_is_authorized(true, &sessions, path, &[]),
                "{path} must require a session"
            );
            assert!(
                !request_is_authorized(true, &sessions, path, &["wrong-token".to_string()]),
                "{path} must reject an unknown token"
            );
        }
    }

    #[test]
    fn a_valid_session_token_is_accepted_however_it_arrives() {
        let sessions = sessions_with("good-token");
        assert!(request_is_authorized(true, &sessions, "/api/config", &["good-token".to_string()]));
        // Several presented tokens: one good is enough (cookie + ?token= on a WS).
        assert!(request_is_authorized(
            true,
            &sessions,
            "/api/chat/ws",
            &["stale".to_string(), "good-token".to_string()],
        ));
    }

    #[test]
    fn only_the_login_form_and_share_endpoints_are_public() {
        let sessions = HashSet::new();
        for path in ["/", "/api/login", "/api/profit/public", "/api/og-image.png"] {
            assert!(request_is_authorized(true, &sessions, path, &[]), "{path} should be public");
        }
        assert!(!request_is_authorized(true, &sessions, "/api/profit", &[]),
            "the full profit endpoint is not the public one");
    }

    #[tokio::test]
    async fn the_generated_certificate_actually_serves_https() {
        // End-to-end proof of the zero-config claim: generate a certificate with
        // no settings involved, serve with it, and complete a real TLS handshake.
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let dir = temp_dir("serve");
        let (cert, key) = ensure_panel_cert(&dir).expect("certificate should be generated");
        let tls = axum_server::tls_rustls::RustlsConfig::from_pem_file(&cert, &key)
            .await
            .expect("certificate should load");

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let app = Router::new().route("/", get(|| async { "panel" }));
        let server = tokio::spawn(async move {
            let _ = axum_server::from_tcp_rustls(listener, tls)
                .serve(app.into_make_service())
                .await;
        });

        // Self-signed by design, so the client must not verify the issuer — this
        // is the same one-time warning a browser shows.
        let client = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .build()
            .expect("client");
        let body = client
            .get(format!("https://127.0.0.1:{port}/"))
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await
            .expect("HTTPS request should succeed")
            .text()
            .await
            .expect("body");
        assert_eq!(body, "panel");

        // And plain HTTP to the TLS port must not be served as if it were fine.
        let plain = client
            .get(format!("http://127.0.0.1:{port}/"))
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await;
        assert!(plain.is_err(), "plaintext request to the TLS port should fail");

        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_panel_password_is_rejected_on_save() {
        let with_password = |p: Option<&str>| crate::config::Config {
            web_gui_password: p.map(|s| s.to_string()),
            ..Default::default()
        };
        assert!(reject_empty_panel_password(&with_password(None)).is_err());
        assert!(reject_empty_panel_password(&with_password(Some(""))).is_err());
        assert!(reject_empty_panel_password(&with_password(Some("a-real-password"))).is_ok());
    }

    /// Every `key:'…'` in the panel's CONFIG_SCHEMA.
    fn panel_schema_keys() -> std::collections::HashSet<String> {
        let panel = include_str!("panel.html");
        // The schema block only — so an unrelated `key:'x'` elsewhere in the
        // panel cannot make this test pass by accident.
        let start = panel
            .find("const CONFIG_SCHEMA = [")
            .expect("panel.html defines CONFIG_SCHEMA");
        let body = &panel[start..];
        let end = body.find("\n];").expect("CONFIG_SCHEMA is terminated");
        let re = regex::Regex::new(r"\{key:'([a-zA-Z0-9_]+)'").unwrap();
        re.captures_iter(&body[..end])
            .map(|c| c[1].to_string())
            .collect()
    }

    /// Every serialized field name of `Config`.
    fn config_field_names() -> Vec<String> {
        let value = serde_json::to_value(crate::config::Config::default())
            .expect("Config serializes to JSON");
        value
            .as_object()
            .expect("Config is a JSON object")
            .keys()
            .cloned()
            .collect()
    }

    /// The web panel is meant to be a COMPLETE editor for config.toml. It had
    /// drifted to covering roughly 60% of the fields because each one had to be
    /// hand-written into the HTML. Now the form renders from CONFIG_SCHEMA, and
    /// this test is what keeps "everything is adjustable from the web" true: a
    /// new field in Config fails here until it is given a schema entry.
    #[test]
    fn config_panel_exposes_every_config_field() {
        let keys = panel_schema_keys();
        assert!(!keys.is_empty(), "CONFIG_SCHEMA parsed as empty");

        let missing: Vec<String> = config_field_names()
            .into_iter()
            // Server-managed COFL credentials, never sent to the browser.
            .filter(|name| name != "sessions")
            .filter(|name| !keys.contains(name))
            .collect();
        assert!(
            missing.is_empty(),
            "these config fields are not editable in the web panel — add them to CONFIG_SCHEMA in panel.html: {missing:?}"
        );
    }

    /// The other direction: a schema entry naming a field that no longer exists
    /// would render a control that silently saves nothing (the patch endpoint
    /// rejects unknown keys, so the user would just get an error on save).
    #[test]
    fn config_panel_has_no_stale_schema_entries() {
        let fields = config_field_names();
        let stale: Vec<String> = panel_schema_keys()
            .into_iter()
            .filter(|k| !fields.contains(k))
            .collect();
        assert!(stale.is_empty(), "CONFIG_SCHEMA names fields that no longer exist in Config: {stale:?}");
    }

    /// A configured certificate must actually be picked up. Removing these
    /// settings left users staring at "rcgen self signed cert" in the browser
    /// after installing a real certificate, with nothing in the log about it.
    #[test]
    fn configured_cert_paths_are_used() {
        assert_eq!(
            choose_panel_cert(Some("/etc/le/fullchain.pem"), Some("/etc/le/privkey.pem")),
            PanelCert::Configured {
                cert: "/etc/le/fullchain.pem".to_string(),
                key: "/etc/le/privkey.pem".to_string(),
            }
        );
        // Whitespace-only counts as unset, matching how the config serializes
        // "not configured" as an empty string.
        assert_eq!(choose_panel_cert(Some("  "), Some("")), PanelCert::SelfSigned);
        assert_eq!(choose_panel_cert(None, None), PanelCert::SelfSigned);
    }

    /// Half a certificate cannot work, and must be called out rather than
    /// quietly behaving like nothing was configured at all.
    #[test]
    fn half_configured_cert_is_reported_not_ignored() {
        assert_eq!(
            choose_panel_cert(Some("/etc/le/fullchain.pem"), None),
            PanelCert::Incomplete { have: "web_tls_cert_path", missing: "web_tls_key_path" }
        );
        assert_eq!(
            choose_panel_cert(None, Some("/etc/le/privkey.pem")),
            PanelCert::Incomplete { have: "web_tls_key_path", missing: "web_tls_cert_path" }
        );
    }

    /// End-to-end: bind the real TLS stack with a configured certificate and
    /// check the certificate the server actually PRESENTS on the wire.
    ///
    /// The unit tests above only cover the decision. This is the part that was
    /// broken: the panel happily served TLS while presenting its own
    /// "rcgen self signed cert" instead of the one the user had installed, so a
    /// test that merely asserts "TLS came up" would have passed throughout.
    #[tokio::test]
    async fn the_server_presents_the_configured_certificate_on_the_wire() {
        // A certificate with its own identity, so it cannot be confused with the
        // panel's self-signed one.
        let issued = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_string()])
            .expect("generate test cert");
        let expected_der = issued.cert.der().to_vec();

        let dir = std::env::temp_dir().join(format!("baf-tls-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let cert_path = dir.join("cert.pem");
        let key_path = dir.join("key.pem");
        std::fs::write(&cert_path, issued.cert.pem()).expect("write cert");
        std::fs::write(&key_path, issued.key_pair.serialize_pem()).expect("write key");

        let tls = build_web_tls(
            Some(cert_path.to_str().unwrap()),
            Some(key_path.to_str().unwrap()),
        )
        .await
        .expect("configured cert loads");

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let app = Router::new().route("/", get(|| async { "ok" }));
        tokio::spawn(async move {
            axum_server::from_tcp_rustls(listener, tls)
                .serve(app.into_make_service())
                .await
                .ok();
        });
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;

        // Accept anything: we are inspecting which certificate is served, not
        // validating it.
        let served_der = tokio::task::spawn_blocking(move || {
            let connector = native_tls::TlsConnector::builder()
                .danger_accept_invalid_certs(true)
                .danger_accept_invalid_hostnames(true)
                .build()
                .expect("connector");
            let tcp = std::net::TcpStream::connect(addr).expect("connect");
            let stream = connector.connect("127.0.0.1", tcp).expect("tls handshake");
            stream
                .peer_certificate()
                .expect("peer cert readable")
                .expect("server sent a certificate")
                .to_der()
                .expect("der")
        })
        .await
        .expect("client task");

        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(
            served_der, expected_der,
            "the panel served a different certificate than the one configured — \
             this is the bug where a real certificate was ignored in favour of the self-signed one"
        );
    }

    /// Write a freshly generated certificate to `dir`, returning its DER.
    fn issue_cert_into(dir: &std::path::Path, san: &str) -> Vec<u8> {
        let issued = rcgen::generate_simple_self_signed(vec![san.to_string()])
            .expect("generate test cert");
        std::fs::write(dir.join("cert.pem"), issued.cert.pem()).expect("write cert");
        std::fs::write(dir.join("key.pem"), issued.key_pair.serialize_pem()).expect("write key");
        issued.cert.der().to_vec()
    }

    /// Fetch the certificate a TLS server presents, validating nothing.
    async fn served_cert_der(addr: std::net::SocketAddr) -> Vec<u8> {
        tokio::task::spawn_blocking(move || {
            let connector = native_tls::TlsConnector::builder()
                .danger_accept_invalid_certs(true)
                .danger_accept_invalid_hostnames(true)
                .build()
                .expect("connector");
            let tcp = std::net::TcpStream::connect(addr).expect("connect");
            let stream = connector.connect("127.0.0.1", tcp).expect("tls handshake");
            stream
                .peer_certificate()
                .expect("peer cert readable")
                .expect("server sent a certificate")
                .to_der()
                .expect("der")
        })
        .await
        .expect("client task")
    }

    /// A renewed certificate on disk must be served WITHOUT restarting the bot.
    ///
    /// Let's Encrypt IP certificates are ~160 hours by policy, so this is not an
    /// edge case: it happens every few days, forever. Loading TLS once at
    /// startup would mean serving an expired certificate from the first renewal
    /// onwards until someone restarted the bot.
    #[tokio::test]
    async fn a_renewed_certificate_is_picked_up_without_a_restart() {
        let dir = std::env::temp_dir().join(format!("baf-tls-renew-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let first_der = issue_cert_into(&dir, "127.0.0.1");

        let cert = dir.join("cert.pem");
        let key = dir.join("key.pem");
        let tls = build_web_tls(cert.to_str(), key.to_str()).await.expect("loads");

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let app = Router::new().route("/", get(|| async { "ok" }));
        let serving = tls.clone();
        tokio::spawn(async move {
            axum_server::from_tcp_rustls(listener, serving)
                .serve(app.into_make_service())
                .await
                .ok();
        });
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        assert_eq!(served_cert_der(addr).await, first_der, "serves the original certificate");

        // Simulate the renewal: same paths, brand new certificate.
        let renewed_der = issue_cert_into(&dir, "127.0.0.1");
        assert_ne!(renewed_der, first_der, "the renewal must be a different certificate");

        // Drive the same reload the background watcher performs, rather than
        // sleeping out its poll interval.
        tls.reload_from_pem_file(&cert, &key).await.expect("reload");

        let after = served_cert_der(addr).await;
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(
            after, renewed_der,
            "after a renewal the panel must present the NEW certificate on the wire"
        );
    }

    /// The panel must still come up when a configured certificate cannot be
    /// loaded — being locked out of the bot is worse than a browser warning —
    /// but see `build_web_tls`, which logs the failure at error level.
    #[tokio::test]
    async fn unreadable_configured_cert_falls_back_instead_of_killing_the_panel() {
        let result = build_web_tls(
            Some("/nonexistent/fullchain.pem"),
            Some("/nonexistent/privkey.pem"),
        )
        .await;
        assert!(result.is_ok(), "a bad cert path must not stop the panel from starting");
    }

    #[test]
    fn config_patch_merges_only_the_given_fields() {
        let mut base = crate::config::Config::default();
        base.ingame_name = Some("Original".to_string());
        base.bed_pre_click_ms = 30;

        let patch: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(r#"{"bed_pre_click_ms": 45}"#).unwrap();
        let merged = merge_config_patch(&base, &patch).expect("patch applies");

        assert_eq!(merged.bed_pre_click_ms, 45, "the edited field changed");
        assert_eq!(
            merged.ingame_name.as_deref(),
            Some("Original"),
            "an untouched field must survive the patch"
        );
    }

    #[test]
    fn config_patch_rejects_unknown_and_mistyped_fields() {
        let base = crate::config::Config::default();

        // A typo'd key would otherwise look like it saved and do nothing.
        let unknown: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(r#"{"bed_pre_click_msec": 45}"#).unwrap();
        let err = merge_config_patch(&base, &unknown).expect_err("unknown key is rejected");
        assert!(err.contains("bed_pre_click_msec"), "got: {err}");

        // Wrong type must fail here, not corrupt config.toml.
        let mistyped: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(r#"{"bed_pre_click_ms": "soon"}"#).unwrap();
        assert!(merge_config_patch(&base, &mistyped).is_err(), "a string is not a u64");
    }

    #[test]
    fn config_patch_never_leaks_cofl_sessions() {
        let mut base = crate::config::Config::default();
        base.sessions.insert(
            "Player".to_string(),
            crate::config::types::CoflSession {
                id: "secret-session-id".to_string(),
                expires: chrono::Utc::now(),
            },
        );
        let json = config_to_json(&base).expect("serializes");
        let text = serde_json::to_string(&json).unwrap();
        assert!(!text.contains("secret-session-id"), "session tokens must never reach the browser");
    }

    #[test]
    fn derive_tag_from_item_name() {
        assert_eq!(derive_item_tag("Aspect of the End"), Some("ASPECT_OF_THE_END".to_string()));
        assert_eq!(derive_item_tag("Mithril Drill SX-R326"), Some("MITHRIL_DRILL_SX_R326".to_string()));
        assert_eq!(derive_item_tag(""), None);
        assert_eq!(derive_item_tag("Unknown"), None);
    }

    #[test]
    fn parse_hypixel_auctions_filters_claimed_and_expired() {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;

        let data = serde_json::json!({
            "success": true,
            "auctions": [
                {
                    "uuid": "abc123",
                    "item_name": "Diamond Sword",
                    "starting_bid": 1000,
                    "highest_bid_amount": 5000,
                    "end": now_ms + 3_600_000, // 1 hour from now
                    "bin": true,
                    "claimed": false
                },
                {
                    "uuid": "def456",
                    "item_name": "Expired Item",
                    "starting_bid": 500,
                    "highest_bid_amount": 0,
                    "end": now_ms - 1000, // Already expired
                    "bin": false,
                    "claimed": false
                },
                {
                    "uuid": "ghi789",
                    "item_name": "Claimed Item",
                    "starting_bid": 2000,
                    "highest_bid_amount": 3000,
                    "end": now_ms + 3_600_000,
                    "bin": false,
                    "claimed": true
                }
            ]
        });

        let entries = parse_hypixel_auctions(&data);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].item_name, "Diamond Sword");
        assert_eq!(entries[0].highest_bid, 5000);
        assert!(entries[0].bin);
        assert!(entries[0].tag.is_some());
        assert_eq!(entries[0].tag.as_deref(), Some("DIAMOND_SWORD"));
    }
}
