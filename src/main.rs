use anyhow::Result;
use dialoguer::{Input, Confirm};
use rustyline;
use frikadellen_baf::{
    config::ConfigLoader,
    logging::{init_logger, print_mc_chat},
    state::CommandQueue,
    websocket::CoflWebSocket,
    bot::BotClient,
    types::Flip,
    web::WebSharedState,
};
use tracing::{debug, error, info, warn};
use tokio::time::{sleep, Duration};
use tokio::sync::broadcast;
use serde_json;
use std::sync::{Arc, Mutex, atomic::{AtomicBool, AtomicI64, Ordering}};
use std::collections::HashMap;
use std::time::Instant;
use frikadellen_baf::utils::restart_process;

const VERSION: &str = "af-3.0";
const PERIODIC_AH_CLAIM_CHECK_INTERVAL_SECS: u64 = 300;
/// If no auction has been listed for this many seconds, force a `/cofl sellinventory`
/// plus claim sold/purchased auctions to unblock stuck inventory.
const INVENTORY_IDLE_SELLINVENTORY_SECS: u64 = 30 * 60; // 30 minutes
const GITHUB_REPO: &str = "TreXito/frikadellen-baf-121";

/// Base delay per consecutive rejoin attempt (seconds).
const REJOIN_BACKOFF_BASE_SECS: u64 = 60;
/// Maximum backoff delay between rejoin attempts (seconds).
const REJOIN_MAX_BACKOFF_SECS: u64 = 300;
/// After this many consecutive rejoin attempts the counter resets so the
/// backoff does not grow unbounded.
const REJOIN_MAX_ATTEMPTS: u32 = 5;

/// Debounce delay before displaying the `/cofl bz l` profit summary (seconds).
/// Coflnet sends each flip as a separate chat message; we wait for the full list
/// to arrive before computing and displaying the total.
const BZ_LIST_DEBOUNCE_SECS: u64 = 2;

/// Delay (seconds) before sending `/cofl bz l` after a SELL order is filled.
/// Coflnet needs a brief window to register the completed flip in its database
/// before the list is requested; 3 seconds covers typical processing latency.
const BZ_LIST_REQUEST_DELAY_SECS: u64 = 3;

/// Seconds in one day — used to convert elapsed seconds to fractional days
/// for Coflnet `/cofl profit` and `/cofl bz h` day-range queries.
const SECS_PER_DAY: f64 = 86400.0;

/// Maximum allowed gap (in seconds) between the last session save and the
/// current startup.  If the gap is larger, the previous session time is
/// discarded so that uptime reflects the *current* session only.
/// A quick restart (crash, manual kill-and-relaunch) within this window
/// carries over the accumulated time; an account switch or long pause resets it.
const MAX_SESSION_GAP_SECS: u64 = 5 * 60; // 5 minutes

/// Extra delay (seconds) added after `BZ_LIST_REQUEST_DELAY_SECS` when
/// requesting `/cofl bz h` after a SELL order is **collected** (vs filled).
/// The collection happens later than the fill, so Coflnet needs slightly
/// more time to register the completed profit.
const BZ_PROFIT_QUERY_EXTRA_DELAY_SECS: u64 = 2;

/// Buffer seconds added past midnight UTC before re-enabling bazaar flips
/// after the daily sell value limit reset — ensures the server-side reset
/// has fully propagated.
const DAILY_LIMIT_RESET_BUFFER_SECS: u64 = 5;

/// How long SkyBlock is treated as down after a "maintenance" line is seen in
/// chat. While this window is active the island guard stops sending `/play sb`
/// (which would just bounce), and the stall guard treats inactivity as expected
/// rather than a frozen connection. It auto-clears so the bot resumes on its own
/// once the server is back — no manual intervention needed.
const MAINTENANCE_COOLDOWN_SECS: i64 = 5 * 60;

/// No GUI window opening for this long is treated as a suspected stall. The bot
/// opens windows constantly in normal operation (bazaar order management, flip
/// purchases, sell/claim), so a long silence means the connection is very likely
/// frozen or the bot was quietly booted to limbo.
const STALL_THRESHOLD_SECS: i64 = 15 * 60; // 15 minutes
/// How often the stall guard re-checks the activity heartbeat.
const STALL_CHECK_INTERVAL_SECS: u64 = 60;
/// Startup grace before the stall guard starts watching, so the initial
/// join/startup workflow isn't mistaken for a stall.
const STALL_GRACE_SECS: u64 = 120;
/// After a soft rejoin recovery, wait this long before re-checking so a live
/// session has time to open a window and clear the stall on its own.
const STALL_RECOVERY_GRACE_SECS: u64 = 90;
/// Consecutive stall detections before we give up on soft recovery and restart
/// the whole process for a clean session.
const STALL_MAX_ATTEMPTS: u32 = 3;

/// Epoch-millis until which SkyBlock is considered under maintenance / down.
/// Set from chat when a maintenance line appears; read by the island and stall
/// guards. 0 = not in maintenance.
static MAINTENANCE_UNTIL_MS: AtomicI64 = AtomicI64::new(0);

/// Epoch-millis of the last sign of life from the game connection (a window
/// opening, spawn, or startup completing). Drives the stall guard. 0 = not yet
/// seeded.
static LAST_ACTIVITY_MS: AtomicI64 = AtomicI64::new(0);

/// Current wall-clock time in epoch-millis.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Record that SkyBlock looks down for maintenance. Returns `true` only on the
/// transition from "up" to "down" so callers can notify exactly once per outage
/// instead of on every repeated maintenance line.
fn note_maintenance() -> bool {
    let was_down = skyblock_in_maintenance();
    MAINTENANCE_UNTIL_MS.store(now_ms() + MAINTENANCE_COOLDOWN_SECS * 1000, Ordering::Release);
    !was_down
}

/// Whether SkyBlock is currently within a maintenance cooldown window.
fn skyblock_in_maintenance() -> bool {
    now_ms() < MAINTENANCE_UNTIL_MS.load(Ordering::Acquire)
}

/// Mark the game connection as alive right now (heartbeat for the stall guard).
fn mark_activity() {
    LAST_ACTIVITY_MS.store(now_ms(), Ordering::Release);
}

/// Seconds since the last sign of life. Returns 0 before the heartbeat is seeded
/// so a fresh, not-yet-active bot is never flagged as stalled.
fn secs_since_activity() -> i64 {
    let last = LAST_ACTIVITY_MS.load(Ordering::Acquire);
    if last == 0 { 0 } else { (now_ms() - last) / 1000 }
}

/// Best-effort current SkyBlock area from the scoreboard (the line carrying the
/// `⏣` area glyph, glyph stripped). Used to name the location in an irregular
/// world-change notice. Returns `None` when the area line isn't present.
fn current_skyblock_area(lines: &[String]) -> Option<String> {
    lines
        .iter()
        .find(|l| l.contains('⏣'))
        .map(|l| l.replace('⏣', "").trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Calculate Hypixel AH fee based on price tier (matches TypeScript calculateAuctionHouseFee).
/// - <10M  → 1%
/// - <100M → 2%
/// - ≥100M → 2.5%
fn calculate_ah_fee(price: u64) -> u64 {
    if price < 10_000_000 {
        price / 100
    } else if price < 100_000_000 {
        price * 2 / 100
    } else {
        price * 25 / 1000
    }
}

/// Format a coin amount with thousands separators.
/// e.g. `24000000` → `"24,000,000"`, `-500000` → `"-500,000"`
fn format_coins(amount: i64) -> String {
    let negative = amount < 0;
    let abs = amount.unsigned_abs();
    let s = abs.to_string();
    let mut result = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(c);
    }
    let formatted: String = result.chars().rev().collect();
    if negative { format!("-{}", formatted) } else { formatted }
}

/// Format an f64 coin amount with comma separators, preserving one decimal
/// digit when the fractional part is non-zero (e.g. 600000.5 → "600,000.5").
fn format_coins_f64(amount: f64) -> String {
    let tenths = (amount * 10.0).round() as i64;
    let int_part = tenths / 10;
    let frac_digit = (tenths % 10).abs();
    let int_str = format_coins(int_part);
    if frac_digit == 0 {
        int_str
    } else {
        format!("{}.{}", int_str, frac_digit)
    }
}

fn is_ban_disconnect(reason: &str) -> bool {
    let lower = reason.to_ascii_lowercase();
    lower.contains("temporarily banned")
        || lower.contains("permanently banned")
        || lower.contains("ban id:")
        || lower.contains("account has been blocked")
        || lower.contains("security-block")
        || lower.contains("block id:")
}

/// Check whether the web GUI port is allowed through UFW.
/// Prints a prominent warning when UFW is active but the port is not listed.
/// No-ops silently if UFW is not installed or the check fails.
#[cfg(target_os = "linux")]
fn check_ufw_port(port: u16) {
    use std::process::Command;
    // Run `ufw status` and capture stdout.
    let output = match Command::new("ufw").arg("status").output() {
        Ok(o) => o,
        Err(_) => return, // UFW not installed — nothing to warn about.
    };
    let text = String::from_utf8_lossy(&output.stdout);
    // If UFW is inactive, the port will be reachable regardless — no warning needed.
    if text.contains("Status: inactive") {
        return;
    }
    // Check if the port appears in the UFW rules using exact token matching.
    // UFW formats rules as "<port>" or "<port>/tcp" or "<port>/udp" at the start of a rule line.
    // We split by whitespace to avoid e.g. port 80 matching "8080".
    let port_str = port.to_string();
    let port_tcp = format!("{}/tcp", port);
    let port_udp = format!("{}/udp", port);
    let allowed = text.split_whitespace().any(|token| {
        token == port_str || token == port_tcp || token == port_udp
    });
    if !allowed {
        warn!("========================================");
        warn!("! WARNING: YOU HAVE SET A PORT FOR THE WEB APP BUT THE PORT ISN'T ALLOWED ON YOUR FIREWALL");
        warn!("! PLEASE EXECUTE THE FOLLOWING COMMAND TO ACCESS IT VIA THE INTERNET:");
        warn!("!   ufw allow {}", port);
        warn!("========================================");
    }
}

/// Parse a Coflnet `/cofl profit` response and return the total profit in coins.
///
/// Expected format (color-stripped):
/// `"According to our data <ign> made <amount> in the last <days> days across <N> auctions"`
///
/// `<amount>` may be a short notation like `82.7M`, `1.5B`, `250K`, or a plain number.
fn parse_cofl_profit_response(clean_msg: &str) -> Option<i64> {
    let rest = clean_msg.strip_prefix("According to our data ")?;
    let made_idx = rest.find(" made ")?;
    let after_made = &rest[made_idx + 6..];
    let end = after_made.find(" in the last ")?;
    let amount_str = after_made[..end].trim();
    parse_short_number(amount_str)
}

/// Parse a SkyBlock "island visitor" chat line and return the bare visitor
/// name. Matches Hypixel's `"[RANK] Name is visiting your island!"` (the rank
/// tag is optional). Input must already be color-stripped.
fn parse_island_visitor(clean: &str) -> Option<String> {
    const MARKER: &str = "is visiting your island!";
    // ASCII-lowercase preserves byte length, so `idx` stays aligned with `clean`.
    let idx = clean.to_ascii_lowercase().find(MARKER)?;
    let mut prefix = clean[..idx].trim();
    // Strip any leading rank tag(s) like "[MVP+]".
    while prefix.starts_with('[') {
        match prefix.find(']') {
            Some(close) => prefix = prefix[close + 1..].trim_start(),
            None => break,
        }
    }
    // The visitor name is the last whitespace-delimited token (drops any rank
    // remnant). Minecraft names contain no spaces.
    let name = prefix.split_whitespace().last()?;
    if is_valid_minecraft_name(name) {
        Some(name.to_string())
    } else {
        None
    }
}

/// Returns true for a syntactically valid Minecraft name (1–16 chars of
/// `[A-Za-z0-9_]`). Used to avoid treating stray chat tokens as player names.
fn is_valid_minecraft_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 16
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Extract the sender's name from a player-chat header such as
/// `"Guild > [MVP+] Someone"`, `"From [VIP] Someone"`, `"[MVP+] Someone"`, or
/// `"Someone"`. Returns `None` when no valid player name can be identified
/// (i.e. the line is almost certainly a system message, not player chat).
fn extract_chat_sender(header: &str) -> Option<String> {
    let mut s = header.trim();
    // Channel prefix ("Guild >", "Party >", "Co-op >", "Officer >", ...).
    if let Some(pos) = s.rfind('>') {
        s = s[pos + 1..].trim();
    }
    // Direct-message "From " prefix.
    if let Some(rest) = s.strip_prefix("From ") {
        s = rest.trim();
    }
    // Leading rank tag(s).
    while s.starts_with('[') {
        match s.find(']') {
            Some(close) => s = s[close + 1..].trim_start(),
            None => break,
        }
    }
    let token = s.split_whitespace().next()?;
    if is_valid_minecraft_name(token) {
        Some(token.to_string())
    } else {
        None
    }
}

/// Case-insensitively test whether a chat `body` *directly addresses* `name`,
/// rather than merely containing it somewhere. A direct address is either the
/// name at the very start of the message (as a whole token) or written as
/// `@name`. So "BafBot help" and "yo @BafBot" count, but "i think BafBot is
/// afk" does not.
fn is_direct_address(body: &str, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let name_l = name.to_ascii_lowercase();
    let body_l = body.trim_start().to_ascii_lowercase();
    let boundary = |c: u8| !(c.is_ascii_alphanumeric() || c == b'_');

    // Name at the start of the message, followed by a boundary (or end of line).
    if let Some(rest) = body_l.strip_prefix(&name_l) {
        if rest.as_bytes().first().map_or(true, |&c| boundary(c)) {
            return true;
        }
    }

    // "@name" anywhere, as a whole token. The '@' itself is a left boundary.
    let at_name = format!("@{}", name_l);
    let bytes = body_l.as_bytes();
    let mut start = 0;
    while let Some(pos) = body_l[start..].find(&at_name) {
        let end = start + pos + at_name.len();
        if bytes.get(end).map_or(true, |&c| boundary(c)) {
            return true;
        }
        start += pos + 1;
        if start >= body_l.len() {
            break;
        }
    }
    false
}

/// Detect when another player is trying to reach the bot in chat. Returns the
/// cleaned line for either:
///   - an **incoming direct message / whisper** to the bot ("From <player>: …"),
///     which always counts — someone is reaching out; or
///   - a **public/guild/party/co-op** line that *directly addresses* the bot
///     (name at the start of the message, or `@name`).
///
/// A name merely appearing mid-sentence in public chat is NOT a mention.
/// Returns `None` for the bot's own messages, outgoing DMs, and system lines.
/// Input must be color-stripped.
fn parse_name_mention(clean: &str, own_name: &str) -> Option<String> {
    if own_name.is_empty() {
        return None;
    }
    // Player chat is "Header: body". Anything without this shape (most system
    // messages) is ignored.
    let (header, body) = clean.split_once(": ")?;
    // A giant header means this is almost certainly a system line that merely
    // happens to contain ": ".
    if header.trim().len() > 48 {
        return None;
    }
    let htrim = header.trim();
    // Our own outgoing DMs ("To <player>: ...") are never a mention of us.
    if htrim.starts_with("To ") {
        return None;
    }
    // An incoming whisper straight to the bot.
    let is_dm = htrim.starts_with("From ");
    // Require a genuine player-chat marker so system lines that merely have a
    // "Word: body" shape (e.g. "Reward: ...") don't false-trigger:
    //   - a channel prefix ("Guild >", "Party >", "Co-op >", "Officer >", ...)
    //   - a direct message ("From <player>")
    //   - a rank tag on ranked public/island chat ("[MVP+] <player>")
    let looks_like_player_chat = htrim.contains('>') || is_dm || htrim.contains('[');
    if !looks_like_player_chat {
        return None;
    }
    // Identify the sender; skip the bot's own messages and non-player lines.
    let sender = extract_chat_sender(header)?;
    if sender.eq_ignore_ascii_case(own_name) {
        return None;
    }
    // A whisper always counts; public/guild/party chat only when the bot is
    // directly addressed (not just name-dropped mid-sentence).
    if is_dm || is_direct_address(body, own_name) {
        Some(clean.to_string())
    } else {
        None
    }
}

/// Parse a human-readable short number like `82.7M`, `1.5B`, `250K`, or `500`.
fn parse_short_number(s: &str) -> Option<i64> {
    let s = s.replace(',', "");
    let (num_part, multiplier) = if let Some(n) = s.strip_suffix('B').or_else(|| s.strip_suffix('b')) {
        (n, 1_000_000_000f64)
    } else if let Some(n) = s.strip_suffix('M').or_else(|| s.strip_suffix('m')) {
        (n, 1_000_000f64)
    } else if let Some(n) = s.strip_suffix('K').or_else(|| s.strip_suffix('k')) {
        (n, 1_000f64)
    } else {
        (s.as_str(), 1f64)
    };
    let val: f64 = num_part.parse().ok()?;
    Some((val * multiplier) as i64)
}

/// Parse a single flip line from `/cofl bz l` output and return the profit.
///
/// Expected format (color-stripped):
///   `"2xJungle Key: 1.05M -> 287K => -768K(1)"`
///   `"128xWorm Membrane: 7.16M -> 7.91M => 741K(7)"`
///
/// The profit is the value between `=> ` and `(`.
fn parse_bz_list_flip_profit(line: &str) -> Option<i64> {
    let arrow_idx = line.find("=> ")?;
    let after_arrow = &line[arrow_idx + 3..];
    let paren_idx = after_arrow.find('(')?;
    let profit_str = after_arrow[..paren_idx].trim();
    parse_short_number(profit_str)
}

/// Parse a single flip line from `/cofl bz l` output and return item name,
/// profit, and flip count.
///
/// Expected format (color-stripped):
///   `"2xJungle Key: 1.05M -> 287K => -768K(1)"`
///   `"128xWorm Membrane: 7.16M -> 7.91M => 741K(7)"`
///
/// Returns `(item_name, profit, flip_count)`.
fn parse_bz_list_flip_detail(line: &str) -> Option<(String, i64, u32)> {
    // Amount prefix: digits before 'x'
    let x_idx = line.find('x')?;
    let amount_str = line[..x_idx].trim();
    if amount_str.is_empty() || !amount_str.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let rest = &line[x_idx + 1..];
    let colon_idx = rest.find(':')?;
    let item_name = rest[..colon_idx].trim().to_string();
    if item_name.is_empty() {
        return None;
    }

    // Profit: between "=> " and "("
    let arrow_idx = rest.find("=> ")?;
    let after_arrow = &rest[arrow_idx + 3..];
    let paren_idx = after_arrow.find('(')?;
    let profit_str = after_arrow[..paren_idx].trim();
    let profit = parse_short_number(profit_str)?;

    // Flip count: between "(" and ")"
    let after_paren = &after_arrow[paren_idx + 1..];
    let close_paren = after_paren.find(')')?;
    let count: u32 = after_paren[..close_paren].trim().parse().ok()?;

    Some((item_name, profit, count))
}

/// Parse a Coflnet `/cofl bz h` response and return the total profit in coins.
///
/// Expected format (color-stripped):
///   `"Bazaar Profit History for <ign> (last <days> days)"`
///   `"Total Profit: -234M"`
///   `"Average Daily Profit: -33.5M"`
///   …
///
/// We look for `"Total Profit: "` and parse the short-number value after it.
fn parse_cofl_bz_h_total_profit(clean_msg: &str) -> Option<i64> {
    let prefix = "Total Profit: ";
    let idx = clean_msg.find(prefix)?;
    let after = &clean_msg[idx + prefix.len()..];
    // Take until the next whitespace or end of string.
    let value_str: String = after.chars()
        .take_while(|c| !c.is_whitespace())
        .collect();
    parse_short_number(&value_str)
}

fn should_enqueue_periodic_auction_claim(
    bot_state: frikadellen_baf::types::BotState,
    queue_empty: bool,
) -> bool {
    bot_state.allows_commands() && queue_empty
}

/// Path to azalea's Microsoft-auth cache — the SAME file `Account::microsoft`
/// writes to (`<.minecraft>/azalea-auth.json`). This is where the "it still
/// remembers my previous login" tokens live; the file sits under `~/.minecraft`,
/// NOT the app directory, which is why deleting program logs / redownloading the
/// bot never clears it.
fn azalea_auth_cache_path() -> Option<std::path::PathBuf> {
    minecraft_folder_path::minecraft_dir().map(|d| d.join("azalea-auth.json"))
}

/// Remove cached Microsoft auth entries from azalea's auth cache so the next
/// start prompts a fresh device-code sign-in (letting the user switch accounts).
///
/// `keys` are the account cache-keys to drop (the IGNs the bot logs in with).
/// An empty `keys` slice clears the ENTIRE cache. Returns the number of entries
/// removed (0 if the file is absent or nothing matched).
fn clear_azalea_auth_cache(keys: &[String]) -> std::io::Result<usize> {
    let Some(path) = azalea_auth_cache_path() else { return Ok(0) };
    if !path.exists() {
        return Ok(0);
    }
    // Wipe-all: drop the whole file.
    if keys.is_empty() {
        let count = std::fs::read_to_string(&path)
            .ok()
            .and_then(|c| serde_json::from_str::<Vec<serde_json::Value>>(&c).ok())
            .map(|a| a.len())
            .unwrap_or(0);
        std::fs::remove_file(&path)?;
        return Ok(count);
    }
    let contents = std::fs::read_to_string(&path)?;
    let mut arr: Vec<serde_json::Value> = serde_json::from_str(&contents).unwrap_or_default();
    let before = arr.len();
    let key_set: std::collections::HashSet<String> =
        keys.iter().map(|k| k.trim().to_lowercase()).collect();
    // azalea serialises the key as `cache_key` (older caches used `email`).
    arr.retain(|entry| {
        let ck = entry
            .get("cache_key")
            .or_else(|| entry.get("email"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        !key_set.contains(&ck.trim().to_lowercase())
    });
    let removed = before - arr.len();
    if removed > 0 {
        if arr.is_empty() {
            std::fs::remove_file(&path)?;
        } else {
            std::fs::write(&path, serde_json::to_string_pretty(&arr)?)?;
        }
    }
    Ok(removed)
}

fn should_drop_bazaar_command_during_ah_pause(
    command_type: &frikadellen_baf::types::CommandType,
    bazaar_flips_paused: bool,
    inventory_full: bool,
) -> bool {
    if !bazaar_flips_paused {
        return false;
    }
    match command_type {
        // Never place new bazaar BUY orders while AH flips are incoming.
        frikadellen_baf::types::CommandType::BazaarBuyOrder { .. } => true,
        // Normally defer ManageOrders during the AH flip window. BUT when the
        // inventory is full the bot won't buy AH flips anyway, and it MUST keep
        // managing orders (collecting fills, freeing order/inventory space) to
        // escape the full-inventory deadlock — so don't defer it then.
        frikadellen_baf::types::CommandType::ManageOrders { .. } => !inventory_full,
        _ => false,
    }
}

/// Flip tracker entry: (flip, actual_buy_price, purchase_instant, flip_receive_instant)
/// buy_price is 0 until ItemPurchased fires and updates it.
/// flip_receive_instant is set when the flip is received and never changed (used for buy-speed).
type FlipTrackerMap = Arc<Mutex<HashMap<String, (Flip, u64, Instant, Instant)>>>;

/// Recover a bought item's originating finder and expected profit (coins) from
/// the flip tracker, for the do_not_relist blocklist. Key is the color-stripped
/// lowercased item name (how flips are tracked). Profit = target − buy − AH fee,
/// matching the purchase-webhook figure. Returns (None, None) when the flip is
/// not tracked (e.g. bought in a previous session) — callers then gate on item
/// id only. Applies to both COFL and finder flips.
fn tracked_finder_profit(tracker: &FlipTrackerMap, item_name: &str) -> (Option<String>, Option<i64>) {
    let key = frikadellen_baf::utils::remove_minecraft_colors(item_name).to_lowercase();
    match tracker.lock() {
        Ok(t) => match t.get(&key) {
            Some(entry) => {
                let buy = entry.1;
                let target = entry.0.target;
                let profit = if buy > 0 && target > 0 {
                    Some(target as i64 - buy as i64 - calculate_ah_fee(target) as i64)
                } else {
                    None
                };
                (entry.0.finder.clone(), profit)
            }
            None => (None, None),
        },
        Err(_) => (None, None),
    }
}

/// GitHub release response (subset of fields).
#[derive(serde::Deserialize)]
struct GithubRelease {
    tag_name: String,
    published_at: Option<String>,
}

/// Check the GitHub releases API to see if the current binary is outdated.
/// Logs a prominent warning if the latest release tag differs from the local
/// version.  The local version is determined by:
///   1. The `.version` file next to the binary (written by the loader), or
///   2. The hardcoded `VERSION` constant (protocol version, as a last resort).
///
/// This avoids false "outdated" warnings when the loader has already updated
/// the binary to the latest release.
async fn check_version_outdated() {
    let client = match reqwest::Client::builder()
        .user_agent("FrikadellenBAF/version-check")
        .timeout(std::time::Duration::from_secs(8))
        .build()
    {
        Ok(c) => c,
        Err(_) => return,
    };
    let url = format!("https://api.github.com/repos/{}/releases/latest", GITHUB_REPO);
    let resp = match client.get(&url).send().await {
        Ok(r) if r.status().is_success() => r,
        _ => return,
    };
    let release: GithubRelease = match resp.json().await {
        Ok(r) => r,
        Err(_) => return,
    };
    let latest_tag = release.tag_name.trim();

    // Read the `.version` file that the loader writes next to the binary.
    // When present and matching the latest release, the binary is up-to-date
    // regardless of the hardcoded VERSION constant.
    let loader_version: Option<String> = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|p| p.join(".version")))
        .and_then(|path| std::fs::read_to_string(path).ok())
        .map(|s| s.trim().to_string());

    let local_version = loader_version.as_deref().unwrap_or(VERSION);

    if latest_tag == local_version {
        return; // Up to date
    }
    let date_info = release.published_at
        .as_deref()
        .and_then(|d| d.split('T').next())
        .unwrap_or("unknown date");
    warn!("========================================");
    warn!("YOU ARE USING AN OUTDATED CLIENT, BUG REPORTS ARE NOT VALID FOR OUTDATED CLIENTS");
    warn!("Current version: {}  |  Latest release: {} ({})", local_version, latest_tag, date_info);
    warn!("Download the latest release or use the FrikadellenBAF-loader for automatic updates.");
    warn!("========================================");
}

/// A single session-time entry stored in `session_times.json`.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
struct SessionTimeEntry {
    /// Accumulated running seconds for this session.
    secs: u64,
    /// Unix timestamp (seconds) when this entry was last saved.
    saved_at: u64,
}

/// Persisted profit totals for a single account in `profit_stats.json`.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default)]
struct ProfitStatsEntry {
    /// Cumulative AH profit in coins.
    ah_total: i64,
    /// Cumulative Bazaar profit in coins.
    bz_total: i64,
    /// Unix seconds when this entry was last saved — profit follows the same
    /// session rule as uptime: a quick restart resumes it, a long pause (>
    /// MAX_SESSION_GAP_SECS) starts a fresh session at 0.
    #[serde(default)]
    saved_at: u64,
}

/// Load per-account profit stats from disk.
fn load_profit_stats(path: &std::path::Path) -> HashMap<String, ProfitStatsEntry> {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return HashMap::new(),
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

/// Persist the current profit totals for a given IGN.
fn save_profit_stats(
    path: &std::path::Path,
    ign: &str,
    tracker: &frikadellen_baf::profit::ProfitTracker,
) {
    let (ah, bz) = tracker.totals();
    let mut map = load_profit_stats(path);
    map.insert(ign.to_string(), ProfitStatsEntry { ah_total: ah, bz_total: bz, saved_at: unix_now() });
    if let Ok(json) = serde_json::to_string_pretty(&map) {
        if let Err(e) = std::fs::write(path, json) {
            tracing::warn!("[Profit] Failed to save profit stats: {}", e);
        }
    }
}

/// Return the current Unix timestamp in seconds.
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Load the session-times map from disk.  Gracefully handles the **old**
/// format (`{ign: u64}`) by treating those entries as expired (secs=0).
fn load_session_times(path: &std::path::Path) -> HashMap<String, SessionTimeEntry> {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return HashMap::new(),
    };
    // Try new format first.
    if let Ok(map) = serde_json::from_str::<HashMap<String, SessionTimeEntry>>(&raw) {
        return map;
    }
    // Fallback: old `{ign: u64}` format — treat all entries as expired.
    if let Ok(old) = serde_json::from_str::<HashMap<String, u64>>(&raw) {
        return old
            .into_iter()
            .map(|(k, _)| (k, SessionTimeEntry { secs: 0, saved_at: 0 }))
            .collect();
    }
    HashMap::new()
}

/// Persist the accumulated session time for a given IGN.
/// Existing entries for other accounts are preserved.
fn save_session_time(path: &std::path::Path, ign: &str, total_secs: u64) {
    let mut times = load_session_times(path);
    times.insert(
        ign.to_string(),
        SessionTimeEntry {
            secs: total_secs,
            saved_at: unix_now(),
        },
    );
    if let Ok(json) = serde_json::to_string_pretty(&times) {
        if let Err(e) = std::fs::write(path, json) {
            tracing::warn!("[SessionTime] Failed to save session times: {}", e);
        }
    }
}

/// Path to the rest-break marker sidecar (next to the executable).
fn rest_break_marker_path() -> std::path::PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("rest_break.json")))
        .unwrap_or_else(|| std::path::PathBuf::from("rest_break.json"))
}

/// Persist a "resting until this UNIX time" marker so a humanization rest break
/// is honored across the process restart that starts it. The bot stays FULLY
/// offline (no Hypixel, no COFL) until the deadline instead of lingering
/// in-process, where the ECS client / AFK handler / reconnect loop could drift
/// it back into the lobby.
fn write_rest_break_marker(ign: &str, until_unix: u64) {
    let v = serde_json::json!({ "ign": ign, "until": until_unix });
    if let Err(e) = std::fs::write(rest_break_marker_path(), v.to_string()) {
        tracing::warn!("[Humanization] Failed to write rest-break marker: {}", e);
    }
}

/// Remaining seconds to stay offline for a pending rest break for `ign`, if any.
/// Returns `None` when there is no marker, it belongs to a different account, or
/// the break has already elapsed.
fn pending_rest_break_secs(ign: &str) -> Option<u64> {
    let contents = std::fs::read_to_string(rest_break_marker_path()).ok()?;
    let v: serde_json::Value = serde_json::from_str(&contents).ok()?;
    // Only honor a marker for THIS account — a different configured account must
    // not inherit a leftover break.
    if v.get("ign").and_then(|x| x.as_str()) != Some(ign) {
        return None;
    }
    let until = v.get("until").and_then(|x| x.as_u64())?;
    let now = unix_now();
    (until > now).then(|| until - now)
}

/// Delete the rest-break marker (break finished or no longer applicable).
fn clear_rest_break_marker() {
    let _ = std::fs::remove_file(rest_break_marker_path());
}

/// Clear the session time for a given IGN (reset to 0).
/// Used on account switch so the outgoing account starts fresh next time.
fn clear_session_time(path: &std::path::Path, ign: &str) {
    let mut times = load_session_times(path);
    times.remove(ign);
    if let Ok(json) = serde_json::to_string_pretty(&times) {
        let _ = std::fs::write(path, json);
    }
}

/// Print a colorful ANSI startup banner to the terminal.
fn print_startup_banner() {
    const C: &str = "\x1b[96m"; // aqua
    const G: &str = "\x1b[93m"; // gold
    const D: &str = "\x1b[90m"; // dim
    const R: &str = "\x1b[0m";
    println!("{C}╔════════════════════════════════════════════╗{R}");
    println!("{C}║   {G}🐟 Frikadellen BAF{C}  —  Auction Flipper       ║{R}");
    println!("{C}║   {D}Hypixel Skyblock bazaar + AH automation{C}   ║{R}");
    println!("{C}╚════════════════════════════════════════════╝{R}");
    println!("{D}   v{VERSION}{R}");
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    init_logger()?;

    // When this process is a managed VPS instance the backend launches it with a
    // vps_secret plus the owning user/instance identity in the environment. In
    // that case prefix every log line with `userId:instanceId` so a host running
    // several users' bots produces logs that can be told apart per user.
    if std::env::var("VPS_SECRET").ok().filter(|s| !s.is_empty()).is_some() {
        let user_id = std::env::var("USER_ID")
            .or_else(|_| std::env::var("OWNER_ID"))
            .unwrap_or_default();
        let instance_id = std::env::var("INSTANCE_ID").unwrap_or_default();
        if !user_id.is_empty() || !instance_id.is_empty() {
            frikadellen_baf::logging::set_vps_log_prefix(&user_id, &instance_id);
        }
    }

    print_startup_banner();
    info!("Starting Frikadellen BAF v{}", VERSION);

    // Check for outdated version (non-loader users).
    // Runs synchronously before the main loop so the warning appears at the very top of the log.
    check_version_outdated().await;

    // Load or create configuration
    let config_loader = Arc::new(ConfigLoader::new());
    let mut config = config_loader.load()?;

    // Prompt for username if not set
    if config.ingame_name.is_none() {
        let name: String = Input::new()
            .with_prompt("Enter your ingame name(s) (comma-separated for multiple accounts)")
            .interact_text()?;
        config.ingame_name = Some(name);
        config_loader.save(&config)?;
    }

    // Ensure a stable instance id so the central backend recognises this bot
    // across reconnects.
    if config.instance_id.is_none() {
        config.instance_id = Some(uuid::Uuid::new_v4().to_string());
        config_loader.save(&config)?;
    }

    // AH/Bazaar flip enable/disable is now handled automatically by COFL
    // based on user settings — no need for local toggles or prompts.
    // We still accept the config values for backward compatibility, but they
    // are effectively always enabled.

    // Prompt for webhook URL if not yet configured (matches TypeScript configHelper.ts pattern
    // of adding new default values to existing config on first run of newer version)
    if config.webhook_url.is_none() {
        let wants_webhook = Confirm::new()
            .with_prompt("Configure Discord webhook for notifications? (optional)")
            .default(false)
            .interact()?;
        if wants_webhook {
            let url: String = Input::new()
                .with_prompt("Enter Discord webhook URL")
                .interact_text()?;
            config.webhook_url = Some(url);
        } else {
            // Mark as configured (empty = disabled) so we don't ask again
            config.webhook_url = Some(String::new());
        }
        config_loader.save(&config)?;
    }

    // Prompt for Discord ID if not yet configured (for pinging on legendary/divine flips and bans)
    if config.discord_id.is_none() {
        let wants_discord_id = Confirm::new()
            .with_prompt("Configure Discord user ID for ping notifications? (optional)")
            .default(false)
            .interact()?;
        if wants_discord_id {
            let id: String = Input::new()
                .with_prompt("Enter your Discord user ID")
                .interact_text()?;
            config.discord_id = Some(id);
        } else {
            config.discord_id = Some(String::new());
        }
        config_loader.save(&config)?;
    }

    // Resolve the active ingame name.
    // When multiple names are configured, the account index is advanced at runtime by the
    // account-switching timer (see below) and the process restarts with exit(0) so that an
    // external supervisor (systemd, a shell loop, etc.) launches the next iteration.
    // We persist the current index in a small sidecar file next to the config so the next
    // invocation knows which account to start with.
    let ingame_names = config.ingame_names();
    if ingame_names.is_empty() {
        anyhow::bail!("No ingame name configured — please set ingame_name in config.toml");
    }

    // Read and advance the stored account index (wraps around the list).
    let account_index_path = match std::env::current_exe() {
        Ok(p) => p.parent().map(|d| d.join("account_index")).unwrap_or_else(|| std::path::PathBuf::from("account_index")),
        Err(_) => std::path::PathBuf::from("account_index"),
    };

    let current_account_index: usize = if ingame_names.len() > 1 {
        match std::fs::read_to_string(&account_index_path) {
            Ok(s) => s.trim().parse::<usize>().unwrap_or(0) % ingame_names.len(),
            Err(_) => 0,
        }
    } else {
        0
    };

    let ingame_name = ingame_names[current_account_index].clone();

    // ---- Session time persistence ----
    // Load the accumulated running time for this account from a sidecar JSON file.
    // Only carry over previous time if the last save was recent (within
    // MAX_SESSION_GAP_SECS), i.e. the user quickly restarted the macro.
    // Account switches and long pauses reset uptime to 0.
    let session_times_path = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("session_times.json")))
        .unwrap_or_else(|| std::path::PathBuf::from("session_times.json"));
    // Sidecar file for persisting AH/BZ profit totals across restarts (e.g. rest breaks).
    let profit_path = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("profit_stats.json")))
        .unwrap_or_else(|| std::path::PathBuf::from("profit_stats.json"));
    let previous_session_secs: u64 = {
        let times = load_session_times(&session_times_path);
        if let Some(entry) = times.get(&ingame_name) {
            let now = unix_now();
            let gap = now.saturating_sub(entry.saved_at);
            if gap <= MAX_SESSION_GAP_SECS {
                entry.secs
            } else {
                info!("Session gap for {} is {}s (>{} max) — starting fresh session",
                    ingame_name, gap, MAX_SESSION_GAP_SECS);
                0
            }
        } else {
            0
        }
    };
    if previous_session_secs > 0 {
        info!("Resumed session for {} — previous accumulated time: {}s ({:.2}h)",
            ingame_name, previous_session_secs, previous_session_secs as f64 / 3600.0);
    }

    // ── Honor a pending humanization rest break ─────────────────────────────
    // A rest break restarts the process with a break-until marker; here, on the
    // fresh start, we wait out the remaining break BEFORE connecting to Hypixel
    // or COFL so the account is genuinely offline for the whole duration (rather
    // than lingering in-process where it could drift back into the lobby).
    if let Some(remaining) = pending_rest_break_secs(&ingame_name) {
        info!(
            "[Humanization] Resuming rest break — staying offline for {:.1}m before connecting",
            remaining as f64 / 60.0
        );
        tokio::time::sleep(Duration::from_secs(remaining)).await;
        info!("[Humanization] Rest break over — connecting");
        if let Some(url) = config.active_webhook_url() {
            frikadellen_baf::webhook::send_webhook_rest_break_end(&ingame_name, url).await;
        }
    }
    clear_rest_break_marker();

    info!("Configuration loaded for player: {} (account {}/{})", ingame_name, current_account_index + 1, ingame_names.len());
    info!("AH Flips: {}", if config.enable_ah_flips { "ENABLED" } else { "DISABLED" });
    info!("Bazaar Flips: {}", if config.enable_bazaar_flips { "ENABLED" } else { "DISABLED" });
    info!("Web GUI Port: {}", config.web_gui_port);

    // Check whether the web GUI port is allowed through UFW (Linux firewall).
    // Only warn on Linux where UFW is commonly used.
    #[cfg(target_os = "linux")]
    check_ufw_port(config.web_gui_port);

    if config.proxy_enabled {
        info!("Proxy: ENABLED — address: {:?}", config.proxy_address);
    }

    // Initialize command queue
    let command_queue = CommandQueue::new();

    // Bazaar-flip pause flag (matches TypeScript bazaarFlipPauser.ts).
    // Set to true for 20 seconds when a `countdown` message arrives (AH flips incoming).
    let bazaar_flips_paused = Arc::new(AtomicBool::new(false));
    // Unix-millis deadline until which the bazaar stays paused. Repeated rapid
    // `countdown` messages just extend this deadline instead of each spawning a
    // resume task (which spammed "Bazaar flips resumed" and churned pause state).
    let bazaar_pause_until = Arc::new(std::sync::atomic::AtomicU64::new(0));

    // Master macro pause — web panel can set this to pause all command processing.
    let macro_paused = Arc::new(AtomicBool::new(false));

    // Runtime enable flags for AH / Bazaar flipping.  Initialized from config
    // (both default to true) so an explicit `enable_*_flips = false` is honored:
    // when bazaar flipping is disabled the bot must not run the destructive
    // startup order management or auto-manage orders on fills — it only checks
    // existing orders.  The web panel can still toggle these at runtime.
    let enable_ah_flips = Arc::new(AtomicBool::new(config.enable_ah_flips));
    let enable_bazaar_flips = Arc::new(AtomicBool::new(config.enable_bazaar_flips));
    // Transient pause flag flipped by the web panel's Disconnect button.
    // When true the COFL WS event loop below drops incoming flips instead
    // of queueing them.  Cleared by the Connect button (or process restart).
    let flip_intake_paused = Arc::new(AtomicBool::new(false));
    let anonymize_webhook_name = Arc::new(AtomicBool::new(false));

    // Broadcast channel for chat messages → web panel clients.
    let (chat_tx, _chat_rx) = broadcast::channel::<String>(256);

    // Flip tracker: stores pending/active AH flips for profit reporting in webhooks.
    // Key = clean item_name (lowercase), value = (flip, actual_buy_price, purchase_time).
    // buy_price starts at 0 until ItemPurchased fires and sets it to the real price.
    let flip_tracker: FlipTrackerMap = Arc::new(Mutex::new(HashMap::new()));

    // Coflnet connection ID — parsed from "Your connection id is XXXX" chat message.
    // Included in startup webhooks (matches TypeScript getCoflnetPremiumInfo().connectionId).
    let cofl_connection_id: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    // Coflnet premium info — parsed from "You have PremiumPlus until ..." writeToChat message.
    // Tuple: (tier, expires_str) e.g. ("Premium Plus", "2026-Feb-10 08:55 UTC").
    let cofl_premium: Arc<Mutex<Option<(String, String)>>> = Arc::new(Mutex::new(None));

    // Auto-detected COFL license index for the first account's IGN.
    // Populated at startup by requesting `/cofl licenses list <first_ign>` and
    // parsing the `N>` numbered index from the response.
    // 0 means no license detected.
    let detected_cofl_license = Arc::new(std::sync::atomic::AtomicU32::new(0));

    // Coflnet authentication flag — set to true when the COFL "Hello <IGN>"
    // message is received. Flip processing is blocked until this is true so the
    // bot does not attempt purchases before COFL auth is complete.
    let cofl_authenticated = Arc::new(AtomicBool::new(false));

    // Set once after auto-sending `/cofl license default <ign>` to prevent repeat attempts.
    // For single-account setups, skip license management entirely (not needed).
    let license_default_sent = Arc::new(AtomicBool::new(ingame_names.len() == 1));

    // Get or generate session ID for Coflnet (matching TypeScript coflSessionManager.ts)
    let session_id = if let Some(session) = config.sessions.get(&ingame_name) {
        // Check if session is expired
        if session.expires < chrono::Utc::now() {
            // Session expired, generate new one
            info!("Session expired for {}, generating new session ID", ingame_name);
            let new_id = uuid::Uuid::new_v4().to_string();
            let new_session = frikadellen_baf::config::types::CoflSession {
                id: new_id.clone(),
                expires: chrono::Utc::now() + chrono::Duration::days(180), // 180 days like TypeScript
            };
            config.sessions.insert(ingame_name.clone(), new_session);
            config_loader.save(&config)?;
            new_id
        } else {
            // Session still valid
            info!("Using existing session ID for {}", ingame_name);
            session.id.clone()
        }
    } else {
        // No session exists, create new one
        info!("No session found for {}, generating new session ID", ingame_name);
        let new_id = uuid::Uuid::new_v4().to_string();
        let new_session = frikadellen_baf::config::types::CoflSession {
            id: new_id.clone(),
            expires: chrono::Utc::now() + chrono::Duration::days(180), // 180 days like TypeScript
        };
        config.sessions.insert(ingame_name.clone(), new_session);
        config_loader.save(&config)?;
        new_id
    };

    info!("Connecting to Coflnet WebSocket...");
    
    // Connect to Coflnet WebSocket
    let (ws_client, ws_rx_primary) = CoflWebSocket::connect(
        config.websocket_url.clone(),
        ingame_name.clone(),
        VERSION.to_string(),
        session_id.clone(),
    ).await?;

    info!("WebSocket connected successfully");

    // ── Multisocket ─────────────────────────────────────────────────────────
    // Merge the primary socket with any extra modsocket connections from
    // `multisocket_urls`. Auction flips from ALL sockets are deduped by UUID so
    // the first socket to deliver wins; later duplicates are dropped before
    // they reach the flip handler. Secondary sockets contribute ONLY auction
    // flips — chat, commands, auth and bazaar flips stay primary-only so
    // nothing else doubles up. With an empty `multisocket_urls` this is a
    // plain pass-through of the primary socket.
    // Latest account-status JSON pushed to finder feeds (never COFL) when
    // `finder_report_purse` is on, so the finder can size flips to this account
    // and avoid overfilling it. Populated by a task spawned once bot_client
    // exists; each finder-feed writer relays whatever is current. Declared out
    // here so it outlives the ws_rx setup block below and is visible when the
    // updater task is spawned later.
    let finder_status: std::sync::Arc<std::sync::Mutex<Option<String>>> =
        std::sync::Arc::new(std::sync::Mutex::new(None));

    let mut ws_rx = {
        use frikadellen_baf::websocket::CoflEvent;

        // Bounded FIFO of recently-seen flip UUIDs shared by all forwarders.
        type SeenFlips = std::sync::Mutex<(std::collections::HashSet<String>, std::collections::VecDeque<String>)>;
        const SEEN_FLIPS_CAP: usize = 512;
        fn flip_already_seen(seen: &SeenFlips, uuid: &str) -> bool {
            let Ok(mut g) = seen.lock() else { return false };
            let (set, order) = &mut *g;
            if !set.insert(uuid.to_string()) {
                return true;
            }
            order.push_back(uuid.to_string());
            if order.len() > SEEN_FLIPS_CAP {
                if let Some(old) = order.pop_front() {
                    set.remove(&old);
                }
            }
            false
        }

        let (agg_tx, agg_rx) = tokio::sync::mpsc::unbounded_channel::<CoflEvent>();
        let seen: Arc<SeenFlips> = Arc::new(std::sync::Mutex::new((
            std::collections::HashSet::new(),
            std::collections::VecDeque::new(),
        )));

        // Primary: forward everything, deduping auction flips.
        {
            let agg_tx = agg_tx.clone();
            let seen = seen.clone();
            let mut rx = ws_rx_primary;
            tokio::spawn(async move {
                while let Some(ev) = rx.recv().await {
                    if let CoflEvent::AuctionFlip(ref f) = ev {
                        if let Some(u) = f.uuid.as_deref() {
                            if flip_already_seen(&seen, u) {
                                debug!("[Multisocket] Duplicate flip {} (primary) — dropped", u);
                                continue;
                            }
                        }
                    }
                    if agg_tx.send(ev).is_err() {
                        break;
                    }
                }
            });
        }

        // Secondary sockets: auction flips only. Entries that are NOT Coflnet
        // modsockets (no "coflnet"/"/modsocket" in the URL) are treated as
        // baf-flip-finder feeds — so the private finder can simply be added to
        // `multisocket_urls` next to COFL (e.g. "ws://127.0.0.1:15101").
        let mut finder_feed_urls: Vec<String> = Vec::new();
        for url in config.multisocket_urls.clone() {
            let url_trim = url.trim().to_string();
            if url_trim.is_empty() || url_trim == config.websocket_url {
                continue;
            }
            if !url_trim.contains("coflnet") && !url_trim.contains("/modsocket") {
                finder_feed_urls.push(url_trim);
                continue;
            }
            match CoflWebSocket::connect(
                url_trim.clone(),
                ingame_name.clone(),
                VERSION.to_string(),
                session_id.clone(),
            )
            .await
            {
                Ok((_extra_client, mut rx)) => {
                    info!("[Multisocket] Connected extra COFL socket: {}", url_trim);
                    let agg_tx = agg_tx.clone();
                    let seen = seen.clone();
                    let url_log = url_trim.clone();
                    tokio::spawn(async move {
                        while let Some(ev) = rx.recv().await {
                            if let CoflEvent::AuctionFlip(ref f) = ev {
                                if let Some(u) = f.uuid.as_deref() {
                                    if flip_already_seen(&seen, u) {
                                        debug!("[Multisocket] Duplicate flip {} ({}) — dropped", u, url_log);
                                        continue;
                                    }
                                }
                                info!("[Multisocket] Flip won by {}", url_log);
                                if agg_tx.send(ev).is_err() {
                                    break;
                                }
                            }
                            // Everything else from secondary sockets is dropped.
                        }
                    });
                }
                Err(e) => {
                    warn!("[Multisocket] Failed to connect extra socket {}: {} — continuing without it", url_trim, e);
                }
            }
        }

        // ── baf-flip-finder feeds ────────────────────────────────────────
        // Our own finder pushes flips over a plain websocket the instant it
        // finds them. They enter the same pipeline as COFL flips (identical
        // Flip struct, same UUID dedupe — whichever source is first wins),
        // so purchasing, tracking, target-based listing and webhooks all
        // work unchanged. Auto-reconnects with backoff. Sources: non-COFL
        // `multisocket_urls` entries and/or the explicit `finder_ws_url`.
        if let Some(u) = config.finder_ws_url.clone().filter(|u| !u.trim().is_empty()) {
            if !finder_feed_urls.contains(&u) {
                finder_feed_urls.push(u);
            }
        }
        for finder_url in finder_feed_urls {
            let agg_tx = agg_tx.clone();
            let seen = seen.clone();
            let token = config.finder_ws_token.clone().unwrap_or_default();
            let finder_status = finder_status.clone();
            let report_purse = config.finder_report_purse;
            tokio::spawn(async move {
                use futures::{SinkExt, StreamExt};
                let mut backoff = 5u64;
                loop {
                    let full_url = if token.is_empty() {
                        finder_url.clone()
                    } else {
                        format!("{}?token={}", finder_url.trim_end_matches('/'), token)
                    };
                    match tokio_tungstenite::connect_async(&full_url).await {
                        Ok((stream, _)) => {
                            info!("[FinderWS] Connected to flip finder: {}", finder_url);
                            backoff = 5;
                            let (mut write, mut read) = stream.split();
                            // Push account status to the finder on a cadence (only
                            // what changed) so it can size flips to this account.
                            // Finder feeds only — this loop never runs on COFL.
                            let mut status_tick = tokio::time::interval(std::time::Duration::from_secs(5));
                            let mut last_status_sent: Option<String> = None;
                            loop {
                                tokio::select! {
                                    _ = status_tick.tick(), if report_purse => {
                                        let cur = finder_status.lock().ok().and_then(|g| g.clone());
                                        if let Some(json) = cur {
                                            if last_status_sent.as_ref() != Some(&json) {
                                                if write.send(tokio_tungstenite::tungstenite::Message::Text(json.clone())).await.is_err() {
                                                    break;
                                                }
                                                last_status_sent = Some(json);
                                            }
                                        }
                                    }
                                    msg = read.next() => {
                                        let Some(msg) = msg else { break };
                                        let txt = match msg {
                                            Ok(tokio_tungstenite::tungstenite::Message::Text(t)) => t,
                                            Ok(tokio_tungstenite::tungstenite::Message::Close(_)) | Err(_) => break,
                                            _ => continue,
                                        };
                                        let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) else { continue };
                                        if v.get("type").and_then(|t| t.as_str()) != Some("flip") {
                                            continue;
                                        }
                                        let Some(f) = v.get("flip") else { continue };
                                        let uuid = f.get("uuid").and_then(|u| u.as_str()).map(String::from);
                                        let Some(u) = uuid.as_deref() else { continue };
                                        if flip_already_seen(&seen, u) {
                                            debug!("[FinderWS] Duplicate flip {} — dropped (COFL was first)", u);
                                            continue;
                                        }
                                        let flip = frikadellen_baf::types::Flip {
                                            item_name: f.get("itemName").and_then(|x| x.as_str()).unwrap_or("?").to_string(),
                                            starting_bid: f.get("price").and_then(|x| x.as_u64()).unwrap_or(0),
                                            target: f.get("target").and_then(|x| x.as_u64()).unwrap_or(0),
                                            finder: Some("BAF_FINDER".to_string()),
                                            profit_perc: f.get("roiPct").and_then(|x| x.as_f64()),
                                            purchase_at_ms: None,
                                            uuid,
                                            list_at: f.get("listAt").and_then(|x| x.as_u64()),
                                        };
                                        if flip.starting_bid == 0 || flip.target == 0 {
                                            continue;
                                        }
                                        info!(
                                            "[FinderWS] Flip: {} for {} (target {}, conf {})",
                                            flip.item_name,
                                            flip.starting_bid,
                                            flip.target,
                                            f.get("confidence").and_then(|x| x.as_f64()).unwrap_or(0.0)
                                        );
                                        if agg_tx.send(CoflEvent::AuctionFlip(flip)).is_err() {
                                            return;
                                        }
                                    }
                                }
                            }
                            warn!("[FinderWS] Disconnected — reconnecting...");
                        }
                        Err(e) => {
                            warn!("[FinderWS] Connect failed: {} (retry in {}s)", e, backoff);
                        }
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(backoff)).await;
                    backoff = (backoff * 2).min(60);
                }
            });
        }

        agg_rx
    };

    // Send "initialized" webhook notification
    if let Some(webhook_url) = config.active_webhook_url() {
        let url = webhook_url.to_string();
        let name = ingame_name.clone();
        let ah = config.enable_ah_flips;
        let bz = config.enable_bazaar_flips;
        // Connection ID and premium may not be available yet at startup (COFL sends them shortly
        // after WS connect), so we delay 3s to give COFL time to send those messages first.
        let conn_id_init = cofl_connection_id.clone();
        let premium_init = cofl_premium.clone();
        tokio::spawn(async move {
            tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
            let conn_id = conn_id_init.lock().ok().and_then(|g| g.clone());
            let premium = premium_init.lock().ok().and_then(|g| g.clone());
            frikadellen_baf::webhook::send_webhook_initialized(&name, ah, bz, conn_id.as_deref(), premium.as_ref().map(|(t, e)| (t.as_str(), e.as_str())), &url).await;
        });
    }

    // When multi-account is enabled, request the COFL licenses list at startup
    // searching by the current account's IGN so we get its global license index.
    // Delay slightly to let the WS authenticate first.
    if ingame_names.len() > 1 {
        let ws_license = ws_client.clone();
        let current_ign = ingame_name.clone();
        tokio::spawn(async move {
            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
            let args = format!("list {}", current_ign);
            let data_json = serde_json::json!(args).to_string();
            let message = serde_json::json!({
                "type": "licenses",
                "data": data_json
            }).to_string();
            if let Err(e) = ws_license.send_message(&message).await {
                warn!("[LicenseDetect] Failed to request licenses list: {}", e);
            } else {
                info!("[LicenseDetect] Requested COFL licenses list for '{}'", current_ign);
            }
        });
    }

    // Initialize bot client (not connected yet — web server starts first so
    // the chat GUI is available during Microsoft auth)
    let mut bot_client = BotClient::new();
    bot_client.set_auto_cookie_hours(config.auto_cookie);
    bot_client.bedtiming = config.bedtiming_enabled();
    bot_client.skip = config.skip_enabled();
    bot_client.bed_spam_click_delay = config.bed_spam_click_delay;
    bot_client.bed_pre_click_ms = config.bed_pre_click_ms;
    bot_client.bazaar_order_cancel_minutes_per_million = config.bazaar_order_cancel_minutes_per_million;
    bot_client.bazaar_flips_paused = bazaar_flips_paused.clone();
    bot_client.enable_bazaar_flips = enable_bazaar_flips.clone();
    bot_client.set_command_queue(command_queue.clone());
    *bot_client.ingame_name.write() = ingame_name.clone();

    // Account-status reporter for the finder feeds. Every 5s it snapshots the
    // live purse, coins locked in active listings, free inventory slots and
    // active-auction count into `finder_status`; the finder-feed writers relay
    // it. Only runs when finder_report_purse is on, and it only reaches finder
    // sockets — the purse never goes to COFL. Skips ticks where the purse can't
    // be read yet (scoreboard not parsed) so a bogus 0 never looks "broke".
    if config.finder_report_purse {
        let bc = bot_client.clone();
        let status_holder = finder_status.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(5));
            loop {
                tick.tick().await;
                let Some(purse) = bc.get_purse() else { continue };
                let (locked, auctions) = bc.active_listing_value_and_count();
                let inv_free = bc.empty_slot_count();
                let worth = purse.saturating_add(locked);
                let json = format!(
                    "{{\"type\":\"purse\",\"purse\":{},\"locked\":{},\"worth\":{},\"invFree\":{},\"auctions\":{}}}",
                    purse, locked, worth, inv_free, auctions
                );
                if let Ok(mut g) = status_holder.lock() {
                    *g = Some(json);
                }
            }
        });
    }

    // Shared profit tracker for AH and Bazaar realized profits.
    // Restore persisted totals from disk so profit statistics survive rest breaks.
    let profit_tracker = {
        let tracker = Arc::new(frikadellen_baf::profit::ProfitTracker::new());
        let saved = load_profit_stats(&profit_path);
        if let Some(entry) = saved.get(&ingame_name) {
            // Same rule as session time: only a QUICK restart resumes the
            // totals. A long pause starts the session profit at 0 — this was
            // previously unconditional, so profit never reset at all.
            let gap = unix_now().saturating_sub(entry.saved_at);
            if gap <= MAX_SESSION_GAP_SECS {
                if entry.ah_total != 0 {
                    tracker.set_ah_total(entry.ah_total);
                    info!("[Profit] Restored AH profit from disk: {} coins", entry.ah_total);
                }
                if entry.bz_total != 0 {
                    tracker.set_bz_total(entry.bz_total);
                    info!("[Profit] Restored BZ profit from disk: {} coins", entry.bz_total);
                }
            } else {
                info!("[Profit] Session gap {}s (> {}s) — profit starts fresh at 0", gap, MAX_SESSION_GAP_SECS);
            }
        }
        tracker
    };

    // Shared tracker for active bazaar orders (web panel + profit calculation).
    let bazaar_tracker = Arc::new(frikadellen_baf::bazaar_tracker::BazaarOrderTracker::new());

    // ── Central backend (baf-backend) gateway ───────────────────────────────
    // Dial out to the shared backend for remote control + profit tracking and
    // show the one-time Discord link code. A no-op when disabled/unconfigured.
    let backend_handle = if config.backend_enabled {
        // Already-owned (discord_id set in config) → no link needed; otherwise the
        // terminal re-prints the code so it isn't lost in the startup scroll.
        let already_linked = config.active_discord_id().is_some();
        // Only mint a link code when this instance is NOT yet linked. Sending a
        // fresh code on every startup of an already-linked bot makes the backend
        // register a brand-new pending/unlinked row per reconnect — that's what
        // produces the pile of duplicate `unlinked` rows for the same IGN. The
        // stable `instance_id` already identifies the bot across reconnects, so a
        // linked instance announces no code at all (`linkCode: null`).
        let link_code = if already_linked {
            String::new()
        } else {
            let raw = uuid::Uuid::new_v4().simple().to_string();
            raw[..6].to_uppercase()
        };
        let linked = Arc::new(AtomicBool::new(already_linked));
        let handle = frikadellen_baf::backend::spawn(frikadellen_baf::backend::BackendDeps {
            url: config.backend_url.clone(),
            instance_id: config.instance_id.clone().unwrap_or_default(),
            cofl_owner_id: None,
            ingame_names: ingame_names.clone(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            link_code: link_code.clone(),
            discord_id: config.active_discord_id().map(|s| s.to_string()),
            allowed_ids: config.backend_allowed_ids_list(),
            macro_paused: macro_paused.clone(),
            command_queue: command_queue.clone(),
            bot_client: bot_client.clone(),
            profit_tracker: profit_tracker.clone(),
            config_loader: config_loader.clone(),
            linked: linked.clone(),
            enable_ah_flips: enable_ah_flips.clone(),
            enable_bazaar_flips: enable_bazaar_flips.clone(),
        });
        if !already_linked {
            // Prominent boxed banner so the code stands out, then re-print it
            // periodically until the bot is linked.
            let chat_tx_link = chat_tx.clone();
            tokio::spawn(async move {
                let mut first = true;
                loop {
                    if linked.load(Ordering::Relaxed) {
                        break;
                    }
                    let banner = format!(
                        "§f[§4BAF§f]: §b╔══════════════════════════════════════╗\n\
                         §f[§4BAF§f]: §b║  §eDISCORD LINK CODE: §6§l{:<8}§r§b       ║\n\
                         §f[§4BAF§f]: §b║  §7Run §f/link {}§7 in Discord{}║\n\
                         §f[§4BAF§f]: §b╚══════════════════════════════════════╝",
                        link_code,
                        link_code,
                        " ".repeat(11usize.saturating_sub(link_code.len()))
                    );
                    print_mc_chat(&banner);
                    let _ = chat_tx_link.send(banner);
                    info!("[Backend] Discord link code: {} (run /link {} in Discord)", link_code, link_code);
                    // Re-show sooner the first time (right after startup scroll), then every 2 min.
                    let wait = if first { 20 } else { 120 };
                    first = false;
                    tokio::time::sleep(tokio::time::Duration::from_secs(wait)).await;
                }
            });
        }
        handle
    } else {
        frikadellen_baf::backend::BackendHandle::disabled()
    };

    // Occasional friendly "stay hydrated" reminder at a random 1min–2h interval.
    {
        let chat_tx_hydrate = chat_tx.clone();
        tokio::spawn(async move {
            use rand::Rng;
            const MESSAGES: [&str; 4] = [
                "§b💧 Stay hydrated — take a sip of water!",
                "§b💧 Hydration check: go drink some water 🚰",
                "§b💧 Quick reminder: water break! 💙",
                "§b💧 Don't forget to drink water while you flip.",
            ];
            loop {
                let secs = rand::rng().random_range(60..=7200);
                tokio::time::sleep(tokio::time::Duration::from_secs(secs)).await;
                let pick = MESSAGES[rand::rng().random_range(0..MESSAGES.len())];
                let msg = format!("§f[§4BAF§f]: {}", pick);
                print_mc_chat(&msg);
                let _ = chat_tx_hydrate.send(msg);
            }
        });
    }

    // Start web control panel server BEFORE bot connect so the chat GUI
    // is available to show login links during Microsoft/Coflnet auth.
    {
        let web_state = WebSharedState {
            bot_client: bot_client.clone(),
            command_queue: command_queue.clone(),
            ws_client: ws_client.clone(),
            bazaar_flips_paused: bazaar_flips_paused.clone(),
            macro_paused: macro_paused.clone(),
            enable_ah_flips: enable_ah_flips.clone(),
            enable_bazaar_flips: enable_bazaar_flips.clone(),
            flip_intake_paused: flip_intake_paused.clone(),
            ingame_names: ingame_names.clone(),
            current_account_index,
            account_index_path: account_index_path.clone(),
            chat_tx: chat_tx.clone(),
            web_gui_password: config.web_gui_password.clone(),
            valid_sessions: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashSet::new(),
            )),
            player_uuid: std::sync::Arc::new(tokio::sync::RwLock::new(None)),
            started_at: std::time::Instant::now(),
            previous_session_secs,
            hypixel_api_key: config.hypixel_api_key.clone(),
            detected_cofl_license: detected_cofl_license.clone(),
            profit_tracker: profit_tracker.clone(),
            anonymize_webhook_name: anonymize_webhook_name.clone(),
            bazaar_tracker: bazaar_tracker.clone(),
            config_loader: config_loader.clone(),
        };
        let web_port = config.web_gui_port;
        let web_tls = if config.web_https {
            Some(frikadellen_baf::web::WebTlsOptions {
                cert_path: config.web_tls_cert_path.clone(),
                key_path: config.web_tls_key_path.clone(),
            })
        } else {
            None
        };
        tokio::spawn(async move {
            frikadellen_baf::web::start_web_server_tls(web_state, web_port, web_tls).await;
        });
    }

    // If a VPS_SECRET environment variable is present, connect to the managed
    // hosting backend at wss://sky.coflnet.com/instances.  This allows the
    // SkyCofl backend to orchestrate instances running on this host.
    if let Some(vps_socket) = frikadellen_baf::vps::VpsSocket::from_env() {
        info!("[VPS] VPS_SECRET detected — starting managed hosting socket");
        tokio::spawn(async move {
            vps_socket.run().await;
        });
    }

    // Connect to Hypixel — Azalea will handle Microsoft OAuth (device-code URL
    // is printed to the terminal; the Coflnet auth link is sent via chat_tx and
    // appears in the web panel automatically).
    //
    // Retry with exponential backoff on auth failure.  Running without a
    // Minecraft connection is useless (no flips, no bazaar, nothing to do),
    // so after exhausting retries we restart the process to re-run the full
    // startup sequence (config reload, fresh WebSocket, etc.).
    info!("Initializing Minecraft bot...");
    info!("Authenticating with Microsoft account...");
    info!("A browser window will open for you to log in");
    {
        const AUTH_MAX_RETRIES: u32 = 3;
        const AUTH_INITIAL_BACKOFF_SECS: u64 = 10;

        let mut last_err: Option<String> = None;
        for attempt in 1..=AUTH_MAX_RETRIES {
            match bot_client.connect(ingame_name.clone(), Some(ws_client.clone())).await {
                Ok(_) => {
                    info!("Bot connection initiated successfully");
                    last_err = None;
                    break;
                }
                Err(e) => {
                    let backoff = AUTH_INITIAL_BACKOFF_SECS.saturating_mul(1u64 << (attempt - 1).min(5)); // 10s, 20s, 40s for 3 retries; .min(5) caps shift for safety
                    warn!(
                        "Failed to connect bot (attempt {}/{}): {} — retrying in {}s",
                        attempt, AUTH_MAX_RETRIES, e, backoff
                    );
                    let baf_msg = format!(
                        "§f[§4BAF§f]: §cAuth failed (attempt {}/{}): {} — retrying in {}s",
                        attempt, AUTH_MAX_RETRIES, e, backoff
                    );
                    print_mc_chat(&baf_msg);
                    let _ = chat_tx.send(baf_msg);
                    last_err = Some(format!("{}", e));
                    // Notify via Discord webhook so the user knows auth is failing
                    if let Some(webhook_url) = config.active_webhook_url() {
                        let err_str = format!("{}", e);
                        frikadellen_baf::webhook::send_webhook_auth_failed(
                            &ingame_name, attempt, AUTH_MAX_RETRIES, &err_str,
                            config.active_discord_id(), webhook_url,
                        ).await;
                    }
                    tokio::time::sleep(Duration::from_secs(backoff)).await;
                }
            }
        }
        if let Some(err) = last_err {
            error!(
                "All {} auth attempts failed (last error: {}) — restarting process",
                AUTH_MAX_RETRIES, err
            );
            // Send final "all attempts failed" webhook before restarting
            if let Some(webhook_url) = config.active_webhook_url() {
                frikadellen_baf::webhook::send_webhook_auth_failed(
                    &ingame_name, AUTH_MAX_RETRIES, AUTH_MAX_RETRIES, &err,
                    config.active_discord_id(), webhook_url,
                ).await;
            }
            let baf_msg = format!(
                "§f[§4BAF§f]: §cAll {} auth attempts failed — restarting...",
                AUTH_MAX_RETRIES
            );
            print_mc_chat(&baf_msg);
            let _ = chat_tx.send(baf_msg);
            // Short delay so the message is visible before restart.
            tokio::time::sleep(Duration::from_secs(2)).await;
            restart_process();
        }
    }

    // Spawn bot event handler
    let bot_client_clone = bot_client.clone();
    let ws_client_for_events = ws_client.clone();
    let config_for_events = config.clone();
    let command_queue_clone = command_queue.clone();
    let ingame_name_for_events = ingame_name.clone();
    let flip_tracker_events = flip_tracker.clone();
    let cofl_connection_id_events = cofl_connection_id.clone();
    let cofl_premium_events = cofl_premium.clone();
    let chat_tx_events = chat_tx.clone();
    let enable_bazaar_flips_events = enable_bazaar_flips.clone();
    let enable_ah_flips_events = enable_ah_flips.clone();
    let profit_tracker_events = profit_tracker.clone();
    let bazaar_tracker_events = bazaar_tracker.clone();
    let backend_handle_events = backend_handle.clone();
    // Tracks when the last AH auction was listed; the idle-inventory timer uses
    // this to detect 30-minute stalls and force `/cofl sellinventory`.
    let last_auction_listed_at: Arc<Mutex<Instant>> = Arc::new(Mutex::new(Instant::now()));
    let last_auction_listed_at_events = last_auction_listed_at.clone();
    let session_start = std::time::Instant::now();
    let prev_secs_events = previous_session_secs;
    tokio::spawn(async move {
        // Rate-limiting for presence notifications so a repeated Hypixel line or
        // a chat flood can't spam the webhook: per-visitor cooldown for island
        // visits, and a single global cooldown between name-mention pings.
        let mut last_visitor_ping: std::collections::HashMap<String, std::time::Instant> =
            std::collections::HashMap::new();
        let mut last_mention_ping: Option<std::time::Instant> = None;
        const VISITOR_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(60);
        const MENTION_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(10);
        while let Some(event) = bot_client_clone.next_event().await {
            match event {
                frikadellen_baf::bot::BotEvent::Login => {
                    info!("✓ Bot logged into Minecraft successfully");
                }
                frikadellen_baf::bot::BotEvent::Spawn => {
                    info!("✓ Bot spawned in world and ready");
                    mark_activity();
                }
                frikadellen_baf::bot::BotEvent::ChatMessage(msg) => {
                    // Print Minecraft chat with color codes converted to ANSI
                    print_mc_chat(&msg);
                    // Broadcast to web panel clients
                    let _ = chat_tx_events.send(msg.clone());

                    // Parse Coflnet profit response:
                    // "According to our data <ign> made <amount> in the last <days> days across <N> auctions"
                    let clean = frikadellen_baf::utils::remove_minecraft_colors(&msg);

                    // SkyBlock maintenance / downtime. When Hypixel is restarting
                    // SkyBlock, a join attempt is answered with a "maintenance"
                    // line — the server is temporarily down. Note it so the island
                    // guard stops bouncing off `/play sb` and the stall guard treats
                    // the quiet as expected. The window auto-clears, so the bot
                    // resumes on its own once SkyBlock is back.
                    if clean.to_lowercase().contains("maintenance") {
                        if note_maintenance() {
                            warn!(
                                "[Maintenance] SkyBlock appears to be down for maintenance — pausing rejoin for {}m",
                                MAINTENANCE_COOLDOWN_SECS / 60
                            );
                            let baf_msg = "§f[§4BAF§f]: §eSkyBlock is under maintenance — pausing rejoin until it's back up".to_string();
                            print_mc_chat(&baf_msg);
                            let _ = chat_tx_events.send(baf_msg);
                        }
                    }

                    if let Some(profit) = parse_cofl_profit_response(&clean) {
                        // `/cofl profit` is the REALIZED total. The panel now shows
                        // THEORETICAL AH profit (accumulated at purchase), so we log
                        // the realized figure for reference but do not overwrite the
                        // theoretical total with it.
                        tracing::info!("[CoflProfit] Realized AH total from Coflnet (not shown on panel): {} coins", profit);
                    }

                    // Parse `/cofl bz h` response for authoritative BZ session profit.
                    // "Total Profit: -234M" (inside "Bazaar Profit History for <ign> ...")
                    if let Some(bz_profit) = parse_cofl_bz_h_total_profit(&clean) {
                        profit_tracker_events.set_bz_total(bz_profit);
                        tracing::info!("[CoflBzH] Updated BZ total from /cofl bz h: {} coins", bz_profit);
                    }

                    // Detect bazaar daily sell value limit
                    if clean.contains("You reached the daily limit") && clean.contains("bazaar") {
                        warn!("[Bazaar] Daily sell value limit reached — disabling bazaar flips until 0:00 UTC");
                        // Send webhook notification
                        if let Some(webhook_url) = config_for_events.active_bazaar_webhook_url() {
                            let url = webhook_url.to_string();
                            let name = ingame_name_for_events.clone();
                            tokio::spawn(async move {
                                frikadellen_baf::webhook::send_webhook_bazaar_daily_limit(&name, &url).await;
                            });
                        }
                        // Schedule auto-clear of daily limit flag at next 0:00 UTC
                        let bot_for_reset = bot_client_clone.clone();
                        let chat_tx_dl = chat_tx_events.clone();
                        tokio::spawn(async move {
                            let midnight = frikadellen_baf::webhook::next_utc_midnight_unix();
                            let now = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs();
                            let secs_until_midnight = midnight.saturating_sub(now);
                            tracing::info!("[Bazaar] Scheduling daily-limit reset in {}s (0:00 UTC)", secs_until_midnight);
                            tokio::time::sleep(tokio::time::Duration::from_secs(secs_until_midnight + DAILY_LIMIT_RESET_BUFFER_SECS)).await;
                            bot_for_reset.clear_bazaar_daily_limit();
                            let reset_msg = "§f[§4BAF§f]: §aBazaar daily limit reset — flips re-enabled".to_string();
                            frikadellen_baf::logging::print_mc_chat(&reset_msg);
                            let _ = chat_tx_dl.send(reset_msg);
                            tracing::info!("[Bazaar] Daily limit reset — bazaar flips re-enabled");
                        });
                        let baf_msg = "§f[§4BAF§f]: §c⚠ Bazaar daily sell limit reached — flips disabled until 0:00 UTC".to_string();
                        frikadellen_baf::logging::print_mc_chat(&baf_msg);
                        let _ = chat_tx_events.send(baf_msg);
                    }

                    // ── Presence notifications (island visitors / name mentions) ──
                    // Someone visiting the bot's island: "[RANK] Name is visiting your island!"
                    if config_for_events.notify_island_visitors {
                        if let Some(visitor) = parse_island_visitor(&clean) {
                            let now = std::time::Instant::now();
                            let fresh = last_visitor_ping
                                .get(&visitor)
                                .map(|t| now.duration_since(*t) >= VISITOR_COOLDOWN)
                                .unwrap_or(true);
                            if fresh {
                                last_visitor_ping.insert(visitor.clone(), now);
                                info!("[Presence] {} is visiting the island", visitor);
                                if let Some(webhook_url) = config_for_events.active_webhook_url() {
                                    let url = webhook_url.to_string();
                                    let name = ingame_name_for_events.clone();
                                    let did = config_for_events.active_discord_id().map(|s| s.to_string());
                                    tokio::spawn(async move {
                                        frikadellen_baf::webhook::send_webhook_island_visitor(
                                            &name, &visitor, did.as_deref(), &url,
                                        ).await;
                                    });
                                }
                            }
                        }
                    }

                    // Bot's own Minecraft name mentioned by another player in chat.
                    if config_for_events.notify_name_mentions {
                        if let Some(line) = parse_name_mention(&clean, &ingame_name_for_events) {
                            let now = std::time::Instant::now();
                            let fresh = last_mention_ping
                                .map(|t| now.duration_since(t) >= MENTION_COOLDOWN)
                                .unwrap_or(true);
                            if fresh {
                                last_mention_ping = Some(now);
                                info!("[Presence] Name mentioned in chat: {}", line);
                                if let Some(webhook_url) = config_for_events.active_webhook_url() {
                                    let url = webhook_url.to_string();
                                    let name = ingame_name_for_events.clone();
                                    let did = config_for_events.active_discord_id().map(|s| s.to_string());
                                    tokio::spawn(async move {
                                        frikadellen_baf::webhook::send_webhook_name_mention(
                                            &name, &line, did.as_deref(), &url,
                                        ).await;
                                    });
                                }
                            }
                        }
                    }
                }
                frikadellen_baf::bot::BotEvent::WindowOpen(id, window_type, title) => {
                    debug!("Window opened: {} (ID: {}, Type: {})", title, id, window_type);
                    // Heartbeat for the stall guard: an opening GUI window is the
                    // clearest proof the game connection is alive and working.
                    mark_activity();

                    // When the "Bazaar Orders" or "Co-op Bazaar Orders" window
                    // opens, send the full window NBT data to COFL so bazaar
                    // order state stays in sync with the SkyCofl backend.
                    let title_lower = title.to_lowercase();
                    if title_lower.contains("bazaar orders") || title_lower.contains("co-op bazaar orders") {
                        let ws_upload = ws_client_for_events.clone();
                        let bot_upload = bot_client_clone.clone();
                        tokio::spawn(async move {
                            // Wait for ContainerSetContent to populate all slots.
                            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                            if let Some(window_json) = bot_upload.get_cached_window_json() {
                                // Verify the cached window is still the Bazaar Orders window.
                                // The macro may have clicked an order which opens "Order Options",
                                // overwriting the cached window JSON. Only upload if the title
                                // still matches "Bazaar Orders" (not "Order Options" or other windows).
                                let is_bazaar_orders = serde_json::from_str::<serde_json::Value>(&window_json)
                                    .ok()
                                    .and_then(|v| v.get("title").and_then(|t| t.as_str()).map(|s| s.to_lowercase()))
                                    .map(|t| t.contains("bazaar orders"))
                                    .unwrap_or(false);

                                if !is_bazaar_orders {
                                    tracing::debug!("[UploadBazaarOrders] Skipping upload — window changed from Bazaar Orders");
                                    return;
                                }

                                let msg = serde_json::json!({
                                    "type": "UploadBazaarOrders",
                                    "data": window_json
                                }).to_string();
                                if let Err(e) = ws_upload.send_message(&msg).await {
                                    tracing::warn!("[UploadBazaarOrders] Failed to send bazaar window data: {}", e);
                                } else {
                                    tracing::info!("[UploadBazaarOrders] Sent bazaar window data to COFL");
                                }
                            } else {
                                tracing::debug!("[UploadBazaarOrders] No cached window JSON available");
                            }
                        });
                    }
                }
                frikadellen_baf::bot::BotEvent::WindowClose => {
                    debug!("Window closed");
                    mark_activity();
                }
                frikadellen_baf::bot::BotEvent::Disconnected(reason) => {
                    warn!("Bot disconnected: {}", reason);
                    if is_ban_disconnect(&reason) {
                        error!("Ban detected — sending webhook and terminating process");
                        if let Some(webhook_url) = config_for_events.active_webhook_url() {
                            frikadellen_baf::webhook::send_webhook_banned(
                                &ingame_name_for_events,
                                &reason,
                                config_for_events.active_discord_id(),
                                webhook_url,
                            ).await;
                        }
                        frikadellen_baf::webhook::send_webhook_banned_public().await;
                        // Terminate immediately so we don't reconnect and re-send the webhook
                        std::process::exit(1);
                    }
                }
                frikadellen_baf::bot::BotEvent::Kicked(reason) => {
                    warn!("Bot kicked: {}", reason);
                }
                frikadellen_baf::bot::BotEvent::NoCookieDetected => {
                    error!("No booster cookie detected — sending webhook and terminating process");
                    if let Some(webhook_url) = config_for_events.active_webhook_url() {
                        frikadellen_baf::webhook::send_webhook_no_cookie(
                            &ingame_name_for_events,
                            config_for_events.active_discord_id(),
                            webhook_url,
                        ).await;
                    }
                    let baf_msg = "§f[§4BAF§f]: §c⚠ No booster cookie — please log in manually and buy one, then start the bot again.".to_string();
                    print_mc_chat(&baf_msg);
                    let _ = chat_tx_events.send(baf_msg);
                    // Terminate — the bot can't flip without a cookie
                    std::process::exit(1);
                }
                frikadellen_baf::bot::BotEvent::StartupComplete { orders_cancelled } => {
                    info!("[Startup] Startup complete - bot is ready to flip! ({} order(s) cancelled)", orders_cancelled);
                    mark_activity();
                    // Clear the bazaar order tracker for a clean slate ONLY when the
                    // startup ManageOrders cycle actually cancelled all in-game orders
                    // (i.e. bazaar flips are enabled).  When bazaar flips are disabled
                    // the startup pass is collect-only and the user's open orders are
                    // still in-game — clearing the tracker would wrongly empty the web
                    // panel, so we keep the discovered orders instead.
                    if enable_bazaar_flips_events.load(Ordering::Relaxed) {
                        let removed = bazaar_tracker_events.clear_all_orders();
                        if removed > 0 {
                            info!("[Startup] Cleared {} stale order(s) from bazaar tracker", removed);
                        }
                    }
                    // Also clear the auction slot blocked flag on startup
                    bot_client_clone.clear_auction_slot_blocked();
                    // Upload scoreboard to COFL (with real data matching TypeScript runStartupWorkflow)
                    {
                        let scoreboard_lines = bot_client_clone.get_scoreboard_lines();
                        let ws = ws_client_for_events.clone();
                        tokio::spawn(async move {
                            let data_json = serde_json::to_string(&scoreboard_lines).unwrap_or_else(|_| "[]".to_string());
                            let scoreboard_msg = serde_json::json!({"type": "uploadScoreboard", "data": data_json}).to_string();
                            let tab_msg = serde_json::json!({"type": "uploadTab", "data": "[]"}).to_string();
                            debug!("[Startup] Sending uploadScoreboard to COFL: {:?}", scoreboard_lines);
                            let _ = ws.send_message(&scoreboard_msg).await;
                            debug!("[Startup] Sending uploadTab to COFL (empty)");
                            let _ = ws.send_message(&tab_msg).await;
                            debug!("[Startup] Uploaded scoreboard ({} lines)", scoreboard_lines.len());
                        });
                    }
                    // COFL now automatically sends bazaar flip recommendations based
                    // on user settings — no need to request them manually.
                    // Send /cofl set maxitemsininventory once on startup so the
                    // inventory does not fill up with items the user cannot remove.
                    {
                        let ws = ws_client_for_events.clone();
                        let max_items = config_for_events.max_items_in_inventory;
                        tokio::spawn(async move {
                            // Small delay to let the socket settle after startup commands
                            sleep(Duration::from_secs(2)).await;
                            let set_value = format!("maxitemsininventory {}", max_items);
                            let data_json = serde_json::to_string(&set_value).unwrap_or_default();
                            let msg = serde_json::json!({
                                "type": "set",
                                "data": data_json
                            }).to_string();
                            if let Err(e) = ws.send_message(&msg).await {
                                error!("[Startup] Failed to send /cofl set maxitemsininventory {}: {}", max_items, e);
                            } else {
                                info!("[Startup] Sent /cofl set maxitemsininventory {}", max_items);
                            }
                        });
                    }
                    // Send startup complete webhook
                    if let Some(webhook_url) = config_for_events.active_webhook_url() {
                        let url = webhook_url.to_string();
                        let name = ingame_name_for_events.clone();
                        let ah = config_for_events.enable_ah_flips;
                        let bz = config_for_events.enable_bazaar_flips;
                        let conn_id = cofl_connection_id_events.lock().ok().and_then(|g| g.clone());
                        let premium = cofl_premium_events.lock().ok().and_then(|g| g.clone());
                        tokio::spawn(async move {
                            frikadellen_baf::webhook::send_webhook_startup_complete(&name, orders_cancelled, ah, bz, conn_id.as_deref(), premium.as_ref().map(|(t, e)| (t.as_str(), e.as_str())), &url).await;
                        });
                    }
                }
                frikadellen_baf::bot::BotEvent::ItemPurchased { item_name, price, buy_speed_ms: event_buy_speed_ms, via_bed: event_via_bed } => {
                    // Send uploadScoreboard (with real data) and uploadTab to COFL
                    let ws = ws_client_for_events.clone();
                    let scoreboard_lines = bot_client_clone.get_scoreboard_lines();
                    tokio::spawn(async move {
                        let data_json = serde_json::to_string(&scoreboard_lines).unwrap_or_else(|_| "[]".to_string());
                        let scoreboard_msg = serde_json::json!({"type": "uploadScoreboard", "data": data_json}).to_string();
                        let tab_msg = serde_json::json!({"type": "uploadTab", "data": "[]"}).to_string();
                        debug!("[ItemPurchased] Sending uploadScoreboard to COFL: {:?}", scoreboard_lines);
                        let _ = ws.send_message(&scoreboard_msg).await;
                        debug!("[ItemPurchased] Sending uploadTab to COFL (empty)");
                        let _ = ws.send_message(&tab_msg).await;
                    });
                    // Queue claim at Normal priority so any pending High-priority flip
                    // purchases run before we open the AH windows to collect.
                    // Skip claiming when inventory is near full to keep space for selling.
                    if bot_client_clone.is_inventory_near_full() {
                        warn!("[ItemPurchased] Skipping claim — inventory near full, prioritizing selling");
                        let baf_msg = "§f[§4BAF§f]: §e⚠ Inventory near full — skipping claim to keep space for selling".to_string();
                        print_mc_chat(&baf_msg);
                        let _ = chat_tx_events.send(baf_msg);
                    } else {
                        command_queue_clone.enqueue(
                            frikadellen_baf::types::CommandType::ClaimPurchasedItem,
                            frikadellen_baf::types::CommandPriority::Normal,
                            false,
                        );
                    }
                    // Look up stored flip data and update with real buy price + purchase time.
                    // Also grab the color-coded item name from the flip for colorful output.
                    // Buy speed comes from the event (flip received → escrow message).
                    // Exact pipeline timestamps (epoch ms): when the flip arrived over
                    // the COFL socket and when the purchase completed (this event).
                    let purchased_at_ms = chrono::Utc::now().timestamp_millis();
                    let (opt_target, opt_profit, colored_name, opt_auction_uuid, opt_finder, opt_received_at_ms, opt_list_at) = {
                        let key = frikadellen_baf::utils::remove_minecraft_colors(&item_name).to_lowercase();
                        match flip_tracker_events.lock() {
                            Ok(mut tracker) => {
                                if let Some(entry) = tracker.get_mut(&key) {
                                    entry.1 = price; // actual buy price
                                    // Receive time: entry.3 is the never-updated receive
                                    // Instant; convert to epoch by subtracting its age.
                                    let received_at_ms = purchased_at_ms - entry.3.elapsed().as_millis() as i64;
                                    entry.2 = Instant::now(); // purchase time
                                    let target = entry.0.target;
                                    let ah_fee = calculate_ah_fee(target);
                                    let expected_profit = target as i64 - price as i64 - ah_fee as i64;
                                    let uuid = entry.0.uuid.clone();
                                    let finder = entry.0.finder.clone();
                                    (Some(target), Some(expected_profit), entry.0.item_name.clone(), uuid, finder, Some(received_at_ms), entry.0.list_at)
                                } else {
                                    (None, None, item_name.clone(), None, None, None, None)
                                }
                            }
                            Err(e) => {
                                warn!("Flip tracker lock failed at ItemPurchased: {}", e);
                                (None, None, item_name.clone(), None, None, None, None)
                            }
                        }
                    };
                    // Finder-bought items: if COFL doesn't list the item itself (e.g.
                    // running finder-only, or COFL has no data for it), request FRESH
                    // pricing from the finder's inventory RPC and list from those
                    // instructions — the flip's target-scaled listAt is deliberately
                    // NOT used (the buy target is a conservative estimate; the sell
                    // price is decided at listing time with live references, real
                    // competition and the cost-basis floor — the single listing
                    // logic). Only acts if the item is STILL in inventory after the
                    // grace window; `opt_list_at` presence is just the "finder
                    // listing enabled" signal (ws-config listingRecommendations).
                    if opt_finder.as_deref() == Some("BAF_FINDER") {
                        if opt_list_at.filter(|&v| v > 0).is_some() {
                            const COFL_LISTING_GRACE_SECS: u64 = 150;
                            let bc = bot_client_clone.clone();
                            let item = item_name.clone();
                            let ws = ws_client_for_events.clone();
                            let chat = chat_tx_events.clone();
                            tokio::spawn(async move {
                                sleep(Duration::from_secs(COFL_LISTING_GRACE_SECS)).await;
                                // Listed already (by COFL or manually) → gone from inventory.
                                let needle = frikadellen_baf::utils::remove_minecraft_colors(&item).to_lowercase();
                                let still_held = bc
                                    .get_cached_inventory_json()
                                    .and_then(|j| serde_json::from_str::<serde_json::Value>(&j).ok())
                                    .and_then(|inv| {
                                        inv.get("slots").and_then(|s| s.as_array()).map(|slots| {
                                            slots.iter().any(|it| {
                                                it.get("displayName")
                                                    .and_then(|v| v.as_str())
                                                    .map(|n| frikadellen_baf::utils::remove_minecraft_colors(n).to_lowercase().contains(&needle))
                                                    .unwrap_or(false)
                                            })
                                        })
                                    })
                                    .unwrap_or(false);
                                if !still_held {
                                    return;
                                }
                                info!("[FinderListing] \"{}\" still in inventory after {}s — requesting fresh finder pricing (inventory RPC)", item, COFL_LISTING_GRACE_SECS);
                                let msg = format!(
                                    "§f[§4BAF§f]: §b📋 §r{}§r §7still held — asking finder for a fresh listing price",
                                    item
                                );
                                print_mc_chat(&msg);
                                let _ = chat.send(msg);
                                if let Some(inv) = bc.get_cached_inventory_json() {
                                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&inv) {
                                        let items = v.get("slots").cloned().unwrap_or(serde_json::json!([]));
                                        if let Err(e) = ws.send_inventory(&items, true).await {
                                            warn!("[FinderListing] inventory upload for pricing failed: {}", e);
                                        }
                                    }
                                }
                            });
                        }
                    }
                    // Report the buy to the backend with full purchase detail (the
                    // all-flips channel renders it as a normal purchase webhook).
                    backend_handle_events.report_purchase(
                        &ingame_name_for_events,
                        &item_name,
                        price as i64,
                        opt_target.map(|t| t as i64),
                        opt_profit,
                        event_buy_speed_ms,
                        opt_finder.as_deref(),
                        bot_client_clone.get_purse(),
                        opt_auction_uuid.as_deref(),
                        event_via_bed,
                        opt_received_at_ms,
                        Some(purchased_at_ms),
                    );
                    // Accumulate THEORETICAL AH profit at purchase time (target −
                    // price − AH fee), i.e. what you'd net if it sold at the COFL
                    // target. The panel/terminal/webhook show this instead of the
                    // realized `/cofl profit` figure, matching the backend's
                    // theoretical profit definition.
                    if let Some(p) = opt_profit {
                        profit_tracker_events.record_ah_profit(p);
                    }
                    // Print colorful purchase announcement (item rarity shown via color code)
                    let profit_str = opt_profit.map(|p| {
                        let color = if p >= 0 { "§a" } else { "§c" };
                        format!(" §7| Expected profit: {}{}§r", color, format_coins(p))
                    }).unwrap_or_default();
                    let kind_label = match event_via_bed {
                        Some(true) => " §7(§dBed§7)§r",
                        Some(false) => " §7(§6Nugget§7)§r",
                        None => "",
                    };
                    let speed_str = event_buy_speed_ms.map(|ms| format!(" §7| Buy speed: §e{}ms{}§r", ms, kind_label)).unwrap_or_default();
                    let baf_msg = format!(
                        "§f[§4BAF§f]: §a✦ PURCHASED §r{}§r §7for §6{}§7 coins!{}{}",
                        colored_name, format_coins(price as i64), profit_str, speed_str
                    );
                    print_mc_chat(&baf_msg);
                    let _ = chat_tx_events.send(baf_msg);
                    // Send webhook: for legendary/divine flips, send the styled
                    // webhook (with ping + color) instead of the regular purchase one.
                    let is_legendary_flip = opt_profit.map_or(false, |p| p >= frikadellen_baf::webhook::LEGENDARY_PROFIT_THRESHOLD as i64);
                    let opt_finder_for_flip = opt_finder.clone();
                    if is_legendary_flip {
                        if let Some(profit) = opt_profit {
                            // Send the legendary/divine styled webhook to the user's
                            // personal webhook (if configured).
                            if let Some(webhook_url) = config_for_events.active_webhook_url() {
                                let url = webhook_url.to_string();
                                let name = ingame_name_for_events.clone();
                                let item = item_name.clone();
                                let did = config_for_events.active_discord_id().map(|s| s.to_string());
                                let purse = bot_client_clone.get_purse();
                                let uuid_str = opt_auction_uuid.clone();
                                let finder = opt_finder_for_flip.clone();
                                if profit >= frikadellen_baf::webhook::DIVINE_PROFIT_THRESHOLD as i64 {
                                    tokio::spawn(async move {
                                        frikadellen_baf::webhook::send_webhook_divine_flip(
                                            &name, &item, price, opt_target, profit, purse,
                                            event_buy_speed_ms, event_via_bed, uuid_str.as_deref(), finder.as_deref(),
                                            did.as_deref(), opt_received_at_ms, Some(purchased_at_ms), &url,
                                        ).await;
                                    });
                                } else {
                                    tokio::spawn(async move {
                                        frikadellen_baf::webhook::send_webhook_legendary_flip(
                                            &name, &item, price, opt_target, profit, purse,
                                            event_buy_speed_ms, event_via_bed, uuid_str.as_deref(), finder.as_deref(),
                                            did.as_deref(), opt_received_at_ms, Some(purchased_at_ms), &url,
                                        ).await;
                                    });
                                }
                            }
                            // Notify the shared public channel only if the user opted
                            // in via `share_legendary_flips` (honoured here rather than
                            // always-on inside the webhook helper).
                            if config_for_events.share_legendary_flips {
                                let item_for_channel = item_name.clone();
                                let finder_for_channel = opt_finder_for_flip.clone();
                                tokio::spawn(async move {
                                    frikadellen_baf::webhook::send_webhook_flip_channel(
                                        &item_for_channel, price, opt_target, profit,
                                        event_buy_speed_ms, finder_for_channel.as_deref(),
                                    ).await;
                                });
                            }
                        }
                    } else {
                        // Regular purchase webhook for non-legendary flips
                        if let Some(webhook_url) = config_for_events.active_webhook_url() {
                            let url = webhook_url.to_string();
                            let name = ingame_name_for_events.clone();
                            let item = item_name.clone();
                            let purse = bot_client_clone.get_purse();
                            let uuid_str = opt_auction_uuid.clone();
                            tokio::spawn(async move {
                                frikadellen_baf::webhook::send_webhook_item_purchased(
                                    &name, &item, price, opt_target, opt_profit, purse,
                                    event_buy_speed_ms, event_via_bed, uuid_str.as_deref(), opt_finder.as_deref(),
                                    opt_received_at_ms, Some(purchased_at_ms), &url,
                                ).await;
                            });
                        }
                    }
                }
                frikadellen_baf::bot::BotEvent::ItemSold { item_name, price, buyer } => {
                    command_queue_clone.enqueue(
                        frikadellen_baf::types::CommandType::ClaimSoldItem,
                        frikadellen_baf::types::CommandPriority::High,
                        true,
                    );
                    // Look up flip data to calculate actual profit + time to sell
                    let (opt_profit, opt_buy_price, opt_time_secs, opt_auction_uuid) = {
                        let key = frikadellen_baf::utils::remove_minecraft_colors(&item_name).to_lowercase();
                        match flip_tracker_events.lock() {
                            Ok(mut tracker) => {
                                if let Some(entry) = tracker.remove(&key) {
                                    let (flip, buy_price, purchase_time, _receive_time) = entry;
                                    if buy_price > 0 {
                                        let ah_fee = calculate_ah_fee(price);
                                        let profit = price as i64 - buy_price as i64 - ah_fee as i64;
                                        let time_secs = purchase_time.elapsed().as_secs();
                                        (Some(profit), Some(buy_price), Some(time_secs), flip.uuid)
                                    } else {
                                        (None, None, None, flip.uuid)
                                    }
                                } else {
                                    (None, None, None, None)
                                }
                            }
                            Err(e) => {
                                warn!("Flip tracker lock failed at ItemSold: {}", e);
                                (None, None, None, None)
                            }
                        }
                    };
                    // NOTE: AH profit is now accumulated as THEORETICAL at purchase
                    // time (see ItemPurchased), so we no longer add realized profit
                    // here — doing so would double-count each flip.
                    backend_handle_events.report_event(
                        "sell",
                        &ingame_name_for_events,
                        Some(&item_name),
                        None,
                        Some(price as i64),
                        opt_profit,
                        false,
                    );
                    // Print colorful sold announcement
                    let profit_str = opt_profit.map(|p| {
                        let color = if p >= 0 { "§a" } else { "§c" };
                        format!(" §7| Profit: {}{}§r", color, format_coins(p))
                    }).unwrap_or_default();
                    let baf_msg = format!(
                        "§f[§4BAF§f]: §6⚡ SOLD §r{} §7to §e{}§7 for §6{}§7 coins!{}",
                        item_name, buyer, format_coins(price as i64), profit_str
                    );
                    print_mc_chat(&baf_msg);
                    let _ = chat_tx_events.send(baf_msg);
                    if let Some(webhook_url) = config_for_events.active_webhook_url() {
                        let url = webhook_url.to_string();
                        let name = ingame_name_for_events.clone();
                        let item = item_name.clone();
                        let b = buyer.clone();
                        let purse = bot_client_clone.get_purse();
                        let uuid_str = opt_auction_uuid.clone();
                        tokio::spawn(async move {
                            frikadellen_baf::webhook::send_webhook_item_sold(
                                &name, &item, price, &b, opt_profit, opt_buy_price,
                                opt_time_secs, purse, uuid_str.as_deref(), &url,
                            ).await;
                        });
                    }
                    // Query Coflnet for authoritative session profit after each sale.
                    // `/cofl profit <ign> <days>` returns the total AH profit over
                    // the session window so the tracker stays in sync with Coflnet.
                    // Skip if session is too short for meaningful data (< ~15 min).
                    {
                        let days = (prev_secs_events as f64 + session_start.elapsed().as_secs_f64()) / SECS_PER_DAY;
                        if days >= 0.01 {
                            let ign = ingame_name_for_events.clone();
                            let args = format!("{} {:.4}", ign, days);
                            let data_json = serde_json::json!(args).to_string();
                            let message = serde_json::json!({
                                "type": "profit",
                                "data": data_json
                            }).to_string();
                            let ws = ws_client_for_events.clone();
                            tokio::spawn(async move {
                                if let Err(e) = ws.send_message(&message).await {
                                    tracing::warn!("[CoflProfit] Failed to send /cofl profit: {}", e);
                                }
                            });
                        }
                    }
                    // An AH auction sold — a listing slot just freed up.
                    // Proactively request `/cofl sellinventory` so COFL can
                    // immediately recommend items to list, instead of waiting
                    // for the user or periodic check.
                    if enable_ah_flips_events.load(Ordering::Relaxed) {
                        let ws_si = ws_client_for_events.clone();
                        let bot_si = bot_client_clone.clone();
                        tokio::spawn(async move {
                            // Small delay to let the claim complete first.
                            tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
                            // Upload fresh inventory then request sellinventory.
                            if let Some(inv_json) = bot_si.get_cached_inventory_json() {
                                let upload_msg = serde_json::json!({
                                    "type": "uploadInventory",
                                    "data": inv_json
                                }).to_string();
                                let _ = ws_si.send_message(&upload_msg).await;
                                // Let COFL ingest the uploaded inventory before selling.
                                tokio::time::sleep(tokio::time::Duration::from_millis(600)).await;
                            }
                            let msg = serde_json::json!({
                                "type": "sellinventory",
                                "data": serde_json::to_string("").unwrap_or_default()
                            }).to_string();
                            if let Err(e) = ws_si.send_message(&msg).await {
                                tracing::warn!("[SellInventory] Failed to auto-request sellinventory after auction sale: {}", e);
                            } else {
                                tracing::info!("[SellInventory] Auto-requested sellinventory after auction sale");
                            }
                        });
                    }
                }
                frikadellen_baf::bot::BotEvent::BazaarOrderPlaced { item_name, amount, price_per_unit, is_buy_order } => {
                    backend_handle_events.report_event(
                        "order_placed",
                        &ingame_name_for_events,
                        Some(&item_name),
                        Some(amount),
                        Some(price_per_unit as i64),
                        None,
                        true,
                    );
                    // Track the order for the web panel and profit calculation on collect.
                    bazaar_tracker_events.add_order(item_name.clone(), amount, price_per_unit, is_buy_order);
                    let (order_color, order_type) = if is_buy_order { ("§a", "BUY") } else { ("§c", "SELL") };
                    let baf_msg = format!(
                        "§f[§4BAF§f]: §6[BZ] {}{}§7 order placed: {}x {} @ §6{}§7 coins/unit",
                        order_color, order_type, amount, item_name, format_coins_f64(price_per_unit)
                    );
                    print_mc_chat(&baf_msg);
                    let _ = chat_tx_events.send(baf_msg);
                    if config_for_events.active_bazaar_webhook_url().is_some() {
                        // Batched into the periodic bazaar digest (see
                        // spawn_bazaar_digest_flusher) instead of one embed per order.
                        frikadellen_baf::webhook::digest_order_placed(is_buy_order, bot_client_clone.get_purse());
                    }
                }
                frikadellen_baf::bot::BotEvent::AuctionListed { item_name, starting_bid, duration_hours } => {
                    backend_handle_events.report_event(
                        "list",
                        &ingame_name_for_events,
                        Some(&item_name),
                        None,
                        Some(starting_bid as i64),
                        None,
                        false,
                    );
                    // Reset the idle-inventory timer so the 30-minute failsafe doesn't fire
                    // while items are being actively listed.
                    *last_auction_listed_at_events.lock().unwrap() = Instant::now();
                    let baf_msg = format!(
                        "§f[§4BAF§f]: §a🏷️ BIN listed: §r{} §7@ §6{}§7 coins for §e{}h",
                        item_name, format_coins(starting_bid as i64), duration_hours
                    );
                    print_mc_chat(&baf_msg);
                    let _ = chat_tx_events.send(baf_msg);
                    if let Some(webhook_url) = config_for_events.active_webhook_url() {
                        let url = webhook_url.to_string();
                        let name = ingame_name_for_events.clone();
                        let item = item_name.clone();
                        let purse = bot_client_clone.get_purse();
                        let active_listings = bot_client_clone.active_auction_count();
                        tokio::spawn(async move {
                            frikadellen_baf::webhook::send_webhook_auction_listed(
                                &name, &item, starting_bid, duration_hours, purse, active_listings, &url,
                            ).await;
                        });
                    }
                }
                frikadellen_baf::bot::BotEvent::AuctionCancelled { item_name, starting_bid } => {
                    let baf_msg = format!(
                        "§f[§4BAF§f]: §c❌ Auction cancelled: §r{} §7@ §6{}§7 coins",
                        item_name, format_coins(starting_bid as i64)
                    );
                    print_mc_chat(&baf_msg);
                    let _ = chat_tx_events.send(baf_msg);
                    if let Some(webhook_url) = config_for_events.active_webhook_url() {
                        let url = webhook_url.to_string();
                        let name = ingame_name_for_events.clone();
                        let item = item_name.clone();
                        let purse = bot_client_clone.get_purse();
                        let remaining_listings = bot_client_clone.active_auction_count();
                        tokio::spawn(async move {
                            frikadellen_baf::webhook::send_webhook_auction_cancelled(
                                &name, &item, starting_bid, purse, remaining_listings, &url,
                            ).await;
                        });
                    }
                }
                frikadellen_baf::bot::BotEvent::BazaarOrderCollected { item_name, is_buy_order, claimed_amount } => {
                    backend_handle_events.report_event(
                        "order_collected",
                        &ingame_name_for_events,
                        Some(&item_name),
                        claimed_amount,
                        None,
                        None,
                        true,
                    );
                    // Remove from tracker.
                    let order_data = bazaar_tracker_events.remove_order(&item_name, is_buy_order);
                    // Determine the actual quantity collected.  `claimed_amount` is
                    // parsed from the "Filled: X/Y" lore in the Manage Orders window.
                    // Fall back to the tracker's original order amount when unavailable.
                    let actual_amount = claimed_amount
                        .or_else(|| order_data.as_ref().map(|o| o.amount))
                        .unwrap_or(0);
                    if let Some(ref order) = order_data {
                        // Store buy cost so we can compute profit when the sell offer is collected.
                        // BUY collections do NOT record profit — profit is only realized on SELL.
                        // Only record cost for the actually claimed quantity — a partial fill
                        // should not inflate the buy cost with the unfilled remainder.
                        if is_buy_order {
                            bazaar_tracker_events.record_buy_cost(&item_name, order.price_per_unit, actual_amount);
                            info!("[BazaarProfit] Recorded buy cost for {} — {} x {:.0} coins/unit",
                                item_name, actual_amount, order.price_per_unit);
                        }
                    } else {
                        debug!("[BazaarProfit] No tracked order for collected {} {} (may be from a previous session)",
                            if is_buy_order { "BUY" } else { "SELL" }, item_name);
                    }
                    // Compute profit/loss for sell offers: sell_total - buy_total - tax.
                    // This is used for the immediate chat display; the session profit
                    // total is driven by `/cofl bz l` via set_bz_total().
                    // Bazaar tax is applied to sell proceeds (default 1.25%).
                    //
                    // When a sell is partially filled, only use the actual sold quantity
                    // for both sell revenue AND buy cost comparison.  Using per-unit buy
                    // cost × actual_sold prevents the false loss that occurred when comparing
                    // partial sell revenue against the TOTAL buy cost for all purchased units.
                    let bazaar_tax_rate = config_for_events.bazaar_tax_rate;
                    let opt_profit: Option<i64> = if !is_buy_order {
                        if let Some(ref sell_order) = order_data {
                            let sell_total = sell_order.price_per_unit * actual_amount as f64;
                            let tax = sell_total * (bazaar_tax_rate / 100.0);
                            let sell_after_tax = sell_total - tax;
                            if let Some((buy_ppu, _buy_amt)) = bazaar_tracker_events.take_buy_cost(&item_name) {
                                // Use per-unit buy cost × actual sold quantity, NOT
                                // buy_ppu × total_buy_amount.  This correctly handles
                                // partial sells (e.g. sold 21 of 64 bought).
                                let buy_total = buy_ppu * actual_amount as f64;
                                let profit = (sell_after_tax - buy_total).round() as i64;
                                // Session BZ profit is tracked via /cofl bz l (set_bz_total),
                                // so we don't call record_bz_profit here.
                                info!("[BazaarProfit] SELL {} — {} units, sell: {:.0}, tax: {:.0} ({:.2}%), buy: {:.0} ({:.0}/ea), profit: {}",
                                    item_name, actual_amount, sell_total, tax, bazaar_tax_rate, buy_total, buy_ppu, profit);
                                Some(profit)
                            } else if let Some(bz_list_profit) = bazaar_tracker_events.get_bz_list_profit(&item_name) {
                                // Fallback: use profit from /cofl bz l for this item
                                info!("[BazaarProfit] SELL {} — sell: {:.0}, tax: {:.0}, no local buy cost, using /cofl bz l profit: {}",
                                    item_name, sell_total, tax, bz_list_profit);
                                Some(bz_list_profit)
                            } else {
                                // No buy cost recorded and no /cofl bz l data —
                                // do NOT report sell proceeds as profit (the item
                                // was not free).
                                info!("[BazaarProfit] SELL {} — sell: {:.0}, tax: {:.0}, no buy cost or /cofl bz l data, skipping profit",
                                    item_name, sell_total, tax);
                                None
                            }
                        } else {
                            None
                        }
                    } else {
                        None
                    };
                    let order_type = if is_buy_order { "BUY" } else { "SELL" };
                    info!("[BazaarOrders] Order collected: {} ({}) x{}", item_name, order_type, actual_amount);
                    // Build the collection message with prices and optional profit.
                    // Use actual_amount (from lore) instead of the tracker's original
                    // order amount so partial fills display correctly (e.g. "1x" not "4x").
                    let price_info = if let Some(ref order) = order_data {
                        let total = order.price_per_unit * actual_amount as f64;
                        format!(" §7({}x @ §6{}§7 = §6{}§7 coins)",
                            actual_amount,
                            format_coins_f64(order.price_per_unit),
                            format_coins_f64(total))
                    } else {
                        String::new()
                    };
                    let profit_info = if let Some(profit) = opt_profit {
                        let (color, sign) = if profit >= 0 { ("§a", "+") } else { ("§c", "") };
                        format!(" §7→ {}{}{}§7 profit", color, sign, format_coins(profit))
                    } else {
                        String::new()
                    };
                    let baf_msg = format!(
                        "§f[§4BAF§f]: §a✅ [BZ] {}§7 order collected: §r{}{}{}",
                        if is_buy_order { "BUY" } else { "SELL" }, item_name, price_info, profit_info
                    );
                    print_mc_chat(&baf_msg);
                    let _ = chat_tx_events.send(baf_msg);
                    // Send bazaar legendary flip to public channel (100M+ profit on SELL),
                    // only if the user opted in via `share_legendary_flips`.
                    if !is_buy_order && config_for_events.share_legendary_flips {
                        if let Some(profit) = opt_profit {
                            if profit >= frikadellen_baf::webhook::LEGENDARY_PROFIT_THRESHOLD as i64 {
                                let item_for_channel = item_name.clone();
                                let channel_amount = actual_amount;
                                let opt_ppu = order_data.as_ref().map(|o| o.price_per_unit);
                                tokio::spawn(async move {
                                    frikadellen_baf::webhook::send_webhook_bazaar_flip_channel(
                                        &item_for_channel,
                                        channel_amount,
                                        opt_ppu.unwrap_or(0.0),
                                        profit,
                                    ).await;
                                });
                            }
                        }
                    }
                    if config_for_events.active_bazaar_webhook_url().is_some() {
                        // Batched into the periodic bazaar digest (net profit is
                        // summed there) instead of one embed per collected order.
                        frikadellen_baf::webhook::digest_order_collected(opt_profit, bot_client_clone.get_purse());
                    }
                    // After collecting a SELL order, request `/cofl bz h` for
                    // authoritative BZ session profit.  A few seconds' delay gives
                    // Coflnet time to register the completed flip in its database.
                    if !is_buy_order {
                        let ws_bz_h = ws_client_for_events.clone();
                        let ign_bz_h = ingame_name_for_events.clone();
                        let ss_bz_h = session_start;
                        let prev_bz_h = prev_secs_events;
                        tokio::spawn(async move {
                            tokio::time::sleep(tokio::time::Duration::from_secs(BZ_LIST_REQUEST_DELAY_SECS + BZ_PROFIT_QUERY_EXTRA_DELAY_SECS)).await;
                            let days = (prev_bz_h as f64 + ss_bz_h.elapsed().as_secs_f64()) / SECS_PER_DAY;
                            if days >= 0.01 {
                                let args = format!("h {} {:.4}", ign_bz_h, days);
                                let data_json = serde_json::json!(args).to_string();
                                let message = serde_json::json!({
                                    "type": "bz",
                                    "data": data_json
                                }).to_string();
                                if let Err(e) = ws_bz_h.send_message(&message).await {
                                    tracing::warn!("[CoflBzH] Failed to send /cofl bz h after SELL collect: {}", e);
                                } else {
                                    tracing::info!("[CoflBzH] Auto-requested /cofl bz h after SELL order collected");
                                }
                            }
                        });
                    }
                }
                frikadellen_baf::bot::BotEvent::BazaarOrderCancelled { item_name, is_buy_order, already_collected } => {
                    // When already_collected is true, a BazaarOrderCollected event
                    // already removed this order from the tracker (partial collect
                    // followed by cancel of the unfilled remainder).  Calling
                    // remove_order again would incorrectly remove a DIFFERENT
                    // same-item order.
                    let order_data = if !already_collected {
                        bazaar_tracker_events.remove_order(&item_name, is_buy_order)
                    } else {
                        None
                    };
                    let order_type = if is_buy_order { "BUY" } else { "SELL" };
                    info!("[BazaarOrders] Order cancelled: {} ({})", item_name, order_type);
                    // Include amount and price in the cancel message so the user knows
                    // exactly which order was cancelled, not just the item name.
                    let detail_str = if let Some(ref order) = order_data {
                        let total = order.price_per_unit * order.amount as f64;
                        format!(" §7({}x @ §6{}§7 = §6{}§7 coins)",
                            order.amount,
                            format_coins_f64(order.price_per_unit),
                            format_coins_f64(total))
                    } else {
                        String::new()
                    };
                    let baf_msg = format!(
                        "§f[§4BAF§f]: §c🚫 [BZ] {}§7 order cancelled: §r{}{}",
                        if is_buy_order { "BUY" } else { "SELL" }, item_name, detail_str
                    );
                    print_mc_chat(&baf_msg);
                    let _ = chat_tx_events.send(baf_msg);
                    if config_for_events.active_bazaar_webhook_url().is_some() {
                        // Batched into the periodic bazaar digest instead of one
                        // embed per cancelled order.
                        frikadellen_baf::webhook::digest_order_cancelled(bot_client_clone.get_purse());
                    }
                }
                frikadellen_baf::bot::BotEvent::BazaarOrderFilled { item_name, is_buy_order } => {
                    // Mark the order as filled in the tracker so the periodic timer
                    // can skip ManageOrders when nothing needs collection.
                    if !item_name.is_empty() {
                        bazaar_tracker_events.mark_filled(&item_name, is_buy_order);
                    }
                    // When a SELL order is filled the flip is complete in Coflnet's
                    // view.  Request `/cofl bz l` (with a short delay so Coflnet
                    // finishes recording the flip) — the response handler will parse
                    // profits and update the session BZ total via set_bz_total().
                    if !is_buy_order {
                        let ws = ws_client_for_events.clone();
                        let ws2 = ws_client_for_events.clone();
                        let ign = ingame_name_for_events.clone();
                        let ss = session_start;
                        let prev_secs = prev_secs_events;
                        tokio::spawn(async move {
                            // Small delay to let Coflnet register the completed flip.
                            tokio::time::sleep(tokio::time::Duration::from_secs(BZ_LIST_REQUEST_DELAY_SECS)).await;
                            let data_json = serde_json::json!("l").to_string();
                            let message = serde_json::json!({
                                "type": "bz",
                                "data": data_json
                            }).to_string();
                            if let Err(e) = ws.send_message(&message).await {
                                tracing::warn!("[BZList] Failed to send /cofl bz l: {}", e);
                            } else {
                                tracing::info!("[BZList] Auto-requested /cofl bz l after SELL fill");
                            }
                            // Also request `/cofl bz h <ign> <days>` for authoritative
                            // BZ session profit (same as AH `/cofl profit`).
                            let days = (prev_secs as f64 + ss.elapsed().as_secs_f64()) / SECS_PER_DAY;
                            if days >= 0.01 {
                                let args = format!("h {} {:.4}", ign, days);
                                let data_json = serde_json::json!(args).to_string();
                                let message = serde_json::json!({
                                    "type": "bz",
                                    "data": data_json
                                }).to_string();
                                if let Err(e) = ws2.send_message(&message).await {
                                    tracing::warn!("[CoflBzH] Failed to send /cofl bz h: {}", e);
                                } else {
                                    tracing::info!("[CoflBzH] Auto-requested /cofl bz h {} {:.4}", ign, days);
                                }
                            }
                        });
                    }
                    // A bazaar buy/sell order was filled — trigger a ManageOrders run
                    // immediately so the items are collected without waiting for the next
                    // periodic check.  Only enqueue if bazaar flips are enabled and no
                    // ManageOrders is already queued/running (prevents duplicate processing
                    // that causes double cancel/collect Hypixel chat messages).
                    //
                    // When inventory is full and the fill is a BUY order, skip
                    // triggering ManageOrders — collecting BUY items requires free
                    // inventory space that we don't have.  The periodic order-check
                    // timer will retry after the 90 s cooldown clears the flag.
                    // SELL fills are always collected (they yield coins, not items).
                    if enable_bazaar_flips_events.load(Ordering::Relaxed) {
                        if is_buy_order && bot_client_clone.is_inventory_full() {
                            info!("[BazaarOrders] BUY order filled but inventory full — deferring ManageOrders for \"{}\"", item_name);
                        } else if command_queue_clone.has_manage_orders() {
                            info!("[BazaarOrders] Order filled — ManageOrders already queued/running, skipping duplicate");
                        } else {
                            info!("[BazaarOrders] Order filled — queuing ManageOrders");
                            command_queue_clone.enqueue(
                                frikadellen_baf::types::CommandType::ManageOrders { cancel_open: false, target_item: None },
                                frikadellen_baf::types::CommandPriority::High,
                                true,
                            );
                        }
                    }
                }
                frikadellen_baf::bot::BotEvent::BazaarOrdersSnapshot { ingame_orders } => {
                    // Reconcile the tracker with the orders actually visible
                    // in the Bazaar Orders window so the web GUI stays in sync.
                    let removed = bazaar_tracker_events.reconcile_with_ingame(&ingame_orders);
                    if removed > 0 {
                        info!("[BazaarOrders] Reconciled tracker: removed {} stale entries not found in-game", removed);
                    }
                }
            }
        }
    });

    // Spawn WebSocket message handler
    let command_queue_clone = command_queue.clone();
    let config_clone = config.clone();
    let config_loader_ws = config_loader.clone();
    let ws_client_clone = ws_client.clone();
    let bot_client_for_ws = bot_client.clone();
    let bazaar_flips_paused_ws = bazaar_flips_paused.clone();
    let bazaar_pause_until_ws = bazaar_pause_until.clone();
    let flip_tracker_ws = flip_tracker.clone();
    let cofl_connection_id_ws = cofl_connection_id.clone();
    let cofl_premium_ws = cofl_premium.clone();
    let enable_ah_flips_ws = enable_ah_flips.clone();
    let enable_bazaar_flips_ws = enable_bazaar_flips.clone();
    let flip_intake_paused_ws = flip_intake_paused.clone();
    let chat_tx_ws = chat_tx.clone();
    let detected_cofl_license_ws = detected_cofl_license.clone();
    let cofl_authenticated_ws = cofl_authenticated.clone();
    let ingame_names_ws = ingame_names.clone();
    let license_default_sent_ws = license_default_sent.clone();
    let ingame_name_ws = ingame_name.clone();
    let bazaar_tracker_ws = bazaar_tracker.clone();
    let profit_tracker_ws = profit_tracker.clone();
    // Accumulator for `/cofl bz l` output: (total_profit, flip_count, last_update).
    // Reset when "Last Completed Bazaar Flips" header is seen; each parsed flip
    // line adds to the total.  A debounce task displays the summary after 2s idle.
    let bz_list_accum: Arc<std::sync::Mutex<(i64, usize, std::time::Instant)>> =
        Arc::new(std::sync::Mutex::new((0, 0, std::time::Instant::now())));
    // Per-item profit accumulator for `/cofl bz l` output, used as a fallback
    // for per-order profit when local buy-cost tracking has no data.
    let bz_list_items: Arc<std::sync::Mutex<std::collections::HashMap<String, (i64, u32)>>> =
        Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    
    tokio::spawn(async move {
        use frikadellen_baf::websocket::CoflEvent;
        use frikadellen_baf::types::{CommandType, CommandPriority};

        while let Some(event) = ws_rx.recv().await {
            match event {
                CoflEvent::Authenticated => {
                    // COFL confirmed the session is authenticated (loggedIn).
                    // This is the reliable signal that enables flip/order buying.
                    if !cofl_authenticated_ws.swap(true, Ordering::Relaxed) {
                        info!("[Coflnet] Authentication confirmed (loggedIn) — flips enabled");
                        let baf_msg = "§f[§4BAF§f]: §aCoflnet authenticated — flip buying enabled".to_string();
                        print_mc_chat(&baf_msg);
                        let _ = chat_tx_ws.send(baf_msg);
                    }
                }
                CoflEvent::AuctionFlip(flip) => {
                    // Skip if AH flips are disabled
                    if !enable_ah_flips_ws.load(Ordering::Relaxed) {
                        continue;
                    }

                    // Skip if the web panel's Disconnect button paused intake.
                    if flip_intake_paused_ws.load(Ordering::Relaxed) {
                        debug!("Skipping AH flip — intake paused (Disconnect): {}", flip.item_name);
                        continue;
                    }

                    // Block COFL flips until Coflnet auth is confirmed. Flips from our
                    // OWN finder don't depend on COFL at all — they must buy fine in
                    // finder-only mode (no COFL license/auth), so they bypass this gate.
                    let from_own_finder = flip.finder.as_deref() == Some("BAF_FINDER");
                    if !from_own_finder && !cofl_authenticated_ws.load(Ordering::Relaxed) {
                        debug!("Skipping flip — Coflnet not yet authenticated: {}", flip.item_name);
                        continue;
                    }

                    // Block flips until startup workflow is complete — the bot
                    // state can briefly be Idle between queued startup commands,
                    // so checking is_startup_in_progress() covers that gap.
                    if bot_client_for_ws.is_startup_in_progress() {
                        debug!("Skipping AH flip during startup: {}", flip.item_name);
                        continue;
                    }

                    // Allow flips even when bot is busy — the command queue
                    // uses Critical priority for PurchaseAuction which will
                    // preempt lower-priority commands.  Previously this gate
                    // silently dropped cofl flips whenever the bot was in
                    // ANY non-idle state (Purchasing, ManagingOrders, Bazaar,
                    // etc.), causing flips to be missed entirely.
                    // Only hard-block during Startup which indicates the bot
                    // is not yet ready to interact with Hypixel at all.
                    if bot_client_for_ws.state() == frikadellen_baf::types::BotState::Startup {
                        debug!("Skipping flip — bot in Startup state: {}", flip.item_name);
                        continue;
                    }

                    // Skip AH flips when inventory is full — selling mode
                    if bot_client_for_ws.is_inventory_full() {
                        debug!("Skipping AH flip — inventory full (selling mode): {}", flip.item_name);
                        continue;
                    }

                    // Print colorful flip announcement (item name keeps its rarity color code)
                    let profit = flip.target.saturating_sub(flip.starting_bid);
                    let baf_msg = format!(
                        "§f[§4BAF§f]: §eTrying to purchase flip: §r{}§r §7for §6{}§7 coins §7(Target: §6{}§7, Profit: §a{}§7)",
                        flip.item_name,
                        format_coins(flip.starting_bid as i64),
                        format_coins(flip.target as i64),
                        format_coins(profit as i64)
                    );
                    print_mc_chat(&baf_msg);
                    let _ = chat_tx_ws.send(baf_msg);

                    // Store flip in tracker so ItemPurchased / ItemSold webhooks can include profit
                    {
                        let key = frikadellen_baf::utils::remove_minecraft_colors(&flip.item_name).to_lowercase();
                        if let Ok(mut tracker) = flip_tracker_ws.lock() {
                            let now = Instant::now();
                            tracker.insert(key, (flip.clone(), 0, now, now));
                        }
                    }

                    // Queue the flip command
                    // Buy-speed start time is now set in execute_command when
                    // /viewauction is sent, so the measurement covers the
                    // relevant path: command-send → coins-in-escrow.
                    command_queue_clone.enqueue(
                        CommandType::PurchaseAuction { flip },
                        CommandPriority::Critical,
                        false, // Not interruptible
                    );
                }
                CoflEvent::BazaarFlip(bazaar_flip) => {
                    // Skip if bazaar flips are disabled
                    if !enable_bazaar_flips_ws.load(Ordering::Relaxed) {
                        continue;
                    }

                    // Skip if the web panel's Disconnect button paused intake.
                    if flip_intake_paused_ws.load(Ordering::Relaxed) {
                        debug!("Skipping bazaar flip — intake paused (Disconnect): {}", bazaar_flip.item_name);
                        continue;
                    }

                    // Block flips until Coflnet auth is confirmed
                    if !cofl_authenticated_ws.load(Ordering::Relaxed) {
                        debug!("Skipping bazaar flip — Coflnet not yet authenticated: {}", bazaar_flip.item_name);
                        continue;
                    }

                    // Only skip during active startup phases (Startup / ManagingOrders).
                    // During ClaimingSold / ClaimingPurchased the flip is queued and will
                    // execute once the claim command finishes — matching TypeScript behaviour.
                    let bot_state = bot_client_for_ws.state();
                    if matches!(bot_state, frikadellen_baf::types::BotState::Startup)
                        || bot_client_for_ws.is_startup_in_progress()
                    {
                        debug!("Skipping bazaar flip during startup ({:?}): {}", bot_state, bazaar_flip.item_name);
                        continue;
                    }

                    // Determine order side so gate checks can distinguish
                    // BUY from SELL.  SELL orders should almost never be dropped
                    // because they empty inventory and free bazaar slots.
                    let effective_is_buy = bazaar_flip.effective_is_buy_order();

                    // Skip if at the Bazaar order limit (21 orders).
                    // SELL orders are still accepted: they are queued and a
                    // ManageOrders run is triggered to free a slot before the
                    // sell order reaches the command processor.
                    if bot_client_for_ws.is_bazaar_at_limit() && effective_is_buy {
                        debug!("Skipping BUY bazaar flip — at order limit: {}", bazaar_flip.item_name);
                        continue;
                    }

                    // Skip if daily sell value limit reached
                    if bot_client_for_ws.is_bazaar_daily_limit() {
                        debug!("Skipping bazaar flip — daily sell value limit reached: {}", bazaar_flip.item_name);
                        continue;
                    }

                    // Skip BUY flips if there are filled orders waiting to be
                    // collected — they still occupy a slot until ManageOrders
                    // collects them.  SELL flips are always accepted because
                    // placing a sell order does not require a free slot.
                    if effective_is_buy && bazaar_tracker_ws.has_filled_orders() {
                        debug!("Skipping BUY bazaar flip — filled orders pending collection: {}", bazaar_flip.item_name);
                        continue;
                    }

                    // Skip if bazaar flips are paused due to incoming AH flip (matching bazaarFlipPauser.ts)
                    if bazaar_flips_paused_ws.load(Ordering::Relaxed) {
                        debug!("Bazaar flips paused (AH flip incoming), skipping: {}", bazaar_flip.item_name);
                        continue;
                    }

                    // Skip BUY orders when inventory is full — items can't be
                    // collected from the bazaar without free inventory space.
                    // SELL orders are still accepted because they remove items
                    // from inventory, freeing space.
                    //
                    // Prevention: stop placing new BUY orders once the inventory is
                    // NEAR full (not just at 0), so the bot keeps headroom to claim
                    // already-filled buy orders and place sell orders instead of
                    // filling completely and deadlocking. SELL orders still flow.
                    if effective_is_buy && bot_client_for_ws.is_inventory_near_full() {
                        debug!("Skipping BUY bazaar flip — inventory near full: {}", bazaar_flip.item_name);
                        continue;
                    }

                    // Cap BUY amounts for unstackable items (e.g. Enchanted Books)
                    // to available inventory slots so the bot doesn't order more
                    // than it can hold.  Each unstackable unit occupies one slot.
                    let order_amount = if effective_is_buy
                        && frikadellen_baf::utils::is_unstackable_item(
                            &bazaar_flip.item_name,
                            bazaar_flip.item_tag.as_deref(),
                        )
                    {
                        let empty = bot_client_for_ws.empty_slot_count() as u64;
                        // Keep at least 2 slots free for AH/sell operations.
                        let max_buy = empty.saturating_sub(2);
                        if max_buy == 0 {
                            debug!("Skipping unstackable BUY — not enough inventory space ({} empty): {}", empty, bazaar_flip.item_name);
                            continue;
                        }
                        let capped = bazaar_flip.amount.min(max_buy);
                        if capped < bazaar_flip.amount {
                            info!(
                                "[BazaarFlips] Capping unstackable BUY amount {} → {} (only {} empty slots): {}",
                                bazaar_flip.amount, capped, empty, bazaar_flip.item_name
                            );
                        }
                        capped
                    } else {
                        bazaar_flip.amount
                    };

                    let (order_color, order_label) = if effective_is_buy { ("§a", "BUY") } else { ("§c", "SELL") };
                    let baf_msg = format!(
                        "§f[§4BAF§f]: §6[BZ] {}{}§7 order: §r{}§r §7x{} @ §6{}§7 coins/unit",
                        order_color, order_label,
                        bazaar_flip.item_name,
                        order_amount,
                        format_coins_f64(bazaar_flip.price_per_unit)
                    );
                    print_mc_chat(&baf_msg);
                    let _ = chat_tx_ws.send(baf_msg);

                    // Queue the bazaar command.
                    // SELL orders always get Critical priority — having items in
                    // inventory is worse than not having them (items block BUY
                    // collection and inventory space). BUY orders use Normal.
                    let priority = if effective_is_buy {
                        CommandPriority::Normal
                    } else {
                        CommandPriority::Critical
                    };
                    let command_type = if effective_is_buy {
                        CommandType::BazaarBuyOrder {
                            item_name: bazaar_flip.item_name.clone(),
                            item_tag: bazaar_flip.item_tag.clone(),
                            amount: order_amount,
                            price_per_unit: bazaar_flip.price_per_unit,
                        }
                    } else {
                        CommandType::BazaarSellOrder {
                            item_name: bazaar_flip.item_name.clone(),
                            item_tag: bazaar_flip.item_tag.clone(),
                            amount: order_amount,
                            price_per_unit: bazaar_flip.price_per_unit,
                        }
                    };

                    command_queue_clone.enqueue(
                        command_type,
                        priority,
                        true, // Interruptible by AH flips
                    );

                    // When at the bazaar order limit and we just queued a SELL
                    // order, pre-queue a ManageOrders run so a filled/stale
                    // order gets collected or cancelled, freeing a slot before
                    // the sell order reaches the command processor.
                    if bot_client_for_ws.is_bazaar_at_limit() && !effective_is_buy && !command_queue_clone.has_manage_orders() {
                        info!("[BazaarFlips] At order limit with SELL queued — pre-queuing ManageOrders to free a slot");
                        command_queue_clone.enqueue(
                            CommandType::ManageOrders { cancel_open: false, target_item: None },
                            CommandPriority::High,
                            false,
                        );
                    }
                }
                CoflEvent::CancelBazaarOrder(order) => {
                    // COFL asked us to cancel a specific open bazaar order,
                    // identified by item name + side (+ price to disambiguate
                    // multiple same-side orders for the same item).
                    let is_buy = order.effective_is_buy_order();
                    let side = if is_buy { "BUY" } else { "SELL" };

                    // Only act once Coflnet auth is confirmed — the order-cancel
                    // GUI flow is gated the same way flip buying is.
                    if !cofl_authenticated_ws.load(Ordering::Relaxed) {
                        debug!("Skipping cancelOrder — Coflnet not yet authenticated: {}", order.item_name);
                        continue;
                    }

                    let price = if order.price_per_unit > 0.0 {
                        Some(order.price_per_unit)
                    } else {
                        None
                    };

                    let msg = match price {
                        Some(p) => format!(
                            "§f[§4BAF§f]: §c🚫 [BZ] COFL cancel {} order: §r{} §7@ §6{:.1}",
                            side, order.item_name, p
                        ),
                        None => format!(
                            "§f[§4BAF§f]: §c🚫 [BZ] COFL cancel {} order: §r{}",
                            side, order.item_name
                        ),
                    };
                    info!(
                        "[COFL] cancelOrder requested: '{}' ({}){}",
                        order.item_name,
                        side,
                        price.map(|p| format!(" @ {:.1}", p)).unwrap_or_default()
                    );
                    print_mc_chat(&msg);
                    let _ = chat_tx_ws.send(msg);

                    // Reflect the intent in the tracker immediately: mark the
                    // order pending-cancel (so the ManageOrders reconcile pass
                    // doesn't re-add it from the pre-cancel window snapshot) and
                    // remove it from the panel. The in-game cancel happens
                    // asynchronously via the targeted ManageOrders run below.
                    bazaar_tracker_ws.mark_cancelling(&order.item_name, is_buy);
                    bazaar_tracker_ws.remove_order(&order.item_name, is_buy);

                    command_queue_clone.enqueue(
                        CommandType::ManageOrders {
                            cancel_open: true,
                            target_item: Some(frikadellen_baf::types::BazaarOrderTarget {
                                item_name: order.item_name.clone(),
                                is_buy,
                                price_per_unit: price,
                            }),
                        },
                        CommandPriority::Critical,
                        false,
                    );
                }
                CoflEvent::ChatMessage(msg) => {
                    // Parse "Your connection id is XXXX" (from chatMessage, matches TypeScript BAF.ts)
                    if let Some(cap) = msg.find("Your connection id is ") {
                        let rest = &msg[cap + "Your connection id is ".len()..];
                        let conn_id: String = rest.chars()
                            .take_while(|c| c.is_ascii_hexdigit())
                            .collect();
                        if conn_id.len() == 32 {
                            info!("[Coflnet] Connection ID: {}", conn_id);
                            if let Ok(mut g) = cofl_connection_id_ws.lock() {
                                *g = Some(conn_id);
                            }
                        }
                    }
                    // Detect Coflnet authentication success.
                    // COFL sends "Hello <IGN> (<email>)" after successful auth, e.g.:
                    //   "[Coflnet]: Hello iLoveTreXitoCfg (tre********@****l.com)"
                    // The message may contain §-color codes. We look for "Hello "
                    // followed by a parenthesized email (with '@' inside) to avoid
                    // matching unrelated messages.
                    if !cofl_authenticated_ws.load(Ordering::Relaxed) {
                        if let Some(hello_pos) = msg.find("Hello ") {
                            let after_hello = &msg[hello_pos..];
                            // Expect "(…@…)" somewhere after "Hello "
                            if let (Some(open), Some(close)) = (after_hello.find('('), after_hello.find(')')) {
                                if open < close && after_hello[open..close].contains('@') {
                                    info!("[Coflnet] Authentication confirmed — flips enabled");
                                    cofl_authenticated_ws.store(true, Ordering::Relaxed);
                                    let baf_msg = "§f[§4BAF§f]: §aCoflnet authenticated — flip buying enabled".to_string();
                                    print_mc_chat(&baf_msg);
                                    let _ = chat_tx_ws.send(baf_msg);
                                }
                            }
                        }
                    }
                    // Parse "You have X until Y" premium info (from writeToChat/chatMessage)
                    // Format: "You have Premium Plus until 2026-Feb-10 08:55 UTC"
                    if let Some(cap) = msg.find("You have ") {
                        let rest = &msg[cap + "You have ".len()..];
                        if let Some(until_pos) = rest.find(" until ") {
                            let tier = rest[..until_pos].trim().to_string();
                            let expires_raw = &rest[until_pos + " until ".len()..];
                            let expires: String = expires_raw.chars()
                                .take_while(|&c| c != '\n' && c != '\\')
                                .collect();
                            let expires = expires.trim().to_string();
                            if !tier.is_empty() && !expires.is_empty() {
                                info!("[Coflnet] Premium: {} until {}", tier, expires);
                                if let Ok(mut g) = cofl_premium_ws.lock() {
                                    *g = Some((tier, expires));
                                }
                                // License is already active on the current account —
                                // no need to send `/cofl license default` later.
                                license_default_sent_ws.store(true, Ordering::Relaxed);
                            }
                        }
                    }
                    // Detect "You don't have a license for <ign>" and auto-send
                    // `/cofl license default <current_ign>` so the user's default
                    // account tier is applied to the current account.
                    if !license_default_sent_ws.load(Ordering::Relaxed) {
                        let clean_msg = frikadellen_baf::utils::remove_minecraft_colors(&msg);
                        if clean_msg.contains("don't have a license for") {
                            license_default_sent_ws.store(true, Ordering::Relaxed);
                            let ws = ws_client_clone.clone();
                            let ign = ingame_name_ws.clone();
                            info!("[LicenseDefault] No license detected — sending /cofl license default {}", ign);
                            let baf_msg = format!(
                                "§f[§4BAF§f]: §eNo license for §b{}§e — setting as default account...",
                                ign
                            );
                            print_mc_chat(&baf_msg);
                            let _ = chat_tx_ws.send(baf_msg);
                            tokio::spawn(async move {
                                if let Err(e) = ws.set_default_license(&ign).await {
                                    warn!("[LicenseDefault] Failed to set default license: {}", e);
                                }
                            });
                        }
                    }
                    // ---- `/cofl bz l` output parsing ----
                    // Coflnet sends "Last Completed Bazaar Flips" followed by lines like:
                    //   "2xJungle Key: 1.05M -> 287K => -768K(1)"
                    // Parse each flip line's profit and accumulate a running total.
                    // Per-item data is stored in the bazaar tracker so it can be used
                    // as a fallback profit source when local buy-cost tracking has
                    // no data for a sell.
                    {
                        let clean = frikadellen_baf::utils::remove_minecraft_colors(&msg);
                        if clean.contains("Last Completed Bazaar Flips") {
                            // Header line — reset the accumulators.
                            if let Ok(mut acc) = bz_list_accum.lock() {
                                *acc = (0, 0, std::time::Instant::now());
                            }
                            if let Ok(mut items) = bz_list_items.lock() {
                                items.clear();
                            }
                        } else if let Some(profit) = parse_bz_list_flip_profit(&clean) {
                            // Also parse per-item detail for fallback profit lookup.
                            if let Some((item_name, item_profit, flip_count)) = parse_bz_list_flip_detail(&clean) {
                                if let Ok(mut items) = bz_list_items.lock() {
                                    let entry = items.entry(item_name).or_insert((0, 0));
                                    entry.0 += item_profit;
                                    entry.1 += flip_count;
                                }
                            }
                            let should_spawn_summary = {
                                if let Ok(mut acc) = bz_list_accum.lock() {
                                    acc.0 += profit;
                                    acc.1 += 1;
                                    acc.2 = std::time::Instant::now();
                                    // Only spawn a summary task for the first flip
                                    // to avoid many duplicate summary outputs.
                                    acc.1 == 1
                                } else {
                                    false
                                }
                            };
                            if should_spawn_summary {
                                let accum = bz_list_accum.clone();
                                let items_clone = bz_list_items.clone();
                                let tracker = bazaar_tracker_ws.clone();
                                let tx = chat_tx_ws.clone();
                                let pt = profit_tracker_ws.clone();
                                tokio::spawn(async move {
                                    // Wait for the full list to arrive.
                                    tokio::time::sleep(tokio::time::Duration::from_secs(BZ_LIST_DEBOUNCE_SECS)).await;
                                    if let Ok(acc) = accum.lock() {
                                        let (total, count, _) = *acc;
                                        if count > 0 {
                                            // Use the `/cofl bz l` total as the authoritative
                                            // BZ session profit (replaces local calculation).
                                            pt.set_bz_total(total);
                                            tracing::info!("[BZList] Updated BZ profit from /cofl bz l: {} coins ({} flips)", total, count);
                                            let (color, sign) = if total >= 0 { ("§a", "+") } else { ("§c", "") };
                                            let summary = format!(
                                                "§f[§4BAF§f]: §6[BZ List] §7{} flips, total profit: {}{}{}",
                                                count, color, sign, format_coins(total)
                                            );
                                            print_mc_chat(&summary);
                                            let _ = tx.send(summary);
                                        }
                                    }
                                    // Push per-item profit data to the tracker for
                                    // fallback use when computing sell order profit.
                                    if let Ok(items) = items_clone.lock() {
                                        if !items.is_empty() {
                                            tracker.set_bz_list_profits(items.clone());
                                            tracing::debug!("[BZList] Stored per-item profits for {} items", items.len());
                                        }
                                    }
                                });
                            }
                        }
                    }
                    // Try to parse the Coflnet chat message as a bazaar flip recommendation.
                    // Coflnet may send flip recommendations as chat messages (e.g.
                    // "Recommending sell order: 2x Item at 30.1K per unit(1)") without a
                    // corresponding structured `bazaarFlip` WebSocket message.
                    if enable_bazaar_flips_ws.load(Ordering::Relaxed)
                        && cofl_authenticated_ws.load(Ordering::Relaxed)
                        && !bazaar_flips_paused_ws.load(Ordering::Relaxed)
                        && !flip_intake_paused_ws.load(Ordering::Relaxed)
                    {
                        if let Ok(Some(rec)) = frikadellen_baf::handlers::BazaarFlipHandler::parse_bazaar_flip_message(&msg) {
                            let bot_state = bot_client_for_ws.state();
                            if !matches!(bot_state, frikadellen_baf::types::BotState::Startup) {
                                let effective_is_buy = rec.effective_is_buy_order();

                                // Gate checks that only apply to BUY orders.
                                // SELL orders bypass these because they empty
                                // inventory and must not be silently dropped.
                                if effective_is_buy && bot_client_for_ws.is_bazaar_at_limit() {
                                    debug!("Skipping BUY bazaar flip from chat — at order limit: {}", rec.item_name);
                                } else if bot_client_for_ws.is_bazaar_daily_limit() {
                                    debug!("Skipping bazaar flip from chat — daily sell value limit reached: {}", rec.item_name);
                                } else if effective_is_buy && bot_client_for_ws.is_inventory_near_full() {
                                    debug!("Skipping BUY bazaar flip from chat — inventory near full: {}", rec.item_name);
                                } else if effective_is_buy && bazaar_tracker_ws.has_filled_orders() {
                                    debug!("Skipping BUY bazaar flip from chat — filled orders pending: {}", rec.item_name);
                                } else {

                                // Cap BUY amounts for unstackable items (e.g. Enchanted Books)
                                let order_amount = if effective_is_buy
                                    && frikadellen_baf::utils::is_unstackable_item(
                                        &rec.item_name,
                                        rec.item_tag.as_deref(),
                                    )
                                {
                                    let empty = bot_client_for_ws.empty_slot_count() as u64;
                                    let max_buy = empty.saturating_sub(2);
                                    if max_buy == 0 {
                                        debug!("Skipping unstackable BUY from chat — not enough space ({} empty): {}", empty, rec.item_name);
                                        continue;
                                    }
                                    let capped = rec.amount.min(max_buy);
                                    if capped < rec.amount {
                                        info!(
                                            "[BazaarFlips] Capping unstackable BUY amount {} → {} (chat): {}",
                                            rec.amount, capped, rec.item_name
                                        );
                                    }
                                    capped
                                } else {
                                    rec.amount
                                };

                                let (order_color, order_label) = if effective_is_buy { ("§a", "BUY") } else { ("§c", "SELL") };
                                let baf_msg = format!(
                                    "§f[§4BAF§f]: §6[BZ] {}{}§7 order: §r{}§r §7x{} @ §6{}§7 coins/unit",
                                    order_color, order_label,
                                    rec.item_name,
                                    order_amount,
                                    format_coins_f64(rec.price_per_unit)
                                );
                                print_mc_chat(&baf_msg);
                                let _ = chat_tx_ws.send(baf_msg);

                                let priority = if effective_is_buy {
                                    CommandPriority::Normal
                                } else {
                                    CommandPriority::Critical
                                };
                                let command_type = if effective_is_buy {
                                    CommandType::BazaarBuyOrder {
                                        item_name: rec.item_name.clone(),
                                        item_tag: rec.item_tag.clone(),
                                        amount: order_amount,
                                        price_per_unit: rec.price_per_unit,
                                    }
                                } else {
                                    CommandType::BazaarSellOrder {
                                        item_name: rec.item_name.clone(),
                                        item_tag: rec.item_tag.clone(),
                                        amount: order_amount,
                                        price_per_unit: rec.price_per_unit,
                                    }
                                };
                                command_queue_clone.enqueue(command_type, priority, true);
                                info!("[BazaarFlips] Queued {} order from chat message: {} x{} @ {:.0}",
                                    order_label, rec.item_name, rec.amount, rec.price_per_unit);

                                // When at the bazaar order limit and we just queued a SELL
                                // order, pre-queue a ManageOrders run to free a slot.
                                if bot_client_for_ws.is_bazaar_at_limit() && !effective_is_buy && !command_queue_clone.has_manage_orders() {
                                    info!("[BazaarFlips] At order limit with SELL queued (chat) — pre-queuing ManageOrders to free a slot");
                                    command_queue_clone.enqueue(
                                        CommandType::ManageOrders { cancel_open: false, target_item: None },
                                        CommandPriority::High,
                                        false,
                                    );
                                }
                                } // end gate checks
                            }
                        }
                    }
                    // Display COFL chat messages with proper color formatting
                    // These are informational messages and should NOT be sent to Hypixel server
                    if config_clone.use_cofl_chat {
                        // Print with color codes if the message contains them
                        print_mc_chat(&msg);
                    } else {
                        // Still show in debug mode but without color formatting
                        debug!("[COFL Chat] {}", msg);
                    }
                    // Broadcast to web panel clients
                    let _ = chat_tx_ws.send(msg);
                }
                CoflEvent::Command(cmd) => {
                    info!("Received command from Coflnet: {}", cmd);
                    
                    // Check if this is a /cofl or /baf command that should be sent back to websocket
                    // Match TypeScript consoleHandler.ts - parse and route commands properly
                    let lowercase_cmd = cmd.trim().to_lowercase();
                    if lowercase_cmd.starts_with("/cofl") || lowercase_cmd.starts_with("/baf") {
                        // Parse /cofl command like the console handler does
                        let parts: Vec<&str> = cmd.trim().split_whitespace().collect();
                        if parts.len() > 1 {
                            let command = parts[1].to_string(); // Clone to own the data
                            let args = parts[2..].join(" ");

                            // Region switch: COFL's `/cofl switchregion` asks the bot to
                            // reconnect to a different modsocket via a `connect <url>`
                            // command. This must be executed locally — persist the new
                            // websocket URL and restart — NOT echoed back over the socket
                            // (which silently failed and left the bot on an unreachable
                            // regional host, e.g. an unresolvable us.sky.coflnet.com).
                            if command == "connect" && !args.is_empty() {
                                // COFL fully switched to TLS. It may hand us a
                                // scheme-less host ("us-sky.coflnet.com/modsocket")
                                // or, on older paths, a plaintext "ws://" URL. Force
                                // the secure scheme either way so a region switch
                                // never downgrades the bot to a plaintext socket the
                                // regional server now refuses.
                                let raw = args.trim();
                                let host = raw
                                    .strip_prefix("wss://")
                                    .or_else(|| raw.strip_prefix("ws://"))
                                    .unwrap_or(raw);
                                let new_url = format!("wss://{}", host);
                                info!("[RegionSwitch] /cofl connect → reconnecting to {}", new_url);
                                let _ = chat_tx_ws.send(format!(
                                    "§f[§4BAF§f]: §bSwitching server → §e{}§7 (restarting)…",
                                    new_url
                                ));
                                let mut new_config = config_clone.clone();
                                new_config.websocket_url = new_url;
                                if let Err(e) = config_loader_ws.save(&new_config) {
                                    error!("[RegionSwitch] Failed to save new websocket URL: {}", e);
                                }
                                restart_process();
                                return;
                            }

                            // Send to websocket with command as type (JSON-stringified data)
                            let ws = ws_client_clone.clone();
                            let inv_client = bot_client_for_ws.clone();
                            let is_sellinventory = command == "sellinventory";
                            tokio::spawn(async move {
                                // For sellinventory: upload the current inventory first so COFL
                                // has fresh data before processing the sell command.
                                if is_sellinventory {
                                    if let Some(inv_json) = inv_client.get_cached_inventory_json() {
                                        info!("[Inventory] sellinventory: uploading inventory first ({} bytes)", inv_json.len());
                                        let upload_msg = serde_json::json!({
                                            "type": "uploadInventory",
                                            "data": inv_json
                                        }).to_string();
                                        if let Err(e) = ws.send_message(&upload_msg).await {
                                            error!("[Inventory] sellinventory: failed to pre-upload inventory: {}", e);
                                        } else {
                                            // Give COFL a moment to ingest the uploaded inventory
                                            // before it processes the sellinventory request.
                                            // Without this the sell command frequently arrives
                                            // before the new inventory is stored, so COFL sells
                                            // against stale/empty data and "does nothing".
                                            sleep(Duration::from_millis(600)).await;
                                        }
                                    } else {
                                        warn!("[Inventory] sellinventory: no cached inventory to upload");
                                    }
                                }

                                let data_json = serde_json::to_string(&args).unwrap_or_else(|_| "\"\"".to_string());
                                let message = serde_json::json!({
                                    "type": command,
                                    "data": data_json
                                }).to_string();
                                
                                if let Err(e) = ws.send_message(&message).await {
                                    error!("Failed to send /cofl command to websocket: {}", e);
                                } else {
                                    info!("Sent /cofl {} to websocket", command);
                                }
                            });
                        }
                    } else {
                        // Execute non-cofl commands sent by Coflnet to Minecraft
                        // This matches TypeScript behavior: bot.chat(data) for non-cofl commands
                        command_queue_clone.enqueue(
                            CommandType::SendChat { message: cmd },
                            CommandPriority::High,
                            false, // Not interruptible
                        );
                    }
                }
                // Handle advanced message types (matching TypeScript BAF.ts)
                CoflEvent::GetInventory => {
                    // TypeScript handles getInventory DIRECTLY in the WS message handler,
                    // calling JSON.stringify(bot.inventory) and sending immediately — no queue.
                    // Hypixel and COFL are separate entities; inventory upload never needs to
                    // wait for a Hypixel command slot, so we do the same here.
                    info!("COFL requested getInventory — sending cached inventory");
                    if let Some(inv_json) = bot_client_for_ws.get_cached_inventory_json() {
                        let payload_bytes = inv_json.len();
                        debug!("[Inventory] Uploading to COFL: payload {} bytes", payload_bytes);
                        info!("[Inventory] uploadInventory payload: {}", inv_json);
                        let message = serde_json::json!({
                            "type": "uploadInventory",
                            "data": inv_json
                        }).to_string();
                        let ws = ws_client_clone.clone();
                        tokio::spawn(async move {
                            if let Err(e) = ws.send_message(&message).await {
                                error!("Failed to upload inventory to websocket: {}", e);
                            } else {
                                info!("Uploaded inventory to COFL ({} bytes)", payload_bytes);
                            }
                        });
                    } else {
                        warn!("getInventory received but no cached inventory yet — ignoring");
                    }
                }
                CoflEvent::TradeResponse => {
                    debug!("Processing tradeResponse - clicking accept button");
                    // TypeScript: clicks slot 39 after checking for "Deal!" or "Warning!"
                    // Sleep is handled in TypeScript before clicking - we'll do the same
                    command_queue_clone.enqueue(
                        CommandType::ClickSlot { slot: 39 },
                        CommandPriority::High,
                        false,
                    );
                }
                CoflEvent::PrivacySettings(data) => {
                    // TypeScript stores this in bot.privacySettings
                    debug!("Received privacySettings: {}", data);
                }
                CoflEvent::SwapProfile(profile_name) => {
                    info!("Processing swapProfile request: {}", profile_name);
                    command_queue_clone.enqueue(
                        CommandType::SwapProfile { profile_name },
                        CommandPriority::High,
                        false,
                    );
                }
                CoflEvent::CreateAuction(data) => {
                    info!("Processing createAuction request");
                    // Parse the auction data
                    match serde_json::from_str::<serde_json::Value>(&data) {
                        Ok(auction_data) => {
                            // Field is "price" in COFL protocol (not "startingBid")
                            let item_raw = auction_data.get("itemName").and_then(|v| v.as_str());
                            let price = auction_data.get("price").and_then(|v| v.as_u64());
                            let duration = auction_data.get("duration").and_then(|v| v.as_u64());
                            // Also extract slot (mineflayer inventory slot 9-44) and id
                            let item_slot = auction_data.get("slot").and_then(|v| v.as_u64());
                            let item_id = auction_data.get("id").and_then(|v| v.as_str()).map(|s| s.to_string());

                            // If itemName is null/absent, fall back to looking up the display
                            // name from the bot's cached inventory at the given slot.
                            // COFL sends null itemName in some protocol versions.
                            let item_raw_resolved: Option<String> = item_raw
                                .map(|s| s.to_string())
                                .or_else(|| {
                                    let slot = item_slot?;
                                    // Mineflayer inventory slots are 9-44 (player inventory).
                                    // Reject values outside this range to avoid silent OOB access.
                                    if !(9..=44).contains(&slot) {
                                        warn!("[createAuction] slot {} is out of valid inventory range 9-44", slot);
                                        return None;
                                    }
                                    let inv_json = bot_client_for_ws.get_cached_inventory_json()?;
                                    let inv: serde_json::Value = serde_json::from_str(&inv_json).ok()?;
                                    let slots = inv.get("slots")?.as_array()?;
                                    let item = slots.get(slot as usize)?;
                                    if item.is_null() {
                                        return None;
                                    }
                                    // Prefer displayName (human-readable), then registry name
                                    item.get("displayName")
                                        .and_then(|v| v.as_str())
                                        .or_else(|| item.get("name").and_then(|v| v.as_str()))
                                        .map(|s| s.to_string())
                                });

                            if item_raw.is_none() {
                                if let Some(ref resolved) = item_raw_resolved {
                                    info!("[createAuction] Resolved null itemName from inventory slot {:?}: {}", item_slot, resolved);
                                } else {
                                    warn!("[createAuction] itemName is null and could not be resolved from inventory slot {:?}", item_slot);
                                }
                            }

                            match (item_raw_resolved.as_deref(), price, duration) {
                                (Some(item_raw), Some(price), Some(duration)) => {
                                    // Strip Minecraft color codes (§X) from item name
                                    let item_name = frikadellen_baf::utils::remove_minecraft_colors(item_raw);

                                    // Check if the ORIGINAL flip was unprofitable at the
                                    // time of purchase.  We compare the COFL target price
                                    // (entry.0.target) against the actual buy price (entry.1)
                                    // to detect flips that should never have been bought.
                                    // We do NOT compare against the current createAuction
                                    // sell price — COFL may recommend a different sell price
                                    // than the original target, and that is fine.
                                    let skip_negative = {
                                        let key = item_name.to_lowercase();
                                        match flip_tracker_ws.lock() {
                                            Ok(tracker) => {
                                                if let Some(entry) = tracker.get(&key) {
                                                    let buy_price = entry.1;
                                                    let target = entry.0.target;
                                                    if buy_price > 0 && target > 0 {
                                                        let ah_fee = calculate_ah_fee(target);
                                                        let expected_profit = target as i64 - buy_price as i64 - ah_fee as i64;
                                                        if expected_profit < 0 {
                                                            let loss_amount = expected_profit.abs();
                                                            warn!("[createAuction] Skipping originally-unprofitable flip: {} — target {} - buy {} - fee {} = {} coins",
                                                                item_name, target, buy_price, ah_fee, expected_profit);
                                                            let baf_msg = format!(
                                                                "§f[§4BAF§f]: §c❌ Skipping AH listing: §r{}§r §7— original flip would lose §c{}§7 coins",
                                                                item_name, format_coins(loss_amount)
                                                            );
                                                            print_mc_chat(&baf_msg);
                                                            let _ = chat_tx_ws.send(baf_msg);
                                                            true
                                                        } else {
                                                            false
                                                        }
                                                    } else {
                                                        false
                                                    }
                                                } else {
                                                    false
                                                }
                                            }
                                            Err(_) => false,
                                        }
                                    };

                                    if skip_negative {
                                        // Don't list — keep item in inventory
                                        continue;
                                    }

                                    // do_not_relist blocklist (COFL + finder): hold the item
                                    // instead of auto-relisting when its item id, originating
                                    // finder, or expected profit matches the configured rules.
                                    let (blk_finder, blk_profit) = tracked_finder_profit(&flip_tracker_ws, &item_name);
                                    if let Some(reason) = config_clone.relist_block_reason(
                                        item_id.as_deref(),
                                        blk_finder.as_deref(),
                                        blk_profit,
                                    ) {
                                        info!("[Relist] Won't relist \"{}\" — {}", item_name, reason);
                                        let baf_msg = format!(
                                            "§f[§4BAF§f]: §e🛑 Won't relist §r{}§r §7— {}",
                                            item_name, reason
                                        );
                                        print_mc_chat(&baf_msg);
                                        let _ = chat_tx_ws.send(baf_msg);
                                        continue;
                                    }

                                    let cmd = CommandType::SellToAuction {
                                        item_name,
                                        starting_bid: price,
                                        duration_hours: duration,
                                        item_slot,
                                        item_id,
                                    };
                                    // If bazaar flips are paused (AH flip window active), defer
                                    // listing until the window ends so the listing flow does not
                                    // race with ongoing AH purchases.
                                    if bazaar_flips_paused_ws.load(Ordering::Relaxed) {
                                        info!("[createAuction] AH flip window active — deferring listing until bazaar flips resume");
                                        let flag = bazaar_flips_paused_ws.clone();
                                        let queue = command_queue_clone.clone();
                                        tokio::spawn(async move {
                                            let deadline = tokio::time::Instant::now()
                                                + tokio::time::Duration::from_secs(30);
                                            loop {
                                                sleep(Duration::from_millis(250)).await;
                                                if !flag.load(Ordering::Relaxed)
                                                    || tokio::time::Instant::now() >= deadline
                                                {
                                                    break;
                                                }
                                            }
                                            info!("[createAuction] Deferral complete — enqueueing SellToAuction");
                                            queue.enqueue(cmd, CommandPriority::High, false);
                                        });
                                    } else {
                                        command_queue_clone.enqueue(cmd, CommandPriority::High, false);
                                    }
                                }
                                _ => {
                                    warn!("createAuction missing required fields (itemName, price, duration): {}", data);
                                }
                            }
                        }
                        Err(e) => {
                            error!("Failed to parse createAuction JSON: {}", e);
                        }
                    }
                }
                CoflEvent::Trade(data) => {
                    debug!("Processing trade request");
                    // Parse trade data to get player name
                    if let Ok(trade_data) = serde_json::from_str::<serde_json::Value>(&data) {
                        if let Some(player) = trade_data.get("playerName").and_then(|v| v.as_str()) {
                            command_queue_clone.enqueue(
                                CommandType::AcceptTrade {
                                    player_name: player.to_string(),
                                },
                                CommandPriority::High,
                                false,
                            );
                        } else {
                            warn!("Failed to parse trade data: {}", data);
                        }
                    }
                }
                CoflEvent::RunSequence(data) => {
                    debug!("Received runSequence: {}", data);
                    warn!("runSequence is not yet fully implemented");
                }
                CoflEvent::Countdown => {
                    // COFL sends this ~10 seconds before AH flips arrive.
                    // Matching TypeScript bazaarFlipPauser.ts: pause bazaar flips for 20 seconds
                    // when both AH flips and bazaar flips are enabled.
                    // Relaxed ordering is fine here — these are simple toggle flags where
                    // eventual visibility across threads is sufficient.
                    // Only relevant when BOTH AH and bazaar flips are enabled — if
                    // bazaar is off there's nothing to pause, so don't print/churn.
                    if enable_bazaar_flips_ws.load(Ordering::Relaxed) && enable_ah_flips_ws.load(Ordering::Relaxed) {
                        // Always (re)extend the pause window to now+20s. Rapid repeated
                        // countdowns just push the deadline out.
                        let now_ms = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis() as u64;
                        let deadline = now_ms + 20_000;
                        bazaar_pause_until_ws.fetch_max(deadline, Ordering::Relaxed);

                        let flag = bazaar_flips_paused_ws.clone();
                        // Close any open window so the bot is free for AH flips, and
                        // drop out of interruptible states immediately.
                        bot_client_for_ws.close_current_window();
                        let current_state = bot_client_for_ws.state();
                        if current_state != frikadellen_baf::types::BotState::Purchasing
                            && current_state != frikadellen_baf::types::BotState::Startup
                        {
                            bot_client_for_ws.set_state(frikadellen_baf::types::BotState::Idle);
                        }

                        // Only the unpaused→paused transition prints and spawns the
                        // single resume watcher; later countdowns are silent no-ops
                        // that merely extended the deadline above.
                        if flag.compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
                            let baf_msg = "§f[§4BAF§f]: §c⚡ AH Flips incoming — pausing bazaar".to_string();
                            print_mc_chat(&baf_msg);
                            let _ = chat_tx_ws.send(baf_msg);
                            let chat_tx_resume = chat_tx_ws.clone();
                            let command_queue_resume = command_queue_clone.clone();
                            let pause_until = bazaar_pause_until_ws.clone();
                            tokio::spawn(async move {
                                // Sleep until the (possibly-extended) deadline passes.
                                loop {
                                    let now = std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .unwrap_or_default()
                                        .as_millis() as u64;
                                    let until = pause_until.load(Ordering::Relaxed);
                                    if now >= until {
                                        break;
                                    }
                                    sleep(Duration::from_millis((until - now).min(20_000))).await;
                                }
                                flag.store(false, Ordering::Relaxed);
                                let baf_msg = "§f[§4BAF§f]: §aBazaar flips resumed".to_string();
                                print_mc_chat(&baf_msg);
                                let _ = chat_tx_resume.send(baf_msg);
                                info!("[BazaarFlips] Bazaar flips resumed after AH flip window");
                                if !command_queue_resume.has_manage_orders() {
                                    info!("[BazaarFlips] Queuing deferred ManageOrders after AH flip window");
                                    command_queue_resume.enqueue(
                                        CommandType::ManageOrders { cancel_open: false, target_item: None },
                                        CommandPriority::Normal,
                                        false,
                                    );
                                }
                            });
                        }
                    }
                }
                CoflEvent::CollectAuctions => {
                    // COFL detected sold/expired auctions to collect. Claim them
                    // proactively so AH slots free up (→ can list → frees inventory
                    // → keeps buying) instead of waiting for the periodic sweep.
                    if !command_queue_clone.has_claim_sold() {
                        info!("[CollectAuctions] COFL signalled sold auctions — queuing ClaimSold");
                        command_queue_clone.enqueue(
                            CommandType::ClaimSoldItem,
                            CommandPriority::High,
                            true,
                        );
                    }
                }
                CoflEvent::LicenseList { entries, page: _ } => {
                    // Auto-detect the license index for the current account's IGN.
                    // We searched by `/cofl licenses list <current_ign>`, so the
                    // response contains entries with global license indices.
                    // Look for any non-NONE license matching the current IGN first,
                    // then fall back to any known IGN.
                    let current_ign = &ingame_name_ws;
                    if let Some((found_ign, global_idx, tier)) = entries.iter().find(|(name, _, tier)| {
                        name.eq_ignore_ascii_case(current_ign) && !tier.eq_ignore_ascii_case("NONE")
                    }) {
                        info!("[LicenseDetect] Found {} license index {} for '{}' ", tier, global_idx, found_ign);
                        detected_cofl_license_ws.store(*global_idx, Ordering::Relaxed);
                    } else if let Some((found_ign, global_idx, tier)) = entries.iter().find(|(name, _, tier)| {
                        // Check all configured IGNs as a fallback
                        ingame_names_ws.iter().any(|ign| name.eq_ignore_ascii_case(ign))
                            && !tier.eq_ignore_ascii_case("NONE")
                    }) {
                        info!("[LicenseDetect] Found {} license index {} for '{}' (other account)", tier, global_idx, found_ign);
                        detected_cofl_license_ws.store(*global_idx, Ordering::Relaxed);
                    } else if let Some((found_ign, _, _)) = entries.iter().find(|(name, _, _)| name.eq_ignore_ascii_case(current_ign)) {
                        info!("[LicenseDetect] Found '{}' but only has NONE licenses — no transfer needed", found_ign);
                    } else {
                        // No active license found for any configured IGN — set the
                        // default account so the user's subscription tier is applied.
                        if !license_default_sent_ws.load(Ordering::Relaxed) {
                            license_default_sent_ws.store(true, Ordering::Relaxed);
                            let ws = ws_client_clone.clone();
                            let ign = ingame_name_ws.clone();
                            let chat_tx = chat_tx_ws.clone();
                            info!("[LicenseDetect] No active license for any configured IGN — sending /cofl license default {}", ign);
                            let baf_msg = format!(
                                "§f[§4BAF§f]: §eNo license found — setting §b{}§e as default account...",
                                ign
                            );
                            print_mc_chat(&baf_msg);
                            let _ = chat_tx.send(baf_msg);
                            tokio::spawn(async move {
                                if let Err(e) = ws.set_default_license(&ign).await {
                                    warn!("[LicenseDefault] Failed to set default license: {}", e);
                                }
                            });
                        } else {
                            info!("[LicenseDetect] No license entries matched but license already handled");
                        }
                    }
                }
                CoflEvent::ListInstructions(v) => {
                    if let Some(items) = v.get("items").and_then(|i| i.as_array()) {
                        if !items.is_empty() {
                            print_mc_chat("§f[§4BAF§f]: §a--- Items to list ---");
                            for it in items {
                                let name = it.get("name").and_then(|x| x.as_str()).unwrap_or("?");
                                let item_id = it.get("id").and_then(|x| x.as_str()).unwrap_or("");
                                let clean = frikadellen_baf::utils::remove_minecraft_colors(name);
                                let (blk_finder, blk_profit) = tracked_finder_profit(&flip_tracker_ws, name);
                                if let Some(reason) = config_clone.relist_block_reason(
                                    (!item_id.is_empty()).then_some(item_id),
                                    blk_finder.as_deref(),
                                    blk_profit,
                                ) {
                                    print_mc_chat(&format!("§f[§4BAF§f]: §eWon't list §f{}§e — {}", clean, reason));
                                    continue;
                                }
                                let list_at = it.get("listAt").and_then(|x| x.as_u64()).unwrap_or(0);
                                print_mc_chat(&format!("§f[§4BAF§f]: §e{} §7→ §a{} coins", clean, list_at));
                            }
                            print_mc_chat("§f[§4BAF§f]: §a--- Listing now ---");
                        }
                        let queue_clone = command_queue_clone.clone();
                        let dur = config_clone.auction_duration_hours;
                        for it in items {
                            let name = match it.get("name").and_then(|x| x.as_str()) { Some(n) => n.to_string(), None => continue };
                            let item_id = it.get("id").and_then(|x| x.as_str()).unwrap_or("");
                            let (blk_finder, blk_profit) = tracked_finder_profit(&flip_tracker_ws, &name);
                            if let Some(reason) = config_clone.relist_block_reason(
                                (!item_id.is_empty()).then_some(item_id),
                                blk_finder.as_deref(),
                                blk_profit,
                            ) {
                                info!("[FinderList] Skipping \"{}\" — {}", name, reason);
                                continue;
                            }
                            let list_at = it.get("listAt").and_then(|x| x.as_u64()).unwrap_or(0);
                            if list_at == 0 { continue; }
                            let clean = frikadellen_baf::utils::remove_minecraft_colors(&name);
                            queue_clone.enqueue(
                                frikadellen_baf::types::CommandType::SellToAuction {
                                    item_name: clean,
                                    starting_bid: list_at,
                                    duration_hours: dur,
                                    item_slot: None,
                                    item_id: None,
                                },
                                frikadellen_baf::types::CommandPriority::Normal,
                                false,
                            );
                        }
                    }
                    if let Some(skipped) = v.get("skipped").and_then(|s| s.as_array()) {
                        for s in skipped {
                            let name = s.get("name").and_then(|x| x.as_str()).unwrap_or("?");
                            let reason = s.get("reason").and_then(|x| x.as_str()).unwrap_or("unpriceable");
                            let clean = frikadellen_baf::utils::remove_minecraft_colors(name);
                            print_mc_chat(&format!("§f[§4BAF§f]: §cWon't list §f{}§c — {}", clean, reason));
                        }
                    }
                }
            }
        }

        warn!("WebSocket event loop ended");
    });

    // Spawn command processor
    let command_queue_processor = command_queue.clone();
    let bot_client_clone = bot_client.clone();
    let bazaar_flips_paused_proc = bazaar_flips_paused.clone();
    let macro_paused_proc = macro_paused.clone();
    let command_delay_ms = config.command_delay_ms;
    let auction_listing_delay_ms = config.auction_listing_delay_ms;
    let chat_tx_proc = chat_tx.clone();
    let ws_client_proc = ws_client.clone();
    let bot_client_proc_inv = bot_client.clone();
    tokio::spawn(async move {
        use frikadellen_baf::types::BotState;
        // Debounce: avoid requesting sellinventory too frequently when inventory is full
        let mut last_sellinventory_request = Instant::now() - Duration::from_secs(300);
        // Debounce: as a last resort when the inventory stays full (typically with
        // bazaar-bought items the bot can't re-list), force a bazaar
        // "Sell Inventory Now" to instantly sell everything sellable and free
        // space, breaking the full-inventory deadlock.
        let mut last_instasell_clear = Instant::now() - Duration::from_secs(300);
        const FORCE_INSTASELL_SECS: u64 = 90;
        // Track how long the bot has been continuously in selling mode so we can
        // periodically force-clear the inventory_full flag (it may be stale if no
        // ContainerSetContent packets arrive while the bot is idle).
        let mut selling_mode_since: Option<Instant> = None;
        const SELLING_MODE_RECHECK_SECS: u64 = 30;
        loop {
            // When macro is paused via web panel, skip command processing entirely.
            if macro_paused_proc.load(Ordering::Relaxed) {
                sleep(Duration::from_millis(500)).await;
                continue;
            }

            // Register for notification BEFORE checking the queue.  This
            // prevents a race where a command is enqueued between
            // start_current() returning None and the await: the stored
            // permit from notify_one() will make `notified` resolve
            // immediately.
            let notified = command_queue_processor.notified();
            tokio::pin!(notified);

            // Process commands from queue
            if let Some(cmd) = command_queue_processor.start_current() {
                debug!("Processing command: {:?}", cmd.command_type);

                // During startup, only allow startup-essential commands through.
                // All other commands are deferred until startup completes to avoid
                // sending chat commands (e.g. /ah, /bz) while still in the lobby.
                if bot_client_clone.is_startup_in_progress() {
                    let is_startup_cmd = matches!(
                        cmd.command_type,
                        frikadellen_baf::types::CommandType::CheckCookie
                        | frikadellen_baf::types::CommandType::ManageOrders { .. }
                        | frikadellen_baf::types::CommandType::ClaimSoldItem
                        | frikadellen_baf::types::CommandType::ClaimPurchasedItem
                    );
                    if !is_startup_cmd {
                        debug!("[Queue] Deferring non-startup command during startup: {:?}", cmd.command_type);
                        command_queue_processor.complete_current();
                        sleep(Duration::from_millis(250)).await;
                        continue;
                    }
                }

                // During AH pause, drop incoming bazaar buy orders and defer
                // ManageOrders — they will be re-queued when bazaar flips resume.
                if should_drop_bazaar_command_during_ah_pause(
                    &cmd.command_type,
                    bazaar_flips_paused_proc.load(Ordering::Relaxed),
                    bot_client_clone.is_inventory_full(),
                ) {
                    if matches!(cmd.command_type, frikadellen_baf::types::CommandType::ManageOrders { .. }) {
                        info!("[Queue] Deferring ManageOrders — AH flip window active, will re-queue on resume");
                        let baf_msg = "§f[§4BAF§f]: §e⏸ Order management deferred — AH flips incoming, will resume after".to_string();
                        print_mc_chat(&baf_msg);
                        let _ = chat_tx_proc.send(baf_msg);
                    } else {
                        debug!("[Queue] Dropping bazaar command {:?} — AH flip window active", cmd.command_type);
                    }
                    command_queue_processor.complete_current();
                    sleep(Duration::from_millis(50)).await;
                    continue;
                }

                // Skip SellToAuction commands when the auction house is at the
                // listing limit — avoids the repeated /ah → "Maximum auction count
                // reached" → idle → next SellToAuction spam loop.
                if matches!(cmd.command_type, frikadellen_baf::types::CommandType::SellToAuction { .. })
                    && bot_client_clone.is_auction_at_limit()
                {
                    debug!("[Queue] Dropping SellToAuction — auction limit reached: {:?}", cmd.command_type);
                    command_queue_processor.complete_current();
                    sleep(Duration::from_millis(50)).await;
                    continue;
                }

                // Full inventory "selling mode": when inventory is full, only
                // allow commands that free up space (AH listings, bazaar sells,
                // order management, claims of SOLD items).  Everything else —
                // including ClaimPurchasedItem (which adds items) — is dropped so
                // the bot focuses exclusively on selling until there's space again.
                if bot_client_clone.is_inventory_full() {
                    // Track how long we've been in selling mode.  If the
                    // inventory_full flag has been set for > SELLING_MODE_RECHECK_SECS
                    // without any sell command executing, force-clear it so the
                    // bot rechecks actual inventory state instead of staying
                    // stuck dropping commands indefinitely.
                    let since = selling_mode_since.get_or_insert_with(Instant::now);
                    if since.elapsed() > Duration::from_secs(SELLING_MODE_RECHECK_SECS) {
                        info!("[SellingMode] Inventory full for >{}s — force-clearing flag to recheck", SELLING_MODE_RECHECK_SECS);
                        bot_client_clone.clear_inventory_full();
                        selling_mode_since = None;
                        // Re-check immediately — if inventory is truly full the
                        // cached slot count in is_inventory_full() will re-confirm.
                        if bot_client_clone.is_inventory_full() {
                            // Still full after recheck — restart the timer
                            selling_mode_since = Some(Instant::now());
                        }
                    }
                } else {
                    // Not in selling mode — reset the timer
                    selling_mode_since = None;
                }
                if bot_client_clone.is_inventory_full() {
                    let is_selling_cmd = matches!(
                        cmd.command_type,
                        frikadellen_baf::types::CommandType::SellToAuction { .. }
                        | frikadellen_baf::types::CommandType::BazaarSellOrder { .. }
                        | frikadellen_baf::types::CommandType::ManageOrders { .. }
                        | frikadellen_baf::types::CommandType::ClaimSoldItem
                        | frikadellen_baf::types::CommandType::SellInventoryBz
                        | frikadellen_baf::types::CommandType::CancelAuction { .. }
                    );
                    if !is_selling_cmd {
                        debug!("[Queue] Dropping {:?} — inventory full (selling mode)", cmd.command_type);
                        // Proactively request /cofl sellinventory to get sell
                        // recommendations (especially bazaar) when inventory is full.
                        // Debounce to avoid spamming COFL — 60s between requests.
                        if last_sellinventory_request.elapsed() > Duration::from_secs(60) {
                            last_sellinventory_request = Instant::now();
                            let ws = ws_client_proc.clone();
                            let inv_client = bot_client_proc_inv.clone();
                            tokio::spawn(async move {
                                // Upload inventory first
                                if let Some(inv_json) = inv_client.get_cached_inventory_json() {
                                    let upload_msg = serde_json::json!({
                                        "type": "uploadInventory",
                                        "data": inv_json
                                    }).to_string();
                                    let _ = ws.send_message(&upload_msg).await;
                                    // Let COFL ingest the uploaded inventory before selling.
                                    tokio::time::sleep(tokio::time::Duration::from_millis(600)).await;
                                }
                                let msg = serde_json::json!({
                                    "type": "sellinventory",
                                    "data": serde_json::to_string("").unwrap_or_default()
                                }).to_string();
                                if let Err(e) = ws.send_message(&msg).await {
                                    tracing::warn!("[SellingMode] Failed to request sellinventory: {}", e);
                                } else {
                                    tracing::info!("[SellingMode] Auto-requested sellinventory (inventory full)");
                                }
                            });
                        }
                        command_queue_processor.complete_current();
                        // When inventory is full, also ensure ManageOrders is queued
                        // to collect filled SELL orders (which yield coins, not items)
                        // and free up bazaar order slots.
                        if !command_queue_processor.has_manage_orders() {
                            command_queue_processor.enqueue(
                                frikadellen_baf::types::CommandType::ManageOrders { cancel_open: false, target_item: None },
                                frikadellen_baf::types::CommandPriority::High,
                                false,
                            );
                        }
                        // Last-resort space recovery: if the inventory has stayed
                        // full (the COFL sellinventory / sell-order route isn't
                        // clearing it — common with bazaar-bought items the bot
                        // can't re-list), force a bazaar "Sell Inventory Now" to
                        // instantly sell everything sellable and free space.
                        if last_instasell_clear.elapsed() > Duration::from_secs(FORCE_INSTASELL_SECS) {
                            last_instasell_clear = Instant::now();
                            warn!("[SellingMode] Inventory still full — forcing bazaar Sell-Inventory-Now to free space");
                            let baf_msg = "§f[§4BAF§f]: §e📦 Inventory full — instantly selling inventory on bazaar to free space".to_string();
                            print_mc_chat(&baf_msg);
                            let _ = chat_tx_proc.send(baf_msg);
                            command_queue_processor.enqueue(
                                frikadellen_baf::types::CommandType::SellInventoryBz,
                                frikadellen_baf::types::CommandPriority::High,
                                false,
                            );
                        }
                        sleep(Duration::from_millis(50)).await;
                        continue;
                    }
                }

                // When inventory is near full (≤4 empty slots), skip claiming
                // purchased items to keep space available for selling.  The
                // purchases are safe in the AH collect bin and will be claimed
                // once inventory drains.
                if matches!(cmd.command_type, frikadellen_baf::types::CommandType::ClaimPurchasedItem)
                    && bot_client_clone.is_inventory_near_full()
                {
                    debug!("[Queue] Deferring ClaimPurchasedItem — inventory near full, prioritizing selling");
                    command_queue_processor.complete_current();
                    sleep(Duration::from_millis(50)).await;
                    continue;
                }

                // Skip BUY bazaar orders when the bazaar order limit is reached.
                // SELL orders are NOT skipped — they are critical for emptying
                // inventory.  The send_command handler will attempt them anyway;
                // if the server rejects them the at_limit flag stays set and
                // a ManageOrders run (already queued at intake) will free a slot.
                if matches!(cmd.command_type, frikadellen_baf::types::CommandType::BazaarBuyOrder { .. })
                    && bot_client_clone.is_bazaar_at_limit()
                {
                    debug!("[Queue] Dropping BUY bazaar order — bazaar limit reached: {:?}", cmd.command_type);
                    command_queue_processor.complete_current();
                    sleep(Duration::from_millis(50)).await;
                    continue;
                }

                // Skip SellToAuction when the auction slot is blocked (stuck item).
                // A ClaimSold / ManageOrders cycle will clear the flag.
                if matches!(cmd.command_type, frikadellen_baf::types::CommandType::SellToAuction { .. })
                    && bot_client_clone.is_auction_slot_blocked()
                {
                    warn!("[Queue] Dropping SellToAuction — auction slot blocked (stuck item): {:?}", cmd.command_type);
                    command_queue_processor.complete_current();
                    sleep(Duration::from_millis(50)).await;
                    continue;
                }

                // Send command to bot for execution
                if let Err(e) = bot_client_clone.send_command(cmd.clone()) {
                    warn!("Failed to send command to bot: {}", e);
                }

                // Per-command-type timeout: how long to wait for the bot to leave the
                // busy state before declaring it stuck and forcing a reset.
                let timeout_secs: u64 = match cmd.command_type {
                    frikadellen_baf::types::CommandType::ClaimPurchasedItem
                    | frikadellen_baf::types::CommandType::ClaimSoldItem
                    | frikadellen_baf::types::CommandType::CheckCookie => 60,
                    // ManageOrders processes ONE order per cycle with a 10s
                    // internal deadline; keep external timeout just above.
                    frikadellen_baf::types::CommandType::ManageOrders { .. } => 15,
                    frikadellen_baf::types::CommandType::BazaarBuyOrder { .. }
                    | frikadellen_baf::types::CommandType::BazaarSellOrder { .. } => 20,
                    frikadellen_baf::types::CommandType::SellToAuction { .. } => 15,
                    _ => 10,
                };

                // Poll until the bot returns to an allows_commands() state or we hit the
                // per-type timeout. A single loop replaces the previous per-type if/else chain.
                let deadline = std::time::Instant::now()
                    + std::time::Duration::from_secs(timeout_secs);
                let mut interrupted = false;
                loop {
                    sleep(Duration::from_millis(250)).await;
                    if bot_client_clone.state().allows_commands()
                        || std::time::Instant::now() >= deadline
                    {
                        break;
                    }

                    // Check if a higher-priority command is waiting and the
                    // current command is interruptible.  This lets AH flips
                    // (Critical priority) preempt bazaar operations.
                    if cmd.interruptible {
                        if let Some(next) = command_queue_processor.peek_queued() {
                            if next.priority < cmd.priority {
                                warn!(
                                    "[Queue] Interrupting {:?} ({:?}) for higher-priority {:?} ({:?})",
                                    cmd.command_type, cmd.priority,
                                    next.command_type, next.priority,
                                );
                                bot_client_clone.set_state(BotState::Idle);
                                interrupted = true;
                                break;
                            }
                        }
                    }
                }

                // Safety reset: if the bot is still in a busy state after the timeout,
                // force it back to Idle so the queue can continue.
                if !interrupted && !bot_client_clone.state().allows_commands() {
                    warn!(
                        "[Queue] Command {:?} timed out after {}s — forcing Idle",
                        cmd.command_type, timeout_secs
                    );
                    bot_client_clone.set_state(BotState::Idle);
                }

                command_queue_processor.complete_current();

                // Always wait the configurable inter-command delay so Hypixel interactions
                // don't run back-to-back.  Skip the delay when we interrupted for an
                // AH flip so it is picked up immediately.
                // Use a longer delay after auction listings to prevent "Sending packets too fast" kicks.
                if !interrupted {
                    let delay = if matches!(cmd.command_type, frikadellen_baf::types::CommandType::SellToAuction { .. }) {
                        std::cmp::max(command_delay_ms, auction_listing_delay_ms)
                    } else {
                        command_delay_ms
                    };
                    sleep(Duration::from_millis(delay)).await;
                }
            } else {
                // Queue is empty — wait for a notification instead of busy-polling.
                // Times out after 500 ms so paused-state and other periodic checks
                // still run promptly even when no commands arrive.
                let _ = tokio::time::timeout(
                    Duration::from_millis(500),
                    &mut notified,
                ).await;
            }
        }
    });

    // Bot will complete its startup sequence automatically
    // The state will transition from Startup -> Idle after initialization
    info!("BAF initialization started - waiting for bot to complete setup...");

    // Set up console input handler for commands
    info!("Console interface ready - type commands and press Enter:");
    info!("  /cofl <command> - Send command to COFL websocket");
    info!("  /<command> - Send command to Minecraft");
    info!("  /trex sellinv - Force-list the whole inventory at finder prices");
    info!("  /trex logout [all] - Clear cached Microsoft login (restart to switch account)");
    info!("  <text> - Send chat message to COFL websocket");
    
    // `/trex sellinv` → wake the finder auto-lister for an immediate FORCED
    // inventory upload (finder ignores its confidence filter, still refuses
    // items without enough sold samples and reports those back).
    let force_list_notify = std::sync::Arc::new(tokio::sync::Notify::new());

    // Spawn console input handler (only when enabled in config).  When disabled,
    // stdin is left untouched — useful when running headless / as a service.
    let ws_client_for_console = ws_client.clone();
    let command_queue_for_console = command_queue.clone();
    let bot_client_for_console = bot_client.clone();
    let force_list_notify_console = force_list_notify.clone();
    let ingame_names_for_console = ingame_names.clone();
    let console_input_enabled = config.enable_console_input;

    tokio::spawn(async move {
        if !console_input_enabled {
            info!("[Console] Console input disabled (enable_console_input = false)");
            return;
        }
        // Rustyline provides readline with history (up/down arrow key navigation) and
        // proper terminal handling. Since it's a blocking API we drive it in a
        // dedicated blocking task and send each line over an mpsc channel.
        let (line_tx, mut line_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        tokio::task::spawn_blocking(move || {
            let mut rl = match rustyline::DefaultEditor::new() {
                Ok(ed) => ed,
                Err(e) => {
                    eprintln!("[Console] Failed to initialize readline: {}", e);
                    return;
                }
            };
            loop {
                match rl.readline("") {
                    Ok(line) => {
                        let _ = rl.add_history_entry(line.as_str());
                        if line_tx.send(line).is_err() {
                            break;
                        }
                    }
                    Err(rustyline::error::ReadlineError::Interrupted) => {
                        // Ctrl-C in readline: forward as a shutdown signal
                        let _ = line_tx.send("__SHUTDOWN__".to_string());
                        break;
                    }
                    Err(rustyline::error::ReadlineError::Eof) => {
                        // Ctrl-D / end of stdin
                        break;
                    }
                    Err(e) => {
                        eprintln!("[Console] Readline error: {}", e);
                        break;
                    }
                }
            }
        });

        while let Some(line) = line_rx.recv().await {
            let input = line.trim();
            if input == "__SHUTDOWN__" {
                info!("Received Ctrl+C — shutting down BAF...");
                std::process::exit(0);
            }
            if input.is_empty() {
                continue;
            }
            
            let lowercase_input = input.to_lowercase();

            // `/hypixel ping` (alias `/ping`): report the TRUE game-connection RTT.
            //
            // The live figure comes from the vanilla F3+3 play-ping
            // (ServerboundPingRequest/ClientboundPongResponse) that the bot sends
            // on its actual game socket every few seconds — so it travels the
            // exact path gameplay does, INCLUDING any SOCKS proxy hop. The SLP
            // probe (measure()) opens a separate status connection straight to
            // mc.hypixel.net, which Cloudflare answers at its nearest edge and
            // which skips the proxy entirely — that's why it reads absurdly low
            // (e.g. 3ms) and must not be treated as the real latency.
            if lowercase_input == "/hypixel ping" || lowercase_input == "/ping" {
                info!("[Ping] Reporting live game-connection RTT (F3+3) + SLP edge probe …");
                tokio::spawn(async move {
                    // Live in-connection RTT — the number that actually matters.
                    match frikadellen_baf::hypixel_ping::latest_live_ping_ms() {
                        Some(live) => print_mc_chat(&format!(
                            "§f[§4BAF§f]: §bPing §7→ §agame connection {}ms §7(live, via proxy path)",
                            live
                        )),
                        None => print_mc_chat(
                            "§f[§4BAF§f]: §eLive game ping not measured yet §7(F3+3 pinger warms up ~15s after login) — showing edge probe only",
                        ),
                    }
                    // SLP edge probe for comparison — explicitly labelled so it is
                    // never mistaken for the real latency.
                    match frikadellen_baf::hypixel_ping::measure(4, std::time::Duration::from_millis(200)).await {
                        Ok(s) => print_mc_chat(&format!(
                            "§f[§4BAF§f]: §7edge/SLP probe → avg {}ms (min {}ms, max {}ms) — skips proxy, hits Cloudflare edge",
                            s.avg.as_millis(), s.min.as_millis(), s.max.as_millis()
                        )),
                        Err(e) => print_mc_chat(&format!("§f[§4BAF§f]: §7edge/SLP probe failed: {}", e)),
                    }
                });
                continue;
            }

            // `/trex sellinv` — force-list the whole inventory at finder prices.
            //
            // The finder is reached one of two ways depending on the deployment:
            //   • COFL-primary (websocket_url = coflnet): the dedicated finder
            //     lister socket (role=lister) does the forced upload — poke it
            //     via force_list_notify. Sending inventory over the primary
            //     socket here would just spam COFL, which can't price it, so
            //     send_inventory() is a no-op in that mode.
            //   • finder-only (websocket_url = finder): send_inventory() uploads
            //     directly over the primary socket.
            // We do both; each is inert in the mode it doesn't apply to.
            if lowercase_input == "/trex sellinv" || lowercase_input == "trex sellinv" {
                print_mc_chat("§f[§4BAF§f]: §bForce-selling inventory via finder...");
                force_list_notify_console.notify_one();
                if let Some(inv) = bot_client_for_console.get_cached_inventory_json() {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&inv) {
                        let items = v.get("slots").cloned().unwrap_or(serde_json::json!([]));
                        if let Err(e) = ws_client_for_console.send_inventory(&items, true).await {
                            print_mc_chat(&format!("§f[§4BAF§f]: §cFailed to upload inventory: {}", e));
                        }
                    }
                } else {
                    print_mc_chat("§f[§4BAF§f]: §cNo cached inventory yet");
                }
                continue;
            }

            // `/trex logout [all]` — clear azalea's cached Microsoft login so the
            // next start prompts a fresh sign-in. Without this, tokens cached in
            // ~/.minecraft/azalea-auth.json survive log-deletion and redownload,
            // so the bot keeps reusing the previous account and never offers a
            // sign-in for a different one. `/trex logout all` wipes every cached
            // account; plain `/trex logout` clears only this bot's configured IGNs.
            if lowercase_input == "/trex logout" || lowercase_input == "trex logout"
                || lowercase_input == "/trex logout all" || lowercase_input == "trex logout all"
            {
                let wipe_all = lowercase_input.ends_with("all");
                let keys: Vec<String> = if wipe_all { Vec::new() } else { ingame_names_for_console.clone() };
                match clear_azalea_auth_cache(&keys) {
                    Ok(0) => print_mc_chat("§f[§4BAF§f]: §eNo cached login found to clear (already logged out)."),
                    Ok(n) => print_mc_chat(&format!(
                        "§f[§4BAF§f]: §aCleared {} cached login{}. §7Restart the bot to sign in with a different account.",
                        n, if n == 1 { "" } else { "s" }
                    )),
                    Err(e) => print_mc_chat(&format!("§f[§4BAF§f]: §cFailed to clear cached login: {}", e)),
                }
                continue;
            }

            // Handle /cofl and /baf commands (matching TypeScript consoleHandler.ts)
            if lowercase_input.starts_with("/cofl") || lowercase_input.starts_with("/baf") {
                let parts: Vec<&str> = input.split_whitespace().collect();
                if parts.len() > 1 {
                    let command = parts[1];
                    let args = parts[2..].join(" ");
                    
                    // Handle locally-processed commands (matching TypeScript consoleHandler.ts)
                    match command.to_lowercase().as_str() {
                        "queue" => {
                            // Show command queue status
                            let depth = command_queue_for_console.len();
                            info!("━━━━━━━ Command Queue Status ━━━━━━━");
                            info!("Queue depth: {}", depth);
                            info!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
                            continue;
                        }
                        "clearqueue" => {
                            // Clear command queue
                            command_queue_for_console.clear();
                            info!("Command queue cleared");
                            continue;
                        }
                        // `/cofl ping` is handled server-side by Coflnet (a ping-pong
                        // round-trip measured over the modsocket); we forward it like
                        // any other command rather than intercepting it.
                        // TODO: Add other local commands like forceClaim, connect, sellbz when implemented
                        _ => {
                            // Fall through to send to websocket
                        }
                    }
                    
                    // Send to websocket with command as type
                    // Match TypeScript: data field must be JSON-stringified (double-encoded)
                    let data_json = match serde_json::to_string(&args) {
                        Ok(json) => json,
                        Err(e) => {
                            error!("Failed to serialize command args: {}", e);
                            "\"\"".to_string()
                        }
                    };
                    let message = serde_json::json!({
                        "type": command,
                        "data": data_json  // JSON-stringified to match TypeScript JSON.stringify()
                    }).to_string();
                    
                    if let Err(e) = ws_client_for_console.send_message(&message).await {
                        error!("Failed to send command to websocket: {}", e);
                    } else {
                        info!("Sent command to COFL: {} {}", command, args);
                    }
                } else {
                    // Bare /cofl or /baf command - send as chat type with empty data
                    let data_json = serde_json::to_string("").unwrap();
                    let message = serde_json::json!({
                        "type": "chat",
                        "data": data_json
                    }).to_string();
                    
                    if let Err(e) = ws_client_for_console.send_message(&message).await {
                        error!("Failed to send bare /cofl command to websocket: {}", e);
                    }
                }
            } 
            // Handle other slash commands - send to Minecraft
            else if input.starts_with('/') {
                command_queue_for_console.enqueue(
                    frikadellen_baf::types::CommandType::SendChat { 
                        message: input.to_string() 
                    },
                    frikadellen_baf::types::CommandPriority::High,
                    false,
                );
                info!("Queued Minecraft command: {}", input);
            }
            // Non-slash messages go to websocket as chat (matching TypeScript)
            else {
                // Match TypeScript: data field must be JSON-stringified
                let data_json = match serde_json::to_string(&input) {
                    Ok(json) => json,
                    Err(e) => {
                        error!("Failed to serialize chat message: {}", e);
                        "\"\"".to_string()
                    }
                };
                let message = serde_json::json!({
                    "type": "chat",
                    "data": data_json  // JSON-stringified to match TypeScript JSON.stringify()
                }).to_string();
                
                if let Err(e) = ws_client_for_console.send_message(&message).await {
                    error!("Failed to send chat to websocket: {}", e);
                } else {
                    debug!("Sent chat to COFL: {}", input);
                }
            }
        }
    });
    
    // COFL now automatically sends bazaar flip recommendations — no periodic
    // request needed (previously sent getbazaarflips every 5 minutes).

    // Periodic scoreboard upload every 5 seconds (matching TypeScript setInterval purse update)
    {
        let ws_client_scoreboard = ws_client.clone();
        let bot_client_scoreboard = bot_client.clone();
        tokio::spawn(async move {
            loop {
                sleep(Duration::from_secs(5)).await;
                if bot_client_scoreboard.state().allows_commands() {
                    let scoreboard_lines = bot_client_scoreboard.get_scoreboard_lines();
                    if !scoreboard_lines.is_empty() {
                        let data_json = serde_json::to_string(&scoreboard_lines).unwrap_or_else(|_| "[]".to_string());
                        let msg = serde_json::json!({"type": "uploadScoreboard", "data": data_json}).to_string();
                        if let Err(e) = ws_client_scoreboard.send_message(&msg).await {
                            debug!("Failed to send periodic scoreboard upload: {}", e);
                        } else {
                            debug!("[Scoreboard] Uploaded to COFL: {:?}", scoreboard_lines);
                        }
                    }
                }
            }
        });
    }

    // Periodic inventory upload to finder (every 60s)
    {
        let bc = bot_client.clone();
        let ws = ws_client.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            interval.tick().await;
            loop {
                interval.tick().await;
                if bc.is_auction_at_limit() { continue; }
                if let Some(inv) = bc.get_cached_inventory_json() {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&inv) {
                        let items = v.get("slots").cloned().unwrap_or(serde_json::json!([]));
                        let _ = ws.send_inventory(&items, false).await;
                    }
                }
            }
        });
    }

    // Periodic bazaar order check — collect filled orders and cancel stale ones.
    // Driven by config.bazaar_order_check_interval_seconds (default 30s).
    if config.enable_bazaar_flips {
        let bot_client_orders = bot_client.clone();
        let command_queue_orders = command_queue.clone();
        let bazaar_flips_paused_orders = bazaar_flips_paused.clone();
        let bazaar_tracker_orders = bazaar_tracker.clone();
        let order_interval = config.bazaar_order_check_interval_seconds;
        let cancel_minutes_per_million = config.bazaar_order_cancel_minutes_per_million;
        tokio::spawn(async move {
            use frikadellen_baf::types::{CommandType, CommandPriority};
            // Give startup workflow time to complete before starting periodic checks
            sleep(Duration::from_secs(120)).await;
            loop {
                sleep(Duration::from_secs(order_interval)).await;
                // Skip during AH pause — ManageOrders would be deferred anyway
                // and will be re-queued when bazaar flips resume.
                if bazaar_flips_paused_orders.load(Ordering::Relaxed) {
                    debug!("[BazaarOrders] Skipping periodic order check — AH flips incoming");
                    continue;
                }
                // Skip when the tracker has no filled orders AND age-based
                // cancellation is disabled.  When cancel_minutes_per_million > 0
                // the periodic run is the only mechanism that cancels stale
                // unfilled orders, so we must not skip it even when nothing
                // appears filled — the ManageOrders handler will close
                // immediately if it finds nothing actionable.
                if !bazaar_tracker_orders.has_filled_orders() && cancel_minutes_per_million == 0 {
                    debug!("[BazaarOrders] No filled orders in tracker — skipping periodic ManageOrders");
                    continue;
                }
                // When inventory is full, ManageOrders can't collect buy orders
                // and repeatedly opening/closing the bazaar GUI generates packet
                // spam that risks a kick.  Wait 90 s to give the player (or
                // InstaSell) time to free space, then clear the flag so
                // ManageOrders can retry BUY collection once.  If the inventory
                // is still full, the flag will be re-set on the next failed
                // claim attempt.
                if bot_client_orders.is_inventory_full() {
                    debug!("[BazaarOrders] Inventory full — waiting extra 90s before next order check");
                    sleep(Duration::from_secs(90)).await;
                    bot_client_orders.clear_inventory_full();
                    debug!("[BazaarOrders] Inventory full cooldown elapsed — clearing flag for retry");
                }
                if bot_client_orders.state().allows_commands() && !command_queue_orders.has_manage_orders() {
                    debug!("[BazaarOrders] Periodic order check triggered (every {}s)", order_interval);
                    command_queue_orders.enqueue(
                        CommandType::ManageOrders { cancel_open: false, target_item: None },
                        CommandPriority::Normal,
                        false,
                    );
                }
            }
        });
    }

    // Periodic stale bazaar order cleanup — remove tracked orders that are older
    // than the cancel timeout so the web panel doesn't accumulate stale entries
    // from orders that were cancelled/collected without emitting events.
    {
        let bazaar_tracker_cleanup = bazaar_tracker.clone();
        let cancel_minutes_per_million = config.bazaar_order_cancel_minutes_per_million;
        tokio::spawn(async move {
            // Max age = 2 × cancel_timeout or at least 30 minutes (in seconds)
            let max_age_secs = std::cmp::max(cancel_minutes_per_million * 2, 30) * 60;
            loop {
                sleep(Duration::from_secs(60)).await;
                let removed = bazaar_tracker_cleanup.remove_stale_orders(max_age_secs);
                if removed > 0 {
                    info!("[BazaarTracker] Cleaned up {} stale order(s) older than {}m", removed, max_age_secs / 60);
                }
            }
        });
    }

    // --- Periodic chatBatch upload to Coflnet ---
    // Sends accumulated Hypixel chat messages as a JSON array so Coflnet's
    // ChatBatchCommand can process purchases, collections, and other events.
    {
        let bot_client_chat_batch = bot_client.clone();
        let ws_client_chat_batch = ws_client.clone();
        tokio::spawn(async move {
            // Wait a bit for the bot to connect before starting uploads.
            sleep(Duration::from_secs(30)).await;
            loop {
                sleep(Duration::from_secs(2)).await;
                let batch = bot_client_chat_batch.drain_chat_batch();
                if batch.is_empty() {
                    continue;
                }
                let data_json = serde_json::to_string(&batch).unwrap_or_else(|_| "[]".to_string());
                let msg = serde_json::json!({
                    "type": "chatBatch",
                    "data": data_json
                }).to_string();
                if let Err(e) = ws_client_chat_batch.send_message(&msg).await {
                    debug!("[ChatBatch] Failed to send chatBatch to Coflnet: {}", e);
                } else {
                    debug!("[ChatBatch] Sent {} message(s) to Coflnet", batch.len());
                }
            }
        });
    }

    // Periodic "My Auctions" check to claim sold/expired auctions that don't emit chat events.
    if config.enable_ah_flips {
        let bot_client_ah_claim = bot_client.clone();
        let command_queue_ah_claim = command_queue.clone();
        tokio::spawn(async move {
            use frikadellen_baf::types::{CommandPriority, CommandType};
            // Give startup workflow time to complete before periodic checks.
            sleep(Duration::from_secs(120)).await;
            loop {
                sleep(Duration::from_secs(PERIODIC_AH_CLAIM_CHECK_INTERVAL_SECS)).await;
                let bot_state = bot_client_ah_claim.state();
                let queue_empty = command_queue_ah_claim.is_empty();
                if should_enqueue_periodic_auction_claim(bot_state, queue_empty) {
                    debug!(
                        "[ClaimSold] Periodic My Auctions check triggered (every {}s)",
                        PERIODIC_AH_CLAIM_CHECK_INTERVAL_SECS
                    );
                    command_queue_ah_claim.enqueue(
                        CommandType::ClaimSoldItem,
                        CommandPriority::Normal,
                        false,
                    );
                }
            }
        });
    }

    // Idle-inventory failsafe: if no AH auction has been listed for 30 minutes,
    // force-claim sold/purchased auctions and request `/cofl sellinventory` to
    // unblock any stuck inventory.
    if config.enable_ah_flips {
        let bot_client_idle = bot_client.clone();
        let command_queue_idle = command_queue.clone();
        let ws_client_idle = ws_client.clone();
        let last_listed_idle = last_auction_listed_at.clone();
        tokio::spawn(async move {
            use frikadellen_baf::types::{CommandPriority, CommandType};
            // Wait for startup to complete before starting idle checks.
            sleep(Duration::from_secs(INVENTORY_IDLE_SELLINVENTORY_SECS)).await;
            loop {
                // Sleep for the remaining time until the threshold, capped to 60s minimum.
                let elapsed = last_listed_idle.lock().unwrap().elapsed().as_secs();
                let remaining = INVENTORY_IDLE_SELLINVENTORY_SECS.saturating_sub(elapsed);
                sleep(Duration::from_secs(remaining.max(60))).await;

                let elapsed = last_listed_idle.lock().unwrap().elapsed().as_secs();
                if elapsed < INVENTORY_IDLE_SELLINVENTORY_SECS {
                    continue;
                }
                let bot_state = bot_client_idle.state();
                if !bot_state.allows_commands() {
                    continue;
                }
                info!(
                    "[IdleInventory] No auction listed for {}m — forcing claim + sellinventory",
                    elapsed / 60
                );

                // Clear stale blocking flags that may have been set earlier in
                // the session.  AH slots can free up from expired auctions
                // (which don't trigger ItemSold), so the bot must retry.
                // Bazaar order-limit flags can also become stale if the
                // "coins from selling/buying" chat message was missed.
                if bot_client_idle.is_auction_at_limit() {
                    info!("[IdleInventory] Clearing stale auction_at_limit flag");
                    bot_client_idle.clear_auction_at_limit();
                }
                if bot_client_idle.is_auction_slot_blocked() {
                    info!("[IdleInventory] Clearing stale auction_slot_blocked flag");
                    bot_client_idle.clear_auction_slot_blocked();
                }
                if bot_client_idle.is_bazaar_at_limit() {
                    info!("[IdleInventory] Clearing stale bazaar_at_limit flag");
                    bot_client_idle.clear_bazaar_at_limit();
                }

                // Force-claim sold auctions
                command_queue_idle.enqueue(
                    CommandType::ClaimSoldItem,
                    CommandPriority::Normal,
                    false,
                );
                // Force-claim purchased items (won bids)
                if !bot_client_idle.is_inventory_near_full() {
                    command_queue_idle.enqueue(
                        CommandType::ClaimPurchasedItem,
                        CommandPriority::Normal,
                        false,
                    );
                }
                // Upload inventory and request `/cofl sellinventory`
                let ws = ws_client_idle.clone();
                let bot_inv = bot_client_idle.clone();
                tokio::spawn(async move {
                    // Small delay to let the claim commands start first.
                    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                    if let Some(inv_json) = bot_inv.get_cached_inventory_json() {
                        let upload_msg = serde_json::json!({
                            "type": "uploadInventory",
                            "data": inv_json
                        }).to_string();
                        let _ = ws.send_message(&upload_msg).await;
                        // Let COFL ingest the uploaded inventory before selling.
                        tokio::time::sleep(tokio::time::Duration::from_millis(600)).await;
                    }
                    let msg = serde_json::json!({
                        "type": "sellinventory",
                        "data": serde_json::to_string("").unwrap_or_default()
                    }).to_string();
                    if let Err(e) = ws.send_message(&msg).await {
                        tracing::warn!("[IdleInventory] Failed to send sellinventory: {}", e);
                    } else {
                        tracing::info!("[IdleInventory] Forced sellinventory after {}m idle", elapsed / 60);
                    }
                });
                // Reset the timer so we don't spam every minute.
                *last_listed_idle.lock().unwrap() = Instant::now();
            }
        });
    }

    // Periodic log cleanup — delete archived logs older than 7 days once a day.
    frikadellen_baf::logging::spawn_periodic_log_cleanup();

    // Keep a fresh Hypixel ping figure so bed timing can lead clicks by the real
    // connection latency (refreshed every 60s; also updated by `/hypixel ping`).
    frikadellen_baf::hypixel_ping::spawn_background_refresher(std::time::Duration::from_secs(60));

    // Island guard: if "Your Island" is not in the scoreboard, send
    // /lobby → /play sb → /is to return to the island.
    // Matching TypeScript AFKHandler.ts tryToTeleportToIsland() logic.
    {
        let bot_client_island = bot_client.clone();
        let command_queue_island = command_queue.clone();
        let chat_tx_island = chat_tx.clone();
        tokio::spawn(async move {
            use frikadellen_baf::types::{CommandType, CommandPriority, BotState};

            // Give the startup workflow time to complete before we start checking.
            sleep(Duration::from_secs(60)).await;

            // Track consecutive rejoin attempts to add cooldown when kicked from SkyBlock.
            let mut consecutive_rejoin_attempts: u32 = 0;

            loop {
                sleep(Duration::from_secs(10)).await;

                // A manual (web GUI) or rest-break disconnect wants the bot to
                // stay OFFLINE. state() is Idle in that case, which would
                // otherwise make us treat the bot as "free" and spam rejoin
                // commands / "returning to island" chat every 10s. Respect the
                // deliberate disconnect and do nothing until a restart clears it.
                if bot_client_island.is_disconnect_requested() {
                    consecutive_rejoin_attempts = 0;
                    continue;
                }

                // SkyBlock is down for maintenance — a `/play sb` right now would
                // just bounce. Hold off (the flag auto-clears) instead of spamming
                // the rejoin sequence into a closed server.
                if skyblock_in_maintenance() {
                    consecutive_rejoin_attempts = 0;
                    continue;
                }

                // Don't interfere while the bot is actively doing work.
                // Any non-Idle state means the bot is in a GUI workflow (bazaar,
                // purchasing, selling, claiming, …) and may have navigated away
                // from the island — that is NOT a reason to rejoin.
                if bot_client_island.state() != BotState::Idle {
                    consecutive_rejoin_attempts = 0;
                    continue;
                }

                let lines = bot_client_island.get_scoreboard_lines();

                // Scoreboard not yet populated — skip until it has data.
                if lines.is_empty() {
                    continue;
                }

                // If "Your Island" is in the sidebar we are home — nothing to do.
                if lines.iter().any(|l| l.contains("Your Island")) {
                    consecutive_rejoin_attempts = 0;
                    continue;
                }

                consecutive_rejoin_attempts += 1;

                // Safety cap: after REJOIN_MAX_ATTEMPTS consecutive failures,
                // reset the counter so the backoff does not grow unbounded.
                if consecutive_rejoin_attempts >= REJOIN_MAX_ATTEMPTS {
                    warn!(
                        "[AFKHandler] {} consecutive rejoin attempts failed — resetting backoff",
                        REJOIN_MAX_ATTEMPTS
                    );
                    consecutive_rejoin_attempts = 1;
                }

                // Exponential backoff: after repeated failures, wait longer to avoid
                // infinite transfer cooldown when kicked from SkyBlock.
                if consecutive_rejoin_attempts > 1 {
                    let backoff_secs = std::cmp::min(REJOIN_BACKOFF_BASE_SECS * consecutive_rejoin_attempts as u64, REJOIN_MAX_BACKOFF_SECS);
                    let baf_msg = format!(
                        "§f[§4BAF§f]: §cRejoin attempt #{} — waiting {}s before retry...",
                        consecutive_rejoin_attempts, backoff_secs
                    );
                    print_mc_chat(&baf_msg);
                    let _ = chat_tx_island.send(baf_msg);
                    warn!("[AFKHandler] Consecutive rejoin attempt #{} — backing off {}s", consecutive_rejoin_attempts, backoff_secs);
                    sleep(Duration::from_secs(backoff_secs)).await;
                }

                // Not on island — send the return sequence. Name the current area
                // when we can read it (e.g. Hypixel evacuated us to the Hub on a
                // server restart) so an irregular world change is visible rather
                // than a bare "not on island".
                let baf_msg = match current_skyblock_area(&lines) {
                    Some(area) => format!(
                        "§f[§4BAF§f]: §eIrregular world change — now in §f{}§e, returning to island...",
                        area
                    ),
                    None => "§f[§4BAF§f]: §eNot detected on island — returning to island...".to_string(),
                };
                print_mc_chat(&baf_msg);
                let _ = chat_tx_island.send(baf_msg);
                info!("[AFKHandler] Off island — sending /lobby → /play sb → /is");

                // Send commands with delays between them so each server
                // transfer has time to complete before the next fires.
                // Check bot state between steps: if the bot left Idle (e.g.
                // a flip arrived), abort the sequence so we don't interfere.
                command_queue_island.enqueue(
                    CommandType::SendChat { message: "/lobby".to_string() },
                    CommandPriority::High,
                    false,
                );
                sleep(Duration::from_secs(5)).await;

                if bot_client_island.state() != BotState::Idle {
                    continue;
                }

                command_queue_island.enqueue(
                    CommandType::SendChat { message: "/play sb".to_string() },
                    CommandPriority::High,
                    false,
                );
                sleep(Duration::from_secs(10)).await;

                if bot_client_island.state() != BotState::Idle {
                    continue;
                }

                command_queue_island.enqueue(
                    CommandType::SendChat { message: "/is".to_string() },
                    CommandPriority::High,
                    false,
                );

                // Wait for the island teleport to finish before checking again.
                sleep(Duration::from_secs(15)).await;
            }
        });
    }

    // Heartbeat / stall guard: the bot opens GUI windows constantly during
    // normal operation (bazaar order management, flip purchases, sells, claims).
    // If NOTHING opens a window for STALL_THRESHOLD_SECS the connection is very
    // likely frozen or the bot was silently booted to limbo — the island guard
    // above can't see it because the (stale) scoreboard may still show the
    // island. First try to shake it loose with a rejoin; if the silence persists
    // across STALL_MAX_ATTEMPTS checks, restart the process for a clean session.
    {
        let bot_client_hb = bot_client.clone();
        let command_queue_hb = command_queue.clone();
        let chat_tx_hb = chat_tx.clone();
        tokio::spawn(async move {
            use frikadellen_baf::types::{CommandType, CommandPriority};

            // Seed the heartbeat and let the join/startup workflow finish before
            // we start watching, so startup is never mistaken for a stall.
            mark_activity();
            sleep(Duration::from_secs(STALL_GRACE_SECS)).await;

            let mut attempts: u32 = 0;
            loop {
                sleep(Duration::from_secs(STALL_CHECK_INTERVAL_SECS)).await;

                // A deliberate offline state (web disconnect / rest break) is not
                // a stall — the bot is meant to be idle.
                if bot_client_hb.is_disconnect_requested() {
                    attempts = 0;
                    continue;
                }

                // SkyBlock down for maintenance explains the quiet; the island
                // guard owns that case and it auto-clears.
                if skyblock_in_maintenance() {
                    attempts = 0;
                    continue;
                }

                let idle_secs = secs_since_activity();
                if idle_secs < STALL_THRESHOLD_SECS {
                    attempts = 0;
                    continue;
                }

                attempts += 1;
                let idle_min = idle_secs / 60;
                warn!(
                    "[Heartbeat] No window activity for {}m — stall suspected (attempt {}/{})",
                    idle_min, attempts, STALL_MAX_ATTEMPTS
                );
                let baf_msg = format!(
                    "§f[§4BAF§f]: §cStall suspected — no activity for {}m (attempt {}/{})",
                    idle_min, attempts, STALL_MAX_ATTEMPTS
                );
                print_mc_chat(&baf_msg);
                let _ = chat_tx_hb.send(baf_msg);

                // Soft recovery exhausted — restart the whole process so a fresh
                // session takes over. restart_process() re-execs and never returns.
                if attempts >= STALL_MAX_ATTEMPTS {
                    let baf_msg = "§f[§4BAF§f]: §cRestarting bot — heartbeat detected a period of inactivity".to_string();
                    print_mc_chat(&baf_msg);
                    let _ = chat_tx_hb.send(baf_msg);
                    error!(
                        "[Heartbeat] Stall unrecovered after {} attempts — restarting process",
                        STALL_MAX_ATTEMPTS
                    );
                    // Give the messages a moment to flush to the panel/log first.
                    sleep(Duration::from_secs(2)).await;
                    restart_process();
                }

                // Soft recovery: force a rejoin. If the connection is alive this
                // reopens windows, which ticks the heartbeat and clears the stall.
                let baf_msg = format!(
                    "§f[§4BAF§f]: §eBot connection restarting — reason: heartbeat inactivity (attempt {}/{})",
                    attempts, STALL_MAX_ATTEMPTS
                );
                print_mc_chat(&baf_msg);
                let _ = chat_tx_hb.send(baf_msg);
                for (cmd, delay) in [("/lobby", 5u64), ("/play sb", 10), ("/is", 15)] {
                    command_queue_hb.enqueue(
                        CommandType::SendChat { message: cmd.to_string() },
                        CommandPriority::High,
                        false,
                    );
                    sleep(Duration::from_secs(delay)).await;
                }

                // Give a live session time to open a window (which resets the
                // heartbeat) before the next check, so we don't escalate too fast.
                sleep(Duration::from_secs(STALL_RECOVERY_GRACE_SECS)).await;
            }
        });
    }

    // Automatic account switching timer.
    // When multiple accounts are configured and `multi_switch_time` is set, switch to the
    // next account after the specified number of hours by persisting the next account index
    // and restarting the process.
    // Subtract previously accumulated session time so the timer continues from where
    // it left off after a restart (e.g. humanization break, manual restart, crash).
    if ingame_names.len() > 1 {
        if let Some(switch_hours) = config.multi_switch_time {
            let switch_secs = (switch_hours * 3600.0) as u64;
            let remaining_secs = switch_secs.saturating_sub(previous_session_secs);
            let next_index = (current_account_index + 1) % ingame_names.len();
            let next_name = ingame_names[next_index].clone();
            let index_path = account_index_path.clone();
            let chat_tx_switch = chat_tx.clone();
            let detected_license_switch = detected_cofl_license.clone();
            let ws_switch = ws_client.clone();
            let session_times_path_switch = session_times_path.clone();
            let ign_switch = ingame_name.clone();
            if remaining_secs == 0 {
                info!(
                    "[AccountSwitch] Session time ({:.1}h) already exceeds switch threshold ({:.1}h) — will switch after 30s startup grace",
                    previous_session_secs as f64 / 3600.0, switch_hours
                );
            } else {
                info!(
                    "[AccountSwitch] Will switch from {} to {} in {:.1}h (total {:.1}h, already {:.1}h)",
                    ingame_name, next_name, remaining_secs as f64 / 3600.0,
                    switch_hours, previous_session_secs as f64 / 3600.0
                );
            }
            tokio::spawn(async move {
                // When remaining_secs is 0 (threshold already exceeded), wait
                // 30s to allow the bot to connect and transfer the license.
                let delay = if remaining_secs == 0 { 30 } else { remaining_secs };
                sleep(Duration::from_secs(delay)).await;
                info!(
                    "[AccountSwitch] Switch time reached — switching to account {} ({})",
                    next_index + 1, next_name
                );
                // Clear session time for the outgoing account so it starts
                // fresh when this account is used again.
                clear_session_time(&session_times_path_switch, &ign_switch);
                info!("[AccountSwitch] Cleared session time for {}", ign_switch);
                // Transfer the COFL license to the next account before restarting.
                let license_index = detected_license_switch.load(Ordering::Relaxed);
                if license_index > 0 {
                    if let Err(e) = ws_switch.transfer_license(license_index, &next_name).await {
                        warn!("[AccountSwitch] Failed to transfer license: {}", e);
                    }
                    // Give COFL time to process the license transfer before restarting.
                    sleep(Duration::from_secs(3)).await;
                }
                // Persist the next account index so the next process invocation picks it up.
                if let Err(e) = std::fs::write(&index_path, next_index.to_string()) {
                    warn!("[AccountSwitch] Failed to write account index: {}", e);
                }
                let baf_msg = format!(
                    "§f[§4BAF§f]: §eSwitching to account §b{}§e...",
                    next_name
                );
                print_mc_chat(&baf_msg);
                let _ = chat_tx_switch.send(baf_msg);
                info!("[AccountSwitch] Restarting process with next account...");
                restart_process();
            });
        }
    }

    // Periodic profit summary webhook every 30 minutes
    if let Some(webhook_url) = config.active_webhook_url() {
        let profit_tracker_webhook = profit_tracker.clone();
        let webhook_url = webhook_url.to_string();
        let name = ingame_name.clone();
        let started = std::time::Instant::now();
        let prev_secs_summary = previous_session_secs;
        tokio::spawn(async move {
            loop {
                sleep(Duration::from_secs(30 * 60)).await;
                let (ah, bz) = profit_tracker_webhook.totals();
                let uptime = prev_secs_summary + started.elapsed().as_secs();
                frikadellen_baf::webhook::send_webhook_profit_summary(
                    &name, ah, bz, uptime, &webhook_url,
                )
                .await;
            }
        });
    }

    // Periodic session-time persistence — save the accumulated running time for
    // this account every 60 seconds so a crash or kill preserves most of the data.
    // Profit totals are saved in the same tick: previously they were only written
    // on humanization rest breaks, so any other restart reset profit to 0 while
    // uptime survived.
    {
        let session_times_path_save = session_times_path.clone();
        let ign_save = ingame_name.clone();
        let started_save = std::time::Instant::now();
        let prev_secs_save = previous_session_secs;
        let profit_tracker_save = profit_tracker.clone();
        let profit_path_save = profit_path.clone();
        tokio::spawn(async move {
            loop {
                sleep(Duration::from_secs(60)).await;
                let total_secs = prev_secs_save + started_save.elapsed().as_secs();
                save_session_time(&session_times_path_save, &ign_save, total_secs);
                save_profit_stats(&profit_path_save, &ign_save, &profit_tracker_save);
            }
        });
    }

    // ── Bazaar webhook digest flusher ─────────────────────────────
    // Consolidate placed/collected/cancelled bazaar order webhooks into ONE
    // embed per minute instead of one embed per order action (less spammy).
    if let Some(url) = config.active_bazaar_webhook_url() {
        frikadellen_baf::webhook::spawn_bazaar_digest_flusher(
            url.to_string(),
            ingame_name.clone(),
            60,
        );
    }

    // ── Human-like rest breaks ───────────────────────────────────
    // When enabled, periodically disconnect from the server for a randomized
    // duration, then restart the process to reconnect.
    // Session time is saved right before restart so it is preserved across
    // the break — the account-switching timer is NOT reset.
    if config.humanization_enabled {
        let chat_tx_human = chat_tx.clone();
        let ign_human = ingame_name.clone();
        let webhook_url_human = config.active_webhook_url().map(|s| s.to_string());
        let session_times_path_human = session_times_path.clone();
        let prev_secs_human = previous_session_secs;
        let started_human = std::time::Instant::now();
        let macro_paused_human = macro_paused.clone();
        // For a real break we must also stop *flips* coming in, not just drop the
        // Minecraft connection: pause flip intake and clear the queue so nothing
        // is dispatched before the process restarts into its offline break wait.
        let flip_intake_paused_human = flip_intake_paused.clone();
        let command_queue_human = command_queue.clone();
        let profit_tracker_human = profit_tracker.clone();
        let profit_path_human = profit_path.clone();
        let min_interval = config.humanization_min_interval_minutes.max(5); // floor at 5 min
        let max_interval = config.humanization_max_interval_minutes.max(min_interval + 1);
        let min_break = config.humanization_min_break_minutes.max(1); // floor at 1 min
        let max_break = config.humanization_max_break_minutes.max(min_break + 1);
        info!(
            "[Humanization] Enabled — interval {}-{}m, break {}-{}m",
            min_interval, max_interval, min_break, max_break
        );
        tokio::spawn(async move {
            use rand::Rng;

            // Sleep for a random interval between min and max
            let interval_secs = {
                let mut rng = rand::rng();
                rng.random_range(min_interval * 60..=max_interval * 60)
            };
            info!(
                "[Humanization] Next rest break in {:.1}m",
                interval_secs as f64 / 60.0
            );
            sleep(Duration::from_secs(interval_secs)).await;

            // If the macro is paused by the user, wait until it's unpaused
            // before starting the rest break.  This prevents the scheduler
            // from overriding the user's manual pause.
            if macro_paused_human.load(std::sync::atomic::Ordering::Relaxed) {
                info!("[Humanization] Macro is paused — deferring rest break until resumed");
                loop {
                    sleep(Duration::from_secs(5)).await;
                    if !macro_paused_human.load(std::sync::atomic::Ordering::Relaxed) {
                        info!("[Humanization] Macro resumed — proceeding with rest break");
                        break;
                    }
                }
            }

            // Pick random break duration
            let break_secs = {
                let mut rng = rand::rng();
                rng.random_range(min_break * 60..=max_break * 60)
            };
            info!(
                "[Humanization] Starting rest break ({:.1}m) — disconnecting from server",
                break_secs as f64 / 60.0
            );

            // Notify via webhook
            if let Some(ref url) = webhook_url_human {
                frikadellen_baf::webhook::send_webhook_rest_break_start(
                    &ign_human,
                    break_secs,
                    url,
                )
                .await;
            }

            // Notify chat
            let baf_msg = format!(
                "§f[§4BAF§f]: §e😴 Taking a rest break ({:.0}m). Disconnecting...",
                break_secs as f64 / 60.0
            );
            print_mc_chat(&baf_msg);
            let _ = chat_tx_human.send(baf_msg);

            // A rest break must leave the account genuinely offline for the whole
            // duration. Dropping the connection in-process and sleeping proved
            // unreliable — the ECS client / AFK handler / reconnect loop could
            // bring the bot back and leave it idling in the lobby (never actually
            // "resting"). Instead we persist a break-until marker and restart NOW:
            // the fresh process waits out the remaining break BEFORE connecting to
            // Hypixel or COFL (see pending_rest_break_secs at startup), so nothing
            // is connected for the entire break.
            flip_intake_paused_human.store(true, std::sync::atomic::Ordering::Relaxed);
            macro_paused_human.store(true, std::sync::atomic::Ordering::Relaxed);
            command_queue_human.clear();

            // Save profit + session time so both survive the restart/break. Session
            // time is saved right before restart so the gap is near-zero.
            save_profit_stats(&profit_path_human, &ign_human, &profit_tracker_human);
            let total_secs = prev_secs_human + started_human.elapsed().as_secs();
            save_session_time(&session_times_path_human, &ign_human, total_secs);

            // Persist the break deadline, then restart. The next process start stays
            // offline until this time and sends the "break over" webhook.
            let until = unix_now() + break_secs;
            write_rest_break_marker(&ign_human, until);
            info!(
                "[Humanization] Rest break ({:.1}m) — saved session {}s, restarting offline until break ends",
                break_secs as f64 / 60.0, total_secs
            );
            restart_process();
        });
    }

    // ── Finder auto-lister ───────────────────────────────────────────────────
    // Finder-only mode: COFL never sends createAuction, so upload our inventory
    // to the finder every minute and list whatever it prices (its listing
    // recommendation). Covers freshly-bought items AND reclaimed expired
    // auctions (both land back in inventory). Own role=lister connection so the
    // finder never routes flips here.
    {
        let finder_list_url = config.finder_ws_url.clone().filter(|u| !u.trim().is_empty()).or_else(|| {
            config
                .multisocket_urls
                .iter()
                .map(|u| u.trim().to_string())
                .find(|u| !u.is_empty() && !u.contains("coflnet") && !u.contains("/modsocket"))
        });
        if config.finder_auto_list {
            if let Some(url) = finder_list_url {
                let token = config.finder_ws_token.clone().unwrap_or_default();
                let bc = bot_client.clone();
                let queue = command_queue.clone();
                let dur = config.auction_duration_hours;
                let config_relist = config.clone();
                let force_notify = force_list_notify.clone();
                let flip_tracker_list = flip_tracker.clone();
                tokio::spawn(async move {
                    use futures::{SinkExt, StreamExt};
                    // Per item-name cooldown so we don't re-instruct while a
                    // listing is still in flight (item leaves inventory once listed).
                    let mut listed_recently: std::collections::HashMap<String, std::time::Instant> = std::collections::HashMap::new();
                    let mut backoff = 5u64;
                    loop {
                        // Explicit "/" path: a bare-authority ws URL makes the
                        // handshake line "GET ?role=… HTTP/1.1", which strict
                        // HTTP parsers (the finder's Node server) reject as 400.
                        let full = if token.is_empty() {
                            format!("{}/?role=lister", url.trim_end_matches('/'))
                        } else {
                            format!("{}/?role=lister&token={}", url.trim_end_matches('/'), token)
                        };
                        match tokio_tungstenite::connect_async(&full).await {
                            Ok((mut stream, _)) => {
                                info!("[FinderList] Connected to finder for auto-listing: {}", url);
                                backoff = 5;
                                // Event-driven: upload the moment an AH slot frees
                                // (sold auction claimed / expired reclaim) or the
                                // inventory content changes (new buy landed), with a
                                // 60s fallback. 10s tick keeps reactions prompt
                                // without hammering the finder.
                                let mut upload = tokio::time::interval(std::time::Duration::from_secs(10));
                                let mut last_auction_count = usize::MAX;
                                let mut last_inv_hash = 0u64;
                                let mut last_upload = std::time::Instant::now() - std::time::Duration::from_secs(3600);
                                loop {
                                    tokio::select! {
                                        _ = upload.tick() => {
                                            // No point pricing when nothing can be listed.
                                            if bc.is_auction_at_limit() || bc.is_auction_slot_blocked() { continue; }
                                            let Some(inv) = bc.get_cached_inventory_json() else { continue };
                                            let count = bc.active_auction_count();
                                            let slot_freed = count < last_auction_count;
                                            last_auction_count = count;
                                            let inv_hash = {
                                                use std::hash::{Hash, Hasher};
                                                let mut h = std::collections::hash_map::DefaultHasher::new();
                                                inv.hash(&mut h);
                                                h.finish()
                                            };
                                            let inv_changed = inv_hash != last_inv_hash;
                                            let stale = last_upload.elapsed() >= std::time::Duration::from_secs(60);
                                            // Debounce content-only changes a little.
                                            if !(slot_freed || stale || (inv_changed && last_upload.elapsed() >= std::time::Duration::from_secs(15))) {
                                                continue;
                                            }
                                            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&inv) {
                                                let items = v.get("slots").cloned().unwrap_or(serde_json::json!([]));
                                                let msg = serde_json::json!({ "type": "inventory", "items": items }).to_string();
                                                if stream.send(tokio_tungstenite::tungstenite::Message::Text(msg)).await.is_err() {
                                                    break;
                                                }
                                                last_inv_hash = inv_hash;
                                                last_upload = std::time::Instant::now();
                                                if slot_freed && !stale {
                                                    info!("[FinderList] AH slot freed ({} active) — asking finder for listing suggestions", count);
                                                } else if inv_changed && !stale {
                                                    info!("[FinderList] Inventory changed — asking finder for listing suggestions");
                                                }
                                            }
                                        }
                                        _ = force_notify.notified() => {
                                            // `/trex sellinv`: immediate forced upload — the finder
                                            // drops its confidence gate and reports unlistable items.
                                            if let Some(inv) = bc.get_cached_inventory_json() {
                                                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&inv) {
                                                    let items = v.get("slots").cloned().unwrap_or(serde_json::json!([]));
                                                    let msg = serde_json::json!({ "type": "inventory", "items": items, "force": true }).to_string();
                                                    info!("[FinderList] Force-sell: uploading inventory to finder");
                                                    listed_recently.clear(); // force = re-instruct everything
                                                    if stream.send(tokio_tungstenite::tungstenite::Message::Text(msg)).await.is_err() {
                                                        break;
                                                    }
                                                    last_upload = std::time::Instant::now();
                                                }
                                            } else {
                                                warn!("[FinderList] Force-sell requested but no cached inventory yet");
                                            }
                                        }
                                        m = stream.next() => {
                                            let txt = match m {
                                                Some(Ok(tokio_tungstenite::tungstenite::Message::Text(t))) => t,
                                                Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_))) | Some(Err(_)) | None => break,
                                                _ => continue,
                                            };
                                            let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) else { continue };
                                            if v.get("type").and_then(|t| t.as_str()) != Some("listInstructions") { continue; }
                                            // Force responses explain every skipped item — surface that.
                                            if let Some(sk) = v.get("skipped").and_then(|s| s.as_array()) {
                                                for s in sk {
                                                    let name = s.get("name").and_then(|x| x.as_str()).unwrap_or("?");
                                                    let reason = s.get("reason").and_then(|x| x.as_str()).unwrap_or("unpriceable");
                                                    let clean = frikadellen_baf::utils::remove_minecraft_colors(name);
                                                    frikadellen_baf::logging::print_mc_chat(&format!("§f[§4BAF§f]: §eWon't list §f{}§e — {}", clean, reason));
                                                    info!("[FinderList] Won't list \"{}\" — {}", clean, reason);
                                                }
                                            }
                                            let Some(items) = v.get("items").and_then(|i| i.as_array()) else { continue };
                                            listed_recently.retain(|_, t| t.elapsed() < std::time::Duration::from_secs(300));
                                            for it in items {
                                                let name = match it.get("name").and_then(|x| x.as_str()) { Some(n) => n.to_string(), None => continue };
                                                let item_id = it.get("id").and_then(|x| x.as_str()).unwrap_or("");
                                                let clean = frikadellen_baf::utils::remove_minecraft_colors(&name);
                                                let (blk_finder, blk_profit) = tracked_finder_profit(&flip_tracker_list, &name);
                                                if let Some(reason) = config_relist.relist_block_reason(
                                                    (!item_id.is_empty()).then_some(item_id),
                                                    blk_finder.as_deref(),
                                                    blk_profit,
                                                ) {
                                                    info!("[FinderList] Won't list \"{}\" — {}", clean, reason);
                                                    frikadellen_baf::logging::print_mc_chat(&format!(
                                                        "§f[§4BAF§f]: §eWon't list §f{}§e — {}",
                                                        clean, reason
                                                    ));
                                                    continue;
                                                }
                                                let list_at = it.get("listAt").and_then(|x| x.as_u64()).unwrap_or(0);
                                                if list_at == 0 { continue; }
                                                if listed_recently.contains_key(&clean) { continue; }
                                                listed_recently.insert(clean.clone(), std::time::Instant::now());
                                                info!("[FinderList] Listing \"{}\" at finder price {}", clean, list_at);
                                                queue.enqueue(
                                                    frikadellen_baf::types::CommandType::SellToAuction {
                                                        item_name: clean.clone(),
                                                        starting_bid: list_at,
                                                        duration_hours: dur,
                                                        item_slot: None,
                                                        item_id: None,
                                                    },
                                                    frikadellen_baf::types::CommandPriority::Normal,
                                                    false,
                                                );
                                                // -- Report listing to finder ---------------------
                                                // Send itemUuid and flipUuid so the finder can
                                                // update its self-listing guard and listing UUIDs.
                                                let item_uuid = it.get("uuid").and_then(|x| x.as_str()).map(String::from);
                                                let flip_uuid = {
                                                    let needle = clean.to_lowercase();
                                                    // Access the entry while the guard is alive and clone the
                                                    // uuid out — a reference into the map can't outlive the lock.
                                                    flip_tracker_list.lock().ok()
                                                        .and_then(|ft| ft.get(&needle).and_then(|entry| entry.0.uuid.clone()))
                                                };
                                                let listed_msg = serde_json::json!({
                                                    "type": "listed",
                                                    "auctionUuid": null,
                                                    "itemUuid": item_uuid,
                                                    "flipUuid": flip_uuid,
                                                    "itemName": &clean,
                                                }).to_string();
                                                let _ = stream.send(tokio_tungstenite::tungstenite::Message::Text(listed_msg)).await;
                                            }
                                        }
                                    }
                                }
                                warn!("[FinderList] Disconnected — reconnecting...");
                            }
                            Err(e) => warn!("[FinderList] Connect failed: {} (retry {}s)", e, backoff),
                        }
                        tokio::time::sleep(std::time::Duration::from_secs(backoff)).await;
                        backoff = (backoff * 2).min(60);
                    }
                });
            }
        }
    }

    // Keep the application running
    info!("BAF is now running. Type commands below or press Ctrl+C to exit.");
    
    // Wait until Ctrl+C (SIGINT) is received
    tokio::signal::ctrl_c().await?;
    info!("Received Ctrl+C — shutting down BAF...");
    // Save final session time and profit before exit.
    let total_secs = previous_session_secs + session_start.elapsed().as_secs();
    save_session_time(&session_times_path, &ingame_name, total_secs);
    save_profit_stats(&profit_path, &ingame_name, &profit_tracker);
    info!("[SessionTime] Saved final session time for {}: {}s ({:.2}h)", ingame_name, total_secs, total_secs as f64 / 3600.0);
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::{is_ban_disconnect, parse_cofl_profit_response, parse_cofl_bz_h_total_profit, parse_short_number, parse_bz_list_flip_detail, should_drop_bazaar_command_during_ah_pause, should_enqueue_periodic_auction_claim, parse_island_visitor, parse_name_mention, is_direct_address, current_skyblock_area, note_maintenance, skyblock_in_maintenance, mark_activity, secs_since_activity};
    use frikadellen_baf::types::{BotState, CommandType};

    #[test]
    fn skyblock_area_from_scoreboard() {
        let lines = vec![
            "SKYBLOCK".to_string(),
            " ⏣ Hub".to_string(),
            "Purse: 1,000".to_string(),
        ];
        assert_eq!(current_skyblock_area(&lines), Some("Hub".to_string()));
        // No area glyph → no area (fall back to the generic notice).
        let no_area = vec!["SKYBLOCK".to_string(), "Purse: 1,000".to_string()];
        assert_eq!(current_skyblock_area(&no_area), None);
    }

    #[test]
    fn maintenance_flag_sets_and_transitions_once() {
        // First maintenance line transitions up→down (should notify).
        assert!(note_maintenance());
        assert!(skyblock_in_maintenance());
        // A repeat while still down does NOT re-transition (no duplicate notify).
        assert!(!note_maintenance());
        assert!(skyblock_in_maintenance());
    }

    #[test]
    fn activity_heartbeat_seeds_and_stays_fresh() {
        // Before seeding, 0 elapsed means "never stalled".
        // After marking, elapsed is small (well under the stall threshold).
        mark_activity();
        assert!(secs_since_activity() < super::STALL_THRESHOLD_SECS);
    }

    #[test]
    fn island_visitor_with_rank() {
        assert_eq!(
            parse_island_visitor("[MVP+] CoolGuy123 is visiting your island!"),
            Some("CoolGuy123".to_string())
        );
    }

    #[test]
    fn island_visitor_without_rank() {
        assert_eq!(
            parse_island_visitor("Steve is visiting your island!"),
            Some("Steve".to_string())
        );
    }

    #[test]
    fn island_visitor_ignores_unrelated_lines() {
        assert_eq!(parse_island_visitor("You are visiting Bob's island"), None);
        assert_eq!(parse_island_visitor("Someone joined the lobby"), None);
    }

    #[test]
    fn name_mention_fires_on_dms_and_direct_address() {
        // Any incoming whisper counts — someone is reaching out.
        assert_eq!(
            parse_name_mention("From [VIP] Friend: yo BafBot", "BafBot"),
            Some("From [VIP] Friend: yo BafBot".to_string())
        );
        assert_eq!(
            parse_name_mention("From Steve: anything at all", "BafBot"),
            Some("From Steve: anything at all".to_string())
        );
        // Public/guild chat: only when directly addressed.
        assert_eq!(
            parse_name_mention("Guild > [MVP+] Steve: BafBot help please", "BafBot"),
            Some("Guild > [MVP+] Steve: BafBot help please".to_string())
        );
        assert_eq!(
            parse_name_mention("Party > Steve: @BafBot come here", "BafBot"),
            Some("Party > Steve: @BafBot come here".to_string())
        );
    }

    #[test]
    fn name_mention_ignores_midsentence_and_noise() {
        // Name mid-sentence in public/guild chat is NOT a mention.
        assert_eq!(parse_name_mention("Guild > [MVP+] Steve: i think BafBot is afk", "BafBot"), None);
        assert_eq!(parse_name_mention("[MVP+] Steve: lol BafBot", "BafBot"), None);
        // Bot's own guild message must not ping.
        assert_eq!(parse_name_mention("Guild > [MVP+] BafBot: BafBot online", "BafBot"), None);
        // Outgoing DM ("To ...") must not ping even if it contains the name.
        assert_eq!(parse_name_mention("To [VIP] Friend: this is BafBot", "BafBot"), None);
        // System line without a valid player sender must not ping.
        assert_eq!(parse_name_mention("Reward: BafBot got 5 coins", "BafBot"), None);
    }

    #[test]
    fn direct_address_only_at_start_or_at_handle() {
        assert!(is_direct_address("BafBot help", "BafBot"));
        assert!(is_direct_address("  BafBot, come here", "bafbot")); // leading space + case
        assert!(is_direct_address("yo @BafBot please", "BafBot"));
        assert!(!is_direct_address("i think BafBot is afk", "BafBot")); // mid-sentence
        assert!(!is_direct_address("BafBotter is cool", "BafBot")); // not a whole token
    }

    #[test]
    fn detects_temporary_ban_disconnect() {
        assert!(is_ban_disconnect("You are temporarily banned for 29d from this server!"));
    }

    #[test]
    fn detects_ban_id_disconnect() {
        assert!(is_ban_disconnect("Disconnect reason ... Ban ID: #692672FA"));
    }

    #[test]
    fn detects_permanent_ban_disconnect() {
        assert!(is_ban_disconnect("You are permanently banned from this server!"));
    }

    #[test]
    fn ignores_non_ban_disconnect() {
        assert!(!is_ban_disconnect("Disconnected: Timed out"));
    }

    #[test]
    fn detects_security_ban_disconnect() {
        assert!(is_ban_disconnect("Your account has been blocked."));
        assert!(is_ban_disconnect("Find out more: https://www.hypixel.net/security-block"));
        assert!(is_ban_disconnect("Block ID: #ABC123"));
    }

    #[test]
    fn periodic_auction_claim_requires_idle_and_empty_queue() {
        assert!(should_enqueue_periodic_auction_claim(BotState::Idle, true));
        assert!(!should_enqueue_periodic_auction_claim(BotState::ClaimingSold, true));
        assert!(!should_enqueue_periodic_auction_claim(BotState::Idle, false));
    }

    #[test]
    fn ah_pause_drops_bazaar_and_manage_orders_commands() {
        let paused = true;
        let buy = CommandType::BazaarBuyOrder {
            item_name: "Booster Cookie".into(),
            item_tag: None,
            amount: 1,
            price_per_unit: 1.0,
        };
        // BUY orders are always dropped during the AH pause, full or not.
        assert!(should_drop_bazaar_command_during_ah_pause(&buy, paused, false));
        assert!(should_drop_bazaar_command_during_ah_pause(&buy, paused, true));
        // BazaarSellOrder should NOT be dropped during AH pause (only buy orders are dropped)
        assert!(!should_drop_bazaar_command_during_ah_pause(
            &CommandType::BazaarSellOrder {
                item_name: "Booster Cookie".into(),
                item_tag: None,
                amount: 1,
                price_per_unit: 1.0,
            },
            paused,
            false,
        ));
        assert!(!should_drop_bazaar_command_during_ah_pause(
            &CommandType::ClaimSoldItem,
            paused,
            false,
        ));
        // ManageOrders IS deferred during AH pause when inventory is NOT full —
        // it would block the AH flip purchase.
        let manage = CommandType::ManageOrders { cancel_open: false, target_item: None };
        assert!(should_drop_bazaar_command_during_ah_pause(&manage, paused, false));
        // ...but when the inventory IS full it must NOT be deferred, so the bot
        // can keep managing orders to free space and escape the deadlock.
        assert!(!should_drop_bazaar_command_during_ah_pause(&manage, paused, true));
        // Nothing is dropped when not paused.
        assert!(!should_drop_bazaar_command_during_ah_pause(&manage, false, true));
    }

    #[test]
    fn parse_cofl_profit_response_82m() {
        let msg = "According to our data TestUser made 82.7M in the last 0.05 days across 6 auctions";
        assert_eq!(parse_cofl_profit_response(msg), Some(82_700_000));
    }

    #[test]
    fn parse_cofl_profit_response_1b() {
        let msg = "According to our data Player123 made 1.5B in the last 2.3 days across 142 auctions";
        assert_eq!(parse_cofl_profit_response(msg), Some(1_500_000_000));
    }

    #[test]
    fn parse_cofl_profit_response_plain() {
        let msg = "According to our data SomeIGN made 500 in the last 0.01 days across 1 auctions";
        assert_eq!(parse_cofl_profit_response(msg), Some(500));
    }

    #[test]
    fn parse_cofl_profit_response_250k() {
        let msg = "According to our data IGN made 250K in the last 0.1 days across 3 auctions";
        assert_eq!(parse_cofl_profit_response(msg), Some(250_000));
    }

    #[test]
    fn parse_cofl_profit_response_no_match() {
        assert_eq!(parse_cofl_profit_response("Some random chat message"), None);
    }

    #[test]
    fn parse_short_number_values() {
        assert_eq!(parse_short_number("82.7M"), Some(82_700_000));
        assert_eq!(parse_short_number("1.5B"), Some(1_500_000_000));
        assert_eq!(parse_short_number("250K"), Some(250_000));
        assert_eq!(parse_short_number("500"), Some(500));
        assert_eq!(parse_short_number("1,500,000"), Some(1_500_000));
        assert_eq!(parse_short_number("abc"), None);
    }

    #[test]
    fn parse_bz_list_flip_detail_profit() {
        let line = "2xJungle Key: 1.05M -> 287K => -768K(1)";
        let (name, profit, count) = parse_bz_list_flip_detail(line).unwrap();
        assert_eq!(name, "Jungle Key");
        assert_eq!(profit, -768_000);
        assert_eq!(count, 1);
    }

    #[test]
    fn parse_bz_list_flip_detail_multiple_flips() {
        let line = "128xWorm Membrane: 7.16M -> 7.91M => 741K(7)";
        let (name, profit, count) = parse_bz_list_flip_detail(line).unwrap();
        assert_eq!(name, "Worm Membrane");
        assert_eq!(profit, 741_000);
        assert_eq!(count, 7);
    }

    #[test]
    fn parse_bz_list_flip_detail_no_match() {
        assert!(parse_bz_list_flip_detail("Some random text").is_none());
        assert!(parse_bz_list_flip_detail("Last Completed Bazaar Flips").is_none());
    }

    #[test]
    fn parse_cofl_bz_h_negative_profit() {
        let msg = "Total Profit: -234M";
        assert_eq!(parse_cofl_bz_h_total_profit(msg), Some(-234_000_000));
    }

    #[test]
    fn parse_cofl_bz_h_positive_profit() {
        let msg = "Total Profit: 1.5B";
        assert_eq!(parse_cofl_bz_h_total_profit(msg), Some(1_500_000_000));
    }

    #[test]
    fn parse_cofl_bz_h_in_context() {
        let msg = "Bazaar Profit History for TestUser (last 1 days)\nTotal Profit: -234M\nAverage Daily Profit: -33.5M";
        assert_eq!(parse_cofl_bz_h_total_profit(msg), Some(-234_000_000));
    }

    #[test]
    fn parse_cofl_bz_h_no_match() {
        assert_eq!(parse_cofl_bz_h_total_profit("Some random message"), None);
    }
}
