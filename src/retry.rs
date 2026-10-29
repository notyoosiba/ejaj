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
/// A transport error is transient by nature. An API error is transient only for
/// the statuses an overloaded endpoint returns: 429 (rate limited) and the 5xx
/// family 500/502/503/504. Every other status is the request's own fault, and a
/// malformed body is a contract bug, so neither is retried.
pub fn should_retry(error: &CallError, attempt: u32) -> Option<Duration> {
    if attempt >= MAX_ATTEMPTS {
        return None;
    }
    let transient = match error {
        CallError::Transport(_) => true,
        CallError::Api { status, .. } => matches!(status, 429 | 500 | 502 | 503 | 504),
        CallError::Malformed(_) => false,
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

    /// A transport failure is retried, and the backoff doubles from one second.
    #[test]
    fn transport_failures_back_off_from_one_second() {
        assert_eq!(should_retry(&transport(), 1), Some(Duration::from_secs(1)));
        assert_eq!(should_retry(&transport(), 2), Some(Duration::from_secs(2)));
    }

    /// The overload statuses are retried.
    #[test]
    fn overload_statuses_are_retried() {
        for status in [429, 500, 502, 503, 504] {
            assert!(
                should_retry(&api(status), 1).is_some(),
                "status {status} must be retried"
            );
        }
    }

    /// A request the provider understood and refused is never retried.
    #[test]
    fn permanent_statuses_are_not_retried() {
        for status in [400, 401, 403, 404, 422] {
            assert!(
                should_retry(&api(status), 1).is_none(),
                "status {status} must not be retried"
            );
        }
    }

    /// A body that does not decode is a contract bug, not a blip.
    #[test]
    fn a_malformed_response_is_not_retried() {
        assert!(should_retry(&CallError::Malformed("bad json".to_string()), 1).is_none());
    }

    /// The attempt budget is a hard stop, however transient the failure.
    #[test]
    fn the_final_attempt_is_never_retried() {
        assert_eq!(should_retry(&transport(), MAX_ATTEMPTS), None);
        assert_eq!(should_retry(&api(503), MAX_ATTEMPTS), None);
    }
}
