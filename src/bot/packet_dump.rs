//! Record the packets a vanilla-client oracle needs to re-check every click
//! and every configuration phase this bot goes through.
//!
//! Off unless `packet_dump = true` in config.toml (a new
//! `logs/packets-<UTC time>.bin` per start) or `BAF_PACKET_DUMP=<file>` is
//! set. It only observes: no packet is changed, delayed or added. Raw frames come from azalea's
//! [`PACKET_TAP`](azalea_client::connection::PACKET_TAP), so inbound items are
//! the exact bytes Hypixel sent, before azalea decodes them.
//!
//! Recorded:
//! - configuration, inbound: every packet, so a phase that never finishes
//!   shows what Hypixel sent last (resource pack pushes, pings, cookies)
//! - game, inbound: login, respawn, reconfiguration start, tags, every
//!   container open/close/content/slot packet, cursor and player inventory
//!   sets, keepalives, pings, cookies, resource pack push/pop, disconnect and
//!   transfer
//! - outbound: container clicks, button clicks, closes, sign updates, carried
//!   slot changes, the configuration acks, client information, brand / custom
//!   payloads, resource pack replies, keepalives, pongs, ping requests and
//!   cookie replies
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
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use azalea_client::connection::PACKET_TAP;
use azalea_protocol::packets::ConnectionProtocol;
use parking_lot::Mutex;
use tracing::{info, warn};

pub(crate) const MAGIC: &[u8; 8] = b"BAFPKT1\n";

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
    "keep_alive",
    "ping",
    "pong_response",
    "cookie_request",
    "store_cookie",
    "resource_pack_push",
    "resource_pack_pop",
    "disconnect",
    "transfer",
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
    "keep_alive",
    "pong",
    "ping_request",
    "cookie_response",
    "accept_code_of_conduct",
    "custom_click_action",
];

/// Name prefix and extension of the per-start files `packet_dump = true` writes.
const AUTO_PREFIX: &str = "packets-";
const AUTO_EXT: &str = ".bin";

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

fn env_mb(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(default)
}

/// Start recording. Call once, before the bot connects.
///
/// `BAF_PACKET_DUMP=<file>` records to that file. Otherwise `enabled_in_config`
/// (`packet_dump` in config.toml) records to a new `logs/packets-<UTC>.bin`,
/// after deleting the oldest such files beyond `BAF_PACKET_DUMP_KEEP_MB`
/// (default 2048 MB). `BAF_PACKET_DUMP_MAX_MB` caps one file (default 512 MB).
pub fn init(enabled_in_config: bool) {
    let cap_mb = env_mb("BAF_PACKET_DUMP_MAX_MB", 512);
    let from_env = std::env::var("BAF_PACKET_DUMP")
        .ok()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty());
    let path = match from_env {
        Some(path) => PathBuf::from(path),
        None if enabled_in_config => {
            let dir = crate::logging::get_logs_dir();
            if let Err(error) = std::fs::create_dir_all(&dir) {
                warn!(
                    "[PacketDump] Cannot create {}: {error}. Recording is off.",
                    dir.display()
                );
                return;
            }
            let keep = env_mb("BAF_PACKET_DUMP_KEEP_MB", 2048) * 1024 * 1024;
            for removed in prune_old_dumps(&dir, keep) {
                info!("[PacketDump] Deleted old dump {}", removed.display());
            }
            dir.join(auto_file_name(chrono::Utc::now()))
        }
        None => return,
    };
    let shown = path.display();

    let mut dump = match Dump::open(&path, cap_mb * 1024 * 1024) {
        Ok(dump) => dump,
        Err(error) => {
            warn!("[PacketDump] Cannot open {shown}: {error}. Recording is off.");
            return;
        }
    };
    if !dump.record(Kind::SessionStart, "session_start", &[]) {
        warn!("[PacketDump] {shown} is already at its {cap_mb} MB cap. Recording is off.");
        return;
    }
    *DUMP.lock() = Some(dump);
    if PACKET_TAP.set(tap).is_err() {
        warn!("[PacketDump] Packet tap was already set. Recording is off.");
        *DUMP.lock() = None;
        return;
    }
    info!(
        "[PacketDump] Recording configuration, container and click packets to {shown} (cap {cap_mb} MB). Packets are observed only, never changed."
    );
}

fn auto_file_name(now: chrono::DateTime<chrono::Utc>) -> String {
    format!("{AUTO_PREFIX}{}{AUTO_EXT}", now.format("%Y%m%d-%H%M%S"))
}

/// Delete the oldest `packets-*.bin` files in `dir` until the rest fit in
/// `keep` bytes. Newest first by modification time, so the latest runs stay.
/// Returns the deleted paths.
fn prune_old_dumps(dir: &Path, keep: u64) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut dumps: Vec<(std::time::SystemTime, PathBuf, u64)> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?;
            if !name.starts_with(AUTO_PREFIX) || !name.ends_with(AUTO_EXT) {
                return None;
            }
            let meta = entry.metadata().ok()?;
            if !meta.is_file() {
                return None;
            }
            let modified = meta.modified().unwrap_or(UNIX_EPOCH);
            Some((modified, entry.path(), meta.len()))
        })
        .collect();
    dumps.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));

    let mut kept = 0u64;
    let mut removed = Vec::new();
    for (_, path, len) in dumps {
        // Once one file does not fit, every older one goes too.
        kept = kept.saturating_add(len);
        if kept <= keep {
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => removed.push(path),
            Err(error) => warn!("[PacketDump] Cannot delete {}: {error}", path.display()),
        }
    }
    removed
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
        (true, Some(ConnectionProtocol::Configuration)) => Some(Kind::InConfig),
        (true, Some(ConnectionProtocol::Game)) if INBOUND_GAME.contains(&name) => {
            Some(Kind::InGame)
        }
        (false, _) if OUTBOUND.contains(&name) => Some(Kind::Out),
        _ => None,
    }
}

impl Dump {
    fn open(path: &Path, cap: u64) -> std::io::Result<Self> {
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
        let p = path.as_path();
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
        let p = path.as_path();
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

    #[test]
    fn every_configuration_packet_and_the_transfer_traffic_is_kept() {
        use ConnectionProtocol::{Configuration, Game};
        // A stalled phase must show everything Hypixel sent in it.
        for name in [
            "resource_pack_push",
            "resource_pack_pop",
            "ping",
            "keep_alive",
            "cookie_request",
            "custom_payload",
            "finish_configuration",
        ] {
            assert_eq!(
                classify(true, Some(Configuration), name),
                Some(Kind::InConfig),
                "{name}"
            );
        }
        for name in [
            "keep_alive",
            "pong_response",
            "resource_pack_push",
            "disconnect",
            "transfer",
        ] {
            assert_eq!(
                classify(true, Some(Game), name),
                Some(Kind::InGame),
                "{name}"
            );
        }
        // Outbound carries no state: a game ping_request in a configuration
        // phase is in the file, and the oracle flags it.
        for name in ["keep_alive", "pong", "ping_request", "cookie_response"] {
            assert_eq!(classify(false, None, name), Some(Kind::Out), "{name}");
        }
        // Still no chat, either way.
        assert_eq!(classify(true, Some(Game), "system_chat"), None);
        assert_eq!(classify(false, None, "chat"), None);
        assert_eq!(classify(false, None, "chat_command_signed"), None);
    }

    #[test]
    fn auto_file_name_sorts_by_start_time() {
        use chrono::TimeZone;
        let at = chrono::Utc.with_ymd_and_hms(2026, 11, 20, 7, 5, 9).unwrap();
        assert_eq!(auto_file_name(at), "packets-20261120-070509.bin");
    }

    #[test]
    fn prune_keeps_the_newest_dumps_and_ignores_other_files() {
        let dir = temp_path("prune");
        std::fs::create_dir_all(&dir).unwrap();
        let base = SystemTime::now();
        let write = |name: &str, len: usize, age_secs: u64| {
            let path = dir.join(name);
            std::fs::write(&path, vec![0u8; len]).unwrap();
            let file = File::options().write(true).open(&path).unwrap();
            file.set_modified(base - std::time::Duration::from_secs(age_secs))
                .unwrap();
        };
        write("packets-3.bin", 40, 10);
        write("packets-2.bin", 40, 20);
        write("packets-1.bin", 40, 30);
        write("latest.log", 500, 40);
        write("packets-notes.txt", 500, 50);

        let removed = prune_old_dumps(&dir, 100);
        let mut left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(removed, vec![dir.join("packets-1.bin")]);
        assert_eq!(
            left,
            vec![
                "latest.log",
                "packets-2.bin",
                "packets-3.bin",
                "packets-notes.txt"
            ]
        );
    }
}
