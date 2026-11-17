//! SkyBlock availability from Hypixel's public status page
//! (status.hypixel.net, an Atlassian Statuspage).
//!
//! Used while the bot sits offline for a SkyBlock maintenance: it polls the page
//! over plain HTTPS, with no Minecraft traffic, and decides when to reconnect.
//! Logging in every few minutes to ask the server itself would put an obviously
//! automated pattern on the account, and so would looping `/play sb` from a lobby.

use std::time::Duration;

/// Statuspage summary endpoint: every component plus scheduled maintenances.
pub const SUMMARY_URL: &str = "https://status.hypixel.net/api/v2/summary.json";

/// Reconnect anyway after this long offline when the page lists no SkyBlock
/// outage (Hypixel said "maintenance" in chat but the page never caught up) or
/// cannot be read at all.
pub const PROBE_UNLISTED_SECS: u64 = 30 * 60;
/// A listed maintenance whose scheduled end passed this long ago is probably
/// over even if the page has not been updated yet.
pub const PROBE_OVERRUN_SECS: u64 = 15 * 60;
/// A listed outage with no scheduled end is probed this often.
pub const PROBE_OPEN_ENDED_SECS: u64 = 2 * 60 * 60;

/// What the status page says about SkyBlock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkyblockStatus {
    /// SkyBlock is listed as under maintenance or in an outage. `until_unix` is
    /// the scheduled end of an in-progress maintenance when the page gives one.
    Down { until_unix: Option<u64> },
    /// The SkyBlock component is listed and nothing marks it down.
    Up,
    /// The page could not be fetched or parsed, or it has no SkyBlock component.
    Unknown,
}

/// Why a maintenance wait ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reconnect {
    /// The page listed SkyBlock as down during this wait and now lists it up.
    BackUp,
    /// The page never listed the outage and the unlisted-probe delay passed.
    Unlisted,
    /// The page stayed unreadable for the unlisted-probe delay.
    Unreadable,
    /// The listed maintenance is past its scheduled end by the overrun margin.
    Overran,
    /// An outage with no scheduled end has run for the open-ended probe delay.
    OpenEnded,
}

impl Reconnect {
    pub fn describe(self) -> &'static str {
        match self {
            Reconnect::BackUp => "status page reports SkyBlock back up",
            Reconnect::Unlisted => "status page never listed the outage, probing after 30m",
            Reconnect::Unreadable => "status page unreadable for 30m, probing",
            Reconnect::Overran => "maintenance is past its scheduled end, probing",
            Reconnect::OpenEnded => "outage has no scheduled end, probing after 2h",
        }
    }
}

fn names_skyblock(v: &serde_json::Value) -> bool {
    v.get("name")
        .and_then(|n| n.as_str())
        .map(|n| n.trim().eq_ignore_ascii_case("skyblock"))
        .unwrap_or(false)
}

/// Parse a Statuspage `summary.json` body.
pub fn parse_summary(body: &str) -> SkyblockStatus {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return SkyblockStatus::Unknown;
    };

    let component_status = v
        .get("components")
        .and_then(|c| c.as_array())
        .and_then(|cs| cs.iter().find(|c| names_skyblock(c)))
        .and_then(|c| c.get("status"))
        .and_then(|s| s.as_str());

    // A scheduled maintenance that is running now and names SkyBlock.
    // "verifying" means the work is done but the game may still be closed.
    let maintenance = v
        .get("scheduled_maintenances")
        .and_then(|m| m.as_array())
        .into_iter()
        .flatten()
        .find(|m| {
            matches!(
                m.get("status").and_then(|s| s.as_str()),
                Some("in_progress") | Some("verifying")
            ) && m
                .get("components")
                .and_then(|c| c.as_array())
                .map(|cs| cs.iter().any(names_skyblock))
                .unwrap_or(false)
        });

    let component_down = matches!(
        component_status,
        Some("under_maintenance") | Some("major_outage") | Some("partial_outage")
    );

    if component_down || maintenance.is_some() {
        let until_unix = maintenance
            .and_then(|m| m.get("scheduled_until"))
            .and_then(|s| s.as_str())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.timestamp().max(0) as u64);
        SkyblockStatus::Down { until_unix }
    } else if component_status.is_some() {
        SkyblockStatus::Up
    } else {
        SkyblockStatus::Unknown
    }
}

/// Fetch and parse the status page. Network or HTTP errors are `Unknown`.
pub async fn fetch_skyblock_status() -> SkyblockStatus {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
    {
        Ok(c) => c,
        Err(_) => return SkyblockStatus::Unknown,
    };
    match client.get(SUMMARY_URL).send().await {
        Ok(resp) if resp.status().is_success() => match resp.text().await {
            Ok(body) => parse_summary(&body),
            Err(_) => SkyblockStatus::Unknown,
        },
        _ => SkyblockStatus::Unknown,
    }
}

/// Decide after one poll whether the maintenance wait is over.
///
/// `saw_down`: the page listed SkyBlock down at some point during this wait.
/// `offline_secs`: how long this wait has run. Every probe path needs at least
/// `PROBE_UNLISTED_SECS` offline, so a probe that lands in maintenance again
/// (and restarts the wait) can never turn into a fast login loop.
pub fn reconnect_decision(
    status: &SkyblockStatus,
    saw_down: bool,
    offline_secs: u64,
    now_unix: u64,
) -> Option<Reconnect> {
    match status {
        SkyblockStatus::Up if saw_down => Some(Reconnect::BackUp),
        SkyblockStatus::Up if offline_secs >= PROBE_UNLISTED_SECS => Some(Reconnect::Unlisted),
        SkyblockStatus::Up => None,
        SkyblockStatus::Unknown if offline_secs >= PROBE_UNLISTED_SECS => {
            Some(Reconnect::Unreadable)
        }
        SkyblockStatus::Unknown => None,
        SkyblockStatus::Down {
            until_unix: Some(until),
        } => (offline_secs >= PROBE_UNLISTED_SECS
            && now_unix >= until.saturating_add(PROBE_OVERRUN_SECS))
        .then_some(Reconnect::Overran),
        SkyblockStatus::Down { until_unix: None } => {
            (offline_secs >= PROBE_OPEN_ENDED_SECS).then_some(Reconnect::OpenEnded)
        }
    }
}

/// A Hypixel system line saying SkyBlock is in maintenance, as opposed to a
/// player talking about it. Player, party and guild chat all put a `name:`
/// speaker before the text, so a colon ahead of the keyword means someone is
/// talking. The real notice is "SkyBlock is currently undergoing maintenance,
/// find out more at https://status.hypixel.net".
pub fn is_maintenance_notice(clean: &str) -> bool {
    let lower = clean.trim().to_lowercase();
    let Some(at) = [
        "undergoing maintenance",
        "under maintenance",
        "down for maintenance",
    ]
    .iter()
    .filter_map(|p| lower.find(p))
    .min() else {
        return false;
    };
    !lower[..at].contains(':')
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLOSED: &str = r#"{
        "components": [
            {"name": "Hypixel Network", "status": "operational"},
            {"name": "SkyBlock", "status": "under_maintenance"}
        ],
        "scheduled_maintenances": [{
            "name": "SkyBlock 0.27.3",
            "status": "in_progress",
            "scheduled_until": "2026-11-17T15:00:00.000-05:00",
            "components": [{"name": "SkyBlock", "status": "under_maintenance"}]
        }]
    }"#;

    #[test]
    fn listed_maintenance_is_down_with_its_end() {
        assert_eq!(
            parse_summary(CLOSED),
            SkyblockStatus::Down {
                until_unix: Some(1_794_945_600)
            }
        );
    }

    #[test]
    fn operational_component_is_up() {
        let body = r#"{"components":[{"name":"SkyBlock","status":"operational"}],
                       "scheduled_maintenances":[]}"#;
        assert_eq!(parse_summary(body), SkyblockStatus::Up);
    }

    #[test]
    fn outage_without_maintenance_has_no_end() {
        let body = r#"{"components":[{"name":"SkyBlock","status":"major_outage"}]}"#;
        assert_eq!(
            parse_summary(body),
            SkyblockStatus::Down { until_unix: None }
        );
    }

    #[test]
    fn maintenance_for_another_game_is_ignored() {
        let body = r#"{"components":[{"name":"SkyBlock","status":"operational"}],
            "scheduled_maintenances":[{"status":"in_progress",
              "scheduled_until":"2026-11-17T15:00:00Z",
              "components":[{"name":"Bed Wars"}]}]}"#;
        assert_eq!(parse_summary(body), SkyblockStatus::Up);
    }

    #[test]
    fn upcoming_maintenance_does_not_count_yet() {
        let body = r#"{"components":[{"name":"SkyBlock","status":"operational"}],
            "scheduled_maintenances":[{"status":"scheduled",
              "scheduled_until":"2026-11-17T15:00:00Z",
              "components":[{"name":"SkyBlock"}]}]}"#;
        assert_eq!(parse_summary(body), SkyblockStatus::Up);
    }

    #[test]
    fn garbage_or_missing_component_is_unknown() {
        assert_eq!(parse_summary("<html>502</html>"), SkyblockStatus::Unknown);
        assert_eq!(
            parse_summary(r#"{"components":[{"name":"Bed Wars","status":"operational"}]}"#),
            SkyblockStatus::Unknown
        );
    }

    #[test]
    fn reconnects_when_the_page_flips_back_up() {
        let up = SkyblockStatus::Up;
        assert_eq!(
            reconnect_decision(&up, true, 60, 0),
            Some(Reconnect::BackUp)
        );
        // Up without ever seeing it down: wait the unlisted delay first.
        assert_eq!(reconnect_decision(&up, false, 60, 0), None);
        assert_eq!(
            reconnect_decision(&up, false, PROBE_UNLISTED_SECS, 0),
            Some(Reconnect::Unlisted)
        );
    }

    #[test]
    fn unreadable_page_probes_after_the_unlisted_delay() {
        let unknown = SkyblockStatus::Unknown;
        assert_eq!(reconnect_decision(&unknown, true, 600, 0), None);
        assert_eq!(
            reconnect_decision(&unknown, true, PROBE_UNLISTED_SECS, 0),
            Some(Reconnect::Unreadable)
        );
    }

    #[test]
    fn listed_maintenance_waits_for_its_end_plus_margin() {
        let end = 1_000_000;
        let down = SkyblockStatus::Down {
            until_unix: Some(end),
        };
        // Well before the end: wait, however long we have been offline.
        assert_eq!(reconnect_decision(&down, true, 5 * 3600, end - 3600), None);
        // Past the end but inside the margin: still wait.
        assert_eq!(reconnect_decision(&down, true, 5 * 3600, end + 60), None);
        // Past end + margin: probe.
        assert_eq!(
            reconnect_decision(&down, true, 5 * 3600, end + PROBE_OVERRUN_SECS),
            Some(Reconnect::Overran)
        );
        // A fresh wait after a failed probe still sits out the unlisted delay.
        assert_eq!(reconnect_decision(&down, true, 60, end + 2 * 3600), None);
    }

    #[test]
    fn open_ended_outage_probes_every_two_hours() {
        let down = SkyblockStatus::Down { until_unix: None };
        assert_eq!(reconnect_decision(&down, true, 3600, 0), None);
        assert_eq!(
            reconnect_decision(&down, true, PROBE_OPEN_ENDED_SECS, 0),
            Some(Reconnect::OpenEnded)
        );
    }

    #[test]
    fn maintenance_notice_ignores_players_talking() {
        assert!(is_maintenance_notice(
            "SkyBlock is currently undergoing maintenance, find out more at https://status.hypixel.net"
        ));
        assert!(!is_maintenance_notice("[MVP+] tcza: why maintenance"));
        assert!(!is_maintenance_notice(
            "[VIP] Frrogger: lost 16k players because of the maintenance"
        ));
        assert!(!is_maintenance_notice(
            "[MVP++] demonmellow: Scheduled - SkyBlock will be undergoing maintenance to prepare"
        ));
        assert!(!is_maintenance_notice(
            "Guild > [MVP+] someone: skyblock is under maintenance lol"
        ));
        assert!(!is_maintenance_notice("Please don't spam the command!"));
    }
}
