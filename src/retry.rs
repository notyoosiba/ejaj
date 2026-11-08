//! The retry policy for one model turn.
//!
//! A turn is `send` plus `decode` (see [`crate::llm`]). A provider answers a
//! transient failure -- a dead connection or an overloaded endpoint -- without
//! the transcript being wrong, so the same request may simply be sent again.
//! Everything else (a rejected request, an unknown model, a body that does not
//! decode) will fail identically on every attempt, so it is reported at once.
//!
//! Not every transient failure is the same, though. A rate-limited account is
//! throttled and will answer again; an account whose quota or billing is
//! exhausted will not, so that marker is treated as permanent. A rate limit may
//! also name how long to wait (capped at thirty seconds), and that hint is
//! honored instead of the base backoff. The base backoff itself doubles from one
//! second and stops at ten seconds, so a long retry budget cannot turn into an
//! unbounded stall.
//!
//! The decision is pure and unit-tested: [`should_retry`] takes the failure and
//! the attempt that produced it and returns the backoff before the next attempt,
//! or `None` to give up. Sleeping and printing stay in the caller, which is what
//! keeps this module free of a clock, a network and a status line.

use crate::api::CallError;
use std::time::Duration;

/// How many attempts one turn gets, the first one included. Two retries are
/// enough to ride out a blip without turning a down endpoint into a long stall.
pub const MAX_ATTEMPTS: u32 = 3;

/// The account-level markers that mean the plan is exhausted for good: retrying
/// cannot pay a bill or refill an expired package.
const ACCOUNT_EXHAUSTION_MARKERS: [&str; 13] = [
    "insufficient_quota",
    "insufficient quota",
    "insufficient_balance",
    "insufficient balance",
    "billing_not_active",
    "credit balance is too low",
    "quota_exceeded",
    "quota exceeded",
    "budget exceeded",
    "usage limit",
    "limit exhausted",
    "package has expired",
    "plan does not yet include",
];

/// Whether a 429 or embedded 2xx message reports a permanently exhausted
/// account or billing state, which no retry can fix.
fn is_permanent_quota_error(message: &str) -> bool {
    let lowered = message.to_lowercase();
    // The per-minute token limit is a throttle, not an empty account, and it
    // contains the permanent `quota_exceeded` substring, so it is checked first.
    if lowered.contains("token_quota_exceeded") {
        return false;
    }
    ACCOUNT_EXHAUSTION_MARKERS
        .iter()
        .any(|marker| lowered.contains(marker))
}

/// The longest wait a rate-limit hint may ask for.
const MAX_RATE_LIMIT_BACKOFF_SECS: u64 = 30;

/// The message forms a 429 uses to name a wait, all matched case-insensitively.
const RETRY_HINT_MARKERS: [&str; 7] = [
    "retry-after:",
    "retry-after=",
    "retry_after:",
    "try again in ",
    "please retry after ",
    "retry after ",
    "\"retrydelay\":",
];

/// The first positive wait, in message order, that a 429 message names in one
/// of the hint forms, capped at [`MAX_RATE_LIMIT_BACKOFF_SECS`]. Absent,
/// non-numeric, zero and fractional candidates are skipped, so a malformed
/// marker never hides a valid one behind it; a longer digit run saturates
/// instead of overflowing.
fn extract_retry_hint_secs(message: &str) -> Option<u64> {
    let lowered = message.to_lowercase();
    let mut earliest: Option<(usize, u64)> = None;
    for marker in RETRY_HINT_MARKERS {
        let mut search_from = 0;
        while let Some(offset) = lowered[search_from..].find(marker) {
            let position = search_from + offset;
            let after_marker = position + marker.len();
            if let Some(secs) = positive_integer_hint(&lowered, after_marker) {
                let is_earliest = match earliest {
                    Some((earliest_position, _)) => position < earliest_position,
                    None => true,
                };
                if is_earliest {
                    earliest = Some((position, secs));
                }
                break;
            }
            search_from = after_marker;
        }
    }
    earliest.map(|(_, secs)| secs.min(MAX_RATE_LIMIT_BACKOFF_SECS))
}

/// The value after `start` when the candidate is a positive integer, once
/// surrounding whitespace and quotes are skipped. `None` when no digit follows,
/// when the number is zero, or when the digit run is only the integer part of a
/// fractional number such as `2.5`: a fractional wait is not a whole number of
/// seconds, so it is rejected rather than truncated.
fn positive_integer_hint(text: &str, start: usize) -> Option<u64> {
    let tail =
        text[start..].trim_start_matches(|c: char| c.is_whitespace() || c == '"' || c == '\'');
    let mut chars = tail.chars().peekable();
    let mut value: u64 = 0;
    let mut saw_digit = false;
    while let Some(c) = chars.next() {
        let Some(digit) = c.to_digit(10) else {
            let fractional =
                matches!(c, '.' | ',') && chars.peek().is_some_and(char::is_ascii_digit);
            return (!fractional && saw_digit && value > 0).then_some(value);
        };
        saw_digit = true;
        value = value.saturating_mul(10).saturating_add(u64::from(digit));
    }
    (saw_digit && value > 0).then_some(value)
}

/// The longest base wait between attempts.
const MAX_BACKOFF_SECS: u64 = 10;

/// The wait before attempt `attempt + 1`, doubling from one second and ceilinged
/// at [`MAX_BACKOFF_SECS`]: 1s, then 2s, then 4s, 8s and 10s for every attempt
/// beyond. The shift is clamped so no attempt number can overflow it.
fn backoff(attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(31);
    Duration::from_secs((1u64 << shift).min(MAX_BACKOFF_SECS))
}

/// Whether the failure from attempt number `attempt` (1-based: the first attempt
/// is 1) is worth another try, and how long to wait first. `None` means the
/// failure is final -- either it is permanent or the attempts are used up.
///
/// A transport error is transient by nature. An API error is transient when the
/// endpoint timed out, is overloaded, or is rate limiting: 408, 429, 500, 502,
/// 503, 504, Anthropic's 529, and the 2xx range (an embedded error inside an
/// otherwise successful body). Every other status is the request's own fault,
/// so it is not retried. A rate limit or embedded error whose message reports an
/// exhausted account -- a missing quota, an inactive billing account, a spent
/// package -- is permanent instead, unless the message names the per-minute
/// token limit, which is only a throttle. A 429 message may also name its wait;
/// the first positive hint in the message is capped at thirty seconds and is
/// never shorter than the base backoff.
/// A malformed body is normally a contract bug, but the one diagnostic that
/// says the response carried no usable content marks a thinking-only reply that
/// another attempt can turn into usable output; every other malformed detail is
/// final.
pub fn should_retry(error: &CallError, attempt: u32) -> Option<Duration> {
    if attempt >= MAX_ATTEMPTS {
        return None;
    }
    match error {
        CallError::Transport(_) => Some(backoff(attempt)),
        CallError::Api {
            status: 200..=299,
            message,
        } => (!is_permanent_quota_error(message)).then(|| backoff(attempt)),
        CallError::Api {
            status: 429,
            message,
        } => {
            if is_permanent_quota_error(message) {
                return None;
            }
            let base = backoff(attempt);
            Some(match extract_retry_hint_secs(message) {
                Some(secs) => Duration::from_secs(secs).max(base),
                None => base,
            })
        }
        CallError::Api {
            status: 408 | 500 | 502 | 503 | 504 | 529,
            ..
        } => Some(backoff(attempt)),
        CallError::Malformed(detail)
            if detail.starts_with("the response carried no usable content") =>
        {
            Some(backoff(attempt))
        }
        CallError::Api { .. } | CallError::Malformed(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transport() -> CallError {
        CallError::Transport("connection reset".to_string())
    }

    fn api(status: i32) -> CallError {
        CallError::Api {
            status,
            message: "nope".to_string(),
        }
    }

    fn api_message(status: i32, message: &str) -> CallError {
        CallError::Api {
            status,
            message: message.to_string(),
        }
    }

    fn malformed(detail: &str) -> CallError {
        CallError::Malformed(detail.to_string())
    }

    /// Every API status the policy treats as transient.
    const RETRYABLE_API_STATUSES: [i32; 9] = [200, 299, 408, 429, 500, 502, 503, 504, 529];

    /// The exact diagnostic prefix an unusable-content response carries.
    const NO_USABLE_CONTENT: &str = "the response carried no usable content";

    /// Core entry-point behavior: every retryable API failure waits one second
    /// before the second attempt, two seconds before the third, and gives up on
    /// the third and final attempt.
    #[test]
    fn when_retrying_given_retryable_api_errors_then_returns_exact_delays() {
        for status in RETRYABLE_API_STATUSES {
            assert_eq!(
                should_retry(&api(status), 1),
                Some(Duration::from_secs(1)),
                "status {status} must wait one second"
            );
            assert_eq!(
                should_retry(&api(status), 2),
                Some(Duration::from_secs(2)),
                "status {status} must wait two seconds"
            );
            assert_eq!(
                should_retry(&api(status), 3),
                None,
                "status {status} must not get a fourth attempt"
            );
        }
    }

    /// Edge-case table: the allowlist is exact. A request the provider
    /// understood and refused, a non-2xx informational or redirect status, and
    /// status zero are never retried, and the inclusive 2xx ends 200 and 299
    /// are the only successful-status codes that qualify.
    #[test]
    fn when_retrying_given_api_status_boundaries_then_matches_exact_allowlist() {
        for status in [400, 401, 403, 404, 422, 199, 300, 0] {
            assert!(
                should_retry(&api(status), 1).is_none(),
                "status {status} must not be retried"
            );
        }
    }

    /// Edge-case table: only the exact unusable-content diagnostic prefix is
    /// retried. Its bare and metadata-suffixed forms qualify; unrelated
    /// malformed details, the empty detail, a case mismatch, and a leading
    /// whitespace variant do not.
    #[test]
    fn when_retrying_given_malformed_detail_variants_then_matches_exact_prefix() {
        for detail in [
            NO_USABLE_CONTENT,
            "the response carried no usable content (content_types=[])",
            r#"the response carried no usable content (stop_reason="end_turn"; content_types=["thinking"])"#,
        ] {
            assert_eq!(
                should_retry(&malformed(detail), 1),
                Some(Duration::from_secs(1)),
                "detail {detail:?} must wait one second"
            );
            assert_eq!(
                should_retry(&malformed(detail), 2),
                Some(Duration::from_secs(2)),
                "detail {detail:?} must wait two seconds"
            );
            assert_eq!(
                should_retry(&malformed(detail), 3),
                None,
                "detail {detail:?} must not get a fourth attempt"
            );
        }

        for detail in [
            "bad json",
            "the response content was not an array",
            "",
            "The response carried no usable content",
            " the response carried no usable content",
        ] {
            assert!(
                should_retry(&malformed(detail), 1).is_none(),
                "detail {detail:?} must not be retried"
            );
        }
    }

    /// Core entry-point behavior: a timeout and every other transient failure --
    /// a dead transport, an embedded error in a 2xx body, a rate limit, an
    /// overload status, or an unusable-content body -- waits one second before
    /// the second attempt, two seconds before the third, and gives up on the
    /// third and final attempt.
    #[test]
    fn when_retrying_given_timeout_and_transient_failures_then_preserves_attempt_limits() {
        for status in [408, 429, 500, 502, 503, 504, 529] {
            assert_eq!(
                should_retry(&api(status), 1),
                Some(Duration::from_secs(1)),
                "status {status} must wait one second"
            );
            assert_eq!(
                should_retry(&api(status), 2),
                Some(Duration::from_secs(2)),
                "status {status} must wait two seconds"
            );
            assert_eq!(
                should_retry(&api(status), 3),
                None,
                "status {status} must not get a fourth attempt"
            );
        }
        for status in [200, 299] {
            assert_eq!(
                should_retry(&api(status), 1),
                Some(Duration::from_secs(1)),
                "embedded status {status} must wait one second"
            );
            assert_eq!(
                should_retry(&api(status), 2),
                Some(Duration::from_secs(2)),
                "embedded status {status} must wait two seconds"
            );
            assert_eq!(
                should_retry(&api(status), 3),
                None,
                "embedded status {status} must not get a fourth attempt"
            );
        }
        for error in [
            transport(),
            malformed("the response carried no usable content"),
            malformed("the response carried no usable content (content_types=[])"),
        ] {
            assert_eq!(
                should_retry(&error, 1),
                Some(Duration::from_secs(1)),
                "{error} must wait one second"
            );
            assert_eq!(
                should_retry(&error, 2),
                Some(Duration::from_secs(2)),
                "{error} must wait two seconds"
            );
            assert_eq!(
                should_retry(&error, 3),
                None,
                "{error} must not get a fourth attempt"
            );
        }
    }

    /// The account-level markers that mean the plan is exhausted for good: no
    /// amount of retrying can pay the bill.
    const PERMANENT_QUOTA_MARKERS: [&str; 13] = [
        "insufficient_quota",
        "insufficient quota",
        "insufficient_balance",
        "insufficient balance",
        "billing_not_active",
        "credit balance is too low",
        "quota_exceeded",
        "quota exceeded",
        "budget exceeded",
        "usage limit",
        "limit exhausted",
        "package has expired",
        "plan does not yet include",
    ];

    /// The per-minute token limit marker: it is a throttle, not an empty
    /// account, and it wins over a permanent marker in the same message.
    const TOKEN_QUOTA_MARKER: &str = "token_quota_exceeded";

    /// Core entry-point behavior: a quota or billing marker on a rate-limited or
    /// embedded error means the account is exhausted, so the turn is not
    /// retried; the per-minute token limit keeps retrying and wins over any
    /// permanent marker in the same message. Markers never change the
    /// classification of a status that is not 429 or 2xx, and never bypass the
    /// final-attempt limit.
    #[test]
    fn when_retrying_given_permanent_account_exhaustion_then_stops_before_retrying() {
        for marker in PERMANENT_QUOTA_MARKERS {
            for message in [
                marker.to_string(),
                marker.to_uppercase(),
                format!("Error: {}", marker.to_ascii_uppercase()),
                format!("the request failed: {}", marker),
            ] {
                for status in [429, 200, 299] {
                    let error = api_message(status, &message);
                    assert_eq!(
                        should_retry(&error, 1),
                        None,
                        "status {status} with quota message {message:?} must not be retried"
                    );
                }
            }
        }

        for message in [
            TOKEN_QUOTA_MARKER.to_string(),
            "token_quota_exceeded: insufficient_quota".to_string(),
            "insufficient_quota then token_quota_exceeded".to_string(),
            "TOKEN_QUOTA_EXCEEDED / quota exceeded / usage limit".to_string(),
        ] {
            for status in [429, 200, 299] {
                assert_eq!(
                    should_retry(&api_message(status, &message), 1),
                    Some(Duration::from_secs(1)),
                    "status {status} with throttling message {message:?} must be retried"
                );
            }
        }

        let exhausted = api_message(429, "insufficient_quota");
        assert_eq!(
            should_retry(&exhausted, MAX_ATTEMPTS),
            None,
            "the final-attempt limit still applies to a permanent quota error"
        );
        assert_eq!(
            should_retry(&api_message(429, "insufficient_quota; retry-after: 20"), 1),
            None,
            "a permanent quota error stays final even when the message names a wait"
        );
        assert_eq!(
            should_retry(&CallError::Transport("insufficient_quota".to_string()), 1),
            Some(Duration::from_secs(1)),
            "a quota marker cannot change a transport failure"
        );
        assert_eq!(
            should_retry(
                &malformed("the response carried no usable content: insufficient_quota"),
                1
            ),
            Some(Duration::from_secs(1)),
            "a quota marker cannot change an unusable-content body"
        );
        assert_eq!(
            should_retry(&malformed("insufficient_quota"), 1),
            None,
            "a quota marker cannot make an unrelated malformed body retryable"
        );
        assert_eq!(
            should_retry(&api_message(429, TOKEN_QUOTA_MARKER), MAX_ATTEMPTS),
            None
        );
        assert_eq!(
            should_retry(&api_message(500, "insufficient_quota"), 1),
            Some(Duration::from_secs(1)),
            "a quota marker must not make an overload status permanent"
        );
        assert_eq!(
            should_retry(&api_message(401, "insufficient_quota"), 1),
            None,
            "a quota marker must not make an unauthorized status retryable"
        );
        assert_eq!(
            should_retry(&api_message(403, "usage limit"), 1),
            None,
            "a quota marker must not make a forbidden status retryable"
        );
    }

    /// Core entry-point behavior: every documented hint form is recognized
    /// case-insensitively on a 429, the first positive candidate in message order
    /// wins, a hint never shortens the base backoff, a hint is capped at thirty
    /// seconds, and the result never drops below two seconds on the second
    /// attempt.
    #[test]
    fn when_retrying_given_rate_limit_delay_variants_then_uses_first_bounded_positive_hint() {
        for (message, expected_secs) in [
            ("retry-after: 1", 1),
            ("retry-after: 3", 3),
            ("Retry-After= 5", 5),
            ("retry_after: 7", 7),
            ("try again in 9 seconds", 9),
            ("please retry after 11", 11),
            ("retry after 13", 13),
            (r#"{"retryDelay":15}"#, 15),
            ("rate limited; Retry-After: 20", 20),
            ("retry-after: 31", 30),
            ("retry-after: 100", 30),
            ("retry-after: 0 then retry-after: 6", 6),
            ("retry-after: soon then retry-after: 8", 8),
            ("try again in 0, try again in 7", 7),
        ] {
            assert_eq!(
                should_retry(&api_message(429, message), 1),
                Some(Duration::from_secs(expected_secs)),
                "message {message:?} must wait {expected_secs} seconds"
            );
        }

        for (message, expected_secs) in [
            ("retry-after: 1", 2),
            ("retry-after: 2", 2),
            ("retry-after: 30", 30),
            ("retry-after: 300", 30),
        ] {
            assert_eq!(
                should_retry(&api_message(429, message), 2),
                Some(Duration::from_secs(expected_secs)),
                "message {message:?} must wait {expected_secs} seconds on the second attempt"
            );
        }

        assert_eq!(
            should_retry(&api_message(429, "retry-after: 20"), 1),
            Some(Duration::from_secs(20)),
            "a twenty-second hint is honored on the first attempt"
        );
        assert_eq!(
            should_retry(&api_message(429, "retry-after: 30"), 1),
            Some(Duration::from_secs(30)),
            "a thirty-second hint is honored on the first attempt"
        );
        assert_eq!(
            should_retry(
                &api_message(429, "retry-after: 999999999999999999999999"),
                1
            ),
            Some(Duration::from_secs(30)),
            "an overflowing hint is capped at thirty seconds"
        );
        for attempt in 1..MAX_ATTEMPTS {
            let wait = should_retry(&api_message(429, "retry-after: 999999"), attempt)
                .expect("a hinted 429 stays retryable");
            assert!(
                wait <= Duration::from_secs(30),
                "attempt {attempt} must never wait more than thirty seconds: {wait:?}"
            );
        }
    }

    /// Edge-case table: an absent, empty, non-numeric, negative or zero hint
    /// leaves the base wait in place; a later valid candidate is still found;
    /// intervening whitespace and JSON quotes are skipped; an arbitrarily long
    /// positive digit run saturates to the thirty-second cap. A hint outside a
    /// 429 message changes nothing.
    #[test]
    fn when_retrying_given_invalid_rate_limit_delays_then_uses_base_wait() {
        for message in [
            "",
            "no delay here",
            "retry-after:",
            "retry-after: ",
            "retry-after: soon",
            "Retry_After: later",
            "try again in ",
            "please retry after zero",
            "retry after -3",
            "retry-after: 0",
            "retry-after: 0 seconds",
            r#"{"retryDelay": "none"}"#,
        ] {
            assert_eq!(
                should_retry(&api_message(429, message), 1),
                Some(Duration::from_secs(1)),
                "message {message:?} must fall back to the one-second base wait"
            );
            assert_eq!(
                should_retry(&api_message(429, message), 2),
                Some(Duration::from_secs(2)),
                "message {message:?} must fall back to the two-second base wait"
            );
        }

        for (message, expected_secs) in [
            ("retry-after: 0 retry-after: 4", 4),
            ("retry-after: soon, retry-after: 5", 5),
            ("retry-after:\t\t12", 12),
            ("retry-after:    9   ", 9),
            ("try again in   6", 6),
            (r#""retryDelay": " 8 ""#, 8),
            (r#""retryDelay":"10""#, 10),
            (
                "retry-after: 9999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999",
                30,
            ),
        ] {
            assert_eq!(
                should_retry(&api_message(429, message), 1),
                Some(Duration::from_secs(expected_secs)),
                "message {message:?} must wait {expected_secs} seconds"
            );
        }

        for (error, description) in [
            (api_message(200, "retry-after: 20"), "an embedded 2xx error"),
            (api_message(299, "retry-after: 20"), "an embedded 299 error"),
            (
                CallError::Transport("retry-after: 20".to_string()),
                "a transport error",
            ),
            (api_message(500, "retry-after: 20"), "an overload status"),
            (api_message(408, "retry-after: 20"), "a timeout status"),
            (
                malformed("the response carried no usable content retry-after: 20"),
                "an unusable-content body",
            ),
        ] {
            assert_eq!(
                should_retry(&error, 1),
                Some(Duration::from_secs(1)),
                "{description} must keep the one-second base wait"
            );
        }
        assert_eq!(
            should_retry(&malformed("retry-after: 20"), 1),
            None,
            "an unrelated malformed body stays final"
        );
    }

    /// Edge-case table: a fractional candidate is not a positive integer, so it
    /// is skipped. When no other candidate follows, the base wait stays in
    /// place; when a valid candidate follows, the first one in message order
    /// wins, capped at thirty seconds and never shorter than the base wait.
    #[test]
    fn when_retrying_given_non_integer_rate_limit_hints_then_uses_next_valid_hint() {
        for message in [
            "retry-after: 2.5",
            "retry-after: 0.5",
            "retry-after: 1.0",
            "Retry_After= 9.9",
            "try again in 3.5 seconds",
            "please retry after 1,5",
            r#"{"retryDelay":0.25}"#,
            "retry-after: 4.75 seconds",
        ] {
            assert_eq!(
                should_retry(&api_message(429, message), 1),
                Some(Duration::from_secs(1)),
                "message {message:?} must fall back to the one-second base wait"
            );
        }

        for (message, expected_secs) in [
            ("retry-after: 2.5 then retry-after: 7", 7),
            ("retry-after: 0.5 then retry-after: 4", 4),
            ("retry-after: 1.0, retry-after: 6", 6),
            ("try again in 3.5 seconds, try again in 9 seconds", 9),
            ("please retry after 1,5 then retry after 12", 12),
            (r#"{"retryDelay":1.5} and Retry-After: 5"#, 5),
            (
                "retry-after: 0.9 then retry-after: 0 then retry-after: 3",
                3,
            ),
            ("retry-after: 2.5 then retry-after: 45", 30),
        ] {
            assert_eq!(
                should_retry(&api_message(429, message), 1),
                Some(Duration::from_secs(expected_secs)),
                "message {message:?} must wait {expected_secs} seconds"
            );
        }

        assert_eq!(
            should_retry(&api_message(429, "retry-after: 2.5 then retry-after: 1"), 1),
            Some(Duration::from_secs(1)),
            "a one-second valid hint keeps the one-second base wait"
        );
        assert_eq!(
            should_retry(&api_message(429, "retry-after: 2.5 then retry-after: 1"), 2),
            Some(Duration::from_secs(2)),
            "a one-second valid hint never shortens the two-second base wait"
        );
        assert_eq!(
            should_retry(
                &api_message(429, "retry-after: 2.5 then retry-after: 7"),
                MAX_ATTEMPTS
            ),
            None,
            "the final attempt is never retried, however valid the hint"
        );
    }

    /// Contract and invariant: the attempt budget stays three, the base wait
    /// starts at one second for attempt zero and one, doubles to eight seconds
    /// by the fourth recheck, and is ceilinged at ten seconds from the fifth
    /// attempt on -- including `u32::MAX`, which must not overflow -- while a
    /// hinted 429 never waits more than thirty seconds.
    #[test]
    fn when_waiting_given_large_attempt_numbers_then_caps_delays_without_overflow() {
        assert_eq!(MAX_ATTEMPTS, 3, "the attempt budget must stay three");
        for (attempt, expected_secs) in [
            (0, 1),
            (1, 1),
            (2, 2),
            (4, 8),
            (5, 10),
            (6, 10),
            (u32::MAX, 10),
        ] {
            assert_eq!(
                backoff(attempt),
                Duration::from_secs(expected_secs),
                "attempt {attempt} must wait {expected_secs} seconds"
            );
        }

        for attempt in 1..MAX_ATTEMPTS {
            let wait = should_retry(&api_message(429, "retry-after: 999"), attempt)
                .expect("a hinted 429 stays retryable");
            assert!(
                wait <= Duration::from_secs(30),
                "attempt {attempt} must never wait more than thirty seconds: {wait:?}"
            );
        }
        assert_eq!(
            should_retry(&api_message(429, "retry-after: 999"), MAX_ATTEMPTS),
            None,
            "the final attempt is never retried"
        );
    }

    /// Edge-case table: status zero, informational, redirect and
    /// request-fault statuses stay final whether or not the message is empty,
    /// only the exact unusable-content prefix is retried for a malformed body,
    /// and a quota marker leaves every other status with its own
    /// classification.
    #[test]
    fn when_retrying_given_status_and_response_boundaries_then_preserves_existing_classification() {
        for status in [0, 199, 300, 400, 401, 403, 404, 422] {
            for message in ["", "boom", "insufficient_quota", "retry-after: 20"] {
                assert_eq!(
                    should_retry(&api_message(status, message), 1),
                    None,
                    "status {status} with message {message:?} must not be retried"
                );
            }
        }

        for status in [200, 299, 408, 429, 500, 502, 503, 504, 529] {
            assert_eq!(
                should_retry(&api_message(status, ""), 1),
                Some(Duration::from_secs(1)),
                "status {status} with an empty message must wait one second"
            );
        }

        for detail in [
            "bad json",
            "the response content was not an array",
            "",
            "The response carried no usable content",
            " the response carried no usable content",
        ] {
            assert_eq!(
                should_retry(&malformed(detail), 1),
                None,
                "malformed detail {detail:?} must not be retried"
            );
        }
        for detail in [
            "the response carried no usable content",
            "the response carried no usable content (content_types=[])",
            "the response carried no usable content!",
        ] {
            assert_eq!(
                should_retry(&malformed(detail), 1),
                Some(Duration::from_secs(1)),
                "malformed detail {detail:?} must wait one second"
            );
        }

        assert_eq!(
            should_retry(&api_message(500, "insufficient_quota"), 1),
            Some(Duration::from_secs(1)),
            "an overload status stays retryable despite a quota marker"
        );
        assert_eq!(
            should_retry(&api_message(401, "insufficient_quota"), 1),
            None,
            "an unauthorized status stays final despite a quota marker"
        );
        assert_eq!(
            should_retry(&api_message(403, "credit balance is too low"), 1),
            None,
            "a forbidden status stays final despite a billing marker"
        );
    }

    /// Regression: the transient cases that existed before the timeout and
    /// quota work keep their exact one-second then two-second waits and their
    /// three-attempt limit, the unusable-content prefix keeps matching exactly,
    /// the non-retryable statuses stay final, and 408 joins the retryable set
    /// without weakening any of it.
    #[test]
    fn when_retrying_given_existing_transient_and_permanent_errors_then_preserves_classification() {
        for status in [200, 299, 408, 429, 500, 502, 503, 504, 529] {
            let error = api_message(status, "overloaded");
            assert_eq!(
                should_retry(&error, 1),
                Some(Duration::from_secs(1)),
                "status {status} must wait one second"
            );
            assert_eq!(
                should_retry(&error, 2),
                Some(Duration::from_secs(2)),
                "status {status} must wait two seconds"
            );
            assert_eq!(
                should_retry(&error, 3),
                None,
                "status {status} must not get a fourth attempt"
            );
        }

        let transport = CallError::Transport("connection reset".to_string());
        assert_eq!(should_retry(&transport, 1), Some(Duration::from_secs(1)));
        assert_eq!(should_retry(&transport, 2), Some(Duration::from_secs(2)));
        assert_eq!(should_retry(&transport, 3), None);

        let unusable = malformed("the response carried no usable content");
        assert_eq!(should_retry(&unusable, 1), Some(Duration::from_secs(1)));
        assert_eq!(should_retry(&unusable, 2), Some(Duration::from_secs(2)));
        assert_eq!(should_retry(&unusable, 3), None);

        for status in [0, 199, 300, 400, 401, 403, 404, 422] {
            assert_eq!(
                should_retry(&api_message(status, "rejected"), 1),
                None,
                "status {status} must stay final"
            );
        }

        for status in [200, 299, 429] {
            assert_eq!(
                should_retry(&api_message(status, "insufficient_quota"), 1),
                None,
                "status {status} reporting an exhausted account must stay final"
            );
        }
    }

    /// Contract and invariant: transport failures keep their 1-then-2-second
    /// backoff, the attempt budget is a hard stop of three attempts, and an
    /// otherwise-retryable API or unusable-content failure is never retried on
    /// the final attempt.
    #[test]
    fn when_retrying_given_transport_failures_and_attempt_limits_then_preserves_bounded_backoff() {
        assert_eq!(MAX_ATTEMPTS, 3, "the attempt budget must stay three");
        assert_eq!(should_retry(&transport(), 1), Some(Duration::from_secs(1)));
        assert_eq!(should_retry(&transport(), 2), Some(Duration::from_secs(2)));
        assert_eq!(should_retry(&transport(), MAX_ATTEMPTS), None);
        assert_eq!(should_retry(&api(529), MAX_ATTEMPTS), None);
        assert_eq!(
            should_retry(&malformed(NO_USABLE_CONTENT), MAX_ATTEMPTS),
            None
        );
    }

    /// Empty/zero boundary: an empty malformed detail and API status zero are
    /// not retryable.
    #[test]
    fn when_retrying_given_empty_details_and_zero_status_then_returns_no_retry() {
        assert_eq!(should_retry(&malformed(""), 1), None);
        assert_eq!(should_retry(&api(0), 1), None);
    }

    /// Minimum valid boundary: the first attempt waits exactly one second for
    /// the lowest retryable API status and for the exact unusable-content
    /// diagnostic.
    #[test]
    fn when_retrying_given_first_attempt_and_minimum_api_status_then_returns_one_second() {
        assert_eq!(should_retry(&api(200), 1), Some(Duration::from_secs(1)));
        assert_eq!(
            should_retry(&malformed(NO_USABLE_CONTENT), 1),
            Some(Duration::from_secs(1))
        );
    }

    /// Maximum and upper boundary: status 299 retries while the adjacent status
    /// 300 does not, the second attempt waits exactly two seconds, and the
    /// third attempt returns no retry.
    #[test]
    fn when_retrying_given_upper_status_and_final_attempt_then_respects_limits() {
        assert_eq!(should_retry(&api(299), 2), Some(Duration::from_secs(2)));
        assert_eq!(should_retry(&api(300), 1), None);
        assert_eq!(should_retry(&api(299), MAX_ATTEMPTS), None);
    }
}
