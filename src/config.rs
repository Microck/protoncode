//! Application configuration management.
//!
//! Defines [`AppConfig`] and helpers for loading, saving, and locating the
//! configuration file and related runtime paths.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use dirs::config_dir;
use serde::{Deserialize, Serialize};

/// Persistent application configuration stored as JSON on disk.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppConfig {
    /// Interval in seconds between Proton Mail page snapshots.
    pub poll_interval_seconds: u64,
    /// How long the OTP notification overlay stays visible, in seconds.
    pub notification_duration_seconds: u64,
    /// Whether ProtonCode should launch automatically on system startup.
    pub launch_on_startup: bool,
    /// Whether the Proton Mail window starts hidden in the system tray.
    pub start_minimized_to_tray: bool,
    /// Whether the overlay shows a "Copy code" button.
    pub copy_button_enabled: bool,
    /// URL of the Proton Mail inbox loaded in the embedded webview.
    pub proton_mail_url: String,
    /// Directory used for webview persistent storage (cookies, session data).
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

/// Returns the platform-specific configuration directory for ProtonCode
/// (e.g. `~/.config/protoncode` on Linux, `%APPDATA%\protoncode` on Windows).
pub fn app_config_dir() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join("protoncode"))
}

impl AppConfig {
    /// Loads the config from disk, creating a default config file if none exists.
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

    /// Persists the current configuration to disk, creating parent directories as needed.
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

    /// Ensures the webview data directory exists at runtime.
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

/// Returns the full path to `config.json` inside the ProtonCode config directory.
pub fn config_path() -> Option<PathBuf> {
    app_config_dir().map(|dir| dir.join("config.json"))
}

/// Returns the full path to `seen-cache.json` inside the ProtonCode config directory.
pub fn seen_cache_path() -> Option<PathBuf> {
    app_config_dir().map(|dir| dir.join("seen-cache.json"))
}

/// Creates all parent directories of `path` if they do not already exist.
pub fn ensure_parent_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create directory {}", parent.display()))?;
    }
    Ok(())
}
