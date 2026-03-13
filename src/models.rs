use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MailSessionState {
    Unauthenticated,
    Restoring,
    Authenticated,
    Expired,
    Error,
    Paused,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OtpCandidateEmail {
    pub message_id: String,
    pub sender: Option<String>,
    pub subject: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub received_at: OffsetDateTime,
    pub body_text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OtpNotification {
    pub source_label: String,
    pub raw_code: String,
    pub masked_code: String,
    pub received_at: OffsetDateTime,
    pub expires_at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OtpMatch {
    pub code: String,
    pub source_label: String,
}

impl OtpNotification {
    pub fn new(
        source_label: String,
        raw_code: String,
        received_at: OffsetDateTime,
        duration_seconds: u64,
    ) -> Self {
        let masked_code = "*".repeat(raw_code.chars().count().max(4));
        let expires_at = received_at + time::Duration::seconds(duration_seconds as i64);

        Self {
            source_label,
            raw_code,
            masked_code,
            received_at,
            expires_at,
        }
    }
}
