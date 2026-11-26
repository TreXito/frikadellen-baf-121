//! Pure helpers for reading item names / lore and parsing bazaar order and
//! auction text. None of these touch the live bot; moved out of client.rs
//! unchanged so that file is easier to navigate.

#[allow(unused_imports)]
use super::*;

/// Remove Minecraft §-prefixed color/format codes from a string
pub(super) fn remove_mc_colors(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '§' {
            chars.next(); // skip the code character
        } else {
            result.push(c);
        }
    }
    result
}

/// Get the display name of an item slot as a plain string (no color codes).
/// Checks `minecraft:custom_name` first (custom-named items), then falls back
/// to `minecraft:item_name` (base item name override used by some Hypixel GUI items).
pub(super) fn get_item_display_name_from_slot(
    item: &azalea_inventory::ItemStack,
) -> Option<String> {
    if let Some(item_data) = item.as_present() {
        if let Ok(value) = serde_json::to_value(item_data) {
            let components = value.get("components");
            // Try minecraft:custom_name first, then minecraft:item_name as fallback
            let name_val = components
                .and_then(|c| c.get("minecraft:custom_name"))
                .or_else(|| components.and_then(|c| c.get("minecraft:item_name")));
            if let Some(name_val) = name_val {
                let raw = if name_val.is_string() {
                    name_val.as_str().unwrap_or("").to_string()
                } else {
                    name_val.to_string()
                };
                // The name may be a JSON chat component string like {"text":"..."}
                let plain = if let Ok(json_val) = serde_json::from_str::<serde_json::Value>(&raw) {
                    extract_text_from_chat_component(&json_val)
                } else {
                    remove_mc_colors(&raw)
                };
                return Some(plain);
            }
        } else {
            // Full item_data serialization failed (e.g., enchantment HashMap keys).
            // Fall back to direct component access for the display name.
            use azalea_inventory::components::CustomName;
            if let Some(cn) = item_data.component_patch.get::<CustomName>() {
                if let Ok(cn_val) = serde_json::to_value(cn) {
                    let plain = extract_text_from_chat_component(&cn_val);
                    if !plain.is_empty() {
                        return Some(plain);
                    }
                }
            }
        }
    }
    None
}

/// Get the display name of an item slot preserving §-color codes for rarity display.
/// Same logic as `get_item_display_name_from_slot` but uses `extract_text_with_colors`
/// so the web panel can render the name in the item's rarity color.
pub(super) fn get_item_display_name_with_colors_from_slot(
    item: &azalea_inventory::ItemStack,
) -> Option<String> {
    if let Some(item_data) = item.as_present() {
        if let Ok(value) = serde_json::to_value(item_data) {
            let components = value.get("components");
            let name_val = components
                .and_then(|c| c.get("minecraft:custom_name"))
                .or_else(|| components.and_then(|c| c.get("minecraft:item_name")));
            if let Some(name_val) = name_val {
                let raw = if name_val.is_string() {
                    name_val.as_str().unwrap_or("").to_string()
                } else {
                    name_val.to_string()
                };
                let colored = if let Ok(json_val) = serde_json::from_str::<serde_json::Value>(&raw)
                {
                    extract_text_with_colors(&json_val)
                } else {
                    raw
                };
                return Some(colored);
            }
        } else {
            use azalea_inventory::components::CustomName;
            if let Some(cn) = item_data.component_patch.get::<CustomName>() {
                if let Ok(cn_val) = serde_json::to_value(cn) {
                    let colored = extract_text_with_colors(&cn_val);
                    if !colored.is_empty() {
                        return Some(colored);
                    }
                }
            }
        }
    }
    None
}

/// Recursively extract plain text from an Azalea/Minecraft chat component
pub(super) fn extract_text_from_chat_component(val: &serde_json::Value) -> String {
    let mut result = String::new();
    if let Some(text) = val.get("text").and_then(|v| v.as_str()) {
        result.push_str(text);
    }
    if let Some(extra) = val.get("extra").and_then(|v| v.as_array()) {
        for part in extra {
            result.push_str(&extract_text_from_chat_component(part));
        }
    }
    remove_mc_colors(&result)
}

/// Recursively extract text from a Minecraft chat component, preserving §-color codes.
/// Converts JSON `"color"` fields back to § codes so the web UI can render them.
pub(super) fn extract_text_with_colors(val: &serde_json::Value) -> String {
    let mut result = String::new();
    // Emit color/format codes from the component's properties
    if let Some(color) = val.get("color").and_then(|v| v.as_str()) {
        let code = match color {
            "black" => Some('0'),
            "dark_blue" => Some('1'),
            "dark_green" => Some('2'),
            "dark_aqua" => Some('3'),
            "dark_red" => Some('4'),
            "dark_purple" => Some('5'),
            "gold" => Some('6'),
            "gray" => Some('7'),
            "dark_gray" => Some('8'),
            "blue" => Some('9'),
            "green" => Some('a'),
            "aqua" => Some('b'),
            "red" => Some('c'),
            "light_purple" => Some('d'),
            "yellow" => Some('e'),
            "white" => Some('f'),
            _ => None,
        };
        if let Some(c) = code {
            result.push('§');
            result.push(c);
        }
    }
    if val.get("bold").and_then(|v| v.as_bool()) == Some(true) {
        result.push_str("§l");
    }
    if val.get("italic").and_then(|v| v.as_bool()) == Some(true) {
        result.push_str("§o");
    }
    if val.get("underlined").and_then(|v| v.as_bool()) == Some(true) {
        result.push_str("§n");
    }
    if val.get("strikethrough").and_then(|v| v.as_bool()) == Some(true) {
        result.push_str("§m");
    }
    if val.get("obfuscated").and_then(|v| v.as_bool()) == Some(true) {
        result.push_str("§k");
    }
    if let Some(text) = val.get("text").and_then(|v| v.as_str()) {
        result.push_str(text);
    }
    if let Some(extra) = val.get("extra").and_then(|v| v.as_array()) {
        for part in extra {
            result.push_str(&extract_text_with_colors(part));
        }
    }
    result
}

/// Read the raw `minecraft:lore` array directly from an item's Lore component.
///
/// This deliberately avoids `serde_json::to_value(item_data)` on the whole stack:
/// that fails for enchanted items (their `HashMap<Enchantment, i32>` has non-string
/// map keys), which previously made lore come back empty even though the item's own
/// `Lore` component serialises fine. Reading the component directly is robust and
/// keeps the top-level `lore` consistent with the `nbt.minecraft:lore` we emit.
pub(super) fn lore_component_array(item: &azalea_inventory::ItemStack) -> Vec<serde_json::Value> {
    use azalea_inventory::components::Lore;
    if let Some(item_data) = item.as_present() {
        if let Some(lore) = item_data.component_patch.get::<Lore>() {
            if let Ok(val) = serde_json::to_value(lore) {
                // Depending on the azalea version the Lore component serialises either
                // as a bare array of text components or as `{ "lines": [...] }`.
                if let Some(arr) = val.as_array() {
                    return arr.clone();
                }
                if let Some(arr) = val.get("lines").and_then(|l| l.as_array()) {
                    return arr.clone();
                }
            }
        }
    }
    Vec::new()
}

/// Get lore lines from an item slot as plain strings (no color codes)
pub(super) fn get_item_lore_from_slot(item: &azalea_inventory::ItemStack) -> Vec<String> {
    let mut lore_lines = Vec::new();
    for entry in lore_component_array(item) {
        let raw = if entry.is_string() {
            entry.as_str().unwrap_or("").to_string()
        } else {
            entry.to_string()
        };
        let plain = if let Ok(json_val) = serde_json::from_str::<serde_json::Value>(&raw) {
            extract_text_from_chat_component(&json_val)
        } else {
            remove_mc_colors(&raw)
        };
        lore_lines.push(plain);
    }
    lore_lines
}

/// Get lore lines from an item slot preserving Minecraft §-color codes.
/// Used by the web panel to render colorful lore tooltips.
pub(super) fn get_item_lore_with_colors_from_slot(
    item: &azalea_inventory::ItemStack,
) -> Vec<String> {
    let mut lore_lines = Vec::new();
    for entry in lore_component_array(item) {
        let raw = if entry.is_string() {
            entry.as_str().unwrap_or("").to_string()
        } else {
            entry.to_string()
        };
        let colored = if let Ok(json_val) = serde_json::from_str::<serde_json::Value>(&raw) {
            extract_text_with_colors(&json_val)
        } else {
            raw
        };
        lore_lines.push(colored);
    }
    lore_lines
}

/// Format a f64 price with commas for bazaar sign input.
/// Uses integer arithmetic (tenths) to avoid floating-point subtraction issues.
/// Commas are inserted into the integer part: 7500000.0 → "7,500,000",
/// 2040950.5 → "2,040,950.5".
pub(super) fn format_price_for_sign(price: f64) -> String {
    // Work in tenths-of-a-coin as an integer to avoid floating-point precision
    // issues when splitting integer and fractional parts of large prices.
    let tenths = (price * 10.0).round() as i64;
    let int_part = tenths / 10;
    let frac_digit = (tenths % 10).unsigned_abs();

    let int_str = format_with_commas(int_part);
    if frac_digit == 0 {
        int_str
    } else {
        format!("{}.{}", int_str, frac_digit)
    }
}

/// Insert commas as thousands separators into an integer.
/// Handles negative numbers correctly: -1234567 → "-1,234,567".
pub(super) fn format_with_commas(n: i64) -> String {
    let is_negative = n < 0;
    let s = n.unsigned_abs().to_string();
    let mut result = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            result.push(',');
        }
        result.push(ch);
    }
    if is_negative {
        result.insert(0, '-');
    }
    result
}

/// Normalize a string for fuzzy item-name matching: lowercase, strip Minecraft
/// color codes, replace hyphens/underscores with spaces, remove common decorative
/// Unicode symbols, and collapse consecutive whitespace.
pub(super) fn normalize_for_matching(s: &str) -> String {
    remove_mc_colors(s)
        .to_lowercase()
        .replace(['-', '_'], " ")
        .replace(['☘', '☂', '✪', '◆', '❤'], "")
        .split_whitespace()
        .collect::<Vec<&str>>()
        .join(" ")
}

pub(super) fn find_slot_by_name(
    slots: &[azalea_inventory::ItemStack],
    name: &str,
) -> Option<usize> {
    let name_lower = name.to_lowercase();

    // Phase 1: fast-path — original exact `contains` check (case-insensitive).
    for (i, item) in slots.iter().enumerate() {
        if let Some(display) = get_item_display_name_from_slot(item) {
            if display.to_lowercase().contains(&name_lower) {
                return Some(i);
            }
        }
    }

    // Phase 2: normalized `contains` — strips hyphens, underscores, decorative
    // symbols and MC color codes so e.g. "turbo wheat v" matches "Turbo-Wheat V".
    let name_norm = normalize_for_matching(name);
    for (i, item) in slots.iter().enumerate() {
        if let Some(display) = get_item_display_name_from_slot(item) {
            if normalize_for_matching(&display).contains(&name_norm) {
                return Some(i);
            }
        }
    }

    // Phase 3: token-based — every whitespace-delimited token of the search term
    // must appear somewhere in the (normalized) slot display name.  This handles
    // cases where the slot name has extra words, e.g. "Turbo-Wheat Hoe V" still
    // matches a search for "turbo wheat v".
    let tokens: Vec<&str> = name_norm.split_whitespace().collect();
    if tokens.len() > 1 {
        for (i, item) in slots.iter().enumerate() {
            if let Some(display) = get_item_display_name_from_slot(item) {
                let display_norm = normalize_for_matching(&display);
                if tokens.iter().all(|tok| display_norm.contains(tok)) {
                    return Some(i);
                }
            }
        }
    }

    None
}

/// Like [`find_slot_by_name`] but restricted to the player-inventory portion of
/// an open container (`inv_start`..end) and to non-empty slots.
///
/// The auction "select item" flow uses this for its name fallback so it can only
/// ever resolve to a real item the player is holding — never a GUI/display slot
/// (clicking one to "sell" a non-item is an impossible action Watchdog flags),
/// and never an empty slot. Returns the absolute slot index.
pub(super) fn find_inventory_slot_by_name(
    slots: &[azalea_inventory::ItemStack],
    inv_start: usize,
    name: &str,
) -> Option<usize> {
    if inv_start >= slots.len() {
        return None;
    }
    let name_lower = name.to_lowercase();
    let name_norm = normalize_for_matching(name);
    let tokens: Vec<&str> = name_norm.split_whitespace().collect();
    // Phase 0: exact contains; 1: normalized contains; 2: all-tokens-present.
    for phase in 0..3u8 {
        for (i, slot) in slots.iter().enumerate().skip(inv_start) {
            if slot.is_empty() {
                continue;
            }
            let display = match get_item_display_name_from_slot(slot) {
                Some(d) => d,
                None => continue,
            };
            let hit = match phase {
                0 => display.to_lowercase().contains(&name_lower),
                1 => normalize_for_matching(&display).contains(&name_norm),
                _ => {
                    tokens.len() > 1 && {
                        let dn = normalize_for_matching(&display);
                        tokens.iter().all(|t| dn.contains(t))
                    }
                }
            };
            if hit {
                return Some(i);
            }
        }
    }
    None
}

pub(super) fn lore_contains_phrase(lore: &[String], needle: &str) -> bool {
    let needle_lower = needle.to_lowercase();
    lore.iter().any(|line| {
        remove_mc_colors(line)
            .to_lowercase()
            .contains(&needle_lower)
    })
}

pub(super) fn find_slot_by_lore_contains(
    slots: &[azalea_inventory::ItemStack],
    needle: &str,
) -> Option<usize> {
    slots.iter().enumerate().find_map(|(i, item)| {
        let lore = get_item_lore_from_slot(item);
        lore_contains_phrase(&lore, needle).then_some(i)
    })
}

/// Parse the remaining grace-period time in seconds from the bed item displayed in
/// slot 31 of the BIN Auction View.  Hypixel typically shows the time in the item's
/// lore as a "M:SS" or "MM:SS" pattern (e.g. "0:45", "1:00").
/// Returns `None` if no time can be extracted.
pub(super) fn parse_bed_remaining_secs(item: &azalea_inventory::ItemStack) -> Option<u64> {
    let name = get_item_display_name_from_slot(item).unwrap_or_default();
    let lore = get_item_lore_from_slot(item);
    let all_text = std::iter::once(name)
        .chain(lore)
        .collect::<Vec<_>>()
        .join(" ");
    parse_bed_remaining_secs_from_text(&all_text)
}

pub(super) fn parse_bed_remaining_secs_from_text(all_text: &str) -> Option<u64> {
    // Match "M:SS" or "MM:SS" — the first such pattern is the time remaining
    let mut chars = all_text.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() {
            let mut minutes = String::from(c);
            while chars.peek().map(|x| x.is_ascii_digit()).unwrap_or(false) {
                minutes.push(chars.next().unwrap());
            }
            if chars.next() == Some(':') {
                let mut secs = String::new();
                for _ in 0..2 {
                    if let Some(d) = chars.next() {
                        if d.is_ascii_digit() {
                            secs.push(d);
                        } else {
                            break;
                        }
                    }
                }
                if secs.len() == 2 {
                    if let (Ok(m), Ok(s)) = (minutes.parse::<u64>(), secs.parse::<u64>()) {
                        if s < 60 {
                            return Some(m * 60 + s);
                        }
                    }
                }
            }
        }
    }
    // Match textual variants like "1m 5s"
    if let Ok(minute_second_re) =
        regex::Regex::new(r"(?i)\b(\d+)\s*m(?:in(?:ute)?s?)?\s*(\d+)\s*s(?:ec(?:ond)?s?)?\b")
    {
        if let Some(caps) = minute_second_re.captures(all_text) {
            if let (Some(m), Some(s)) = (caps.get(1), caps.get(2)) {
                if let (Ok(m), Ok(s)) = (m.as_str().parse::<u64>(), s.as_str().parse::<u64>()) {
                    if s < 60 {
                        return Some(m * 60 + s);
                    }
                }
            }
        }
    }
    // Match seconds-only variants like "59s" / "59 sec"
    if let Ok(second_only_re) = regex::Regex::new(r"(?i)\b(\d+)\s*s(?:ec(?:ond)?s?)?\b") {
        if let Some(caps) = second_only_re.captures(all_text) {
            if let Some(s) = caps.get(1) {
                if let Ok(s) = s.as_str().parse::<u64>() {
                    return Some(s);
                }
            }
        }
    }
    None
}

/// Check if a lowercased string contains "ended" as a standalone word
/// (e.g. "auction ended") but NOT as a suffix of another word (e.g.
/// "recommended", "mended", "befriended").  Uses a regex word boundary.
pub(super) fn contains_word_ended(text: &str) -> bool {
    static RE: Lazy<regex::Regex> = Lazy::new(|| regex::Regex::new(r"\bended\b").unwrap());
    RE.is_match(text)
}

/// Returns true if the item is a claimable (sold/ended/expired) auction slot.
pub(super) fn is_claimable_auction_slot(item: &azalea_inventory::ItemStack) -> bool {
    is_claimable_auction_lore(&get_item_lore_from_slot(item))
}

/// Claimable-auction check on plain lore lines.
///
/// A finished auction's `Status:` line decides: a BIN keeps its
/// "Buy it now: N coins" line after it sold or expired ("Status: Sold!",
/// "Status: Expired!"), so the active-auction indicators below must not veto
/// it. They did, and expired or sold BINs were never claimed. Without a status
/// line this matches TypeScript ingameMessageHandler claimableIndicators /
/// activeIndicators.
pub(super) fn is_claimable_auction_lore(lore: &[String]) -> bool {
    if lore.is_empty() {
        return false;
    }
    let combined = lore.join("\n").to_lowercase();
    let finished_status = combined.lines().any(|line| {
        line.trim().strip_prefix("status:").is_some_and(|status| {
            status.contains("sold") || status.contains("expired") || contains_word_ended(status)
        })
    });
    if finished_status {
        return true;
    }
    // Must have at least one claimable indicator (from TypeScript claimableIndicators)
    let has_claimable = combined.contains("sold!")
        || contains_word_ended(&combined)
        || combined.contains("expired")
        || combined.contains("click to claim")
        || combined.contains("claim your");
    // Must NOT have active-auction indicators (from TypeScript activeIndicators)
    let is_active = combined.contains("ends in")
        || combined.contains("buy it now")
        || combined.contains("starting bid");
    has_claimable && !is_active
}

/// Whether a claimable "Manage Auctions" slot was listed by *this* account.
///
/// Only consulted when `only_claim_own_auctions` is on. Anything we cannot
/// positively attribute is left for its owner — see [`crate::auction_ownership`]
/// for why an undecidable slot is skipped rather than claimed.
pub(super) fn claim_slot_is_ours(item: &azalea_inventory::ItemStack, slot: usize) -> bool {
    use crate::auction_ownership::{ownership_of, Ownership};
    let Some(display_name) = get_item_display_name_from_slot(item) else {
        return false;
    };
    let price = extract_price_from_lore(&get_item_lore_from_slot(item));
    match ownership_of(&display_name, price) {
        Ownership::Ours => true,
        Ownership::Foreign => {
            debug!(
                "[ClaimSold] Slot {} ('{}') is not one of ours — leaving it for the co-op",
                slot,
                remove_mc_colors(&display_name)
            );
            false
        }
        Ownership::Unknown => {
            warn!(
                "[ClaimSold] Cannot tell whether slot {} ('{}') is ours — skipping it. \
                 No listing history yet and no auctions returned for this account; \
                 it will resolve once the bot lists something or the API answers.",
                slot,
                remove_mc_colors(&display_name)
            );
            false
        }
    }
}

pub(super) fn is_my_auctions_window_title(window_title: &str) -> bool {
    window_title.contains("Manage Auctions") || window_title.contains("My Auctions")
}

/// Calculate the number of "window-content" slots (excluding the 36 player
/// inventory slots appended at the bottom).  Capped at 54 (max chest size).
pub(super) fn window_content_slot_count(total_slots: usize) -> usize {
    total_slots.saturating_sub(36).min(54)
}

pub(super) fn is_bazaar_orders_window_title(window_title: &str) -> bool {
    let lower = window_title.to_lowercase();
    lower.contains("manage orders")
        || lower.contains("your orders")
        || lower.contains("bazaar orders")
}

pub(super) fn starts_with_phrase_delimited(text: &str, phrase: &str) -> bool {
    if !text.starts_with(phrase) {
        return false;
    }
    match text[phrase.len()..].chars().next() {
        None => true,
        Some(c) => !c.is_ascii_alphanumeric(),
    }
}

pub(super) fn is_bazaar_order_entry_name(name: &str) -> bool {
    let lower = name.trim_start().to_lowercase();
    if starts_with_phrase_delimited(&lower, "buy order") {
        return true;
    }
    if starts_with_phrase_delimited(&lower, "sell offer") {
        return true;
    }
    // Legacy order-list entries are usually "BUY <item>" / "SELL <item>".
    // Avoid matching malformed "buy orderX"/"sell offerY" labels.
    if lower.starts_with("buy ") && !lower.starts_with("buy order") {
        return true;
    }
    if lower.starts_with("sell ") && !lower.starts_with("sell offer") {
        return true;
    }
    false
}

pub(super) fn normalize_bazaar_order_text(text: &str) -> String {
    remove_mc_colors(text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Parse a "[Bazaar] Your Buy Order/Sell Offer for X was filled!" notification.
/// Returns `(item_name, is_buy_order)` or `None` if the message doesn't match.
pub(super) fn parse_bazaar_filled_notification(message: &str) -> Option<(String, bool)> {
    if !message.contains("[Bazaar]") || !message.contains("was filled") {
        return None;
    }
    let is_buy = message.contains("Buy Order");
    let is_sell = message.contains("Sell Offer");
    if !is_buy && !is_sell {
        return None;
    }
    let prefix = if is_buy {
        "Buy Order for "
    } else {
        "Sell Offer for "
    };
    let item_name = message
        .split(prefix)
        .nth(1)
        .and_then(|s| s.split(" was filled").next())
        .unwrap_or("")
        .trim()
        .to_string();
    if item_name.is_empty() {
        return None;
    }
    Some((item_name, is_buy))
}

/// Extract the clean item name from a ManageOrders order slot display name.
/// For enchanted books (and other items where the display name is generic),
/// prefers the lore-based identity which contains the specific enchantment
/// name (e.g. "Blast Protection VII" instead of "Enchanted Book").
/// Falls back to stripping the BUY/SELL prefix from the display name.
pub(super) fn clean_order_item_name(
    order_name: &str,
    order_identity: &Option<(bool, String)>,
) -> String {
    let stripped = remove_mc_colors(order_name).trim().to_string();

    // Check if the display name is generic (e.g. "SELL Enchanted Book" or "BUY Enchanted Book").
    // When the lore-based identity has a *different* item name, prefer it.
    if let Some((_, ref lore_item)) = order_identity {
        // Strip the order prefix from display name for comparison.
        // These prefixes are lowercase because we compare against a lowercased
        // copy of the display name (different from the case-preserved prefixes
        // used below for the display name extraction path).
        let display_item = {
            let lower = stripped.to_lowercase();
            let mut found = String::new();
            for prefix in ["buy order: ", "sell offer: ", "buy ", "sell "] {
                if let Some(rest) = lower.strip_prefix(prefix) {
                    found = rest.trim().to_string();
                    break;
                }
            }
            found
        };
        let lore_lower = lore_item.to_lowercase();
        // If lore identity differs from display name, use lore (it's more specific).
        // e.g. display="enchanted book", lore="blast protection vii"
        if !display_item.is_empty() && !lore_lower.is_empty() && display_item != lore_lower {
            return crate::utils::to_title_case(lore_item);
        }
    }

    // First try stripping prefix from the ORIGINAL display name (preserves case)
    for prefix in ["BUY ", "SELL ", "Buy Order: ", "Sell Offer: "] {
        if let Some(rest) = stripped.strip_prefix(prefix) {
            let rest = rest.trim();
            if !rest.is_empty() {
                return rest.to_string();
            }
        }
    }
    // Case-insensitive fallback for unexpected prefix casing
    let lower = stripped.to_lowercase();
    for prefix in ["buy order: ", "sell offer: ", "buy ", "sell "] {
        if let Some(rest) = lower.strip_prefix(prefix) {
            if !rest.is_empty() {
                // Apply title case to recover proper display name
                return crate::utils::to_title_case(rest.trim());
            }
        }
    }
    // Last resort: use identity (lowercased) with title case
    if let Some((_, item)) = order_identity {
        return crate::utils::to_title_case(item);
    }
    stripped
}

pub(super) fn parse_bazaar_order_identity_from_name(name: &str) -> Option<(bool, String)> {
    let normalized = normalize_bazaar_order_text(name);
    if let Some(item) = normalized.strip_prefix("buy order: ") {
        return Some((true, item.trim().to_string()));
    }
    if let Some(item) = normalized.strip_prefix("sell offer: ") {
        return Some((false, item.trim().to_string()));
    }
    if let Some(item) = normalized.strip_prefix("buy ") {
        if !item.starts_with("order") {
            return Some((true, item.trim().to_string()));
        }
    }
    if let Some(item) = normalized.strip_prefix("sell ") {
        if !item.starts_with("offer") {
            return Some((false, item.trim().to_string()));
        }
    }
    None
}

pub(super) fn parse_bazaar_order_identity_from_lore(lore: &[String]) -> Option<(bool, String)> {
    let mut side: Option<bool> = None;
    let mut item_name: Option<String> = None;

    for line in lore {
        let clean = normalize_bazaar_order_text(line);
        if side.is_none() {
            if clean.contains("buy order") {
                side = Some(true);
            } else if clean.contains("sell offer") {
                side = Some(false);
            }
        }
        if item_name.is_none() {
            for prefix in ["item:", "product:", "commodity:"] {
                if let Some(rest) = clean.strip_prefix(prefix) {
                    let candidate = rest.trim();
                    if !candidate.is_empty() {
                        item_name = Some(candidate.to_string());
                        break;
                    }
                }
            }
        }
    }

    match (side, item_name) {
        (Some(is_buy), Some(item)) => Some((is_buy, item)),
        _ => None,
    }
}

pub(super) fn is_buy_bazaar_order_name(name: &str) -> bool {
    let lower = name.trim_start().to_lowercase();
    starts_with_phrase_delimited(&lower, "buy order") || lower.starts_with("buy ")
}

pub(super) fn parse_bazaar_order_identity(name: &str, lore: &[String]) -> Option<(bool, String)> {
    // Prefer lore-based identity when available — it contains the *specific*
    // item name (e.g. "Blast Protection VII") while the display name may be
    // generic (e.g. "Enchanted Book").  Fall back to name-based parsing when
    // the lore has no identity data.
    parse_bazaar_order_identity_from_lore(lore)
        .or_else(|| parse_bazaar_order_identity_from_name(name))
}

pub(super) fn should_treat_as_bazaar_order_slot(
    name: &str,
    identity: Option<&(bool, String)>,
) -> bool {
    is_bazaar_order_entry_name(name) || identity.is_some()
}

/// Returns `true` when the order lore indicates items/coins are ready to collect.
///
/// Hypixel shows "Filled: X/Y 100%!" for fully filled orders and includes
/// "Click to claim!" or "items to claim" in the tooltip.  Partially filled
/// orders also show "items to claim" even though they are not 100%.
///
/// We intentionally check for *any* claimable indicator so that both fully
/// and partially filled orders are considered for collection.
pub(super) fn is_order_claimable_from_lore(lore: &[String]) -> bool {
    for line in lore {
        let clean = remove_mc_colors(line).to_lowercase();
        // "Filled: 64/64 100%!" — fully filled
        if clean.contains("100%") {
            return true;
        }
        // "Click to claim!" — present on fully filled orders
        if clean.contains("click to claim") {
            return true;
        }
        // "You have X items to claim!" — present on filled buy orders
        if clean.contains("to claim") {
            return true;
        }
    }
    false
}

/// Parse the filled amount from Hypixel's order lore.
///
/// Lore lines contain `"Filled: X/Y Z%"` (e.g. `"Filled: 32/64 50%"`).
/// Returns `Some((filled, total))` when found.  Used to determine the actual
/// quantity claimed (not the original order amount) so buy-cost recording and
/// profit calculations are accurate for partial fills.
pub(super) fn parse_filled_amount_from_lore(lore: &[String]) -> Option<(u64, u64)> {
    for line in lore {
        let clean = remove_mc_colors(line);
        // Expected format: "Filled: 32/64 50%"
        // After lowercasing: "filled: 32/64 50%"
        let lower = clean.to_lowercase();
        if let Some(idx) = lower.find("filled:") {
            // Extract the part after "filled:" — e.g. " 32/64 50%"
            let after = &clean[idx + "filled:".len()..].trim_start();
            // Split at '/' to get filled and total
            if let Some(slash_pos) = after.find('/') {
                let filled_str = after[..slash_pos].replace(',', "");
                let rest = &after[slash_pos + 1..];
                // Total ends at the next space or end of string
                let total_str = rest
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .replace(',', "");
                if let (Ok(filled), Ok(total)) =
                    (filled_str.parse::<u64>(), total_str.parse::<u64>())
                {
                    return Some((filled, total));
                }
            }
        }
    }
    None
}

/// Parse the total order amount from Hypixel's order lore.
///
/// Lore lines contain `"Order amount: 64x"` (e.g. `"Order amount: 2,560x"`).
/// Returns the total amount when found.
pub(super) fn parse_order_amount_from_lore(lore: &[String]) -> Option<u64> {
    for line in lore {
        let clean = remove_mc_colors(line);
        let lower = clean.to_lowercase();
        if let Some(idx) = lower.find("order amount:") {
            let after = &clean[idx + "order amount:".len()..].trim_start();
            // Strip trailing 'x' and commas, e.g. "2,560x" → "2560"
            let num_str: String = after
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == ',')
                .collect::<String>()
                .replace(',', "");
            if let Ok(amount) = num_str.parse::<u64>() {
                return Some(amount);
            }
        }
    }
    // Fallback: try to extract the total from the Filled line (X/Y → Y)
    parse_filled_amount_from_lore(lore).map(|(_, total)| total)
}

/// Parse the price per unit from Hypixel's order lore.
///
/// Lore lines contain `"Price per unit: 1,958.0 coins"`.
/// Returns the price when found.
pub(super) fn parse_unit_price_from_lore(lore: &[String]) -> Option<f64> {
    for line in lore {
        let clean = remove_mc_colors(line);
        let lower = clean.to_lowercase();
        if let Some(idx) = lower.find("price per unit:") {
            let after = &clean[idx + "price per unit:".len()..].trim_start();
            let num_str: String = after
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == ',' || *c == '.')
                .collect::<String>()
                .replace(',', "");
            if let Ok(price) = num_str.parse::<f64>() {
                return Some(price);
            }
        }
    }
    None
}

/// Returns true when Hypixel chat indicates the purchase flow is terminally invalid
/// and should be aborted immediately instead of waiting for the GUI watchdog timeout.
pub(super) fn is_terminal_purchase_failure_message(message: &str) -> bool {
    message.contains("You didn't participate in this auction!")
        || message.contains("This auction wasn't found!")
        || message.contains("The auction wasn't found!")
        || message.contains("You cannot view this auction!")
        || message.contains("You cannot afford this auction!")
        || message.contains("don't have the space required to claim")
        || message.contains("Your inventory is full!")
}
