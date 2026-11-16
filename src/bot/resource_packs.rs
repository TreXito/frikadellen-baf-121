//! Resource pack handling that behaves like a real client.
//!
//! azalea's `AcceptResourcePacksPlugin` answers a resource pack request with
//! `Accepted` **and** `SuccessfullyLoaded` inside the same ECS update, without
//! ever fetching the pack (verified against upstream `main`, see
//! `azalea/src/accept_resource_packs.rs`).
//!
//! A real client cannot do that. Vanilla 26.3 (`ServerPackManager`) reports
//! three statuses for every pushed pack, including a re-push of a pack it
//! already has after a server transfer:
//!
//! 1. `ACCEPTED` as soon as the push is accepted (`acceptPack`).
//! 2. `DOWNLOADED` when the download finishes (`onDownload`). A cached pack
//!    counts: `HttpUtil.downloadFile` hash-checks the cached file and returns
//!    it as downloaded, so this still goes out, a short moment after `ACCEPTED`.
//! 3. `SUCCESSFULLY_LOADED` after the resource reload applies it, which takes
//!    seconds.
//!
//! This plugin replaces azalea's: `Accepted` goes out immediately (correct for
//! a forced pack), `Downloaded` after a cache-check delay, `SuccessfullyLoaded`
//! after a plausible reload delay, and the request is logged so the operator
//! can see whether the server asks for a pack at all.
//!
//! azalea's `Action` enum predates `DOWNLOADED`: it lists `SuccessfullyLoaded`
//! 0, `Declined` 1, `FailedDownload` 2, `Accepted` 3, then `InvalidUrl` 4.
//! Vanilla's wire value 4 is `DOWNLOADED` (`INVALID_URL` is 5), so
//! [`DOWNLOADED_CONFIG`] and [`DOWNLOADED_GAME`] name azalea's variant 4 for
//! what it is on the wire.

use std::time::{Duration, Instant};

use azalea_client::packet::config::SendConfigPacketEvent;
use azalea_client::packet::game::{ResourcePackEvent, SendGamePacketEvent};
use azalea_client::InConfigState;
use azalea_protocol::packets::{config, game::s_resource_pack};
use bevy_ecs::prelude::*;
use tracing::info;
use uuid::Uuid;

/// Vanilla `ServerboundResourcePackPacket.Action.DOWNLOADED` (wire value 4).
/// azalea's enum has no such variant and calls 4 `InvalidUrl`.
const DOWNLOADED_CONFIG: config::s_resource_pack::Action =
    config::s_resource_pack::Action::InvalidUrl;
const DOWNLOADED_GAME: s_resource_pack::Action = s_resource_pack::Action::InvalidUrl;

/// Hash check of the cached pack file, `Accepted` to `Downloaded`.
const MIN_DOWNLOAD_MS: u64 = 40;
const MAX_DOWNLOAD_MS: u64 = 400;
/// Resource reload that applies the pack, `Downloaded` to `SuccessfullyLoaded`.
const MIN_LOAD_MS: u64 = 1_200;
const MAX_LOAD_MS: u64 = 5_000;

/// A pushed pack whose `Downloaded` and `SuccessfullyLoaded` replies are still
/// waiting on their delays.
#[derive(Component)]
struct PendingPackAck {
    id: Uuid,
    /// `Some` until `Downloaded` has been sent.
    downloaded_due: Option<Instant>,
    loaded_due: Instant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PackStatus {
    Accepted,
    Downloaded,
    SuccessfullyLoaded,
}

pub struct RealisticResourcePacksPlugin;

impl bevy_app::Plugin for RealisticResourcePacksPlugin {
    fn build(&self, app: &mut bevy_app::App) {
        app.add_systems(bevy_app::Update, (on_request, flush_due));
    }
}

/// Acknowledge the request now, and schedule the "downloaded" and "finished
/// loading" replies.
fn on_request(
    mut events: MessageReader<ResourcePackEvent>,
    mut commands: Commands,
    query: Query<Option<&InConfigState>>,
) {
    for event in events.read() {
        let Ok(in_config) = query.get(event.entity) else {
            continue;
        };
        let download = jitter(MIN_DOWNLOAD_MS, MAX_DOWNLOAD_MS);
        let load = jitter(MIN_LOAD_MS, MAX_LOAD_MS);
        info!(
            "[ResourcePack] requested in {} state ({} bytes url: {}), acking Accepted, Downloaded in {}ms, Loaded {}ms later",
            if in_config.is_some() { "config" } else { "game" },
            event.hash.len(),
            event.url,
            download.as_millis(),
            load.as_millis()
        );

        send_status(
            &mut commands,
            event.entity,
            in_config.is_some(),
            event.id,
            PackStatus::Accepted,
        );

        let now = Instant::now();
        commands.entity(event.entity).insert(PendingPackAck {
            id: event.id,
            downloaded_due: Some(now + download),
            loaded_due: now + download + load,
        });
    }
}

/// Send the deferred `Downloaded`, then `SuccessfullyLoaded`, in whichever
/// state the client is in by then.
fn flush_due(
    mut commands: Commands,
    mut query: Query<(Entity, &mut PendingPackAck, Option<&InConfigState>)>,
) {
    let now = Instant::now();
    for (entity, mut pending, in_config) in query.iter_mut() {
        if let Some(due) = pending.downloaded_due {
            if now < due {
                continue;
            }
            info!("[ResourcePack] sending Downloaded for {}", pending.id);
            send_status(
                &mut commands,
                entity,
                in_config.is_some(),
                pending.id,
                PackStatus::Downloaded,
            );
            pending.downloaded_due = None;
            // SuccessfullyLoaded is seconds later; never in the same update.
            continue;
        }
        if now < pending.loaded_due {
            continue;
        }
        info!(
            "[ResourcePack] sending SuccessfullyLoaded for {}",
            pending.id
        );
        send_status(
            &mut commands,
            entity,
            in_config.is_some(),
            pending.id,
            PackStatus::SuccessfullyLoaded,
        );
        commands.entity(entity).remove::<PendingPackAck>();
    }
}

fn send_status(
    commands: &mut Commands,
    entity: Entity,
    in_config: bool,
    id: Uuid,
    status: PackStatus,
) {
    if in_config {
        commands.trigger(SendConfigPacketEvent::new(
            entity,
            config::ServerboundResourcePack {
                id,
                action: config_action(status),
            },
        ));
    } else {
        commands.trigger(SendGamePacketEvent::new(
            entity,
            s_resource_pack::ServerboundResourcePack {
                id,
                action: game_action(status),
            },
        ));
    }
}

fn config_action(status: PackStatus) -> config::s_resource_pack::Action {
    match status {
        PackStatus::Accepted => config::s_resource_pack::Action::Accepted,
        PackStatus::Downloaded => DOWNLOADED_CONFIG,
        PackStatus::SuccessfullyLoaded => config::s_resource_pack::Action::SuccessfullyLoaded,
    }
}

fn game_action(status: PackStatus) -> s_resource_pack::Action {
    match status {
        PackStatus::Accepted => s_resource_pack::Action::Accepted,
        PackStatus::Downloaded => DOWNLOADED_GAME,
        PackStatus::SuccessfullyLoaded => s_resource_pack::Action::SuccessfullyLoaded,
    }
}

fn jitter(min_ms: u64, max_ms: u64) -> Duration {
    let span = max_ms.saturating_sub(min_ms).max(1);
    Duration::from_millis(min_ms + (mono_ms() % span))
}

fn mono_ms() -> u64 {
    static START: once_cell::sync::Lazy<std::time::Instant> =
        once_cell::sync::Lazy::new(std::time::Instant::now);
    START.elapsed().as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use azalea::buf::AzBuf;

    fn wire(packet: impl AzBuf) -> Vec<u8> {
        let mut buf = Vec::new();
        packet.azalea_write(&mut buf).unwrap();
        buf
    }

    /// Vanilla 26.3 `ServerboundResourcePackPacket.Action` ordinals:
    /// SUCCESSFULLY_LOADED 0, ACCEPTED 3, DOWNLOADED 4.
    #[test]
    fn statuses_use_vanilla_wire_values() {
        let id = Uuid::nil();
        for (status, value) in [
            (PackStatus::Accepted, 3u8),
            (PackStatus::Downloaded, 4),
            (PackStatus::SuccessfullyLoaded, 0),
        ] {
            let config_bytes = wire(config::ServerboundResourcePack {
                id,
                action: config_action(status),
            });
            let game_bytes = wire(s_resource_pack::ServerboundResourcePack {
                id,
                action: game_action(status),
            });
            assert_eq!(config_bytes.last(), Some(&value), "config {status:?}");
            assert_eq!(game_bytes.last(), Some(&value), "game {status:?}");
            assert_eq!(config_bytes.len(), 17);
        }
    }
}
