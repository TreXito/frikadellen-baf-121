//! `BAF_PACKET_DUMP=<file>`: record the packets a vanilla-client oracle needs to
//! re-check every click this bot sends.
//!
//! Off unless the variable is set. It only observes: no packet is changed,
//! delayed or added. Raw frames come from azalea's
//! [`PACKET_TAP`](azalea_client::connection::PACKET_TAP), so inbound items are
//! the exact bytes Hypixel sent, before azalea decodes them.
//!
//! Recorded:
//! - configuration: registry data, tags, known packs, enabled features, finish
//! - game, inbound: login, respawn, reconfiguration start, tags, every
//!   container open/close/content/slot packet, cursor and player inventory sets
//! - outbound: container clicks, button clicks, closes, sign updates, carried
//!   slot changes, the configuration acks, and the login-time client
//!   information, brand / custom payloads and resource pack replies
//!
//! Chat is never recorded. `tools/vanilla-oracle` replays the file through
//! Mojang's own 26.3 client code and reports every click whose bytes differ from
//! what a vanilla client would have sent for the same menu.
//!
//! File format, little-endian. The file starts with [`MAGIC`] when created, and
//! every process start appends a `session_start` record, so one file can hold
//! several runs. Each record is:
//! `u8 kind | u64 unix micros | u8 name len | name | u32 len | raw packet`,
//! where `raw packet` is the VarInt packet id followed by the packet fields.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use azalea_client::connection::PACKET_TAP;
use azalea_protocol::packets::ConnectionProtocol;
use parking_lot::Mutex;
use tracing::{info, warn};

pub(crate) const MAGIC: &[u8; 8] = b"BAFPKT1\n";

const INBOUND_CONFIG: &[&str] = &[
    "registry_data",
    "update_tags",
    "select_known_packs",
    "update_enabled_features",
    "finish_configuration",
];

const INBOUND_GAME: &[&str] = &[
    "login",
    "respawn",
    "start_configuration",
    "update_tags",
    "open_screen",
    "container_set_content",
    "container_set_slot",
    "container_close",
    "set_cursor_item",
    "set_player_inventory",
];

const OUTBOUND: &[&str] = &[
    "select_known_packs",
    "finish_configuration",
    "configuration_acknowledged",
    "container_click",
    "container_button_click",
    "container_close",
    "sign_update",
    "set_carried_item",
    "client_information",
    "custom_payload",
    "resource_pack",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Kind {
    InConfig = 0,
    InGame = 1,
    Out = 2,
    SessionStart = 3,
}

struct Dump {
    out: BufWriter<File>,
    written: u64,
    cap: u64,
}

static DUMP: Mutex<Option<Dump>> = Mutex::new(None);
static STOPPED: AtomicBool = AtomicBool::new(false);

/// Start recording when `BAF_PACKET_DUMP` names a file. Call once, before the
/// bot connects. `BAF_PACKET_DUMP_MAX_MB` caps the file (default 512 MB).
pub fn init_from_env() {
    let Ok(path) = std::env::var("BAF_PACKET_DUMP") else {
        return;
    };
    let path = path.trim();
    if path.is_empty() {
        return;
    }
    let cap_mb = std::env::var("BAF_PACKET_DUMP_MAX_MB")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(512);

    let mut dump = match Dump::open(path, cap_mb * 1024 * 1024) {
        Ok(dump) => dump,
        Err(error) => {
            warn!("[PacketDump] Cannot open {path}: {error}. Recording is off.");
            return;
        }
    };
    if !dump.record(Kind::SessionStart, "session_start", &[]) {
        warn!("[PacketDump] {path} is already at its {cap_mb} MB cap. Recording is off.");
        return;
    }
    *DUMP.lock() = Some(dump);
    if PACKET_TAP.set(tap).is_err() {
        warn!("[PacketDump] Packet tap was already set. Recording is off.");
        *DUMP.lock() = None;
        return;
    }
    info!(
        "[PacketDump] Recording container, click and registry packets to {path} (cap {cap_mb} MB). Packets are observed only, never changed."
    );
}

fn tap(inbound: bool, state: Option<ConnectionProtocol>, name: &'static str, raw: &[u8]) {
    if STOPPED.load(Ordering::Relaxed) {
        return;
    }
    let Some(kind) = classify(inbound, state, name) else {
        return;
    };
    let mut guard = DUMP.lock();
    let Some(dump) = guard.as_mut() else {
        return;
    };
    if !dump.record(kind, name, raw) {
        STOPPED.store(true, Ordering::Relaxed);
        *guard = None;
        warn!("[PacketDump] Reached the size cap or a write failed. Recording stopped.");
    }
}

fn classify(inbound: bool, state: Option<ConnectionProtocol>, name: &str) -> Option<Kind> {
    match (inbound, state) {
        (true, Some(ConnectionProtocol::Configuration)) if INBOUND_CONFIG.contains(&name) => {
            Some(Kind::InConfig)
        }
        (true, Some(ConnectionProtocol::Game)) if INBOUND_GAME.contains(&name) => {
            Some(Kind::InGame)
        }
        (false, _) if OUTBOUND.contains(&name) => Some(Kind::Out),
        _ => None,
    }
}

impl Dump {
    fn open(path: &str, cap: u64) -> std::io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let existing = file.metadata()?.len();
        let mut out = BufWriter::new(file);
        let mut written = existing;
        if existing == 0 {
            out.write_all(MAGIC)?;
            out.flush()?;
            written = MAGIC.len() as u64;
        }
        Ok(Self { out, written, cap })
    }

    /// Append one record and flush it, so a crash or a kill keeps everything up
    /// to the last packet. `false` when the cap is reached or the write failed.
    fn record(&mut self, kind: Kind, name: &str, raw: &[u8]) -> bool {
        // Packet names are short and frames are capped far below 4 GiB, so this
        // only guards the casts in `write_record`.
        if name.len() > usize::from(u8::MAX) || u32::try_from(raw.len()).is_err() {
            return true;
        }
        let size = (1 + 8 + 1 + name.len() + 4 + raw.len()) as u64;
        if self.written + size > self.cap {
            let _ = self.out.flush();
            return false;
        }
        let micros = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_micros() as u64)
            .unwrap_or(0);
        if let Err(error) = self.write_record(kind, micros, name, raw) {
            warn!("[PacketDump] Write failed: {error}");
            return false;
        }
        self.written += size;
        true
    }

    fn write_record(
        &mut self,
        kind: Kind,
        micros: u64,
        name: &str,
        raw: &[u8],
    ) -> std::io::Result<()> {
        self.out.write_all(&[kind as u8])?;
        self.out.write_all(&micros.to_le_bytes())?;
        self.out.write_all(&[name.len() as u8])?;
        self.out.write_all(name.as_bytes())?;
        self.out.write_all(&(raw.len() as u32).to_le_bytes())?;
        self.out.write_all(raw)?;
        self.out.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse records the way the oracle does.
    fn read_records(bytes: &[u8]) -> Vec<(u8, String, Vec<u8>)> {
        assert_eq!(&bytes[..8], MAGIC);
        let mut at = 8;
        let mut records = Vec::new();
        while at < bytes.len() {
            let kind = bytes[at];
            at += 1 + 8;
            let name_len = bytes[at] as usize;
            at += 1;
            let name = String::from_utf8(bytes[at..at + name_len].to_vec()).unwrap();
            at += name_len;
            let len = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
            at += 4;
            records.push((kind, name, bytes[at..at + len].to_vec()));
            at += len;
        }
        records
    }

    fn temp_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "baf-packet-dump-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn records_round_trip_and_reopening_appends_without_a_second_header() {
        let path = temp_path("roundtrip");
        let p = path.to_str().unwrap();
        {
            let mut dump = Dump::open(p, 1 << 20).unwrap();
            assert!(dump.record(Kind::SessionStart, "session_start", &[]));
            assert!(dump.record(Kind::InGame, "container_set_slot", &[0x14, 1, 2, 3]));
        }
        {
            let mut dump = Dump::open(p, 1 << 20).unwrap();
            assert!(dump.record(Kind::Out, "container_click", &[0x11, 9]));
        }
        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(
            read_records(&bytes),
            vec![
                (3, "session_start".to_string(), vec![]),
                (1, "container_set_slot".to_string(), vec![0x14, 1, 2, 3]),
                (2, "container_click".to_string(), vec![0x11, 9]),
            ]
        );
    }

    #[test]
    fn cap_stops_recording_before_the_file_grows_past_it() {
        let path = temp_path("cap");
        let p = path.to_str().unwrap();
        let mut dump = Dump::open(p, 8 + 40).unwrap();
        assert!(dump.record(Kind::InGame, "open_screen", &[0; 10]));
        assert!(!dump.record(Kind::InGame, "open_screen", &[0; 10]));
        drop(dump);
        let len = std::fs::metadata(&path).unwrap().len();
        let _ = std::fs::remove_file(&path);
        assert!(len <= 48, "file grew to {len}");
    }

    #[test]
    fn only_oracle_packets_are_kept() {
        use ConnectionProtocol::{Configuration, Game};
        assert_eq!(
            classify(true, Some(Configuration), "registry_data"),
            Some(Kind::InConfig)
        );
        assert_eq!(
            classify(true, Some(Game), "container_set_content"),
            Some(Kind::InGame)
        );
        assert_eq!(classify(false, None, "container_click"), Some(Kind::Out));
        assert_eq!(classify(false, None, "client_information"), Some(Kind::Out));
        // Chat, movement and chunk traffic never reach the file.
        assert_eq!(classify(true, Some(Game), "player_chat"), None);
        assert_eq!(classify(true, Some(Game), "level_chunk_with_light"), None);
        assert_eq!(classify(false, None, "chat_command"), None);
        assert_eq!(classify(false, None, "move_player_pos"), None);
        // A config-only name arriving in game state is not mistaken for config.
        assert_eq!(classify(true, Some(Game), "registry_data"), None);
    }
}
