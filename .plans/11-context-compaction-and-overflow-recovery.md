# 11: Context Compaction Resilience and Overflow Recovery (`src/loops/agent.rs`)

This document specifies three focused upgrades to context compaction and overflow handling in `src/loops/agent.rs`:
1. **Retry transient failures on `request_handover_summary`:** apply `retry::should_retry` when sending the tool-free Tier-2 handover summary request so a single transient network blip or 503/529/200-overload response during compaction does not abort the phase with `FailureKind::ContextGuard`.
2. **Prior-checkpoint consolidation rule in `HANDOVER_SUMMARY_INSTRUCTION`:** instruct the summarizer to merge any earlier `[HANDOVER SUMMARY]` into a single consolidated summary under the four required labels rather than copying it verbatim.
3. **Reactive context-overflow recovery on HTTP 400 / 413 / 422 responses:** when a provider with a smaller context window (e.g., 32k-128k local or mid-tier models) rejects an ordinary turn below the 400 KiB Tier-1 watermark with a context-overflow error (`context_length_exceeded`, `model_context_window_exceeded`, `maximum context length`, `prompt is too long`, `context window`, `too many tokens`, `request_too_large`, or HTTP `413`), compact the history once via `force_compact_on_overflow` and retry the turn.

## Problem Statement

Currently in `src/loops/agent.rs`:
1. **No retry on `request_handover_summary` (`src/loops/agent.rs:1331`):**
   `request_handover_summary` calls `config.send(&prepared.outgoing)` once without `retry::should_retry`. If the endpoint returns a transient `503` or `429` on the summary turn, `prepare_for_send` immediately fails with `Stop::Failed(FailureKind::ContextGuard)`.
2. **Repeated compaction checkpoints can accumulate without consolidation instruction (`src/loops/agent.rs:66`):**
   `HANDOVER_SUMMARY_INSTRUCTION` does not explicitly instruct the model to fold an earlier `[HANDOVER SUMMARY]` turn into the new summary under the four required labels.
3. **No reactive compaction when smaller-window models overflow below 400 KiB (`src/loops/agent.rs:711-746`):**
   `prepare_for_send` only triggers Tier-1 compaction when `prepared.context_bytes > 400 * 1024`. Against a 32k or 64k context window, the provider can return HTTP `400` (`context_length_exceeded` / `prompt is too long`) or HTTP `413` at 120-300 KiB, which `send_turn_with_retry` treats as a fatal `FailureKind::Api`.

---

## Guardrails for Existing `ejaj` Tests

- **Keep `provider.prepare_tool_free(&summary_messages, phase)` in `request_handover_summary`:**
  - `request_handover_summary` must continue using `prepare_tool_free` so the model is not offered tools during summarization and `when_summarizing_given_active_phase_provider_then_sends_one_tool_free_request_with_exact_labels` in `src/loops/agent.rs` continues to pass unchanged.
- **Preserve `HANDOVER_SUMMARY_MARKER`, `HANDOVER_SUMMARY_LABELS`, and `TIER_TWO_CONTEXT_BYTES` validation:**
  - Existing unit tests in `src/loops/agent.rs` (`when_summarizing_given_missing_or_malformed_summary_then_stops_without_ordinary_request` at line 5608, `when_summarizing_given_tool_call_or_missing_required_label_then_stops_without_ordinary_send` at line 5845, and `when_summarizing_given_invalid_response_or_unpaired_history_then_stops_with_context_guard` at line 6985) assert that missing `HANDOVER_SUMMARY_LABELS` entries and post-summary contexts above `TIER_TWO_CONTEXT_BYTES` stop with `Stop::Failed(FailureKind::ContextGuard)`. Keep those validations intact.
  - Keep `HANDOVER_SUMMARY_MARKER` and all four `HANDOVER_SUMMARY_LABELS` substrings inside `HANDOVER_SUMMARY_INSTRUCTION` so `src/loops/agent.rs:5502-5507, 5600, 5709-5714` continue to pass.
- **Preserve tool-pairing validation and the 4 MiB hard body guard (`CONTEXT_GUARD_BYTES`):**
  - `validate_tool_pairing(messages)` and `fits_hard_body_guard(&prepared.outgoing.body)` remain hard preconditions before every provider send.
- **Scope tracker updates strictly to `TODO.md`:**
  - Update `TODO.md` only; do not edit `.plans/11-context-compaction-and-overflow-recovery.md` during implementation.

---

## Implementation Code for `src/loops/agent.rs`

```rust
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

And when an ordinary turn in `run_agent_loop_capturing` fails with `is_context_overflow_error(&error)` on a transcript that has not yet been reactively compacted on that turn:
- Run `force_compact_on_overflow(messages, config, phase, provider)` once (applying `compact_history` and, when the compacted middle still has replaceable turns above `TIER_TWO_CONTEXT_BYTES` or when Tier-1 alone did not shrink the transcript, replacing the compactable middle via `request_handover_summary`), re-prepare the request, and retry the turn once before returning `report_call_error`.

---

## Verification

```bash
cargo test --lib loops::agent && cargo test --test test_titles && cargo clippy --all-targets -- -D warnings
```

Add unit tests in `src/loops/agent.rs`:
- `when_summarizing_given_transient_503_on_first_summary_attempt_then_retries_and_succeeds`
- `when_summarizing_given_prior_handover_checkpoint_then_instruction_requires_single_consolidated_summary`
- `when_sending_turn_given_http_400_context_length_exceeded_below_watermark_then_compacts_and_retries`
