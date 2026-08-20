use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::CURRENT_HIGH_DEMAND_PHRASE;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TurnError {
    pub message: String,
    #[serde(default)]
    pub additional_details: Option<String>,
    #[serde(default)]
    pub codex_error_info: Option<Value>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompletedStatus {
    Completed,
    Interrupted,
    Failed,
    InProgress,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TurnDecision {
    Success,
    Retryable(String),
    RetryImmediately(String),
    RetryImmediatelyQuiet(String),
    WaitForInternalRetry(String),
    WaitForInternalRetryQuiet(String),
    Pause(String),
}

pub const CODEX_INTERNAL_RETRY_LIMIT: u8 = 5;

pub fn classify_error_notification(
    error: &TurnError,
    will_retry: bool,
    failure_phrases: &[String],
) -> TurnDecision {
    if is_fatal_error(error) {
        return TurnDecision::Pause(redact_error_reason(error));
    }
    if will_retry {
        if contains_current_high_demand_notice(&error_text(error)) {
            return TurnDecision::WaitForInternalRetryQuiet(redact_error_reason(error));
        }
        return TurnDecision::WaitForInternalRetry(redact_error_reason(error));
    }
    if codex_error_kind(error.codex_error_info.as_ref()).as_deref()
        == Some("responsetoomanyfailedattempts")
    {
        return TurnDecision::RetryImmediately(format!(
            "Codex 内部 {CODEX_INTERNAL_RETRY_LIMIT} 次重试已耗尽"
        ));
    }
    classify_error(error, failure_phrases)
}

pub fn classify_turn_completion(
    status: CompletedStatus,
    final_reply: &str,
    error: Option<&TurnError>,
    failure_phrases: &[String],
    previous_empty_replies: u8,
    max_empty_replies: u8,
) -> TurnDecision {
    match status {
        CompletedStatus::InProgress => {
            TurnDecision::WaitForInternalRetry("Codex 仍在处理当前任务".into())
        }
        CompletedStatus::Interrupted => TurnDecision::Retryable("当前任务已中断".into()),
        CompletedStatus::Failed => error.map_or_else(
            || TurnDecision::Pause("任务失败，但 Codex 未提供错误信息".into()),
            |error| classify_error(error, failure_phrases),
        ),
        CompletedStatus::Completed => {
            if final_reply.trim().is_empty() {
                if previous_empty_replies < max_empty_replies {
                    TurnDecision::Retryable(format!(
                        "Codex 返回空回复（{}/{max_empty_replies}）",
                        previous_empty_replies + 1
                    ))
                } else {
                    TurnDecision::Pause(format!(
                        "连续收到 {max_empty_replies} 次空回复，请人工检查"
                    ))
                }
            } else if starts_with_current_high_demand_notice(final_reply) {
                TurnDecision::RetryImmediatelyQuiet("检测到 Codex 高需求提示；立即重试".into())
            } else if starts_with_failure_phrase(final_reply, failure_phrases) {
                TurnDecision::Retryable("检测到 Codex 繁忙提示".into())
            } else {
                TurnDecision::Success
            }
        }
    }
}

fn classify_error(error: &TurnError, failure_phrases: &[String]) -> TurnDecision {
    let kind = codex_error_kind(error.codex_error_info.as_ref());
    let normalized = normalize_for_match(&format!(
        "{} {}",
        error.message,
        error.additional_details.as_deref().unwrap_or_default()
    ));
    let http_status = error
        .codex_error_info
        .as_ref()
        .and_then(find_http_status_code)
        .or_else(|| find_http_status_in_text(&normalized));

    // Authentication, quota and policy failures always win over a coincident
    // HTTP 429 or a broad user phrase. Retrying these forever would be both
    // noisy and misleading.
    if is_fatal_error_with_normalized(kind.as_deref(), &normalized) {
        return TurnDecision::Pause(redact_error_reason(error));
    }

    if contains_current_high_demand_notice(&normalized) {
        return TurnDecision::RetryImmediatelyQuiet("检测到 Codex 高需求提示；立即重试".into());
    }

    if matches!(http_status, Some(429 | 502 | 503 | 504)) {
        return TurnDecision::Retryable(format!("临时 HTTP 状态码 {}", http_status.unwrap()));
    }

    if matches!(
        kind.as_deref(),
        Some("serveroverloaded" | "internalservererror")
    ) || normalized.contains("server overloaded")
        || error_contains_failure_phrase(&normalized, failure_phrases)
    {
        return TurnDecision::Retryable(redact_error_reason(error));
    }

    if matches!(
        kind.as_deref(),
        Some(
            "httpconnectionfailed"
                | "responsestreamconnectionfailed"
                | "responsestreamdisconnected"
                | "responsetoomanyfailedattempts"
        )
    ) || contains_retryable_transport_text(&normalized)
    {
        return TurnDecision::Retryable(redact_error_reason(error));
    }

    TurnDecision::Pause(redact_error_reason(error))
}

fn error_text(error: &TurnError) -> String {
    format!(
        "{} {}",
        error.message,
        error.additional_details.as_deref().unwrap_or_default()
    )
}

fn is_fatal_error(error: &TurnError) -> bool {
    let kind = codex_error_kind(error.codex_error_info.as_ref());
    let normalized = normalize_for_match(&error_text(error));
    is_fatal_error_with_normalized(kind.as_deref(), &normalized)
}

fn is_fatal_error_with_normalized(kind: Option<&str>, normalized: &str) -> bool {
    matches!(
        kind,
        Some(
            "unauthorized"
                | "usagelimitexceeded"
                | "sessionbudgetexceeded"
                | "cyberpolicy"
                | "badrequest"
                | "threadrollbackfailed"
                | "sandboxerror"
                | "contextwindowexceeded"
        )
    ) || contains_fatal_text(normalized)
}

fn contains_current_high_demand_notice(input: &str) -> bool {
    let normalized = normalize_for_match(input);
    normalized.contains(&normalize_for_match(CURRENT_HIGH_DEMAND_PHRASE))
}

fn starts_with_current_high_demand_notice(input: &str) -> bool {
    let normalized = normalize_for_match(input);
    normalized.starts_with(&normalize_for_match(CURRENT_HIGH_DEMAND_PHRASE))
}

fn contains_retryable_transport_text(normalized: &str) -> bool {
    [
        "stream disconnected before completion",
        "error sending request for url",
        "response stream disconnected",
        "connection closed before message completed",
        "connection reset by peer",
        "connection prematurely closed",
    ]
    .iter()
    .any(|phrase| normalized.contains(phrase))
}

fn contains_fatal_text(normalized: &str) -> bool {
    [
        "unauthorized",
        "not logged in",
        "authentication required",
        "quota exhausted",
        "usage limit exceeded",
        "insufficient credits",
        "invalid configuration",
        "policy refusal",
        "policy rejected",
    ]
    .iter()
    .any(|phrase| normalized.contains(phrase))
}

fn redact_error_reason(error: &TurnError) -> String {
    let summary = error.message.trim();
    if summary.is_empty() {
        "Codex 返回了未说明原因的错误".into()
    } else {
        summary.chars().take(240).collect()
    }
}

#[must_use]
pub fn normalize_for_match(input: &str) -> String {
    input
        .trim_start_matches(['\u{feff}', '\u{200b}'])
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

#[must_use]
pub fn starts_with_failure_phrase(reply: &str, failure_phrases: &[String]) -> bool {
    let reply = normalize_for_match(reply);
    failure_phrases.iter().any(|phrase| {
        let phrase = normalize_for_match(phrase);
        !phrase.is_empty() && reply.starts_with(&phrase)
    })
}

fn error_contains_failure_phrase(normalized_error: &str, failure_phrases: &[String]) -> bool {
    failure_phrases.iter().any(|phrase| {
        let phrase = normalize_for_match(phrase);
        !phrase.is_empty() && normalized_error.contains(&phrase)
    })
}

fn codex_error_kind(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(kind) => Some(normalize_identifier(kind)),
        Value::Object(object) => object.keys().next().map(|kind| normalize_identifier(kind)),
        _ => None,
    }
}

fn normalize_identifier(input: &str) -> String {
    input
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn find_http_status_code(value: &Value) -> Option<u16> {
    match value {
        Value::Object(object) => {
            if let Some(status) = object.get("httpStatusCode").and_then(Value::as_u64) {
                return u16::try_from(status).ok();
            }
            object.values().find_map(find_http_status_code)
        }
        Value::Array(values) => values.iter().find_map(find_http_status_code),
        _ => None,
    }
}

fn find_http_status_in_text(normalized: &str) -> Option<u16> {
    [429_u16, 502, 503, 504].into_iter().find(|status| {
        let status = status.to_string();
        [
            format!("http {status}"),
            format!("http status {status}"),
            format!("status {status}"),
            format!("status code {status}"),
        ]
        .iter()
        .any(|needle| normalized.contains(needle))
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn phrases() -> Vec<String> {
        vec!["Server overloaded; retry later.".into()]
    }

    #[test]
    fn phrase_matching_normalizes_only_the_reply_prefix() {
        assert!(starts_with_failure_phrase(
            "\u{feff}  SERVER   overloaded; retry later. Details follow.",
            &phrases()
        ));
        assert!(!starts_with_failure_phrase(
            "A normal answer mentioning Server overloaded; retry later.",
            &phrases()
        ));
    }

    #[test]
    fn repeated_current_high_demand_notice_is_retryable() {
        let phrase = "We're currently experiencing high demand, which may cause temporary errors.";
        let repeated = format!("{phrase}{phrase}");

        assert!(starts_with_failure_phrase(&repeated, &[phrase.into()]));
    }

    #[test]
    fn current_high_demand_notice_retries_quietly_after_five_attempts() {
        let phrase = "We're currently experiencing high demand, which may cause temporary errors.";

        assert!(matches!(
            classify_turn_completion(
                CompletedStatus::Completed,
                phrase,
                None,
                &[phrase.into()],
                5,
                5,
            ),
            TurnDecision::RetryImmediatelyQuiet(_)
        ));
    }

    #[test]
    fn current_high_demand_error_notification_is_quiet_whether_codex_retries() {
        let error = TurnError {
            message: CURRENT_HIGH_DEMAND_PHRASE.into(),
            ..TurnError::default()
        };
        assert!(matches!(
            classify_error_notification(&error, true, &phrases()),
            TurnDecision::WaitForInternalRetryQuiet(_)
        ));
        assert!(matches!(
            classify_error_notification(&error, false, &phrases()),
            TurnDecision::RetryImmediatelyQuiet(_)
        ));
    }

    #[test]
    fn fatal_error_still_wins_over_current_high_demand_notice() {
        let error = TurnError {
            message: format!("{CURRENT_HIGH_DEMAND_PHRASE} unauthorized"),
            codex_error_info: Some(json!("unauthorized")),
            ..TurnError::default()
        };
        assert!(matches!(
            classify_error_notification(&error, false, &phrases()),
            TurnDecision::Pause(_)
        ));
        assert!(matches!(
            classify_error_notification(&error, true, &phrases()),
            TurnDecision::Pause(_)
        ));
    }

    #[test]
    fn internal_retry_does_not_schedule_an_extra_turn() {
        let error = TurnError {
            message: "temporary disconnect".into(),
            codex_error_info: Some(json!({
                "responseStreamDisconnected": { "httpStatusCode": 503 }
            })),
            ..TurnError::default()
        };
        assert!(matches!(
            classify_error_notification(&error, true, &phrases()),
            TurnDecision::WaitForInternalRetry(_)
        ));
    }

    #[test]
    fn exhausted_codex_internal_retries_start_a_new_external_retry() {
        let error = TurnError {
            message: "response retry limit reached".into(),
            codex_error_info: Some(json!({
                "responseTooManyFailedAttempts": { "httpStatusCode": 503 }
            })),
            ..TurnError::default()
        };

        assert!(matches!(
            classify_error_notification(&error, false, &phrases()),
            TurnDecision::RetryImmediately(_)
        ));
    }

    #[test]
    fn transient_and_fatal_errors_are_separated() {
        let overloaded = TurnError {
            message: "busy".into(),
            codex_error_info: Some(json!("serverOverloaded")),
            ..TurnError::default()
        };
        assert!(matches!(
            classify_error_notification(&overloaded, false, &phrases()),
            TurnDecision::Retryable(_)
        ));

        let unauthorized = TurnError {
            message: "please sign in".into(),
            codex_error_info: Some(json!("unauthorized")),
            ..TurnError::default()
        };
        assert!(matches!(
            classify_error_notification(&unauthorized, false, &phrases()),
            TurnDecision::Pause(_)
        ));

        let textual_gateway_error = TurnError {
            message: "request failed with HTTP status 503 Service Unavailable".into(),
            ..TurnError::default()
        };
        assert!(matches!(
            classify_error_notification(&textual_gateway_error, false, &phrases()),
            TurnDecision::Retryable(_)
        ));
    }

    #[test]
    fn fatal_quota_error_wins_over_http_429_and_user_phrases() {
        let error = TurnError {
            message: "usage limit exceeded; Server overloaded; retry later.".into(),
            codex_error_info: Some(json!({
                "usageLimitExceeded": { "httpStatusCode": 429 }
            })),
            ..TurnError::default()
        };

        assert!(matches!(
            classify_error_notification(&error, false, &phrases()),
            TurnDecision::Pause(_)
        ));
    }

    #[test]
    fn textual_response_stream_disconnect_keeps_retrying() {
        let disconnected = TurnError {
            message: "stream disconnected before completion: error sending request for url (https://anyrouter.top/v1/responses)".into(),
            ..TurnError::default()
        };

        assert!(matches!(
            classify_error_notification(&disconnected, false, &phrases()),
            TurnDecision::Retryable(reason)
                if reason.contains("stream disconnected before completion")
        ));
    }

    #[test]
    fn empty_reply_pauses_after_configured_limit() {
        assert!(matches!(
            classify_turn_completion(CompletedStatus::Completed, "", None, &phrases(), 4, 5),
            TurnDecision::Retryable(_)
        ));
        assert!(matches!(
            classify_turn_completion(CompletedStatus::Completed, "", None, &phrases(), 5, 5),
            TurnDecision::Pause(_)
        ));
    }
}
