# Anthropic Retry Fixes (`TODO.md`)

This document defines the fixes for three retry bugs in `ejaj`.

---

## Bug Descriptions

### Bug 1: `should_retry` does not retry HTTP `529` (Anthropic Overloaded)
- **File:** `src/retry.rs` (line 40)
- **Problem:** `should_retry` retries only HTTP status codes `429`, `500`, `502`, `503`, and `504`. Anthropic returns HTTP `529` when its servers are overloaded. `should_retry` returns `None` for `529`, and the request fails on the first attempt.
- **Fix:** Add `529` to the retryable status match in `should_retry`.

### Bug 2: `should_retry` does not retry an `error` object inside a `2xx` response body
- **Files:** `src/retry.rs` (line 40), `src/llm/anthropic.rs` (lines 130–134)
- **Problem:** A provider can send HTTP `200 OK` headers and then return an overload error inside the JSON body (`{"type":"error","error":{"message":"overloaded"}}`). `AnthropicProvider::decode` (as well as `ChatProvider::decode` and `ResponsesProvider::decode`) maps this to `CallError::Api { status: 200, message }`. Because `status` is `200`, `should_retry` returns `None` and does not retry.
- **Fix:** Add `200..=299` to the retryable `CallError::Api` status match in `should_retry`. A `CallError::Api` only has a `200..=299` status when the `2xx` response body carried an embedded `error` object.

### Bug 3 (Option A): `should_retry` does not retry an unusable / thinking-only Anthropic response
- **Files:** `src/retry.rs` (line 41), `src/llm/anthropic.rs` (lines 173–175, 363–387, 1303–1305, 1765)
- **Problem:** In `Phase::Summary`, `AnthropicProvider` sends `"output_config": { "effort": "high" }`. If the model returns only `thinking` or `redacted_thinking` blocks and no `text` or `tool_use` block, `AnthropicProvider::decode` returns `CallError::Malformed("the response carried no usable content ...")`. Because `should_retry` returns `false` for all `CallError::Malformed` values, the turn fails immediately without a retry.
- **Fix (Option A):** In `should_retry` (`src/retry.rs`), retry `CallError::Malformed(detail)` when `detail.starts_with("the response carried no usable content")`. Keep all other `CallError::Malformed` values (such as `"the response content was not an array"`) non-retryable. Update the affected unit tests in `src/retry.rs` and `src/llm/anthropic.rs`.

---

## Proposed Code Change (`src/retry.rs`)

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

## Tasks

- [x] **T01 — Retry HTTP `529`, `2xx` embedded API errors, and unusable-content responses in `src/retry.rs` and `src/llm/anthropic.rs`**
  - `depends_on`: `[]`
  - `files`: `src/retry.rs`, `src/llm/anthropic.rs`, `TODO.md`
  - `goal`: Make `retry::should_retry` retry HTTP `529` overload responses, `200..=299` embedded API error responses, and `CallError::Malformed` responses whose detail starts with `"the response carried no usable content"`.
  - `scope`:
    1. In `src/retry.rs`, update `should_retry` so `CallError::Api { status, .. }` matches `200..=299 | 429 | 500 | 502 | 503 | 504 | 529`.
    2. In `src/retry.rs`, update `should_retry` so `CallError::Malformed(detail)` returns `true` when `detail.starts_with("the response carried no usable content")` and `false` for all other malformed details.
    3. In `src/retry.rs` unit tests, verify that statuses `200`, `299`, `429`, `500`, `502`, `503`, `504`, and `529` are retried, permanent statuses (`400`, `401`, `403`, `404`, `422`) are not retried, `"the response carried no usable content..."` is retried, and other malformed errors (`"bad json"`, `"the response content was not an array"`) are not retried.
    4. In `src/llm/anthropic.rs` unit tests (`when_decoding_given_anthropic_api_errors_then_preserves_status_message_and_retry_policy` and `when_decoding_given_sensitive_unusable_content_then_excludes_private_payload`), update the `should_retry` assertions for embedded `200` errors, status `529`, and `"the response carried no usable content"` errors to match the new retry contract.
    5. Mark `T01` as `- [x]` in `TODO.md`.
  - `verify`: `git grep -Fq -e '- [x] **T01 ' -- TODO.md && cargo test --lib retry && cargo test --lib llm::anthropic && cargo clippy --all-targets -- -D warnings`
