use tracing::{error, info};

/// Restart the current process by re-executing the binary.
///
/// On Unix, uses `exec()` to replace the current process image (PID stays the same).
/// On other platforms, spawns a new process and exits.
///
/// This is used for account switching so the bot restarts itself without needing
/// an external supervisor (e.g. `while true; do ./bot; done`).
pub fn restart_process() -> ! {
    let exe = std::env::current_exe().expect("Failed to get current executable path");
    let args: Vec<String> = std::env::args().skip(1).collect();
    info!(
        "[Restart] Re-executing {} with args {:?}",
        exe.display(),
        args
    );
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // exec() replaces the current process image — PID stays the same
        let err = std::process::Command::new(&exe).args(&args).exec();
        // exec() only returns on error
        error!("[Restart] Failed to exec: {}", err);
        std::process::exit(1);
    }
    #[cfg(not(unix))]
    {
        match std::process::Command::new(&exe).args(&args).spawn() {
            Ok(_) => std::process::exit(0),
            Err(e) => {
                error!("[Restart] Failed to spawn new process: {}", e);
                std::process::exit(1);
            }
        }
    }
}
