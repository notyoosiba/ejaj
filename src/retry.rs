//! The retry policy for one model turn.
//!
//! A turn is `send` plus `decode` (see [`crate::llm`]). A provider answers a
//! transient failure -- a dead connection or an overloaded endpoint -- without
//! the transcript being wrong, so the same request may simply be sent again.
//! Everything else (a rejected request, an unknown model, a body that does not
//! decode) will fail identically on every attempt, so it is reported at once.
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

/// The wait before attempt `attempt + 1`, doubling from one second: 1s, then 2s.
fn backoff(attempt: u32) -> Duration {
    Duration::from_secs(1 << (attempt - 1))
}

/// Whether the failure from attempt number `attempt` (1-based: the first attempt
/// is 1) is worth another try, and how long to wait first. `None` means the
/// failure is final -- either it is permanent or the attempts are used up.
///
/// A transport error is transient by nature. An API error is transient when the
/// endpoint is overloaded or rate limited: the 2xx range (an embedded error
/// inside an otherwise successful body), 429, 500, 502, 503, 504 and Anthropic's
/// 529. Every other status is the request's own fault, so it is not retried.
/// A malformed body is normally a contract bug, but the one diagnostic that
/// says the response carried no usable content marks a thinking-only reply that
/// another attempt can turn into usable output; every other malformed detail is
/// final.
pub fn should_retry(error: &CallError, attempt: u32) -> Option<Duration> {
    if attempt >= MAX_ATTEMPTS {
        return None;
    }
    let transient = match error {
        CallError::Transport(_) => true,
        CallError::Api { status, .. } => {
            matches!(status, 200..=299 | 429 | 500 | 502 | 503 | 504 | 529)
        }
        CallError::Malformed(detail) => {
            detail.starts_with("the response carried no usable content")
        }
    };
    transient.then(|| backoff(attempt))
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

    fn malformed(detail: &str) -> CallError {
        CallError::Malformed(detail.to_string())
    }

    /// Every API status the policy treats as transient.
    const RETRYABLE_API_STATUSES: [i32; 8] = [200, 299, 429, 500, 502, 503, 504, 529];

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
