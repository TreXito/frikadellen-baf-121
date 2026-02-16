use anyhow::Result;
use std::path::PathBuf;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

pub fn init_logger() -> Result<()> {
    let log_path = get_log_path();
    
    // Create log directory if it doesn't exist
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Create file appender
    let file_appender = RollingFileAppender::new(
        Rotation::DAILY,
        log_path.parent().unwrap_or(&PathBuf::from(".")),
        "baf.log",
    );

    // Create filter with specific rules to suppress noise
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| {
            EnvFilter::new("info")
                // Suppress Azalea chunk entity warnings (they're just noise)
                .add_directive("azalea_world=error".parse().unwrap())
                .add_directive("azalea_entity=error".parse().unwrap())
        });

    // Set up subscriber with both console and file output
    tracing_subscriber::registry()
        .with(filter)
        .with(
            fmt::layer()
                .with_writer(std::io::stdout)
                .with_ansi(true)
                .with_target(false)
        )
        .with(
            fmt::layer()
                .with_writer(file_appender)
                .with_ansi(false)
                .with_target(true)
        )
        .init();

    tracing::info!("Logger initialized, writing to {:?}", log_path);
    Ok(())
}

fn get_log_path() -> PathBuf {
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
    
    exe_dir.join("baf.log")
}

/// Remove Minecraft color codes from a string
pub fn remove_color_codes(text: &str) -> String {
    let re = regex::Regex::new(r"§[0-9a-fk-or]").unwrap();
    re.replace_all(text, "").to_string()
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
    let colored = mc_to_ansi(message);
    println!("{}", colored);
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
}
