use regex::Regex;

use crate::models::{OtpCandidateEmail, OtpMatch};

const OTP_CONTEXT_TERMS: &[&str] = &[
    "code",
    "verification",
    "2fa",
    "two-factor",
    "two factor",
    "one-time",
    "one time",
    "otp",
    "security",
    "signin",
    "sign in",
];

pub fn detect_otp(email: &OtpCandidateEmail) -> Option<OtpMatch> {
    let normalized = normalize_text(&email.body_text);
    let regex = Regex::new(r"\b([0-9]{4,8})\b").expect("valid OTP regex");

    let source_label = email
        .sender
        .clone()
        .or_else(|| email.subject.clone())
        .unwrap_or_else(|| "Proton Mail".to_owned());

    for captures in regex.captures_iter(&normalized) {
        let code = captures.get(1)?.as_str();
        let start = captures.get(0)?.start().saturating_sub(48);
        let end = (captures.get(0)?.end() + 48).min(normalized.len());
        let context = &normalized[start..end];

        if OTP_CONTEXT_TERMS.iter().any(|term| context.contains(term)) {
            return Some(OtpMatch {
                code: code.to_owned(),
                source_label,
            });
        }
    }

    None
}

fn normalize_text(input: &str) -> String {
    input
        .replace('\u{a0}', " ")
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .to_lowercase()
}

#[cfg(test)]
mod tests {
    use time::OffsetDateTime;

    use super::detect_otp;
    use crate::models::OtpCandidateEmail;

    fn email(body_text: &str) -> OtpCandidateEmail {
        OtpCandidateEmail {
            message_id: "msg-1".to_owned(),
            sender: Some("Example".to_owned()),
            subject: Some("Security code".to_owned()),
            received_at: OffsetDateTime::UNIX_EPOCH,
            body_text: body_text.to_owned(),
        }
    }

    #[test]
    fn extracts_code_with_context_term() {
        let result = detect_otp(&email(
            "Your verification code is 123456. It expires in 10 minutes.",
        ));
        assert_eq!(
            result.as_ref().map(|value| value.code.as_str()),
            Some("123456")
        );
    }

    #[test]
    fn ignores_plain_numbers_without_context() {
        let result = detect_otp(&email("Invoice 2024 was paid on 2026-03-13."));
        assert!(result.is_none());
    }

    #[test]
    fn supports_shorter_codes() {
        let result = detect_otp(&email("Use OTP 4812 to finish signing in."));
        assert_eq!(
            result.as_ref().map(|value| value.code.as_str()),
            Some("4812")
        );
    }
}
