use super::types::Config;
use anyhow::{Context, Result};
use std::fs;
use std::path::PathBuf;
use tracing::info;

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
            info!("Config file not found, creating default config at {:?}", self.config_path);
            let config = Config::default();
            self.save(&config)?;
            return Ok(config);
        }

        let contents = fs::read_to_string(&self.config_path)
            .context("Failed to read config file")?;
        
        let config = Self::parse_config(&contents)?;
        
        // Re-save after every load so that newly added config fields
        // appear in the file with their default values (matches TypeScript
        // initConfigHelper: "add new default values to existing config").
        self.save(&config)?;
        
        info!("Loaded configuration from {:?}", self.config_path);
        Ok(config)
    }

    fn parse_config(contents: &str) -> Result<Config> {
        let value: toml::Value = toml::from_str(contents)
            .context("Failed to parse config file")?;

        value.try_into().context("Failed to deserialize config file")
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

#[cfg(test)]
mod tests {
    use super::ConfigLoader;

    #[test]
    fn parse_config_ignores_unknown_fields() {
        // confirm_skip is an unknown field — parsing must succeed (not panic/error).
        let config = ConfigLoader::parse_config("confirm_skip = true")
            .expect("config with unknown field should still parse");
        // Known defaults still apply (bed timing defaults to on)
        assert!(config.bedtiming_enabled());
    }
}

impl Default for ConfigLoader {
    fn default() -> Self {
        Self::new()
    }
}
