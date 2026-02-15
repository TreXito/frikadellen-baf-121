use super::types::Config;
use anyhow::{Context, Result};
use std::fs;
use std::path::PathBuf;
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
        // Use platform-specific config directory
        #[cfg(target_os = "windows")]
        {
            let appdata = std::env::var("APPDATA").unwrap_or_else(|_| ".".to_string());
            PathBuf::from(appdata).join("BAF").join("config.toml")
        }
        
        #[cfg(not(target_os = "windows"))]
        {
            dirs::config_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("baf")
                .join("config.toml")
        }
    }

    pub fn load(&self) -> Result<Config> {
        if !self.config_path.exists() {
            info!("Config file not found, creating default config at {:?}", self.config_path);
            let config = Config::default();
            self.save(&config)?;
            return Ok(config);
        }

        let contents = fs::read_to_string(&self.config_path)
            .context("Failed to read config file")?;
        
        let config: Config = toml::from_str(&contents)
            .context("Failed to parse config file")?;
        
        info!("Loaded configuration from {:?}", self.config_path);
        Ok(config)
    }

    pub fn save(&self, config: &Config) -> Result<()> {
        // Ensure parent directory exists
        if let Some(parent) = self.config_path.parent() {
            fs::create_dir_all(parent)
                .context("Failed to create config directory")?;
        }

        let toml_string = toml::to_string_pretty(config)
            .context("Failed to serialize config")?;
        
        fs::write(&self.config_path, toml_string)
            .context("Failed to write config file")?;
        
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
