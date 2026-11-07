# 11: Context Compaction Resilience and Overflow Recovery (`src/loops/agent.rs`)

This document specifies four focused upgrades to context compaction and overflow handling in `src/loops/agent.rs`:
1. **Retry transient failures on `request_handover_summary`:** apply `retry::should_retry` when sending the tool-free Tier-2 handover summary request so a single transient network blip or 503/529/200-overload response during compaction does not abort the phase with `FailureKind::ContextGuard`.
2. **Prior-checkpoint consolidation rule in `HANDOVER_SUMMARY_INSTRUCTION`:** instruct the summarizer to merge any earlier `[HANDOVER SUMMARY]` into a single consolidated summary under the four required labels rather than copying it verbatim.
3. **Tier-1 fallback when Tier-2 handover summary fails soft validation:** if a non-empty text summary response omits one of the four `HANDOVER_SUMMARY_LABELS` (or leaves the replacement slightly above `TIER_TWO_CONTEXT_BYTES`), fall back to the already-compacted Tier-1 transcript when it fits within `CONTEXT_GUARD_BYTES` (4 MiB) instead of hard-failing the pass.
4. **Reactive context-overflow recovery on HTTP 400 / 413 / 422 responses:** when a provider with a smaller context window (e.g., 32k-128k local or mid-tier models) rejects an ordinary turn below the 400 KiB Tier-1 watermark with a context-overflow error (`context_length_exceeded`, `model_context_window_exceeded`, `maximum context length`, `prompt is too long`, `request_too_large`, or HTTP `413`), compact the history once and retry the turn.

## Problem Statement

Currently in `src/loops/agent.rs`:
1. **No retry on `request_handover_summary` (`src/loops/agent.rs:1330`):**
   `request_handover_summary` calls `config.send(&prepared.outgoing)` once without `retry::should_retry`. If the endpoint returns a transient `503` or `429` on the summary turn, `prepare_for_send` immediately fails with `Stop::Failed(FailureKind::ContextGuard)`.
2. **Fragile label validation kills valid Tier-1 transcripts (`src/loops/agent.rs:1065-1075, 1351-1362`):**
   When `prepared.context_bytes > TIER_TWO_CONTEXT_BYTES` (200 KiB) after Tier-1 stubbing, `prepare_for_send` requires `request_handover_summary` to start with `[HANDOVER SUMMARY]` and contain all 4 exact substrings (`Completed/work context:`, `Decisions:`, `Unresolved work/blockers:`, `Relevant findings:`). If the model writes `Unresolved blockers:` instead of `Unresolved work/blockers:`, `prepare_for_send` aborts the entire phase with `FailureKind::ContextGuard`—even though the Tier-1 compacted transcript fits well within `CONTEXT_GUARD_BYTES` (4 MiB).
3. **No reactive compaction when smaller-window models overflow below 400 KiB (`src/loops/agent.rs:711-746`):**
   `prepare_for_send` only triggers Tier-1 compaction when `prepared.context_bytes > 400 * 1024`. Against a 32k or 64k context window, the provider can return HTTP `400` (`context_length_exceeded` / `prompt is too long`) or HTTP `413` at 120-300 KiB, which `send_turn_with_retry` treats as a fatal `FailureKind::Api`.

---

## Guardrails for Existing `ejaj` Tests

- **Keep `provider.prepare_tool_free(&summary_messages, phase)` in `request_handover_summary`:**
  - `request_handover_summary` must continue using `prepare_tool_free` so the model is not offered tools during summarization and `when_summarizing_given_active_phase_provider_then_sends_one_tool_free_request_with_exact_labels` in `src/loops/agent.rs` continues to pass unchanged.
- **Preserve tool-pairing validation and the 4 MiB hard body guard (`CONTEXT_GUARD_BYTES`):**
  - `validate_tool_pairing(messages)` and `fits_hard_body_guard(&prepared.outgoing.body)` remain hard preconditions before every provider send.
- **Coordinate Tier-1 fallback with existing unit tests in `src/loops/agent.rs`:**
  - Hard summary failures (transport/API failure after retries, empty response, or unexpected tool call) continue to stop with `Stop::Failed(FailureKind::ContextGuard)`.
  - Soft label-heading omissions when the Tier-1 `prepared` request fits within `CONTEXT_GUARD_BYTES` log a `[WARN]` and fall back to the Tier-1 compacted transcript.

---

## Implementation Code for `src/loops/agent.rs`

```rust
enum SummaryError {
    /// A failure that must stop the pass (API/transport error, empty body, or
    /// tool call in a tool-free summary turn).
    Fatal(String),
    /// A non-empty text summary that omitted a required heading or left context
    /// above Tier 2 while Tier 1 still fits within the 4 MiB hard guard.
    SoftValidation(String),
}

/// Detect whether an API error from a provider is a context-window or payload
/// overflow rejection that compaction can recover from.
fn is_context_overflow_error(error: &CallError) -> bool {
    let CallError::Api { status, message } = error else {
        return false;
    };
    if *status == 413 {
        return true;
    }
    if !matches!(*status, 400 | 422) {
        return false;
    }
    let lowered = message.to_ascii_lowercase();
    const OVERFLOW_MARKERS: [&str; 7] = [
        "context_length_exceeded",
        "model_context_window_exceeded",
        "maximum context length",
        "prompt is too long",
        "context window",
        "too many tokens",
        "request_too_large",
    ];
    OVERFLOW_MARKERS
        .iter()
        .any(|marker| lowered.contains(marker))
}

/// Send the tool-free handover summary request with transient retry support.
fn send_summary_with_retry(
    config: &Config,
    provider: &dyn crate::llm::LlmProvider,
    outgoing: &crate::api::Outgoing,
) -> Result<Message, String> {
    let mut attempt = 1;
    loop {
        let result = match config.send(outgoing) {
            Ok(raw) => provider.decode(raw),
            Err(error) => Err(error),
        };
        match result {
            Ok(message) => return Ok(message),
            Err(error) => match retry::should_retry(&error, attempt) {
                Some(delay) => {
                    let _ = status::shared().stderr_log(&format!(
                        "[WARN] handover summary attempt {attempt}/{} failed: {error}; retrying.",
                        retry::MAX_ATTEMPTS
                    ));
                    std::thread::sleep(delay);
                    attempt += 1;
                }
                None => return Err(format!("the handover summary request failed: {error}")),
            },
        }
    }
}
```

In `prepare_for_send` (`src/loops/agent.rs:1065`):
```rust
    if prepared.context_bytes > TIER_TWO_CONTEXT_BYTES {
        match request_handover_summary(messages, config, phase, provider) {
            Ok(summary) => {
                let candidate = replacement_history(messages, prefix_end, tail_start, summary)?;
                let candidate_prepared = provider.prepare(&candidate, phase);
                if !fits_hard_body_guard(&candidate_prepared.outgoing.body) {
                    return Err(format!(
                        "the encoded request body is still over the {CONTEXT_GUARD_BYTES}-byte hard guard after the handover summary"
                    ));
                }
                if candidate_prepared.context_bytes <= TIER_TWO_CONTEXT_BYTES {
                    *messages = candidate;
                    prepared = candidate_prepared;
                } else {
                    let _ = status::shared().stderr_log(&format!(
                        "[WARN] handover summary left context at {} bytes (over {TIER_TWO_CONTEXT_BYTES}); keeping Tier 1 compacted transcript",
                        candidate_prepared.context_bytes
                    ));
                }
            }
            Err(SummaryError::SoftValidation(reason)) => {
                let _ = status::shared().stderr_log(&format!(
                    "[WARN] {reason}; keeping Tier 1 compacted transcript"
                ));
            }
            Err(SummaryError::Fatal(detail)) => return Err(detail),
        }
    }
```

---

## Verification

```bash
cargo test --lib loops::agent && cargo clippy --all-targets -- -D warnings
```

Add unit tests in `src/loops/agent.rs`:
- `when_summarizing_given_transient_503_on_first_summary_attempt_then_retries_and_succeeds`
- `when_summarizing_given_missing_label_and_tier_one_below_hard_guard_then_falls_back_to_tier_one`
- `when_sending_turn_given_http_400_context_length_exceeded_below_watermark_then_compacts_and_retries`
