use std::collections::{HashSet, VecDeque};
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use time::OffsetDateTime;
use tracing::warn;

use crate::config::{AppConfig, ensure_parent_dir};
use crate::models::{MailSessionState, OtpCandidateEmail, OtpNotification};
use crate::otp::detect_otp;

const RECENT_CACHE_LIMIT: usize = 128;

#[derive(Debug, Clone)]
pub struct AppState {
    pub config: AppConfig,
    pub session_state: MailSessionState,
    pub current_notification: Option<OtpNotification>,
    recent_message_ids: HashSet<String>,
    recent_message_order: VecDeque<String>,
    seen_cache_path: PathBuf,
}

impl AppState {
    pub fn load() -> Result<Self> {
        let config = AppConfig::load_or_default()?;
        Self::from_config(config)
    }

    pub fn from_config(config: AppConfig) -> Result<Self> {
        config.ensure_runtime_dirs()?;
        let seen_cache_path = config.user_data_dir.join("seen-cache.json");

        let (recent_message_ids, recent_message_order) =
            load_seen_cache(&seen_cache_path).unwrap_or_default();
        Ok(Self {
            config,
            session_state: MailSessionState::Unauthenticated,
            current_notification: None,
            recent_message_ids,
            recent_message_order,
            seen_cache_path,
        })
    }

    pub fn set_session_state(&mut self, state: MailSessionState) {
        self.session_state = state;
    }

    pub fn register_candidate(&mut self, email: &OtpCandidateEmail) -> Option<OtpNotification> {
        if self.recent_message_ids.contains(&email.message_id) {
            return None;
        }

        let otp = detect_otp(email)?;
        let notification = OtpNotification::new(
            otp.source_label,
            otp.code,
            OffsetDateTime::now_utc(),
            self.config.notification_duration_seconds,
        );

        self.track_message(email.message_id.clone());
        self.current_notification = Some(notification.clone());
        Some(notification)
    }

    pub fn clear_notification(&mut self) {
        self.current_notification = None;
    }

    pub fn save_config(&self) -> Result<()> {
        self.config.save()
    }

    pub fn latest_notification_code(&self) -> Option<&str> {
        self.current_notification
            .as_ref()
            .map(|notification| notification.raw_code.as_str())
    }

    fn track_message(&mut self, message_id: String) {
        if self.recent_message_ids.insert(message_id.clone()) {
            self.recent_message_order.push_back(message_id);
        }

        while self.recent_message_order.len() > RECENT_CACHE_LIMIT {
            if let Some(stale) = self.recent_message_order.pop_front() {
                self.recent_message_ids.remove(&stale);
            }
        }

        if let Err(error) = self.persist_seen_cache() {
            warn!(?error, "failed to persist seen cache");
        }
    }

    fn persist_seen_cache(&self) -> Result<()> {
        let path = &self.seen_cache_path;
        ensure_parent_dir(&path)?;
        let ids: Vec<_> = self.recent_message_order.iter().cloned().collect();
        let payload = serde_json::to_vec_pretty(&ids).context("failed to serialize seen cache")?;
        fs::write(&path, payload)
            .with_context(|| format!("failed to write seen cache to {}", path.display()))
    }
}

fn load_seen_cache(path: &PathBuf) -> Result<(HashSet<String>, VecDeque<String>)> {
    if !path.exists() {
        return Ok(Default::default());
    }

    let bytes = fs::read(&path)
        .with_context(|| format!("failed to read seen cache from {}", path.display()))?;
    let ids = serde_json::from_slice::<Vec<String>>(&bytes)
        .with_context(|| format!("failed to parse seen cache from {}", path.display()))?;
    let set = ids.iter().cloned().collect();
    Ok((set, ids.into_iter().collect()))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::AppState;
    use crate::config::AppConfig;
    use crate::models::OtpCandidateEmail;
    use time::OffsetDateTime;

    fn test_config() -> AppConfig {
        let mut config = AppConfig::default();
        let unique = format!(
            "protoncode-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("current system time is valid")
                .as_nanos()
        );
        config.user_data_dir = PathBuf::from(std::env::temp_dir()).join(unique);
        config
    }

    #[test]
    fn ignores_duplicate_message_ids() {
        let mut state = AppState::from_config(test_config()).unwrap();
        let email = OtpCandidateEmail {
            message_id: "same-message".to_owned(),
            sender: Some("Example".to_owned()),
            subject: Some("Code".to_owned()),
            received_at: OffsetDateTime::UNIX_EPOCH,
            body_text: "Your verification code is 555111.".to_owned(),
        };

        assert!(state.register_candidate(&email).is_some());
        assert!(state.register_candidate(&email).is_none());
    }
}
