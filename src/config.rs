use anyhow::{Context, Result as AnyhowResult};
use serde::Deserialize;
use std::fs;

use crate::error::DotsymError;

#[derive(Deserialize, Debug, Clone)]
pub struct Config {
    pub separator: String,
    pub dir: String,
    #[serde(default)]
    pub hostname: Option<String>,
}

pub fn load_config() -> AnyhowResult<Config> {
    let home_dir = dirs::home_dir()
        .ok_or(DotsymError::HomeDirectoryNotFound)?;

    let config_path = home_dir.join(".config/dotsym/dotsym.toml");

    if !config_path.exists() {
        return Err(DotsymError::ConfigNotFound {
            path: config_path
        }.into());
    }

    let config_content = fs::read_to_string(&config_path)
        .with_context(|| format!("Failed to read config file at {}", config_path.display()))?;

    let config: Config = toml::from_str(&config_content)
        .map_err(|e| DotsymError::ConfigInvalid {
            path: config_path,
            source: e
        })?;

    Ok(config)
}