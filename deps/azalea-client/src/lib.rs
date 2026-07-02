#![doc = include_str!("../README.md")]
#![feature(error_generic_member_access)]

pub mod account;
mod client;
pub mod local_player;
pub mod ping;
pub mod player;
mod plugins;

#[cfg(feature = "log")]
#[doc(hidden)]
pub mod test_utils;

#[deprecated = "moved to `account::Account`."]
pub type Account = account::Account;

pub use azalea_physics::local_player::{PhysicsState, SprintDirection, WalkDirection};
pub use azalea_protocol::common::client_information::ClientInformation;
// Re-export bevy-tasks so plugins can make sure that they're using the same
// version.
pub use bevy_tasks;
pub use client::{
    InConfigState, InGameState, JoinedClientBundle, LocalPlayerBundle, start_ecs_runner,
};
pub use movement::{StartSprintEvent, StartWalkEvent};
pub use plugins::*;

/// Global switch for TCP_NODELAY (disabling Nagle's algorithm) on new game
/// connections. Upstream `Connection::new` sets nodelay on the direct path but
/// `Connection::new_with_proxy` does NOT set it on the SOCKS-proxied socket, so
/// proxied bots suffer up to ~40ms of send buffering on small packets (clicks,
/// commands). The join plugin reads this when a connection is established and
/// forces the socket accordingly. Default: on. The host app may toggle it.
pub static TCP_NODELAY: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(true);
