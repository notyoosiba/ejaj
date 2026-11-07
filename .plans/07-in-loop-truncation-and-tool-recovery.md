# 07: Stop-Reason Truncation Detection, In-Loop Missing-Tool Recovery, and Tool Repetition Guard (`src/llm/`, `src/loops/agent.rs`)

This document specifies how to:
1. Detect output-token truncation (`stop_reason == "max_tokens"`, `finish_reason == "length"`, and `incomplete_details.reason == "max_output_tokens"`) across all three wire providers (`AnthropicProvider`, `ChatProvider`, `ResponsesProvider`), and optionally align `AnthropicProvider::MAX_TOKENS` with `SUMMARY_MAX_TOKENS` (`16384`).
2. Recover in-loop when an unattended agent response omits a required tool call or hits the output token limit, rather than immediately terminating the loop with `Stop::TurnEnd`.
3. Guard against cross-turn identical tool-call repetition: detect when a model calls the exact same tool(s) with identical canonical JSON arguments across consecutive turns (including rejected tool calls), inject a redirect warning into the delivered tool result at 3 consecutive repetitions (quoting up to 500 bytes of canonical arguments), and abort early at 5 consecutive repetitions instead of burning all 100 steps.

## Problem Statement

Currently in `src/loops/agent.rs:782-790`:
```rust
        if tool_calls.is_empty() {
            if phase == Phase::Summary {
                let answer = messages[assistant_index].content.clone();
                return Stop::Summary(answer);
            }
            return Stop::TurnEnd;
        }
```
In every unattended tool-driven phase (`Phase::Spec`, `Phase::Tickets`, `Phase::Triage`, `Phase::Code`, and `Phase::Review`), a single turn without a tool call immediately exits the loop with `Stop::TurnEnd`:
1. **Output token truncation (`max_tokens` / `length` / `max_output_tokens`):** When a model generates a long reasoning block or a large `finish_spec`, `finish_tickets`, or `execute_script` payload that hits the output token ceiling, the provider cuts off the response before the tool call is completed. The decoded `Message` contains only partial text and `tool_calls: None`, aborting the phase.
2. **Forgotten control or command tool call:** When a model narrates its analysis ("I will now run the verification gate") and ends its turn without invoking a tool, `ejaj` immediately abandons the 100-step loop instead of prompting the model to emit the tool call.
3. **Cross-turn identical tool-call loops:** When a model gets stuck on a failing `execute_script` or `bash` call and repeats the exact same tool call with identical arguments turn after turn, `ejaj` runs the duplicate call up to `MAX_STEPS` (100) before stopping with `FailureKind::StepLimit`.

Catching missing tool calls and truncated turns inside `run_agent_loop_capturing` up to `MAX_CONSECUTIVE_FORMAT_RECOVERIES = 2` and detecting consecutive identical tool-call turns (`TOOL_LOOP_WARN_THRESHOLD = 3`, `TOOL_LOOP_ABORT_THRESHOLD = 5`) lets the model self-correct on the next turn without losing its in-memory transcript. (See `PROPOSAL-2-review-pass-retry.md` for the outer single-pass retry when a pass still exits after exhausting in-loop recovery.)

---

## Guardrails for Existing `ejaj` Tests

- **Preserve interactive `Phase::Plan` and tool-free `Phase::Summary`:**
  - `Phase::Plan` legitimately ends a turn with `tool_calls.is_empty()` when asking the user interview questions (`Stop::TurnEnd` returns control to `plan>>`), unless `truncated == true`.
  - `Phase::Summary` has no control tool and ends on a tool-free turn (`Stop::Summary(answer)`), unless `truncated == true`.
- **Keep wire serialization of `Message` unchanged:**
  - `ChatProvider` serializes `Message` directly via `serde_json::to_value(&request)` (`src/llm/chat_completions.rs:48`). Add `#[serde(skip)] pub truncated: bool` on `Message` in `src/api.rs` (defaulting to `false`) so it never appears on the wire or affects `context_bytes`.
- **Preserve `Stop::TurnEnd` after exhausting consecutive recovery turns:**
  - If the model still fails to emit a tool call after `MAX_CONSECUTIVE_FORMAT_RECOVERIES` (`2` consecutive recovery prompts), return `Stop::TurnEnd`.

---

## Improvements Checklist

1. **Add `#[serde(skip)] pub truncated: bool` to `Message` (`src/api.rs`):**
   - Initialize `truncated: false` in `Message::text` and `Message::tool`.
2. **Detect Output Token Truncation Across All Three Wire Providers (`src/llm/`):**
   - **Chat Completions (`src/api.rs` & `src/llm/chat_completions.rs`):**
     - Add `#[serde(default)] pub finish_reason: Option<String>` to `struct Choice` in `src/api.rs`.
     - In `ChatProvider::decode`, set `choice.message.truncated = choice.finish_reason.as_deref() == Some("length");`.
   - **Responses API (`src/llm/responses_api.rs`):**
     - Add `#[serde(default)] status: Option<String>` and `#[serde(default)] incomplete_details: Option<Value>` to `ResponsesResponse`.
     - Mark `truncated = true` when `status == Some("incomplete")` and `incomplete_details["reason"] == Some("max_output_tokens")`. If `content.is_none() && tool_calls.is_none() && truncated`, return an empty `Message` with `truncated: true` so the loop can recover.
   - **Anthropic Messages API (`src/llm/anthropic.rs`):**
     - Mark `truncated = object.get("stop_reason").and_then(Value::as_str) == Some("max_tokens")`. If `content.is_none() && tool_calls.is_none() && truncated`, return an empty `Message` with `truncated: true`.
     - Optionally raise `const MAX_TOKENS: u64 = 4096;` to `16384` (matching `SUMMARY_MAX_TOKENS = 16384`) and update the `max_tokens` assertion in `src/llm/anthropic.rs` unit tests.
3. **In-Loop Self-Correction in `run_agent_loop_capturing` (`src/loops/agent.rs`):**
   - Define `const MAX_CONSECUTIVE_FORMAT_RECOVERIES: usize = 2;`.
   - When `tool_calls.is_empty()`, if `missing_tool_or_truncation_recovery(phase, assistant.truncated)` returns `Some(prompt)` and `consecutive_recoveries < MAX_CONSECUTIVE_FORMAT_RECOVERIES` and `step < MAX_STEPS`, increment `consecutive_recoveries`, log `[WARN]` via `stderr_log`, push `Message::user(with_step_warning(prompt, step))`, and `continue`.
   - Reset `consecutive_recoveries = 0` whenever `!tool_calls.is_empty()`.
4. **Cross-Turn Tool-Call Repetition Guard (`src/loops/agent.rs`):**
   - Define `const TOOL_LOOP_WARN_THRESHOLD: usize = 3;` and `const TOOL_LOOP_ABORT_THRESHOLD: usize = 5;`.
   - Canonicalize each `ToolCall` into `(call.function.name.clone(), canonical_arguments(&call.function.arguments))`.
   - Track `last_turn_signature` and `consecutive_identical_tool_turns`:
     - At `>= TOOL_LOOP_ABORT_THRESHOLD`, log `[WARN]` and return `Stop::Failed(FailureKind::StepLimit)`.
     - At `>= TOOL_LOOP_WARN_THRESHOLD`, log `[WARN]` and append the loop-redirect notice (with canonical arguments preview capped at 500 UTF-8 bytes) to the last `Role::Tool` message.

---

## Verification

```bash
cargo test --lib llm && cargo test --lib loops::agent && cargo test --test pipeline && cargo clippy --all-targets -- -D warnings
```
