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
    let normalized_subject = email
        .subject
        .as_deref()
        .map(normalize_text)
        .unwrap_or_default();
    let normalized_body = normalize_text(&email.body_text);
    let subject_has_context = has_context_term(&normalized_subject);

    let source_label = email
        .sender
        .clone()
        .or_else(|| email.subject.clone())
        .unwrap_or_else(|| "Proton Mail".to_owned());

    if let Some(code) = find_otp_code(&normalized_body, subject_has_context) {
        return Some(OtpMatch { code, source_label });
    }

    find_otp_code(&normalized_subject, false).map(|code| OtpMatch { code, source_label })
}

fn normalize_text(input: &str) -> String {
    input
        .replace('\u{a0}', " ")
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .to_lowercase()
}

fn find_otp_code(text: &str, allow_subject_context_fallback: bool) -> Option<String> {
    let regex = Regex::new(r"\b((?:[0-9][\s-]?){3,7}[0-9])\b").expect("valid OTP regex");
    let mut candidates = Vec::new();

    for captures in regex.captures_iter(text) {
        let raw_candidate = captures.get(1)?.as_str();
        if looks_like_date(raw_candidate) {
            continue;
        }

        let code: String = raw_candidate
            .chars()
            .filter(|ch| ch.is_ascii_digit())
            .collect();
        if !(4..=8).contains(&code.len()) {
            continue;
        }

        let start = captures.get(0)?.start().saturating_sub(72);
        let end = (captures.get(0)?.end() + 72).min(text.len());
        let context = &text[start..end];

        if has_context_term(context) {
            return Some(code);
        }

        candidates.push(code);
    }

    if allow_subject_context_fallback && candidates.len() == 1 && is_sparse_code_surface(text) {
        return candidates.into_iter().next();
    }

    None
}

fn has_context_term(text: &str) -> bool {
    OTP_CONTEXT_TERMS.iter().any(|term| text.contains(term))
}

fn looks_like_date(candidate: &str) -> bool {
    let trimmed = candidate.trim();
    let date_like = Regex::new(r"^\d{4}-\d{2}-\d{2}$").expect("valid date regex");
    date_like.is_match(trimmed)
}

fn is_sparse_code_surface(text: &str) -> bool {
    text.chars().all(|ch| {
        ch.is_ascii_digit()
            || ch.is_whitespace()
            || matches!(
                ch,
                '-' | ':' | '.' | ',' | '(' | ')' | '[' | ']' | '|' | '/'
            )
    })
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

    #[test]
    fn supports_grouped_digits() {
        let result = detect_otp(&email("Your verification code is 123 456."));
        assert_eq!(
            result.as_ref().map(|value| value.code.as_str()),
            Some("123456")
        );
    }

    #[test]
    fn detects_code_from_all_mail_row_surface() {
        let mut candidate = email("Acme\nVerification code\n123456\nExpires in 10 minutes");
        candidate.sender = Some("Acme".to_owned());
        candidate.subject = Some("Verification code".to_owned());

        let result = detect_otp(&candidate);
        assert_eq!(
            result.as_ref().map(|value| value.code.as_str()),
            Some("123456")
        );
    }

    #[test]
    fn uses_subject_context_when_body_contains_only_code() {
        let mut candidate = email("123456");
        candidate.subject = Some("Security code".to_owned());

        let result = detect_otp(&candidate);
        assert_eq!(
            result.as_ref().map(|value| value.code.as_str()),
            Some("123456")
        );
    }

    #[test]
    fn ignores_date_like_values_even_with_subject_context() {
        let mut candidate = email("2026-03-13");
        candidate.subject = Some("Verification code".to_owned());

        let result = detect_otp(&candidate);
        assert!(result.is_none());
    }

    #[test]
    fn detects_code_after_opened_message_fallback() {
        let mut candidate = email(
            "Acme security notice\nUse verification code 778899 to finish signing in.\nThis code expires in 10 minutes.",
        );
        candidate.subject = Some("Security notice".to_owned());

        let result = detect_otp(&candidate);
        assert_eq!(
            result.as_ref().map(|value| value.code.as_str()),
            Some("778899")
        );
    }
}
