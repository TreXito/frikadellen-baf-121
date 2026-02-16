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

    // Create filter
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info"));

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

/// Print a Minecraft chat message to console (with color code processing)
pub fn print_mc_chat(message: &str) {
    let clean = remove_color_codes(message);
    tracing::info!("[MC Chat] {}", clean);
}
