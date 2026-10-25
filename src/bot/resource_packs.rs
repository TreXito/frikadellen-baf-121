//! Resource pack handling that behaves like a real client.
//!
//! azalea's `AcceptResourcePacksPlugin` answers a resource pack request with
//! `Accepted` **and** `SuccessfullyLoaded` inside the same ECS update, without
//! ever fetching the pack (verified against upstream `main`, see
//! `azalea/src/accept_resource_packs.rs`).
//!
//! A real client cannot do that. `Accepted` is "I will download it";
//! `SuccessfullyLoaded` reports a *completed* download and apply, which for a
//! server pack is at least a few hundred milliseconds and usually seconds. So
//! every azalea bot on earth tells the server it finished loading a multi
//! megabyte pack in 0ms, in the first seconds of every connection. That is not a
//! cheat signal, it is a client that cannot exist, and it is identical on every
//! account that shares this code.
//!
//! This plugin replaces that plugin: `Accepted` goes out immediately (correct
//! for a forced pack), `SuccessfullyLoaded` goes out after a plausible delay,
//! and the request is logged so the operator can see whether the server asks
//! for a pack at all.

use std::time::{Duration, Instant};

use azalea_client::packet::config::SendConfigPacketEvent;
use azalea_client::packet::game::{ResourcePackEvent, SendGamePacketEvent};
use azalea_client::InConfigState;
use azalea_protocol::packets::{config, game::s_resource_pack};
use bevy_ecs::prelude::*;
use tracing::info;
use uuid::Uuid;

/// A real client's "download and apply" window for a small server pack.
const MIN_LOAD_MS: u64 = 1_200;
const MAX_LOAD_MS: u64 = 5_000;

/// Marker for a pack whose `SuccessfullyLoaded` is waiting on a plausible delay.
#[derive(Component)]
struct PendingPackAck {
    id: Uuid,
    due: Instant,
}

pub struct RealisticResourcePacksPlugin;

impl bevy_app::Plugin for RealisticResourcePacksPlugin {
    fn build(&self, app: &mut bevy_app::App) {
        app.add_systems(bevy_app::Update, (on_request, flush_due));
    }
}

/// Acknowledge the request now, and schedule the "finished loading" reply.
fn on_request(
    mut events: MessageReader<ResourcePackEvent>,
    mut commands: Commands,
    query: Query<Option<&InConfigState>>,
) {
    for event in events.read() {
        let Ok(in_config) = query.get(event.entity) else {
            continue;
        };
        let span = MAX_LOAD_MS.saturating_sub(MIN_LOAD_MS).max(1);
        let delay = Duration::from_millis(MIN_LOAD_MS + (mono_ms() % span));
        info!(
            "[ResourcePack] requested in {} state ({} bytes url: {}), acking Accepted, Loaded in {}ms",
            if in_config.is_some() { "config" } else { "game" },
            event.hash.len(),
            event.url,
            delay.as_millis()
        );

        if in_config.is_some() {
            commands.trigger(SendConfigPacketEvent::new(
                event.entity,
                config::ServerboundResourcePack {
                    id: event.id,
                    action: config::s_resource_pack::Action::Accepted,
                },
            ));
        } else {
            commands.trigger(SendGamePacketEvent::new(
                event.entity,
                s_resource_pack::ServerboundResourcePack {
                    id: event.id,
                    action: s_resource_pack::Action::Accepted,
                },
            ));
        }

        commands.entity(event.entity).insert(PendingPackAck {
            id: event.id,
            due: Instant::now() + delay,
        });
    }
}

/// Send the deferred `SuccessfullyLoaded`, in whichever state the client is in
/// by then.
fn flush_due(
    mut commands: Commands,
    query: Query<(Entity, &PendingPackAck, Option<&InConfigState>)>,
) {
    for (entity, pending, in_config) in query.iter() {
        if Instant::now() < pending.due {
            continue;
        }
        info!(
            "[ResourcePack] sending SuccessfullyLoaded for {}",
            pending.id
        );
        if in_config.is_some() {
            commands.trigger(SendConfigPacketEvent::new(
                entity,
                config::ServerboundResourcePack {
                    id: pending.id,
                    action: config::s_resource_pack::Action::SuccessfullyLoaded,
                },
            ));
        } else {
            commands.trigger(SendGamePacketEvent::new(
                entity,
                s_resource_pack::ServerboundResourcePack {
                    id: pending.id,
                    action: s_resource_pack::Action::SuccessfullyLoaded,
                },
            ));
        }
        commands.entity(entity).remove::<PendingPackAck>();
    }
}

fn mono_ms() -> u64 {
    static START: once_cell::sync::Lazy<std::time::Instant> =
        once_cell::sync::Lazy::new(std::time::Instant::now);
    START.elapsed().as_millis() as u64
}
