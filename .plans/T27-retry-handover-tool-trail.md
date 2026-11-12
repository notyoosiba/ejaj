# T27: Capture Recent Tool-Activity Trail in `[RETRY HANDOVER]` (`src/loops/agent.rs`, `src/loops/code.rs`, `src/orchestrator.rs`, `tests/hybrid_pipeline.rs`)

## Prerequisite

Depends on `T26` (`.plans/T26-hybrid-code-retry-giga-escalation.md`).

## Problem

When a ticket's first code-agent attempt fails (`TurnEnd`, `FailureKind::StepLimit`, `FailureKind::ContextGuard`, or `task_complete` followed by a failed first verification gate), `[RETRY HANDOVER]` (`src/orchestrator.rs:893-921`) omits Attempt 1's recent tool calls, error lines, and spill paths:

- When Attempt 1 stops at `StepLimit`, `ContextGuard`, or `TurnEnd` without reaching the verification gate, `first_gate_output` is `None` (`"(the first attempt did not reach a failed verification gate)"`), and `narration` only holds the last prose reply.
- Even when Attempt 1 declared `task_complete` and failed the first gate, Attempt 2 cannot see which `bash` commands or `execute_script` calls Attempt 1 ran right before stopping, what `[ERROR: ...]` lines they produced, or which `.ejaj/<timestamp>/tools-output/step-<N>-call-<M>.log` spill file already holds the full command output on disk.

---

## Guardrails for Existing `ejaj` Tests

- **Preserve all existing `retry_tests.rs` and `src/orchestrator.rs` invariants (`tests/support/retry_tests.rs`, `src/orchestrator.rs`):**
  - Keep `code::Attempt::new(outcome)` (`src/loops/code.rs:47-53`) defaulting `narration: None` and `tool_trail: None` so `Outcome::into()` and scripted `Lifecycle` mocks (`Forwarding` at `src/orchestrator.rs:1479`, `Script` at `src/orchestrator.rs:1845`) compile and behave unchanged.
  - Omit `Recent tool activity:` completely when `tool_trail` is `None` or whitespace-only so tool-free first attempts (`tests/support/retry_tests.rs:188, 291, 394`) emit the exact same `[RETRY HANDOVER]` bytes as today.
  - Place `Recent tool activity:\n{trail}\n\n` **before** `Repository working-tree diff (staged, unstaged, and non-ignored untracked changes; .ejaj/ excluded):` so `when_retrying_given_failed_first_gate_then_preserves_original_kickoff_diagnostic_diff_and_gate_count` (`tests/support/retry_tests.rs:256-270`), which splits `[RETRY HANDOVER]` on `"Repository working-tree diff"` and forbids `".ejaj"` inside the diff section, continues to pass even when `trail` cites a `.ejaj/.../tools-output/...` spill path.
  - Bound raw tool-argument inspection so multi-megabyte adversarial arguments (`tests/support/retry_tests.rs:561`, ~4.9 MB `oversized_arguments` for `ContextGuard`) are never parsed in full by `serde_json` and never exceed the `1,500`-byte `tool_trail` cap.
- **Preserve caller-intent test-title rules (`tests/test_titles.rs`):**
  - Every new test must follow `when_<action>_given_<context>_then_<outcome>` and avoid forbidden internal/builder identifiers.
- **Scope tracker updates strictly to `TODO.md`:**
  - Mark the `T27` row `[x]` in `TODO.md` right before running the verification gate; do not edit `.plans/T27-retry-handover-tool-trail.md`.

---

## Proposed Design

### 1. Capture Attempt 1's Recent Tool & Error Trail (`src/loops/agent.rs`, `src/loops/code.rs`)

1. **Extend `AgentAttempt` and `code::Attempt`:**
   - Add `pub tool_trail: Option<String>` to `AgentAttempt` (`src/loops/agent.rs:339-344`).
   - Add `pub tool_trail: Option<String>` to `code::Attempt` (`src/loops/code.rs:38-53`), initializing `tool_trail: None` in `Attempt::new(outcome)` and forwarding `tool_trail: run.tool_trail` in `Session::run_ticket_attempt_in` (`src/loops/code.rs:164-168`).
2. **Summarize recent delivered tool calls (`summarize_recent_tool_activity` in `src/loops/agent.rs`):**
   - Call `let tool_trail = summarize_recent_tool_activity(messages);` at the end of `run_agent_loop_attempt_in` (`src/loops/agent.rs:370-389`).
   - Define explicit bounds:
     - `const RECENT_TOOL_CALL_LIMIT: usize = 3;`
     - `const RECENT_TOOL_CALL_PREVIEW_BYTES: usize = 160;`
     - `const RECENT_TOOL_STATUS_PREVIEW_BYTES: usize = 240;`
     - `const RECENT_TOOL_TRAIL_MAX_BYTES: usize = 1_500;`
     - `const RECENT_TOOL_JSON_PARSE_LIMIT_BYTES: usize = 16_384;`
   - **Matching delivered calls:**
     - Scan `messages` in chronological order for `Role::Assistant` messages carrying `tool_calls`.
     - Pair each `ToolCall` with the first subsequent `Role::Tool` message where `message.tool_call_id.as_deref() == Some(call.id.as_str())`.
     - Skip any `ToolCall` without a delivered `Role::Tool` message (such as an accepted `task_complete` handoff, which returns before `deliver`, or an aborted repeated-call turn).
     - Return `None` when zero delivered calls exist; otherwise keep the last `RECENT_TOOL_CALL_LIMIT` (`3`) pairs in chronological order.
   - **Formatting each call target (`format_recent_call_preview`):**
     - Only parse `call.function.arguments` with `serde_json::from_str::<serde_json::Value>` when `call.function.arguments.len() <= RECENT_TOOL_JSON_PARSE_LIMIT_BYTES`.
     - When `call.function.name == "bash"` and JSON field `"command"` is a string, format `bash: <collapsed_command>` (collapsing consecutive whitespace/newlines to single spaces).
     - When `call.function.name == "execute_script"` and JSON field `"code"` is a string, format `execute_script: <collapsed_code>`.
     - Otherwise, inspect at most the first `512` UTF-8 bytes of `call.function.arguments` (collapsed to single-space text): render `<name>()` when empty or `"{}"`, else `<name>(<collapsed_args>)`.
     - Bound the resulting preview to `RECENT_TOOL_CALL_PREVIEW_BYTES` (`160` UTF-8 bytes, cut on a UTF-8 char boundary with `"..."` inside the cap when shortened).
   - **Formatting each tool-result status (`format_recent_result_status`):**
     - Strip any trailing loop-injected `[WARN: step budget ...]` or `[WARN: repeated tool calls]...` suffix before classifying status, while scanning the raw result for any `[full output saved to ...]` spill footer line (`spill_footer`).
     - If `is_tool_result_stub(trimmed)` (`src/loops/agent.rs:1661`) is true, use `trimmed` bounded to `RECENT_TOOL_STATUS_PREVIEW_BYTES`.
     - Else if `trimmed.starts_with("[ERROR")` or `trimmed.starts_with("Error:")`:
       - When line 1 is `"[ERROR: execute_script failed]"`, format `"[ERROR: execute_script failed]: <first_error_line>"` using `first_error_line(trimmed)` (`src/loops/agent.rs:1163`).
       - When line 1 starts with `"[ERROR: exit code "` or `"[ERROR: command timed out"`, take line 1 and, if a non-empty detail line exists after `[diagnostics]` (or otherwise after line 1), append ` — <detail_line>`.
       - Otherwise take the first non-empty line of `trimmed`.
       - Bound that error text to `RECENT_TOOL_STATUS_PREVIEW_BYTES` (`240` UTF-8 bytes, `"..."` inside the cap on a UTF-8 char boundary), and append ` {spill_footer}` when a spill footer line is present and not already contained in the status text.
     - Else (successful tool result): render `"ok"` (or `"ok {spill_footer}"` when a spill footer line is present).
   - **Assembling and capping the trail:**
     - Render one line per retained call as `- {call_preview} -> {status_summary}`, joined with `"\n"`.
     - Bound the joined string to `RECENT_TOOL_TRAIL_MAX_BYTES` (`1_500` UTF-8 bytes, cut on a UTF-8 char boundary with `"..."` inside the `1_500`-byte cap when shortened).

### 2. Render `Recent tool activity:` in `[RETRY HANDOVER]` (`src/orchestrator.rs`)

1. Pass `first.tool_trail.as_deref()` from all three retry arms of `work_with_retry` (`src/orchestrator.rs:752-791`) into `retry_once` (`src/orchestrator.rs:803-819`) and `render_retry_handover` (`src/orchestrator.rs:893-921`).
2. In `render_retry_handover`:
   - Normalize `tool_trail.map(str::trim).filter(|text| !text.is_empty())`.
   - When `None`, omit the section completely so existing handovers without tool calls remain byte-identical.
   - When `Some(trail)`, render `Recent tool activity:\n{trail}\n\n` immediately after `First verification gate output:\n{gate}\n\n` and immediately before `Repository working-tree diff (staged, unstaged, and non-ignored untracked changes; .ejaj/ excluded):\n{diff}\n`.

### 3. Test Coverage (`src/loops/agent.rs`, `src/orchestrator.rs`, `tests/hybrid_pipeline.rs`)

- `src/loops/agent.rs`: caller-intent unit tests verifying `summarize_recent_tool_activity` across zero delivered calls (`None`), `> 3` calls (retains chronological last 3 and skips unanswered `task_complete`), `execute_script` and `bash` error lines plus `[full output saved to ...]` spill footers, and oversized multi-megabyte/multibyte UTF-8 arguments (`<= 1,500` bytes).
- `src/orchestrator.rs`: caller-intent unit tests verifying `render_retry_handover` omits `Recent tool activity:` when `None`/blank and places it immediately before `Repository working-tree diff` when populated.
- `tests/hybrid_pipeline.rs`: assert that Attempt 2's `[RETRY HANDOVER]` in the hybrid retry integration test includes `Recent tool activity:` from Attempt 1's delivered tool call.

---

## Affected Files

- `src/loops/agent.rs`
- `src/loops/code.rs`
- `src/orchestrator.rs`
- `tests/hybrid_pipeline.rs`
- `TODO.md`

## Verification

```bash
cargo test --lib loops::agent && cargo test --lib orchestrator && cargo test --test hybrid_pipeline && cargo test --test pipeline && cargo test --test test_titles && cargo clippy --all-targets -- -D warnings
```
