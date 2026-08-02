//! Tells the claim flow which auctions in "Manage Auctions" are actually *ours*.
//!
//! On a co-op profile the auction house is shared: everything a co-op member
//! lists shows up in the same "Manage Auctions" GUI the bot sweeps, and both
//! "Claim All" and the blind claimable-slot scan collect their sales into the
//! bot's purse. `only_claim_own_auctions` turns that off — but only if we can
//! answer "did this account list that?" for an arbitrary GUI slot, which is
//! what this module is for.
//!
//! Three sources feed the answer, cheapest first:
//!   1. **Our own listing history** — every "BIN Auction started for X!" the bot
//!      sees is recorded here and persisted, so it survives restarts.
//!   2. **The account's auctions from the Hypixel API** (`skyblock/auction?player=`),
//!      when `hypixel_api_key` is configured. Keyed on the auctioneer UUID, so a
//!      co-op member's listing is never in the response.
//!   3. **Coflnet** (`/api/player/{uuid}/auctions`, keyless) as the fallback —
//!      the 30 most recent auctions of this account.
//!
//! (2) and (3) are refreshed by a background task so the claim path itself never
//! waits on the network; a GUI window sitting open while we block on HTTP is how
//! stale-window clicks (and bans) happen.
//!
//! When nothing can attribute a slot the answer is [`Ownership::Unknown`] and
//! the caller **skips** it. Leaving coins unclaimed is recoverable; paying them
//! into the wrong purse is not, and avoiding exactly that is why the option
//! exists.

use once_cell::sync::Lazy;
use parking_lot::RwLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::{debug, info, warn};

/// File name for the persisted "we listed this" history (stored next to the
/// executable / in the logs dir, like the bazaar tracker's state).
const OWN_LISTINGS_FILE: &str = "own_auctions.json";

/// How long a recorded listing stays relevant. Hypixel's longest auction is 48h
/// and an unclaimed one sits around for a while after that, so a week is a
/// comfortable margin while keeping the file small.
const LISTING_TTL_SECS: u64 = 7 * 24 * 60 * 60;

/// Hard cap on retained listings, in case something goes very wrong upstream.
const MAX_LISTINGS: usize = 2000;

/// Pages of Coflnet's player-auction endpoint to pull (10 auctions per page).
const COFL_PAGES: u32 = 3;

/// Shortest name that may be matched by containment rather than equality. The
/// GUI shows reforges and stars the API/our records may not ("Withered
/// Valkyrie ✪✪✪✪✪➌" vs "Valkyrie"), so containment is required — but on a very
/// short name it would match far too much.
const MIN_CONTAINMENT_LEN: usize = 4;

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Auctions this account listed itself, from the local history.
static OWN_LISTINGS: Lazy<RwLock<Vec<OwnAuction>>> = Lazy::new(|| RwLock::new(Vec::new()));

/// Auctions the Hypixel/Coflnet API reports for this account.
static REMOTE_AUCTIONS: Lazy<RwLock<Vec<OwnAuction>>> = Lazy::new(|| RwLock::new(Vec::new()));

/// Whether a remote refresh has ever succeeded. Distinguishes "the API says this
/// account has no auctions" from "we never got an answer".
static REMOTE_FETCHED: AtomicBool = AtomicBool::new(false);

/// One auction known to be ours: the normalized item name plus every price it
/// could show up under in the GUI (starting bid, and the winning bid for a
/// non-BIN auction).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OwnAuction {
    /// Normalized (color-stripped, decoration-stripped, lowercased) item name.
    pub name: String,
    /// Prices this auction may display. Empty = match on the name alone.
    #[serde(default)]
    pub prices: Vec<i64>,
    /// Unix seconds when we recorded it, for TTL pruning.
    pub recorded_at: u64,
}

/// What we can say about a "Manage Auctions" slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ownership {
    /// This account listed it — safe to claim.
    Ours,
    /// We have ownership data and this is not in it — a co-op member's auction.
    Foreign,
    /// No ownership data at all (nothing listed yet this install and no
    /// successful API refresh). Undecidable.
    Unknown,
}

/// Enable/disable own-auctions-only claiming (the `only_claim_own_auctions`
/// config option). Called at startup and whenever the panel saves the config.
pub fn set_enabled(enabled: bool) {
    let was = ENABLED.swap(enabled, Ordering::Relaxed);
    if was != enabled {
        info!(
            "[AuctionOwnership] Own-auctions-only claiming {}",
            if enabled { "ENABLED — co-op members' sales will be left alone" } else { "disabled" }
        );
    }
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Strip Minecraft color codes, rarity/star decorations and punctuation so a GUI
/// display name, an API `item_name` and our own listing name compare equal.
pub fn normalize_item_name(name: &str) -> String {
    let stripped = crate::utils::remove_minecraft_colors(name);
    let mut out = String::with_capacity(stripped.len());
    let mut last_was_space = true;
    for ch in stripped.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_was_space = false;
        } else if !last_was_space {
            // Any decoration (✪ ➌ ⚚ [Lvl 100] brackets, commas, …) collapses to
            // a single separator, so spacing differences never matter.
            out.push(' ');
            last_was_space = true;
        }
    }
    out.trim().to_string()
}

/// True when two normalized names describe the same item. Exact match, or
/// containment either way so a reforge/star prefix on one side is tolerated.
fn names_match(a: &str, b: &str) -> bool {
    if a.is_empty() || b.is_empty() {
        return false;
    }
    if a == b {
        return true;
    }
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    short.len() >= MIN_CONTAINMENT_LEN && long.contains(short)
}

/// Record an auction this account just listed ("BIN Auction started for X!").
pub fn note_own_listing(item_name: &str, price: i64) {
    let name = normalize_item_name(item_name);
    if name.is_empty() {
        return;
    }
    {
        let mut listings = OWN_LISTINGS.write();
        // Refresh the timestamp of an identical existing entry instead of
        // growing the file every time the same item is relisted.
        if let Some(existing) = listings
            .iter_mut()
            .find(|l| l.name == name && l.prices.contains(&price))
        {
            existing.recorded_at = now_secs();
        } else {
            listings.push(OwnAuction {
                name: name.clone(),
                prices: if price > 0 { vec![price] } else { Vec::new() },
                recorded_at: now_secs(),
            });
        }
        prune(&mut listings);
    }
    debug!("[AuctionOwnership] Recorded own listing '{}' @ {}", name, price);
    save_own_listings();
}

fn prune(listings: &mut Vec<OwnAuction>) {
    let cutoff = now_secs().saturating_sub(LISTING_TTL_SECS);
    listings.retain(|l| l.recorded_at >= cutoff);
    if listings.len() > MAX_LISTINGS {
        let drop = listings.len() - MAX_LISTINGS;
        listings.drain(0..drop);
    }
}

/// Decide whether a "Manage Auctions" slot belongs to this account.
///
/// `price` is whatever the slot lore exposed (`None` when it could not be
/// parsed, which is common for sold entries). When we have a price on both
/// sides it must match; otherwise the name alone decides.
pub fn ownership_of(item_name: &str, price: Option<i64>) -> Ownership {
    let name = normalize_item_name(item_name);
    if name.is_empty() {
        return Ownership::Unknown;
    }
    let own = OWN_LISTINGS.read();
    let remote = REMOTE_AUCTIONS.read();
    let has_data = !own.is_empty() || REMOTE_FETCHED.load(Ordering::Relaxed);

    let matches = |a: &OwnAuction| -> bool {
        if !names_match(&a.name, &name) {
            return false;
        }
        match price {
            Some(p) if !a.prices.is_empty() => a.prices.contains(&p),
            _ => true,
        }
    };

    if own.iter().any(&matches) || remote.iter().any(&matches) {
        return Ownership::Ours;
    }
    if has_data {
        Ownership::Foreign
    } else {
        Ownership::Unknown
    }
}

// ── Persistence ─────────────────────────────────────────────

fn persistence_path() -> std::path::PathBuf {
    crate::logging::get_logs_dir().join(OWN_LISTINGS_FILE)
}

fn save_own_listings() {
    #[cfg(test)]
    return;
    #[cfg(not(test))]
    {
        let listings = OWN_LISTINGS.read().clone();
        let dir = crate::logging::get_logs_dir();
        if let Err(e) = std::fs::create_dir_all(&dir) {
            warn!("[AuctionOwnership] Failed to create persistence dir: {}", e);
            return;
        }
        match serde_json::to_string(&listings) {
            Ok(json) => {
                if let Err(e) = std::fs::write(persistence_path(), json) {
                    warn!("[AuctionOwnership] Failed to write own_auctions.json: {}", e);
                }
            }
            Err(e) => warn!("[AuctionOwnership] Failed to serialize own listings: {}", e),
        }
    }
}

/// Load the persisted listing history. Call once at startup.
pub fn load_own_listings() {
    let path = persistence_path();
    if !path.exists() {
        return;
    }
    match std::fs::read_to_string(&path) {
        Ok(json) => match serde_json::from_str::<Vec<OwnAuction>>(&json) {
            Ok(mut listings) => {
                prune(&mut listings);
                info!("[AuctionOwnership] Loaded {} own listing(s) from disk", listings.len());
                *OWN_LISTINGS.write() = listings;
            }
            Err(e) => warn!("[AuctionOwnership] Failed to parse {}: {}", path.display(), e),
        },
        Err(e) => warn!("[AuctionOwnership] Failed to read {}: {}", path.display(), e),
    }
}

// ── Remote refresh ──────────────────────────────────────────

/// Resolve a Minecraft username to a dashed UUID via Mojang.
async fn fetch_player_uuid(client: &reqwest::Client, username: &str) -> Option<String> {
    let url = format!("https://api.mojang.com/users/profiles/minecraft/{}", username);
    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let json: serde_json::Value = resp.json().await.ok()?;
    let raw_id = json.get("id")?.as_str()?;
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

/// Pull this account's auctions from Hypixel (needs `hypixel_api_key`).
/// `?player=` is keyed on the auctioneer, so co-op members are never included.
async fn fetch_hypixel_auctions(
    client: &reqwest::Client,
    uuid: &str,
    api_key: &str,
) -> Option<Vec<OwnAuction>> {
    let url = format!(
        "https://api.hypixel.net/v2/skyblock/auction?player={}",
        uuid.replace('-', "")
    );
    let resp = client.get(&url).header("API-Key", api_key).send().await.ok()?;
    if !resp.status().is_success() {
        warn!("[AuctionOwnership] Hypixel API returned {} — falling back to Coflnet", resp.status());
        return None;
    }
    let json: serde_json::Value = resp.json().await.ok()?;
    if !json.get("success").and_then(|v| v.as_bool()).unwrap_or(false) {
        warn!("[AuctionOwnership] Hypixel API success=false — falling back to Coflnet");
        return None;
    }
    let auctions = json.get("auctions")?.as_array()?;
    Some(
        auctions
            .iter()
            .filter_map(|a| {
                let name = normalize_item_name(a.get("item_name").and_then(|v| v.as_str())?);
                if name.is_empty() {
                    return None;
                }
                let mut prices = Vec::new();
                for key in ["starting_bid", "highest_bid_amount", "price"] {
                    if let Some(p) = a.get(key).and_then(|v| v.as_i64()) {
                        if p > 0 && !prices.contains(&p) {
                            prices.push(p);
                        }
                    }
                }
                Some(OwnAuction { name, prices, recorded_at: now_secs() })
            })
            .collect(),
    )
}

/// Pull this account's most recent auctions from Coflnet (keyless fallback).
async fn fetch_cofl_auctions(client: &reqwest::Client, uuid: &str) -> Option<Vec<OwnAuction>> {
    let mut out: Vec<OwnAuction> = Vec::new();
    for page in 0..COFL_PAGES {
        let url = format!("https://sky.coflnet.com/api/player/{}/auctions?page={}", uuid, page);
        let resp = match client.get(&url).send().await {
            Ok(r) if r.status().is_success() => r,
            Ok(r) => {
                warn!("[AuctionOwnership] Coflnet returned {} for page {}", r.status(), page);
                break;
            }
            Err(e) => {
                warn!("[AuctionOwnership] Coflnet request failed: {}", e);
                break;
            }
        };
        let arr = match resp.json::<Vec<serde_json::Value>>().await {
            Ok(a) => a,
            Err(e) => {
                warn!("[AuctionOwnership] Failed to parse Coflnet auctions: {}", e);
                break;
            }
        };
        let empty = arr.is_empty();
        for a in arr {
            let Some(name) = a.get("itemName").and_then(|v| v.as_str()).map(normalize_item_name)
            else {
                continue;
            };
            if name.is_empty() {
                continue;
            }
            let mut prices = Vec::new();
            for key in ["startingBid", "highestBid"] {
                if let Some(p) = a.get(key).and_then(|v| v.as_i64()) {
                    if p > 0 && !prices.contains(&p) {
                        prices.push(p);
                    }
                }
            }
            out.push(OwnAuction { name, prices, recorded_at: now_secs() });
        }
        // Last page reached — Coflnet returns a short/empty array at the end.
        if empty {
            break;
        }
    }
    if out.is_empty() {
        // An account with genuinely zero recent auctions is indistinguishable
        // from a failed fetch here, so treat it as "no answer" and let the
        // local listing history carry the decision.
        return None;
    }
    Some(out)
}

/// Refresh the API-side view of this account's auctions. Returns true when a
/// source answered.
pub async fn refresh_remote(ingame_name: &str, hypixel_api_key: Option<&str>) -> bool {
    if ingame_name.is_empty() {
        return false;
    }
    let Ok(client) = reqwest::Client::builder().timeout(Duration::from_secs(10)).build() else {
        return false;
    };
    let Some(uuid) = fetch_player_uuid(&client, ingame_name).await else {
        warn!("[AuctionOwnership] Could not resolve UUID for '{}'", ingame_name);
        return false;
    };

    let mut fetched = None;
    if let Some(key) = hypixel_api_key.filter(|k| !k.is_empty()) {
        fetched = fetch_hypixel_auctions(&client, &uuid, key).await;
    }
    if fetched.is_none() {
        fetched = fetch_cofl_auctions(&client, &uuid).await;
    }

    match fetched {
        Some(auctions) => {
            debug!(
                "[AuctionOwnership] Refreshed {} auction(s) for {}",
                auctions.len(),
                ingame_name
            );
            *REMOTE_AUCTIONS.write() = auctions;
            REMOTE_FETCHED.store(true, Ordering::Relaxed);
            true
        }
        None => false,
    }
}

/// How often the background task re-pulls the account's auctions.
pub const REFRESH_INTERVAL_SECS: u64 = 180;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_away_colors_and_decorations() {
        assert_eq!(normalize_item_name("§6Withered Valkyrie ✪✪✪✪✪➌"), "withered valkyrie");
        assert_eq!(normalize_item_name("§d[Lvl 100] Golden Dragon"), "lvl 100 golden dragon");
        assert_eq!(normalize_item_name("  Hyperion  "), "hyperion");
    }

    #[test]
    fn matches_names_through_reforge_and_stars() {
        assert!(names_match("hyperion", "heroic hyperion"));
        assert!(names_match("withered valkyrie", "withered valkyrie"));
        assert!(!names_match("hyperion", "terminator"));
        // Too short to match by containment — would swallow unrelated items.
        assert!(!names_match("bow", "terminator bow"));
    }

    // One test, because the stores behind `ownership_of` are process-global and
    // `cargo test` runs test fns in parallel — split up, they clobber each other.
    #[test]
    fn ownership_verdicts() {
        OWN_LISTINGS.write().clear();
        REMOTE_AUCTIONS.write().clear();
        REMOTE_FETCHED.store(false, Ordering::Relaxed);
        // Nothing listed yet and no API answer — undecidable, so the caller skips.
        assert_eq!(ownership_of("Hyperion", Some(100)), Ownership::Unknown);

        note_own_listing("§6Hyperion", 900_000_000);
        assert_eq!(ownership_of("Heroic Hyperion ✪✪✪✪✪", Some(900_000_000)), Ownership::Ours);
        // Same item, a co-op member's different price.
        assert_eq!(ownership_of("Hyperion", Some(750_000_000)), Ownership::Foreign);
        // Price unreadable from the lore — the name alone decides.
        assert_eq!(ownership_of("Hyperion", None), Ownership::Ours);
        assert_eq!(ownership_of("Terminator", Some(900_000_000)), Ownership::Foreign);
        OWN_LISTINGS.write().clear();
    }
}
