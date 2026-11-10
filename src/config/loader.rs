use super::types::Config;
use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use tracing::{info, warn};

pub struct ConfigLoader {
    config_path: PathBuf,
}

impl ConfigLoader {
    pub fn new() -> Self {
        let config_path = Self::get_config_path();
        Self { config_path }
    }

    fn get_config_path() -> PathBuf {
        // Use executable directory for config file
        // This allows multiple instances to run with different configs
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

        exe_dir.join("config.toml")
    }

    pub fn load(&self) -> Result<Config> {
        if !self.config_path.exists() {
            info!(
                "Config file not found, creating default config at {:?}",
                self.config_path
            );
            let mut config = Config::default();
            if let Some(password) = config.ensure_web_gui_password() {
                println!("{}", Self::password_box(&password, config.web_gui_port));
            }
            self.save(&config)?;
            return Ok(config);
        }

        let contents =
            fs::read_to_string(&self.config_path).context("Failed to read config file")?;

        let mut config = Self::parse_config(&contents)?;
        config.normalize_do_not_relist_ids();
        // Undo the region pinning older builds wrote here. COFL redirects to the
        // nearest modsocket on its own, so the neutral host is correct for
        // everyone and a stale regional one can be unreachable.
        if let Some(previous) = config.reset_regional_websocket_url() {
            info!(
                "Reset regional websocket_url {:?} to {:?} — Coflnet redirects to your \
                 nearest server automatically",
                previous, config.websocket_url
            );
        }
        // Heal configs written before the panel required a password, and configs
        // where the field was blanked out. The save below persists it.
        let generated_password = config.ensure_web_gui_password();
        // Copy the file aside BEFORE the save below rewrites it, so a bad
        // migration is one `mv` away from being undone.
        self.backup_before_migration(&contents, generated_password.is_some());
        // Only a freshly generated password is ever shown. Once the panel is
        // initialised the secret stays in config.toml: echoing it on every start
        // meant anyone watching a stream or a screen-share of the console got the
        // keys to the panel.
        if let Some(notice) = Self::panel_notice(
            generated_password.as_deref(),
            config.web_gui_password.as_deref(),
            config.web_gui_port,
            &self.config_path,
        ) {
            println!("{notice}");
        }

        // Re-save after every load so that newly added config fields
        // appear in the file with their default values (matches TypeScript
        // initConfigHelper: "add new default values to existing config").
        self.save(&config)?;

        info!("Loaded configuration from {:?}", self.config_path);
        Ok(config)
    }

    /// Settings this build drops from config.toml. Their presence in the file
    /// on disk is what marks it as "written by an older build".
    ///
    /// `web_tls_cert_path` / `web_tls_key_path` are NOT in here: dropping them
    /// silently deleted the certificate people had configured and left the panel
    /// serving its own self-signed cert, with nothing in the log to say why.
    /// Only `web_https` stays removed — TLS is unconditional now, so a flag that
    /// could turn it off is the actual footgun.
    const REMOVED_KEYS: [&'static str; 1] = ["web_https"];

    /// Save a copy of config.toml before this build's panel-security migration
    /// rewrites it — generating a password, or dropping the old TLS settings.
    ///
    /// Only for that migration, not for every save: the loader re-saves on every
    /// single load, and a backup per start would bury the one copy that matters.
    /// A failure here is logged and ignored, because refusing to start the bot
    /// over an un-writable backup would be a worse outcome than no backup.
    fn backup_before_migration(&self, contents: &str, generated_password: bool) {
        let has_removed_keys = Self::REMOVED_KEYS.iter().any(|k| contents.contains(k));
        if !generated_password && !has_removed_keys {
            return;
        }

        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let mut name = self
            .config_path
            .file_name()
            .unwrap_or_default()
            .to_os_string();
        name.push(format!(".bak-{stamp}"));
        let backup_path = self.config_path.with_file_name(name);
        // Never clobber an existing backup: two starts in the same second must
        // not let the second one overwrite the original with an already-migrated
        // copy.
        if backup_path.exists() {
            return;
        }

        match fs::write(&backup_path, contents) {
            Ok(()) => warn!(
                "Config backed up to {:?} before the panel-security update — restore that file if anything looks wrong",
                backup_path
            ),
            Err(e) => warn!("Could not back up config before updating it: {}", e),
        }
    }

    /// What the console says about the panel at startup, if anything.
    ///
    /// The password appears ONLY when `generated` is set, i.e. on the one start
    /// that created it. For an already-initialised panel the notice names the
    /// file the password lives in, never the password.
    fn panel_notice(
        generated: Option<&str>,
        existing: Option<&str>,
        port: u16,
        config_path: &Path,
    ) -> Option<String> {
        match (generated, existing) {
            (Some(password), _) => Some(Self::password_box(password, port)),
            (None, Some(e)) if !e.is_empty() => Some(Self::panel_location(port, config_path)),
            _ => None,
        }
    }

    /// The box shown once, when a panel password is first generated.
    ///
    /// Printed with `println!` rather than `warn!` on purpose: every warning is
    /// also written to latest.log, which the panel offers as a download and which
    /// users paste into Discord for support. This password can pause the bot,
    /// read its chat and rewrite its config, so it must never sit in that file.
    /// The console belongs to whoever started the bot, and config.toml holds the
    /// same value in plain text.
    ///
    /// It used to print on every start so a forgotten password was one restart
    /// away, but a console that is streamed or screen-shared then leaked the
    /// panel on each launch.
    fn password_box(password: &str, port: u16) -> String {
        use crate::logging::mc_to_ansi;
        // Each line is padded to the same inner width so the box lines up for
        // any port or password length; colour is applied after padding.
        let line = |color: &str, text: &str| mc_to_ansi(&format!("§8║  {color}{:<54}§8  ║", text));
        [
            mc_to_ansi("§8╔══════════════════════════════════════════════════════════╗"),
            line("§a§l", "WEB PANEL PASSWORD GENERATED"),
            line("§e", password),
            line(
                "§7",
                &format!("Open https://localhost:{port} and log in with it."),
            ),
            line("§7", "Shown this once. It is stored in config.toml as"),
            line("§7", "web_gui_password; change it there or in the panel."),
            mc_to_ansi("§8╚══════════════════════════════════════════════════════════╝"),
        ]
        .join("\n")
    }

    /// One line for every start after the first: where the panel is and where
    /// its password lives, never the password itself.
    fn panel_location(port: u16, config_path: &Path) -> String {
        crate::logging::mc_to_ansi(&format!(
            "§f[§4BAF§f]: §7Web panel: §bhttps://localhost:{port}§7, password is web_gui_password in §f{}",
            config_path.display()
        ))
    }

    fn parse_config(contents: &str) -> Result<Config> {
        let value: toml::Value = toml::from_str(contents).context("Failed to parse config file")?;

        value
            .try_into()
            .context("Failed to deserialize config file")
    }

    pub fn save(&self, config: &Config) -> Result<()> {
        // Ensure parent directory exists
        if let Some(parent) = self.config_path.parent() {
            fs::create_dir_all(parent).context("Failed to create config directory")?;
        }

        let toml_string = toml::to_string_pretty(config).context("Failed to serialize config")?;

        fs::write(&self.config_path, toml_string).context("Failed to write config file")?;

        info!("Saved configuration to {:?}", self.config_path);
        Ok(())
    }

    pub fn update_property<F>(&self, mut updater: F) -> Result<()>
    where
        F: FnMut(&mut Config),
    {
        let mut config = self.load()?;
        updater(&mut config);
        self.save(&config)?;
        Ok(())
    }
}

impl Default for ConfigLoader {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::ConfigLoader;
    use std::path::{Path, PathBuf};

    /// A loader pointed at a throwaway file instead of the executable's dir.
    fn temp_loader(name: &str) -> (ConfigLoader, PathBuf) {
        let dir = std::env::temp_dir().join(format!("baf-cfg-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("config.toml");
        (
            ConfigLoader {
                config_path: path.clone(),
            },
            path,
        )
    }

    /// Every `config.toml.bak-*` sitting next to the config.
    fn backups_of(path: &Path) -> Vec<PathBuf> {
        let dir = path.parent().expect("config has a parent dir");
        let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
            .expect("read temp dir")
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.contains(".bak-"))
            })
            .collect();
        found.sort();
        found
    }

    fn cleanup(path: &Path) {
        let _ = std::fs::remove_dir_all(path.parent().expect("parent"));
    }

    #[test]
    fn a_generated_config_gets_a_random_panel_password() {
        let (loader, path) = temp_loader("fresh");
        let config = loader.load().expect("fresh config should be created");
        let password = config
            .web_gui_password
            .expect("a password should be generated");
        assert!(!password.is_empty());
        // It must reach the file, not just the in-memory config — the user has
        // to be able to read it back after the startup banner scrolls past.
        let written = std::fs::read_to_string(&path).expect("config should be on disk");
        assert!(
            written.contains(&password),
            "generated password should be persisted"
        );
        cleanup(&path);
    }

    #[test]
    fn loading_an_unprotected_config_heals_it() {
        // This is the whole point: panels already in the wild with no password
        // get one the next time they start, without the user doing anything.
        let (loader, path) = temp_loader("unprotected");
        std::fs::write(&path, "web_gui_password = \"\"\n").expect("write config");
        let config = loader.load().expect("config should load");
        let password = config
            .web_gui_password
            .expect("a password should be generated");
        assert_eq!(password.len(), 20);
        let reloaded = loader.load().expect("config should reload");
        assert_eq!(
            reloaded.web_gui_password.as_deref(),
            Some(password.as_str()),
            "a healed password must be stable across restarts, not regenerated every load"
        );
        cleanup(&path);
    }

    #[test]
    fn the_original_config_is_backed_up_before_it_is_migrated() {
        let (loader, path) = temp_loader("backup");
        let original = "web_gui_password = \"\"\nweb_https = false\nweb_gui_port = 9123\n";
        std::fs::write(&path, original).expect("write config");
        loader.load().expect("config should load");

        let backups = backups_of(&path);
        assert_eq!(
            backups.len(),
            1,
            "expected exactly one backup, got {backups:?}"
        );
        let saved = std::fs::read_to_string(&backups[0]).expect("backup readable");
        assert_eq!(
            saved, original,
            "the backup must be the file as it was, not the migrated version"
        );
        cleanup(&path);
    }

    /// The panel password must never reach latest.log.
    ///
    /// The panel can pause the bot, read its chat and rewrite its config, and
    /// that log file is downloadable from the panel and routinely pasted into
    /// Discord for support. Announcing the password with `warn!` put it in
    /// there. This pins that the announcement goes to stdout only, by checking
    /// no tracing event carries it.
    #[test]
    fn the_panel_password_is_never_written_to_the_log() {
        use std::sync::{Arc, Mutex};
        use tracing_subscriber::layer::{Context, SubscriberExt};
        use tracing_subscriber::Layer;

        struct Capture(Arc<Mutex<String>>);
        impl<S: tracing::Subscriber> Layer<S> for Capture {
            fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
                struct Visit<'a>(&'a mut String);
                impl tracing::field::Visit for Visit<'_> {
                    fn record_debug(
                        &mut self,
                        _f: &tracing::field::Field,
                        v: &dyn std::fmt::Debug,
                    ) {
                        self.0.push_str(&format!("{v:?}"));
                    }
                }
                if let Ok(mut buf) = self.0.lock() {
                    event.record(&mut Visit(&mut buf));
                }
            }
        }

        let secret = "SuperSecretPanelPw123";
        let captured = Arc::new(Mutex::new(String::new()));
        let subscriber = tracing_subscriber::registry().with(Capture(captured.clone()));
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(
                "{}",
                ConfigLoader::panel_location(8080, Path::new("config.toml"))
            );
            let _ = ConfigLoader::panel_notice(None, Some(secret), 8080, Path::new("config.toml"));
        });

        let logged = captured.lock().unwrap().clone();
        assert!(
            !logged.contains(secret),
            "the panel password must not appear in any log event, got: {logged:?}"
        );
    }

    /// Streaming the console must not leak the panel: after the first start the
    /// password is never part of what gets printed.
    #[test]
    fn an_initialised_panel_never_prints_its_password() {
        let secret = "SuperSecretPanelPw123";
        let path = Path::new("/srv/baf/config.toml");
        let notice = ConfigLoader::panel_notice(None, Some(secret), 8080, path)
            .expect("a location line should still be shown");
        assert!(!notice.contains(secret), "password leaked: {notice:?}");
        assert!(
            notice.contains("8080") && notice.contains("config.toml"),
            "got {notice:?}"
        );
    }

    #[test]
    fn the_password_is_shown_once_when_generated() {
        let secret = "SuperSecretPanelPw123";
        let notice =
            ConfigLoader::panel_notice(Some(secret), Some(secret), 8080, Path::new("config.toml"))
                .expect("a generated password must be announced");
        assert!(notice.contains(secret));
    }

    #[test]
    fn a_missing_password_prints_nothing() {
        assert!(ConfigLoader::panel_notice(None, None, 8080, Path::new("config.toml")).is_none());
        assert!(
            ConfigLoader::panel_notice(None, Some(""), 8080, Path::new("config.toml")).is_none()
        );
    }

    #[test]
    fn restoring_the_backup_undoes_the_migration() {
        // The actual promise being made to the user: copy the file back and you
        // are exactly where you started.
        let (loader, path) = temp_loader("revert");
        let original = "web_gui_password = \"\"\nweb_https = false\n";
        std::fs::write(&path, original).expect("write config");
        loader.load().expect("config should load");

        let backups = backups_of(&path);
        std::fs::copy(&backups[0], &path).expect("restore backup");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        cleanup(&path);
    }

    #[test]
    fn a_config_that_needs_no_migration_is_not_backed_up() {
        // Otherwise every restart drops another file next to the config and the
        // one backup that matters is impossible to pick out.
        let (loader, path) = temp_loader("nobackup");
        std::fs::write(&path, "web_gui_password = \"already-set\"\n").expect("write config");
        for _ in 0..3 {
            loader.load().expect("config should load");
        }
        assert!(
            backups_of(&path).is_empty(),
            "nothing changed, so nothing to back up"
        );
        cleanup(&path);
    }

    #[test]
    fn migrating_twice_does_not_overwrite_the_original_backup() {
        // A second start within the same second must not replace the pristine
        // copy with an already-migrated one.
        let (loader, path) = temp_loader("twice");
        let original = "web_gui_password = \"\"\n";
        std::fs::write(&path, original).expect("write config");
        loader.load().expect("first load");
        let first = backups_of(&path);
        assert_eq!(first.len(), 1);

        // Blank it again so the next load re-migrates in the same second.
        std::fs::write(&path, "web_gui_password = \"\"\n").expect("rewrite config");
        loader.load().expect("second load");
        let saved = std::fs::read_to_string(&first[0]).expect("backup readable");
        assert_eq!(saved, original, "the first backup must survive untouched");
        cleanup(&path);
    }

    #[test]
    fn an_existing_password_is_never_replaced() {
        // Forcing a password on unprotected panels must not disturb anyone who
        // already set one: no surprise lockout, no "why did my password change".
        let (loader, path) = temp_loader("existing");
        std::fs::write(&path, "web_gui_password = \"my-own-password\"\n").expect("write config");
        for _ in 0..3 {
            let config = loader.load().expect("config should load");
            assert_eq!(config.web_gui_password.as_deref(), Some("my-own-password"));
        }
        let written = std::fs::read_to_string(&path).expect("config on disk");
        assert!(
            written.contains("my-own-password"),
            "on-disk password should be untouched"
        );
        cleanup(&path);
    }

    #[test]
    fn parse_config_ignores_unknown_fields() {
        // confirm_skip is an unknown field — parsing must succeed (not panic/error).
        let config = ConfigLoader::parse_config("confirm_skip = true")
            .expect("config with unknown field should still parse");
        // Known defaults still apply (bed timing defaults to on)
        assert!(config.bedtiming_enabled());
    }
}
