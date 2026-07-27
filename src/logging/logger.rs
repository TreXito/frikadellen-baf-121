use anyhow::Result;
use once_cell::sync::Lazy;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::{filter::LevelFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer};

/// When `true`, the logger prefixes all messages with `userId:instanceId`.
/// Set once at startup via `set_vps_log_prefix`.
static VPS_MODE: AtomicBool = AtomicBool::new(false);

/// Prefix string used in VPS mode: `"userId:instanceId "`.
static VPS_PREFIX: Lazy<std::sync::Mutex<String>> =
    Lazy::new(|| std::sync::Mutex::new(String::new()));

/// Enable VPS-mode logging.  All subsequent log messages (via `print_mc_chat`
/// and tracing macros) will be prefixed with `userId:instanceId`.
pub fn set_vps_log_prefix(user_id: &str, instance_id: &str) {
    let prefix = format!("{}:{}", user_id, instance_id);
    if let Ok(mut p) = VPS_PREFIX.lock() {
        *p = prefix;
    }
    VPS_MODE.store(true, Ordering::Relaxed);
}

/// Returns the VPS prefix string (e.g. `"user123:inst-abc"`) if VPS mode is
/// active, or an empty string otherwise.
pub fn vps_prefix() -> String {
    if VPS_MODE.load(Ordering::Relaxed) {
        VPS_PREFIX.lock().map(|p| p.clone()).unwrap_or_default()
    } else {
        String::new()
    }
}

/// The log filter used when `RUST_LOG` says nothing.
///
/// Split out so the noise-suppression rules can be tested: each one exists
/// because a library warns about something the user can neither fix nor act on,
/// and a regression here shows up as console spam in front of customers.
fn default_filter() -> EnvFilter {
    EnvFilter::new("info")
        // Azalea emits harmless errors and warnings (set_equipment "Unexpected
        // enum variant 7", chunk entity warnings). Anything that matters
        // surfaces through our own logging when we handle the event.
        .add_directive("azalea_world=off".parse().unwrap())
        .add_directive("azalea_entity=off".parse().unwrap())
        .add_directive("azalea_client=off".parse().unwrap())
        .add_directive("azalea_protocol=off".parse().unwrap())
        // "Illegal SNI extension: ignoring IP address presented as hostname".
        // The panel is reached by IP, and a client that puts an IP literal in
        // SNI trips this on EVERY connection. rustls ignores the field and the
        // handshake succeeds, so it is noise the user cannot act on — but it
        // reads like a security warning and lands in front of customers. Real
        // rustls problems are logged at error and still come through.
        .add_directive("rustls::msgs::handshake=error".parse().unwrap())
}

pub fn init_logger() -> Result<()> {
    let logs_dir = get_logs_dir();
    std::fs::create_dir_all(&logs_dir)?;
    rotate_previous_latest_log(&logs_dir)?;
    // Clean up log files older than 7 days at startup
    cleanup_old_logs(&logs_dir, 7);

    // Create file appender
    let file_appender = RollingFileAppender::new(
        Rotation::NEVER,
        &logs_dir,
        "latest.log",
    );


    // Create filter with specific rules to suppress noise.
    // Azalea library crates emit harmless errors & warnings (e.g. set_equipment
    // "Unexpected enum variant 7", chunk entity warnings) that spam the console.
    // We suppress them entirely – any important azalea error will surface through
    // our own logging when we handle the event.
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| default_filter());

    // Set up subscriber with both console and file output
    tracing_subscriber::registry()
        .with(filter)
        .with(
            fmt::layer()
                .with_writer(std::io::stdout)
                .with_ansi(true)
                .with_target(false)
                .with_filter(LevelFilter::WARN)
        )
        .with(
            fmt::layer()
                .with_writer(file_appender)
                .with_ansi(false)
                .with_target(true)
        )
        .init();

    tracing::info!("Logger initialized, writing to {:?}", logs_dir.join("latest.log"));
    Ok(())
}

pub fn get_logs_dir() -> PathBuf {
    // Use executable directory for log file
    // This allows multiple instances to run with separate logs
    let exe_dir = match std::env::current_exe() {
        Ok(exe_path) => {
            exe_path.parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| {
                    eprintln!("Warning: Could not get parent directory of executable, using current directory");
                    PathBuf::from(".")
                })
        }
        Err(e) => {
            eprintln!("Warning: Could not get executable path ({}), using current directory", e);
            PathBuf::from(".")
        }
    };

    exe_dir.join("logs")
}

fn rotate_previous_latest_log(logs_dir: &Path) -> Result<()> {
    let latest_log = logs_dir.join("latest.log");
    if !latest_log.exists() {
        return Ok(());
    }

    let session_start = std::fs::metadata(&latest_log)
        .and_then(|m| m.modified())
        .ok()
        .map(chrono::DateTime::<chrono::Utc>::from)
        .unwrap_or_else(chrono::Utc::now);

    let base_name = session_start
        .with_timezone(&chrono::Local)
        .format("%Y-%m-%d_%H-%M-%S")
        .to_string();

    let mut archived_log = logs_dir.join(format!("{}.log", base_name));
    let mut suffix = 1usize;
    while archived_log.exists() {
        archived_log = logs_dir.join(format!("{}_{}.log", base_name, suffix));
        suffix += 1;
    }
    std::fs::rename(latest_log, archived_log)?;
    Ok(())
}

const SECS_PER_DAY: u64 = 86_400;

/// Delete archived log files older than `max_age_days` days.
/// Only removes `*.log` files that are NOT `latest.log`.
pub fn cleanup_old_logs(logs_dir: &Path, max_age_days: u64) {
    let cutoff = std::time::SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(max_age_days * SECS_PER_DAY));
    let cutoff = match cutoff {
        Some(c) => c,
        None => return, // clock error, skip cleanup
    };

    let entries = match std::fs::read_dir(logs_dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    let mut removed = 0u32;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        // Only delete archived log files, never the active latest.log
        if name == "latest.log" || !name.ends_with(".log") {
            continue;
        }
        let modified = match entry.metadata().and_then(|m| m.modified()) {
            Ok(t) => t,
            Err(_) => continue,
        };
        if modified < cutoff {
            if let Err(e) = std::fs::remove_file(&path) {
                eprintln!("Warning: Failed to remove old log {}: {}", name, e);
            } else {
                removed += 1;
            }
        }
    }
    if removed > 0 {
        // Logger may not be initialized yet at startup, so use eprintln
        eprintln!("Cleaned up {} old log file(s) from {:?}", removed, logs_dir);
    }
}

/// Spawn a background task that cleans up old logs once per day.
pub fn spawn_periodic_log_cleanup() {
    let logs_dir = get_logs_dir();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(SECS_PER_DAY)).await;
            cleanup_old_logs(&logs_dir, 7);
        }
    });
}

/// Pre-compiled regex for stripping Minecraft §-color codes.  Compiled once on
/// first use instead of on every call, avoiding ~1 ms of regex compilation overhead
/// per invocation (significant during rapid chat/lore processing).
static COLOR_CODE_RE: Lazy<regex::Regex> =
    Lazy::new(|| regex::Regex::new(r"§[0-9a-fk-or]").unwrap());

/// Remove Minecraft color codes from a string
pub fn remove_color_codes(text: &str) -> String {
    COLOR_CODE_RE.replace_all(text, "").to_string()
}

/// Convert Minecraft color codes to ANSI color codes for terminal display
pub fn mc_to_ansi(text: &str) -> String {
    text.replace("§0", "\x1b[30m")     // Black
        .replace("§1", "\x1b[34m")     // Dark Blue
        .replace("§2", "\x1b[32m")     // Dark Green
        .replace("§3", "\x1b[36m")     // Dark Aqua
        .replace("§4", "\x1b[31m")     // Dark Red
        .replace("§5", "\x1b[35m")     // Dark Purple
        .replace("§6", "\x1b[33m")     // Gold
        .replace("§7", "\x1b[37m")     // Gray
        .replace("§8", "\x1b[90m")     // Dark Gray
        .replace("§9", "\x1b[94m")     // Blue
        .replace("§a", "\x1b[92m")     // Green
        .replace("§b", "\x1b[96m")     // Aqua
        .replace("§c", "\x1b[91m")     // Red
        .replace("§d", "\x1b[95m")     // Light Purple
        .replace("§e", "\x1b[93m")     // Yellow
        .replace("§f", "\x1b[97m")     // White
        .replace("§l", "\x1b[1m")      // Bold
        .replace("§m", "\x1b[9m")      // Strikethrough
        .replace("§n", "\x1b[4m")      // Underline
        .replace("§o", "\x1b[3m")      // Italic
        .replace("§r", "\x1b[0m")      // Reset
        + "\x1b[0m" // Always reset at the end
}

/// Print a Minecraft chat message to console (with color code processing)
pub fn print_mc_chat(message: &str) {
    let prefix = vps_prefix();
    let colored = mc_to_ansi(message);
    if prefix.is_empty() {
        println!("{}", colored);
    } else {
        println!("[{}] {}", prefix, colored);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_remove_color_codes() {
        let text = "§f[§4BAF§f]: §aHello §eWorld";
        let clean = remove_color_codes(text);
        assert_eq!(clean, "[BAF]: Hello World");
    }

    #[test]
    fn test_mc_to_ansi() {
        let text = "§f[§4BAF§f]: §aTest";
        let ansi = mc_to_ansi(text);
        
        // Should contain ANSI escape codes
        assert!(ansi.contains("\x1b["));
        // Should end with reset
        assert!(ansi.ends_with("\x1b[0m"));
        // Original text parts should still be present (ANSI codes are inserted, not replacing text)
        assert!(ansi.contains("BAF"));
        assert!(ansi.contains("Test"));
    }

    #[test]
    fn test_mc_to_ansi_colors() {
        // Test all basic colors are converted
        assert!(mc_to_ansi("§0").contains("\x1b[30m")); // Black
        assert!(mc_to_ansi("§1").contains("\x1b[34m")); // Dark Blue
        assert!(mc_to_ansi("§2").contains("\x1b[32m")); // Dark Green
        assert!(mc_to_ansi("§4").contains("\x1b[31m")); // Dark Red
        assert!(mc_to_ansi("§a").contains("\x1b[92m")); // Green
        assert!(mc_to_ansi("§c").contains("\x1b[91m")); // Red
        assert!(mc_to_ansi("§e").contains("\x1b[93m")); // Yellow
        assert!(mc_to_ansi("§f").contains("\x1b[97m")); // White
        
        // Test formatting codes
        assert!(mc_to_ansi("§l").contains("\x1b[1m"));  // Bold
        assert!(mc_to_ansi("§r").contains("\x1b[0m"));  // Reset
    }

    use std::sync::{Arc, Mutex};
    use tracing_subscriber::layer::Context;

    /// Records the targets of every event that survives the filter.
    struct Capture(Arc<Mutex<Vec<String>>>);

    impl<S: tracing::Subscriber> Layer<S> for Capture {
        fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
            self.0.lock().unwrap().push(event.metadata().target().to_string());
        }
    }

    fn targets_passing_default_filter(emit: impl FnOnce()) -> Vec<String> {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry()
            .with(default_filter())
            .with(Capture(seen.clone()));
        tracing::subscriber::with_default(subscriber, emit);
        let out = seen.lock().unwrap().clone();
        out
    }

    /// A customer asked "what does that mean" about a warning they cannot act
    /// on. rustls logs it for EVERY connection that presents an IP as the SNI
    /// hostname, which is what a panel reached by IP gets, so it spams the
    /// console and reads like a security problem. The handshake succeeds
    /// regardless — rustls simply ignores the field.
    #[test]
    fn the_illegal_sni_warning_is_not_shown_to_users() {
        let seen = targets_passing_default_filter(|| {
            tracing::warn!(
                target: "rustls::msgs::handshake",
                "Illegal SNI extension: ignoring IP address presented as hostname"
            );
        });
        assert!(
            seen.is_empty(),
            "the per-connection SNI warning must not reach the console, got: {seen:?}"
        );
    }

    /// Suppressing the noise must not blind us to genuine TLS failures.
    #[test]
    fn real_rustls_errors_still_come_through() {
        let seen = targets_passing_default_filter(|| {
            tracing::error!(target: "rustls::msgs::handshake", "something actually broke");
            tracing::error!(target: "rustls", "a different rustls module");
        });
        assert_eq!(seen.len(), 2, "errors must survive the filter, got: {seen:?}");
    }

    /// Our own logging is what the filter exists to protect.
    #[test]
    fn our_own_warnings_are_unaffected() {
        let seen = targets_passing_default_filter(|| {
            tracing::warn!(target: "frikadellen_baf::web::server", "[WebTLS] something worth seeing");
            tracing::info!(target: "frikadellen_baf", "startup");
        });
        assert_eq!(seen.len(), 2, "the bot's own logs must not be filtered, got: {seen:?}");
    }

    #[test]
    fn azalea_noise_stays_suppressed() {
        let seen = targets_passing_default_filter(|| {
            tracing::error!(target: "azalea_world", "Unexpected enum variant 7");
            tracing::warn!(target: "azalea_client", "chunk entity warning");
        });
        assert!(seen.is_empty(), "azalea noise must stay off, got: {seen:?}");
    }

}
