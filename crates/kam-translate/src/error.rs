//! Client-facing error normalization. Never expose upstream account-ban triggers.

use serde_json::{json, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorFormat {
    Claude,
    OpenAi,
}

// Claude Code's client-side risk system treats these upstream phrases as an
// account-ban signal. They must never reach a client, even when they appear in
// an otherwise useful quota/context error. Keep this list synchronized with
// `src/main/proxy/proxy-utils.ts` in Kiro-account-manager.
const BAN_TRIGGER_PHRASES: &[&str] = &[
    "this organization has been disabled",
    "oauth authentication is currently not allowed",
    "account disabled",
    "account is disabled",
    "invalid account",
    "too many active sessions",
];

pub fn sanitize_error_message(message: &str) -> String {
    let lower = message.to_ascii_lowercase();
    // This is deliberately the first and unconditional classification. Moving
    // it below quota/auth/context handling can leak a CRS trigger through a
    // more specific branch.
    if BAN_TRIGGER_PHRASES
        .iter()
        .any(|phrase| lower.contains(phrase))
    {
        return "service unavailable".into();
    }
    if lower.contains("prompt is too long") || lower.contains("context length exceeded") {
        return "prompt is too long: context length exceeded".into();
    }
    if is_quota_error(&lower) {
        if lower.contains("throttlingexception") {
            let suffix = message
                .split_once("ThrottlingException")
                .map(|(_, rest)| rest)
                .unwrap_or("");
            return redact_endpoint_names(&format!("ThrottlingException{suffix}"));
        }
        return redact_endpoint_names(message);
    }
    if lower.contains("401") || lower.contains("403") || lower.contains("auth") {
        return "Service temporarily unavailable, please retry".into();
    }
    if lower.contains("internal server error")
        || lower.contains("bad gateway")
        || lower.contains("502")
        || lower.contains("500")
        || lower.contains("503")
        || lower.contains("504")
        || lower.contains("service unavailable")
        || lower.contains("gateway timeout")
    {
        return "Upstream service error, please retry later".into();
    }
    if is_network_error(&lower) {
        return "Service temporarily unavailable, please retry".into();
    }
    redact_endpoint_names(message)
}

fn is_quota_error(lower: &str) -> bool {
    [
        "402",
        "429",
        "quota",
        "credit",
        "throttlingexception",
        "reached the limit",
        "payment required",
    ]
    .iter()
    .any(|word| lower.contains(word))
}

fn is_network_error(lower: &str) -> bool {
    [
        "network",
        "fetch failed",
        "socket",
        "econnreset",
        "econnrefused",
        "etimedout",
        "timeout",
        "connection reset",
        "premature close",
        "other side closed",
        "terminated",
        "operation was aborted",
    ]
    .iter()
    .any(|word| lower.contains(word))
}

fn redact_endpoint_names(message: &str) -> String {
    let filtered = redact_domain_token(message, "codewhisperer.us-east-1.amazonaws.com");
    let filtered = redact_domain_token(&filtered, "q.us-east-1.amazonaws.com");
    let filtered = replace_ascii_case_insensitive(&filtered, "CodeWhisperer", "endpoint");
    replace_ascii_case_insensitive(&filtered, "AmazonQ", "endpoint")
}

fn redact_domain_token(message: &str, domain: &str) -> String {
    let lower = message.to_ascii_lowercase();
    let mut output = String::with_capacity(message.len());
    let mut cursor = 0;
    while let Some(relative) = lower[cursor..].find(domain) {
        let start = cursor + relative;
        output.push_str(&message[cursor..start]);
        output.push_str("upstream");
        cursor = start + domain.len();
        while cursor < message.len() {
            let character = message[cursor..].chars().next().expect("character");
            if character.is_whitespace() {
                break;
            }
            cursor += character.len_utf8();
        }
    }
    output.push_str(&message[cursor..]);
    output
}

fn replace_ascii_case_insensitive(message: &str, needle: &str, replacement: &str) -> String {
    let lower = message.to_ascii_lowercase();
    let needle = needle.to_ascii_lowercase();
    let mut output = String::with_capacity(message.len());
    let mut cursor = 0;
    while let Some(relative) = lower[cursor..].find(&needle) {
        let start = cursor + relative;
        output.push_str(&message[cursor..start]);
        output.push_str(replacement);
        cursor = start + needle.len();
    }
    output.push_str(&message[cursor..]);
    output
}

pub fn error_envelope(format: ErrorFormat, status: u16, message: &str) -> Value {
    let safe = sanitize_error_message(message);
    match format {
        ErrorFormat::Claude => json!({
            "type": "error",
            "error": {"type": claude_error_type(status), "message": safe}
        }),
        ErrorFormat::OpenAi => json!({
            "error": {"message": safe, "type": openai_error_type(status), "code": Value::Null}
        }),
    }
}

fn claude_error_type(status: u16) -> &'static str {
    match status {
        400 | 405 | 413 | 422 => "invalid_request_error",
        401 | 403 => "authentication_error",
        404 => "not_found_error",
        429 => "rate_limit_error",
        _ => "overloaded_error",
    }
}

fn openai_error_type(status: u16) -> &'static str {
    match status {
        400 | 405 | 413 | 422 => "invalid_request_error",
        401 | 403 => "authentication_error",
        404 => "not_found_error",
        429 => "rate_limit_error",
        _ => "server_error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ports_quota_cases_from_typescript() {
        assert_eq!(
            sanitize_error_message("429 Quota exhausted on CodeWhisperer"),
            "429 Quota exhausted on endpoint"
        );
        assert_eq!(
            sanitize_error_message("429 on CodeWhisperer: ThrottlingException: rate exceeded"),
            "ThrottlingException: rate exceeded"
        );
    }

    #[test]
    fn ports_auth_cases_from_typescript() {
        for text in ["401 Unauthorized", "Auth error 403"] {
            assert_eq!(
                sanitize_error_message(text),
                "Service temporarily unavailable, please retry"
            );
        }
    }

    #[test]
    fn ports_server_cases_from_typescript() {
        for text in [
            "500 Internal Server Error",
            "Internal Server Error",
            "502 Bad Gateway",
        ] {
            assert_eq!(
                sanitize_error_message(text),
                "Upstream service error, please retry later"
            );
        }
    }

    #[test]
    fn ports_network_cases_from_typescript() {
        for text in [
            "Network connection failed",
            "fetch failed",
            "SocketError: other side closed",
            "ECONNRESET",
        ] {
            assert_eq!(
                sanitize_error_message(text),
                "Service temporarily unavailable, please retry"
            );
        }
    }

    #[test]
    fn ports_context_length_case_from_typescript() {
        assert_eq!(
            sanitize_error_message(
                "API error 400: Service temporarily unavailable: prompt is too long; context length exceeded"
            ),
            "prompt is too long: context length exceeded"
        );
    }

    #[test]
    fn ports_every_crs_blocking_case_from_typescript() {
        for text in [
            "Error: this organization has been disabled by admin",
            "OAuth authentication is currently not allowed",
            "API error 400: account disabled",
            "API error 400: account is disabled",
            "API error 400: invalid account",
            "API error 400: too many active sessions",
            "429 quota exhausted because account is disabled",
        ] {
            assert_eq!(sanitize_error_message(text), "service unavailable");
        }
    }

    #[test]
    fn ports_endpoint_redaction_cases_from_typescript() {
        assert_eq!(
            sanitize_error_message("CodeWhisperer request failed"),
            "endpoint request failed"
        );
        assert_eq!(
            sanitize_error_message("AmazonQ request failed"),
            "endpoint request failed"
        );
        assert_eq!(
            sanitize_error_message(
                "request to codewhisperer.us-east-1.amazonaws.com/generateAssistantResponse failed"
            ),
            "request to upstream failed"
        );
        assert_eq!(
            sanitize_error_message("request to q.us-east-1.amazonaws.com/private?token=x failed"),
            "request to upstream failed"
        );
        assert_eq!(
            sanitize_error_message("AMAZONQ and CODEWHISPERER failed"),
            "endpoint and endpoint failed"
        );
    }
}
