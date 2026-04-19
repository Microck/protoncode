//! Data models shared across the ProtonCode application.
//!
//! Defines the core types for mail session state, OTP candidate emails,
//! parsed OTP notifications, and OTP matches.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// Current authentication / connectivity state of the Proton Mail webview session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MailSessionState {
    /// No authenticated session is present.
    Unauthenticated,
    /// A previously saved session marker is being restored.
    Restoring,
    /// The user is logged in and the mailbox is accessible.
    Authenticated,
    /// The session has expired and needs re-authentication.
    Expired,
    /// An error occurred while checking or restoring the session.
    Error,
    /// OTP monitoring has been paused by the user.
    Paused,
}

/// A raw email from Proton Mail that may contain an OTP code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OtpCandidateEmail {
    /// Unique Proton Mail message identifier.
    pub message_id: String,
    /// Sender display name, if available.
    pub sender: Option<String>,
    /// Email subject line, if available.
    pub subject: Option<String>,
    /// Timestamp when the email was received.
    #[serde(with = "time::serde::rfc3339")]
    pub received_at: OffsetDateTime,
    /// Plain-text body of the email.
    pub body_text: String,
}

/// A user-facing OTP notification with display metadata and expiration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OtpNotification {
    /// Human-readable label identifying the source (sender or subject).
    pub source_label: String,
    /// The raw OTP code string.
    pub raw_code: String,
    /// Masked representation of the code for display purposes.
    pub masked_code: String,
    /// When the notification was created.
    pub received_at: OffsetDateTime,
    /// When the notification should automatically expire.
    pub expires_at: OffsetDateTime,
}

/// A successfully extracted OTP code paired with its source label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OtpMatch {
    /// The extracted one-time password code.
    pub code: String,
    /// Human-readable label identifying the source of the code.
    pub source_label: String,
}

impl OtpNotification {
    /// Creates a new notification with an auto-generated masked code and computed expiration.
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
