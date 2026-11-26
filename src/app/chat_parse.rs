//! Parsers for Coflnet / Hypixel chat lines.
//!
//! Moved out of main.rs unchanged; see `app/mod.rs`.

#[allow(unused_imports)]
use crate::*;

/// Parse a Coflnet `/cofl profit` response and return the total profit in coins.
///
/// Expected format (color-stripped):
/// `"According to our data <ign> made <amount> in the last <days> days across <N> auctions"`
///
/// `<amount>` may be a short notation like `82.7M`, `1.5B`, `250K`, or a plain number.
pub(crate) fn parse_cofl_profit_response(clean_msg: &str) -> Option<i64> {
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
pub(crate) fn parse_island_visitor(clean: &str) -> Option<String> {
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
pub(crate) fn is_valid_minecraft_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 16
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Extract the sender's name from a player-chat header such as
/// `"Guild > [MVP+] Someone"`, `"From [VIP] Someone"`, `"[MVP+] Someone"`, or
/// `"Someone"`. Returns `None` when no valid player name can be identified
/// (i.e. the line is almost certainly a system message, not player chat).
pub(crate) fn extract_chat_sender(header: &str) -> Option<String> {
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
pub(crate) fn is_direct_address(body: &str, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let name_l = name.to_ascii_lowercase();
    let body_l = body.trim_start().to_ascii_lowercase();
    let boundary = |c: u8| !(c.is_ascii_alphanumeric() || c == b'_');

    // Name at the start of the message, followed by a boundary (or end of line).
    if let Some(rest) = body_l.strip_prefix(&name_l) {
        if rest.as_bytes().first().is_none_or(|&c| boundary(c)) {
            return true;
        }
    }

    // "@name" anywhere, as a whole token. The '@' itself is a left boundary.
    let at_name = format!("@{}", name_l);
    let bytes = body_l.as_bytes();
    let mut start = 0;
    while let Some(pos) = body_l[start..].find(&at_name) {
        let end = start + pos + at_name.len();
        if bytes.get(end).is_none_or(|&c| boundary(c)) {
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
pub(crate) fn parse_name_mention(clean: &str, own_name: &str) -> Option<String> {
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
pub(crate) fn parse_short_number(s: &str) -> Option<i64> {
    let s = s.replace(',', "");
    let (num_part, multiplier) =
        if let Some(n) = s.strip_suffix('B').or_else(|| s.strip_suffix('b')) {
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
pub(crate) fn parse_bz_list_flip_profit(line: &str) -> Option<i64> {
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
pub(crate) fn parse_bz_list_flip_detail(line: &str) -> Option<(String, i64, u32)> {
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
pub(crate) fn parse_cofl_bz_h_total_profit(clean_msg: &str) -> Option<i64> {
    let prefix = "Total Profit: ";
    let idx = clean_msg.find(prefix)?;
    let after = &clean_msg[idx + prefix.len()..];
    // Take until the next whitespace or end of string.
    let value_str: String = after.chars().take_while(|c| !c.is_whitespace()).collect();
    parse_short_number(&value_str)
}
