# 12: HTTP 408 Timeout Retry, Quota-vs-Rate-Limit Classification, and 429 Retry-After Hints (`src/retry.rs`)

> **Status:** `[x]` Complete (T13 done) — HTTP `408` retry, permanent `429`/`2xx` quota fast-fail with the `token_quota_exceeded` exception, `429` retry-delay hints, and saturating capped backoff are implemented and unit-tested in `src/retry.rs`.

> **Note:** HTTP `529` (Anthropic Overloaded), `200..=299` embedded API errors, and `CallError::Malformed` responses starting with `"the response carried no usable content"` are **already implemented and tested** in `src/retry.rs:38-52` and `src/llm/anthropic.rs`.

This document specifies the remaining upgrades to `should_retry` in `src/retry.rs`:
1. Add **HTTP `408`** (Request Timeout) to the retryable status codes.
2. Distinguish **HTTP `429` (and `2xx` embedded) rate-limit throttles** (transient, retryable) from **account quota/billing exhaustion** (permanent, fast-fail without retrying).
3. Extract explicit retry-delay hints (`retry-after`, `try again in Ns`, `please retry after Ns`, `retryDelay`) from HTTP `429` error messages (clamped to `MAX_RATE_LIMIT_BACKOFF_SECS = 30`).
4. Cap base exponential backoff at `MAX_BACKOFF_SECS = 10` with saturating shift arithmetic.

## Current Baseline in `src/retry.rs:38-52`

```rust
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
```

---

## Guardrails for Existing `ejaj` Tests

- **Preserve `MAX_ATTEMPTS = 3` and `attempt = 1 -> 1s`, `attempt = 2 -> 2s` backoff durations:**
  - `src/retry.rs` and `src/llm/anthropic.rs` assert `should_retry(&error, 1) == Some(Duration::from_secs(1))`, `should_retry(&error, 2) == Some(Duration::from_secs(2))`, and `should_retry(&error, 3) == None`.
- **Follow caller-intent BDD test naming (`tests/test_titles.rs`):**
  - Every unit test in `src/retry.rs` must use lowercase `snake_case` `when_<action>_given_<context>_then_<outcome>` naming and avoid internal function names in test titles.

---

## Improvements Checklist

1. **Add HTTP `408` to Retryable Statuses (`src/retry.rs`):**
   - Match `408 | 500 | 502 | 503 | 504 | 529` in `should_retry`.
2. **Filter Out Permanent Quota/Billing Exhaustion on `429` and `200..=299` (`is_permanent_quota_error`):**
   - If `message` (lowercased) contains `"token_quota_exceeded"`, treat it as a transient per-minute token rate limit (`return false`).
   - Otherwise, if `message` (lowercased) contains any of `"insufficient_quota"`, `"insufficient quota"`, `"insufficient_balance"`, `"insufficient balance"`, `"billing_not_active"`, `"credit balance is too low"`, `"quota_exceeded"`, `"quota exceeded"`, `"budget exceeded"`, `"usage limit"`, `"limit exhausted"`, `"package has expired"`, or `"plan does not yet include"`, return `None` from `should_retry`.
3. **Extract Explicit Retry-Delay Hints from `429` Messages (`extract_retry_hint_secs`):**
   - When an explicit positive hint is present in `message`, return `Some(hint.max(backoff(attempt)).min(Duration::from_secs(MAX_RATE_LIMIT_BACKOFF_SECS)))` (with `MAX_RATE_LIMIT_BACKOFF_SECS = 30`). When `message` carries no explicit hint, return `Some(backoff(attempt))`.
4. **Cap Per-Attempt Backoff at 10 Seconds (`backoff`):**
   - Compute `Duration::from_secs((1u64 << attempt.saturating_sub(1).min(31)).min(MAX_BACKOFF_SECS))` with `MAX_BACKOFF_SECS = 10`.

---

## Replacement Code for `src/retry.rs`

```rust
pub const MAX_ATTEMPTS: u32 = 3;
const MAX_BACKOFF_SECS: u64 = 10;
const MAX_RATE_LIMIT_BACKOFF_SECS: u64 = 30;

fn backoff(attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(31);
    let secs = (1u64 << shift).min(MAX_BACKOFF_SECS);
    Duration::from_secs(secs)
}

fn extract_retry_hint_secs(message: &str) -> Option<u64> {
    let lowered = message.to_ascii_lowercase();
    for prefix in [
        "retry-after:",
        "retry-after=",
        "retry_after:",
        "try again in ",
        "please retry after ",
        "retry after ",
        "\"retrydelay\":",
    ] {
        if let Some(pos) = lowered.find(prefix) {
            let tail = lowered[pos + prefix.len()..].trim_start_matches([' ', '"', '\'']);
            let digits: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(secs) = digits.parse::<u64>()
                && secs > 0
            {
                return Some(secs.min(MAX_RATE_LIMIT_BACKOFF_SECS));
            }
        }
    }
    None
}

fn is_permanent_quota_error(message: &str) -> bool {
    let lowered = message.to_ascii_lowercase();
    if lowered.contains("token_quota_exceeded") {
        return false;
    }
    const QUOTA_MARKERS: [&str; 13] = [
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
    QUOTA_MARKERS.iter().any(|marker| lowered.contains(marker))
}

pub fn should_retry(error: &CallError, attempt: u32) -> Option<Duration> {
    if attempt >= MAX_ATTEMPTS {
        return None;
    }
    match error {
        CallError::Transport(_) => Some(backoff(attempt)),
        CallError::Api { status, message }
            if (*status == 429 || (200..=299).contains(status))
                && !is_permanent_quota_error(message) =>
        {
            let base = backoff(attempt);
            let hinted = extract_retry_hint_secs(message).map(Duration::from_secs);
            Some(hinted.map_or(base, |h| h.max(base)))
        }
        CallError::Api { status, .. }
            if matches!(status, 408 | 500 | 502 | 503 | 504 | 529) =>
        {
            Some(backoff(attempt))
        }
        CallError::Malformed(detail)
            if detail.starts_with("the response carried no usable content") =>
        {
            Some(backoff(attempt))
        }
        CallError::Api { .. } | CallError::Malformed(_) => None,
    }
}
```

---

## Verification

```bash
cargo test --lib retry && cargo test --test test_titles && cargo clippy --all-targets -- -D warnings
```
