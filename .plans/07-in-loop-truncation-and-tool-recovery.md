# 07: Stop-Reason Truncation Detection, In-Loop Truncation Recovery, and Tool Repetition Guard (`src/api.rs`, `src/llm/`, `src/loops/agent.rs`)

This document specifies how to:
1. Detect output-token truncation (`stop_reason == "max_tokens"`, `finish_reason == "length"`, and `status == "incomplete"` with `incomplete_details.reason == "max_output_tokens"`) across all three wire providers (`AnthropicProvider`, `ChatProvider`, `ResponsesProvider`) while keeping `AnthropicProvider::MAX_TOKENS` (`4096`) and `SUMMARY_MAX_TOKENS` (`16384`) unchanged.
2. Recover in-loop when an assistant response hits the output-token limit (`assistant.truncated == true`) before emitting a tool call (or before completing a `Plan`/`Summary` turn), rather than immediately terminating the loop with `Stop::TurnEnd` or a truncated `Stop::Summary`.
3. Guard against cross-turn identical tool-call repetition: detect when a model calls the exact same tool(s) with identical canonical JSON arguments across consecutive turns (including rejected tool calls), inject a redirect warning into the delivered tool result at 3 consecutive repetitions (quoting up to 500 bytes of canonical arguments), and abort early at 5 consecutive repetitions instead of burning all 100 steps.

## Problem Statement

Currently in `src/loops/agent.rs:796-806`:
```rust
        if tool_calls.is_empty() {
            if phase == Phase::Summary {
                let answer = messages[assistant_index].content.clone();
                return Stop::Summary(answer);
            }
            return Stop::TurnEnd;
        }
```
1. **Output token truncation (`max_tokens` / `length` / `max_output_tokens`):** When a model generates a long reasoning block or a large `finish_spec`, `finish_tickets`, or `execute_script` payload that hits the output token ceiling, the provider cuts off the response before the tool call is completed. The decoded `Message` contains only partial text (or no usable blocks) and `tool_calls: None`, aborting the phase.
2. **Cross-turn identical tool-call loops:** When a model gets stuck on a failing `execute_script` or `bash` call and repeats the exact same tool call with identical arguments turn after turn, `ejaj` runs the duplicate call up to `MAX_STEPS` (100) before stopping with `FailureKind::StepLimit`.

Catching truncated tool-free turns (`assistant.truncated == true` and `tool_calls.is_empty()`) inside `run_agent_loop_capturing` up to `MAX_CONSECUTIVE_FORMAT_RECOVERIES = 2` and detecting consecutive identical tool-call turns (`TOOL_LOOP_WARN_THRESHOLD = 3`, `TOOL_LOOP_ABORT_THRESHOLD = 5`) lets the model self-correct on the next turn without losing its in-memory transcript. (See `PROPOSAL-2-review-pass-retry.md` for the outer single-pass retry when an unattended pass ends with a non-truncated `Stop::TurnEnd`, `Malformed`, `StepLimit`, or `ContextGuard`.)

---

## Guardrails for Existing `ejaj` Tests

- **Preserve non-truncated tool-free turns across all phases (`assistant.truncated == false`):**
  - Over 30 unit tests in `src/loops/agent.rs`, plus `tests/support/retry_tests.rs` and `tests/pipeline.rs`, drive `run_agent_loop` with a single-reply `Script` returning a non-truncated tool-free turn (`reply(Some("done"), &[])` or `reply(None, &[])`) and assert `Stop::TurnEnd` (or `Stop::Summary(answer)` in `Phase::Summary`) after 1 request (`Script::send` panics on any extra request).
  - Trigger in-loop recovery in `run_agent_loop_capturing` **only when `assistant.truncated` is `true` and `tool_calls.is_empty()`**. Non-truncated tool-free turns (`assistant.truncated == false`) must continue to return `Stop::Summary(answer)` in `Phase::Summary` and `Stop::TurnEnd` in all other phases immediately on the first turn.
- **Keep wire serialization of `Message` and `AnthropicProvider::MAX_TOKENS` unchanged:**
  - `ChatProvider` serializes `Message` directly via `serde_json::to_value(&request)` (`src/llm/chat_completions.rs:61`). Add `#[serde(skip)] pub truncated: bool` on `Message` in `src/api.rs` (defaulting to `false`) so it never appears on the wire or affects `context_bytes`.
  - Keep `const MAX_TOKENS: u64 = 4096;` and `const SUMMARY_MAX_TOKENS: u64 = 16384;` in `src/llm/anthropic.rs` unchanged.
- **Initialize `truncated: false` in all direct `Message { ... }` struct literals:**
  - Update the direct `Message { ... }` struct literals in `src/api.rs` (`Message::text`, `Message::tool`), `src/llm/anthropic.rs` (tests), `src/llm/responses_api.rs` (tests), `src/loops/agent.rs` (`request_handover_summary` and tests), `src/loops/plan.rs` (tests), and `src/loops/summary.rs` (tests) with `truncated: false` so `cargo test --lib` compiles cleanly.
- **Keep `T14` batched or slice-isolated and scope tracker edits strictly to `TODO.md`:**
  - In `T14`, batch `src/api.rs` and the three small same-shape provider decoder updates (`src/llm/chat_completions.rs`, `src/llm/responses_api.rs`, `src/llm/anthropic.rs`) into a single ticket sharing `T14`'s verification gate (`cargo test --lib api && cargo test --lib llm && cargo test --test test_titles && cargo clippy --all-targets -- -D warnings`); if sliced into multiple tickets, each ticket's `acceptance_criteria` must contain only criteria for its own module/provider and never criteria for a provider listed in `out_of_scope`.
  - Update tracker status in `TODO.md` only; do not edit `.plans/07-in-loop-truncation-and-tool-recovery.md` during `T14` or `T15`.
- **Preserve `Stop::TurnEnd` / `Stop::Summary` after exhausting consecutive recovery turns:**
  - If the model still returns a truncated tool-free turn after `MAX_CONSECUTIVE_FORMAT_RECOVERIES` (`2` consecutive recovery prompts), return `Stop::Summary(answer)` for `Phase::Summary` or `Stop::TurnEnd` for all other phases.

---

## Improvements Checklist

1. **Add `#[serde(skip)] pub truncated: bool` to `Message` (`src/api.rs`, `T14`):**
   - Initialize `truncated: false` in `Message::text` and `Message::tool` (`src/api.rs`), and in all existing direct `Message { ... }` struct literals in `src/llm/{anthropic,responses_api}.rs` and `src/loops/{agent,plan,summary}.rs`.
2. **Detect Output Token Truncation Across All Three Wire Providers (`src/llm/`, `T14`):**
   - **Chat Completions (`src/api.rs` & `src/llm/chat_completions.rs`):**
     - Add `#[serde(default)] pub finish_reason: Option<String>` to `struct Choice` in `src/api.rs`.
     - In `ChatProvider::decode`, set `choice.message.truncated = choice.finish_reason.as_deref() == Some("length");` after existing HTTP and embedded API error checks.
   - **Responses API (`src/llm/responses_api.rs`):**
     - Add `#[serde(default)] status: Option<String>` and `#[serde(default)] incomplete_details: Option<Value>` to `ResponsesResponse`.
     - Mark `truncated = true` only when `status.as_deref() == Some("incomplete")` and `incomplete_details.as_ref().and_then(|d| d.get("reason")).and_then(Value::as_str) == Some("max_output_tokens")`. If `output` is missing, empty, or yields neither non-empty text nor tool calls and `truncated` is `true`, return an empty `Message` (`role: Role::Assistant`, `content: None`, `tool_calls: None`, `tool_call_id: None`, `reasoning_content: None`, `extra: Map::new()`, `truncated: true`) instead of `no_output_items()`.
   - **Anthropic Messages API (`src/llm/anthropic.rs`):**
     - Mark `truncated = object.get("stop_reason").and_then(Value::as_str) == Some("max_tokens")` after existing nested-error and non-2xx checks. If `content` is missing (`None`) or an array that yields neither non-empty text nor tool calls and `truncated` is `true`, return the same empty `Message` with `truncated: true` instead of `no_usable_content(object)` (preserving `malformed("the response content was not an array")` when `content` is present but not an array). Keep `MAX_TOKENS = 4096` unchanged.
3. **In-Loop Truncation Self-Correction in `run_agent_loop_capturing` (`src/loops/agent.rs`, `T15`):**
   - Define `const MAX_CONSECUTIVE_FORMAT_RECOVERIES: usize = 2;`.
   - When `tool_calls.is_empty()` and `messages[assistant_index].truncated` is `true`, if `consecutive_recoveries < MAX_CONSECUTIVE_FORMAT_RECOVERIES` and `step < MAX_STEPS`, increment `consecutive_recoveries`, log `[WARN]` via `stderr_log`, push `Message::user(with_step_warning(truncation_recovery_prompt(phase), step))`, and `continue`.
   - Reset `consecutive_recoveries = 0` whenever `!tool_calls.is_empty()` or `!messages[assistant_index].truncated`.
4. **Cross-Turn Tool-Call Repetition Guard (`src/loops/agent.rs`, `T15`):**
   - Define `const TOOL_LOOP_WARN_THRESHOLD: usize = 3;` and `const TOOL_LOOP_ABORT_THRESHOLD: usize = 5;`.
   - Canonicalize each `ToolCall` into `(call.function.name.clone(), canonical_arguments(&call.function.arguments))`.
   - Track `last_turn_signature` and `consecutive_identical_tool_turns`:
     - At `>= TOOL_LOOP_ABORT_THRESHOLD`, log `[WARN]` and return `Stop::Failed(FailureKind::StepLimit)`.
     - At `>= TOOL_LOOP_WARN_THRESHOLD`, log `[WARN]` and append the loop-redirect notice (with canonical arguments preview capped at 500 UTF-8 bytes) to the last `Role::Tool` message.

---

## Verification

```bash
cargo test --lib api && cargo test --lib llm && cargo test --lib loops::agent && cargo test --test pipeline && cargo test --test test_titles && cargo clippy --all-targets -- -D warnings
```
