//! Coin formatting, AH fee maths, ban detection and the UFW port check.
//!
//! Moved out of main.rs unchanged; see `app/mod.rs`.

#[allow(unused_imports)]
use crate::*;

/// Calculate Hypixel AH fee based on price tier (matches TypeScript calculateAuctionHouseFee).
/// - <10M  → 1%
/// - <100M → 2%
/// - ≥100M → 2.5%
pub(crate) fn calculate_ah_fee(price: u64) -> u64 {
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
pub(crate) fn format_coins(amount: i64) -> String {
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
    if negative {
        format!("-{}", formatted)
    } else {
        formatted
    }
}

/// Format an f64 coin amount with comma separators, preserving one decimal
/// digit when the fractional part is non-zero (e.g. 600000.5 → "600,000.5").
pub(crate) fn format_coins_f64(amount: f64) -> String {
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

pub(crate) fn is_ban_disconnect(reason: &str) -> bool {
    let lower = reason.to_ascii_lowercase();
    lower.contains("temporarily banned")
        || lower.contains("permanently banned")
        || lower.contains("ban id:")
        || lower.contains("account has been blocked")
        || lower.contains("security-block")
        || lower.contains("block id:")
}

/// Ban / security notice delivered as a CHAT line rather than a disconnect.
/// Deliberately STRICTER than `is_ban_disconnect`: only tokens that appear in
/// Hypixel's own ban/security notices (a ban/block id, a security-block, or the
/// "account has been blocked" line), never a bare "banned", so a normal player's
/// chat message cannot trigger a false process kill.
pub(crate) fn is_ban_chat(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    lower.contains("ban id:")
        || lower.contains("block id:")
        || lower.contains("security-block")
        || lower.contains("account has been blocked")
}

/// Check whether the web GUI port is allowed through UFW.
/// Prints a prominent warning when UFW is active but the port is not listed.
/// No-ops silently if UFW is not installed or the check fails.
#[cfg(target_os = "linux")]
pub(crate) fn check_ufw_port(port: u16) {
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
    let allowed = text
        .split_whitespace()
        .any(|token| token == port_str || token == port_tcp || token == port_udp);
    if !allowed {
        warn!("========================================");
        warn!("! WARNING: YOU HAVE SET A PORT FOR THE WEB APP BUT THE PORT ISN'T ALLOWED ON YOUR FIREWALL");
        warn!("! PLEASE EXECUTE THE FOLLOWING COMMAND TO ACCESS IT VIA THE INTERNET:");
        warn!("!   ufw allow {}", port);
        warn!("========================================");
    }
}
