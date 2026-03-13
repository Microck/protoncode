use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use dirs::config_dir;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppConfig {
    pub poll_interval_seconds: u64,
    pub notification_duration_seconds: u64,
    pub launch_on_startup: bool,
    pub start_minimized_to_tray: bool,
    pub copy_button_enabled: bool,
    pub proton_mail_url: String,
    pub user_data_dir: PathBuf,
}

impl Default for AppConfig {
    fn default() -> Self {
        let base_dir = app_config_dir().unwrap_or_else(|| PathBuf::from("."));
        let user_data_dir = base_dir.join("webview-data");

        Self {
            poll_interval_seconds: 8,
            notification_duration_seconds: 8,
            launch_on_startup: false,
            start_minimized_to_tray: true,
            copy_button_enabled: true,
            proton_mail_url: "https://mail.proton.me/u/0/inbox".to_owned(),
            user_data_dir,
        }
    }
}

pub fn app_config_dir() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join("protoncode"))
}

impl AppConfig {
    pub fn load_or_default() -> Result<Self> {
        let path = config_path().context("config path unavailable")?;
        if !path.exists() {
            let config = Self::default();
            config.save()?;
            return Ok(config);
        }

        let bytes = fs::read(&path)
            .with_context(|| format!("failed to read config from {}", path.display()))?;
        let config = serde_json::from_slice::<Self>(&bytes)
            .with_context(|| format!("failed to parse config from {}", path.display()))?;
        Ok(config)
    }

    pub fn save(&self) -> Result<()> {
        let path = config_path().context("config path unavailable")?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create config directory {}", parent.display())
            })?;
        }
        fs::create_dir_all(&self.user_data_dir).with_context(|| {
            format!(
                "failed to create webview data directory {}",
                self.user_data_dir.display()
            )
        })?;

        let payload = serde_json::to_vec_pretty(self).context("failed to serialize config")?;
        fs::write(&path, payload)
            .with_context(|| format!("failed to write config to {}", path.display()))?;
        Ok(())
    }

    pub fn ensure_runtime_dirs(&self) -> Result<()> {
        fs::create_dir_all(&self.user_data_dir).with_context(|| {
            format!(
                "failed to create webview data directory {}",
                self.user_data_dir.display()
            )
        })?;
        Ok(())
    }
}

pub fn config_path() -> Option<PathBuf> {
    app_config_dir().map(|dir| dir.join("config.json"))
}

pub fn seen_cache_path() -> Option<PathBuf> {
    app_config_dir().map(|dir| dir.join("seen-cache.json"))
}

pub fn ensure_parent_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create directory {}", parent.display()))?;
    }
    Ok(())
}
