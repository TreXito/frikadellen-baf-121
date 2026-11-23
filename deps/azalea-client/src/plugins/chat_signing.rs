use std::time::{Duration, Instant};

use azalea_auth::certs::{Certificates, FetchCertificatesError};
use azalea_protocol::packets::game::{
    ServerboundChatSessionUpdate,
    s_chat_session_update::{ProfilePublicKeyData, RemoteChatSessionData},
};
use bevy_app::prelude::*;
use bevy_ecs::prelude::*;
use bevy_tasks::{IoTaskPool, Task, futures_lite::future};
use chrono::Utc;
use tracing::{debug, error};
use uuid::Uuid;

use super::{client_chat, login::IsAuthenticated, packet::game::SendGamePacketEvent};
use crate::{InGameState, account::Account};

pub struct ChatSigningPlugin;
impl Plugin for ChatSigningPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            Update,
            (
                request_certs_if_needed,
                poll_request_certs_task,
                handle_queued_certs_to_send,
            )
                .chain()
                .before(client_chat::handler::handle_send_chat_kind_event),
        );
    }
}

#[derive(Component)]
pub struct RequestCertsTask(pub Task<Result<Certificates, FetchCertificatesError>>);

/// A component that makes us have to wait until the given time to refresh the
/// certs.
///
/// This is used to avoid spamming requests if requesting certs fails. Usually,
/// we just check [`Certificates::expires_at`].
#[derive(Component, Debug)]
pub struct OnlyRefreshCertsAfter {
    pub refresh_at: Instant,
}
/// A component that's present when this client has sent its certificates to the
/// server.
///
/// This should be removed if you want to re-send the certs.
///
/// If you want to get the client's actual certificates, you can get that from
/// the `certs` in the [`Account`] component.
#[derive(Component)]
pub struct ChatSigningSession {
    pub session_id: Uuid,
    pub messages_sent: u32,
}

pub fn poll_request_certs_task(
    mut commands: Commands,
    mut query: Query<(
        Entity,
        &mut RequestCertsTask,
        &Account,
        Has<ChatSigningSession>,
        Has<InGameState>,
    )>,
) {
    for (entity, mut auth_task, account, has_session, in_game) in query.iter_mut() {
        if let Some(poll_res) = future::block_on(future::poll_once(&mut auth_task.0)) {
            debug!("Finished requesting certs");
            commands.entity(entity).remove::<RequestCertsTask>();

            match poll_res {
                Ok(certs) => {
                    account.set_certs(certs);
                    // A login that is waiting (NeedsChatSession) picks the key
                    // up from the account. A refreshed key replaces a live
                    // session, like vanilla's setKeyPair, but only in game
                    // state: during configuration the next login sends it.
                    if has_session && in_game {
                        commands.entity(entity).insert(NeedsChatSession);
                    }
                }
                Err(err) => {
                    error!("Error requesting certs: {err:?}. Retrying in an hour.");

                    commands.entity(entity).insert(OnlyRefreshCertsAfter {
                        refresh_at: Instant::now() + Duration::from_secs(60 * 60),
                    });
                }
            }
        }
    }
}

#[allow(clippy::type_complexity)]
pub fn request_certs_if_needed(
    mut commands: Commands,
    mut query: Query<
        (
            Entity,
            &Account,
            Option<&OnlyRefreshCertsAfter>,
            Has<ChatSigningSession>,
            Has<NeedsChatSession>,
        ),
        (
            Without<RequestCertsTask>,
            Without<QueuedCertsToSend>,
            With<InGameState>,
            With<IsAuthenticated>,
        ),
    >,
) {
    for (entity, account, only_refresh_certs_after, has_session, needs_session) in query.iter_mut()
    {
        if let Some(only_refresh_certs_after) = only_refresh_certs_after
            && only_refresh_certs_after.refresh_at > Instant::now()
        {
            continue;
        }

        // vanilla's ProfileKeyPairManager hands out its cached key pair until
        // the key's refreshedAfter time
        let fresh_certs = account
            .certs()
            .filter(|certs| Utc::now() < certs.refresh_after);
        match (needs_session, has_session, fresh_certs) {
            // a new session (one per game login) reuses a key that is not due
            // a refresh, without asking Mojang again
            (true, _, Some(certs)) => {
                commands.entity(entity).insert(QueuedCertsToSend { certs });
                continue;
            }
            // nothing owed, or the live session's key is still fresh
            (false, false, _) | (false, true, Some(_)) => continue,
            // no key yet, or the live session's key is due a refresh
            (true, _, None) | (false, true, None) => {}
        }

        if let Some(access_token) = account.access_token() {
            let task_pool = IoTaskPool::get();

            debug!("Started task to fetch certs");
            let task = task_pool.spawn(async_compat::Compat::new(async move {
                azalea_auth::certs::fetch_certificates(&access_token).await
            }));
            commands
                .entity(entity)
                .insert(RequestCertsTask(task))
                .remove::<OnlyRefreshCertsAfter>();
        }
    }
}

/// Present while this client owes the server a fresh
/// [`ServerboundChatSessionUpdate`].
///
/// Vanilla's `ClientPacketListener.handleLogin` drops its chat session on every
/// game login and, when the login reports online mode, sends a new one (new
/// random session id, cached profile key) on its next tick. Hypixel sends a
/// game login after every server transfer, so a client that keeps one session
/// per connection skips a packet every vanilla client sends there.
#[derive(Component)]
pub struct NeedsChatSession;

/// Called for every game login packet, like vanilla's `handleLogin`: the old
/// session is gone, and an online-mode server gets a new one.
pub fn restart_session_on_login(ecs: &mut World, player: Entity, online_mode: bool) {
    let Ok(mut entity) = ecs.get_entity_mut(player) else {
        return;
    };
    entity.remove::<(ChatSigningSession, QueuedCertsToSend)>();
    if online_mode {
        entity.insert(NeedsChatSession);
    } else {
        entity.remove::<NeedsChatSession>();
    }
}

/// Called on `start_configuration`. Vanilla's game listener stops ticking
/// there, so a session it had not sent yet is never sent for that login.
/// Without this, a session still owed would go out after the next
/// `finish_configuration`, before that connection's own login packet.
pub fn forget_owed_session(ecs: &mut World, player: Entity) {
    if let Ok(mut entity) = ecs.get_entity_mut(player) {
        entity.remove::<(NeedsChatSession, QueuedCertsToSend)>();
    }
}

/// A component that's present on players that should send their chat signing
/// certificates as soon as possible.
///
/// This is removed when the certificates get sent.
#[derive(Component)]
pub struct QueuedCertsToSend {
    pub certs: Certificates,
}

pub fn handle_queued_certs_to_send(
    mut commands: Commands,
    query: Query<(Entity, &QueuedCertsToSend), (With<IsAuthenticated>, With<InGameState>)>,
) {
    for (entity, queued_certs) in &query {
        let certs = &queued_certs.certs;

        let session_id = Uuid::new_v4();

        let chat_session = RemoteChatSessionData {
            session_id,
            profile_public_key: ProfilePublicKeyData {
                expires_at: certs.expires_at.timestamp_millis() as u64,
                key: certs.public_key_der.clone(),
                key_signature: certs.signature_v2.clone(),
            },
        };

        debug!("Sending chat signing certs to server");

        commands.trigger(SendGamePacketEvent::new(
            entity,
            ServerboundChatSessionUpdate { chat_session },
        ));
        commands
            .entity(entity)
            .remove::<(QueuedCertsToSend, NeedsChatSession)>()
            .insert(ChatSigningSession {
                session_id,
                messages_sent: 0,
            });
    }
}
