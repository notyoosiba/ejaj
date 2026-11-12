# Proposal 21: Escalate Ticket Retries to `giga` in `hybrid` Mode & Enrich `[RETRY HANDOVER]` (`src/config.rs`, `src/loops/agent.rs`, `src/loops/code.rs`, `src/orchestrator.rs`, `tests/common/mod.rs`, `tests/hybrid_pipeline.rs`)

## Problem

When a ticket's first code-agent attempt fails in `hybrid` mode (`TurnEnd`, `FailureKind::StepLimit`, `FailureKind::ContextGuard`, or `task_complete` followed by a failed first verification gate), `work_with_retry` (`src/orchestrator.rs:734-795`) runs `retry_once` (`src/orchestrator.rs:803-835`), which suffers from two limitations:

1. **`retry_once` stays on the `mid` provider in `hybrid` mode (`src/config.rs:195-215`, `src/orchestrator.rs:302-318`):**
   - In `Mode::Hybrid` (where `giga` is guaranteed present), `Config::provider(Phase::Code)` routes `Phase::Code` to `self.mid` (`chat-completions`, e.g., `deepseek-flash`).
   - Because `Production::work` always builds `code::Session::new(self.config, self.scope)`, Attempt 2 (`handover.is_some()`) runs on the exact same `mid` model that just failed Attempt 1, instead of escalating the single rescue attempt to the configured `giga` provider (`responses` wire format, `giga` role/model/pricing, and `giga` trajectory metadata).
2. **`[RETRY HANDOVER]` omits Attempt 1's recent tool calls, error lines, and spill paths (`src/loops/agent.rs:339-389`, `src/loops/code.rs:38-59, 164-168`, `src/orchestrator.rs:893-921`):**
   - When Attempt 1 stops at `StepLimit`, `ContextGuard`, or `TurnEnd` without reaching the verification gate, `first_gate_output` is `None` (`"(the first attempt did not reach a failed verification gate)"`), and `narration` only holds the last prose reply.
   - Even when Attempt 1 declared `task_complete` and failed the first gate, Attempt 2 cannot see which `bash` commands or `execute_script` calls Attempt 1 ran right before stopping, what `[ERROR: ...]` lines they produced, or which `.ejaj/<timestamp>/tools-output/step-<N>-call-<M>.log` spill file already holds the full command output on disk.

---

## Guardrails for Existing `ejaj` Tests

- **Preserve `Config::provider(Phase::Code)` and Attempt 1 `mid` routing in `hybrid` mode (`tests/hybrid_pipeline.rs`):**
  - `assert_hybrid_routing` (`tests/hybrid_pipeline.rs:63-87`), `when_running_given_recorded_hybrid_pipeline_then_preserves_giga_mid_and_smoll_routes` (`tests/hybrid_pipeline.rs:200-267`), `when_routing_given_each_hybrid_phase_then_uses_its_established_endpoint` (`tests/hybrid_pipeline.rs:272-295`), `when_building_given_explicit_giga_settings_then_uses_canonical_builder` (`tests/hybrid_pipeline.rs:301-342`), and `when_triaging_tickets_given_hybrid_mode_then_routes_the_audit_to_giga` (`tests/hybrid_pipeline.rs:409-460`) assert that `config.provider(Phase::Code)` is `"chat-completions"` (`mid`) and that first-attempt `Phase::Code` requests in `hybrid` mode go to `mid`.
  - Keep `Config::provider(Phase::Code)` and Attempt 1 (`handover.is_none()`) routed to `mid`; escalate to `giga` **only** when `handover.is_some()` (Attempt 2 / `retry_once`).
- **Preserve `Mode::Mid`, `Mode::Giga`, and `Phase::Summary` routing (`src/config.rs:195-300`):**
  - `Mode::Mid` retries stay on `Mode::Mid`; `Mode::Giga` retries stay on `Mode::Giga`; `Phase::Summary` continues to route exclusively to Smoll in every mode.
- **Preserve all existing `retry_tests.rs` and `src/orchestrator.rs` invariants (`tests/support/retry_tests.rs`, `src/orchestrator.rs`):**
  - Keep `retries_after_first_stop` (`src/orchestrator.rs:722-724`) limited to `FailureKind::StepLimit | FailureKind::ContextGuard` (plus `TurnEnd` and first-gate failure in `work_with_retry`), so fatal `Api`, `Transport`, and `Malformed` stops (`tests/support/retry_tests.rs:706, 810`) still skip retry and gate execution.
  - Keep `code::Attempt::new(outcome)` (`src/loops/code.rs:47-53`) defaulting `narration: None` and `tool_trail: None` so `Outcome::into()` and scripted `Lifecycle` mocks (`Forwarding` at `src/orchestrator.rs:1479`, `Script` at `src/orchestrator.rs:1845`) compile and behave unchanged.
  - Omit `Recent tool activity:` completely when `tool_trail` is `None` or whitespace-only so tool-free first attempts (`tests/support/retry_tests.rs:188, 291, 394`) emit the exact same `[RETRY HANDOVER]` bytes as today.
  - Place `Recent tool activity:\n{trail}\n\n` **before** `Repository working-tree diff (staged, unstaged, and non-ignored untracked changes; .ejaj/ excluded):` so `when_retrying_given_failed_first_gate_then_preserves_original_kickoff_diagnostic_diff_and_gate_count` (`tests/support/retry_tests.rs:256-270`), which splits `[RETRY HANDOVER]` on `"Repository working-tree diff"` and forbids `".ejaj"` inside the diff section, continues to pass even when `trail` cites a `.ejaj/.../tools-output/...` spill path.
  - Bound raw tool-argument inspection so multi-megabyte adversarial arguments (`tests/support/retry_tests.rs:561`, ~4.9 MB `oversized_arguments` for `ContextGuard`) are never parsed in full by `serde_json` and never exceed the `1,500`-byte `tool_trail` cap.
- **Preserve caller-intent test-title rules (`tests/test_titles.rs`):**
  - Every new test must follow `when_<action>_given_<context>_then_<outcome>` and avoid forbidden internal/builder identifiers (`new`, `with_giga`, `provider_role`, etc.).
- **Scope tracker updates strictly to `TODO.md`:**
  - Update `TODO.md` only; do not edit `.plans/PROPOSAL-21-hybrid-code-retry-giga-escalation.md` during implementation.

---

## Proposed Design

### 1. Escalate `Phase::Code` Retry Attempts to `giga` in `hybrid` Mode (`src/config.rs`, `src/orchestrator.rs`)

1. Add `pub(crate) fn for_code_retry(&self) -> Config` on `Config` (`src/config.rs`):
   ```rust
   /// Configuration for a ticket's second (retry) code-agent attempt:
   /// in `Mode::Hybrid` (where `giga` is configured), escalate the retry to
   /// `Mode::Giga` so the `giga` provider rescues a ticket that `mid` did not
   /// finish. `Mode::Mid` and `Mode::Giga` keep their existing mode unchanged.
   pub(crate) fn for_code_retry(&self) -> Config {
       let mut retry_config = self.clone();
       if retry_config.mode == llm::Mode::Hybrid && retry_config.giga.is_some() {
           retry_config.mode = llm::Mode::Giga;
       }
       retry_config
   }
   ```
   Because `provider`, `provider_role`, `provider_model`, `input_price`, and trajectory persistence (`src/loops/agent.rs:1025-1032`) all resolve from `self.mode`, Attempt 2 in `hybrid` mode automatically dispatches over the `responses` wire format to the `giga` endpoint, reports `giga/<giga_model>` on the status line and in `trajectories/code-*.traj.json`, and prices input tokens using `giga`'s configured rate.
2. In `Production::work` (`src/orchestrator.rs:302-318`), select `self.config.for_code_retry()` when `handover.is_some()`:
   ```rust
   let selected_config = if handover.is_some() {
       self.config.for_code_retry()
   } else {
       self.config.clone()
   };
   let session = code::Session::new(&selected_config, self.scope);
   ```

### 2. Capture Attempt 1's Recent Tool & Error Trail (`src/loops/agent.rs`, `src/loops/code.rs`)

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
     - If `is_tool_result_stub(trimmed)` is true, use `trimmed` bounded to `RECENT_TOOL_STATUS_PREVIEW_BYTES`.
     - Else if `trimmed.starts_with("[ERROR")` or `trimmed.starts_with("Error:")`:
       - When line 1 is `"[ERROR: execute_script failed]"`, format `"[ERROR: execute_script failed]: <first_error_line>"` using `first_error_line(trimmed)` (`src/loops/agent.rs:1163`).
       - When line 1 starts with `"[ERROR: exit code "` or `"[ERROR: command timed out"`, take line 1 and, if a non-empty detail line exists after `[diagnostics]` (or otherwise after line 1), append ` — <detail_line>`.
       - Otherwise take the first non-empty line of `trimmed`.
       - Bound that error text to `RECENT_TOOL_STATUS_PREVIEW_BYTES` (`240` UTF-8 bytes, `"..."` inside the cap on a UTF-8 char boundary), and append ` {spill_footer}` when a spill footer line is present and not already contained in the status text.
     - Else (successful tool result): render `"ok"` (or `"ok {spill_footer}"` when a spill footer line is present).
   - **Assembling and capping the trail:**
     - Render one line per retained call as `- {call_preview} -> {status_summary}`, joined with `"\n"`.
     - Bound the joined string to `RECENT_TOOL_TRAIL_MAX_BYTES` (`1_500` UTF-8 bytes, cut on a UTF-8 char boundary with `"..."` inside the `1_500`-byte cap when shortened).

### 3. Render `Recent tool activity:` in `[RETRY HANDOVER]` (`src/orchestrator.rs`)

1. Pass `first.tool_trail.as_deref()` from all three retry arms of `work_with_retry` (`src/orchestrator.rs:752-791`) into `retry_once` (`src/orchestrator.rs:803-819`) and `render_retry_handover` (`src/orchestrator.rs:893-921`).
2. In `render_retry_handover`:
   - Normalize `tool_trail.map(str::trim).filter(|text| !text.is_empty())`.
   - When `None`, omit the section completely so existing handovers without tool calls remain byte-identical.
   - When `Some(trail)`, render `Recent tool activity:\n{trail}\n\n` immediately after `First verification gate output:\n{gate}\n\n` and immediately before `Repository working-tree diff (staged, unstaged, and non-ignored untracked changes; .ejaj/ excluded):\n{diff}\n`.

### 4. Scripted Responses-API Harness Seam & Test Coverage (`tests/common/mod.rs`, `tests/hybrid_pipeline.rs`, unit tests)

1. **Responses-API empty `TurnEnd(None)` support in `ScriptedTransport` (`tests/common/mod.rs:1741-1762`):**
   - In `code_reply_response`, when `is_responses` is `true` and `output.is_empty()` (`CodeReply::TurnEnd(None)` routed to `giga`), push an empty assistant message item (`{"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": ""}]}`) instead of returning `{"output": []}` (which `responses_api` rejects as `Malformed`), so scripted `TurnEnd(None)` replies behave identically on both `mid` and `giga`.
2. **Test coverage:**
   - `src/config.rs`: unit tests verifying `for_code_retry` escalates `Mode::Hybrid` (with `giga`) to `Mode::Giga` (`responses` provider, `"giga"` role, `giga` model, `giga` input price) while preserving `Mode::Mid`, `Mode::Giga`, and `Phase::Summary` Smoll routing.
   - `src/loops/agent.rs`: unit tests verifying `summarize_recent_tool_activity` across zero delivered calls (`None`), `> 3` calls (retains chronological last 3 and skips unanswered `task_complete`), `execute_script` and `bash` error lines plus `[full output saved to ...]` spill footers, and oversized multi-megabyte/multibyte UTF-8 arguments (`<= 1,500` bytes).
   - `src/orchestrator.rs`: unit tests verifying `render_retry_handover` omits `Recent tool activity:` when `None`/blank and places it immediately before `Repository working-tree diff` when populated.
   - `tests/hybrid_pipeline.rs`: end-to-end hybrid retry integration test verifying Attempt 1 runs on `mid` (`common::test_url()`), fails after a tool call, and Attempt 2 escalates to `giga` (`common::test_giga_url()`, Responses API wire format) carrying `[RETRY HANDOVER]` with `Recent tool activity:` before `Review` and `Summary` complete the run.

---

## Affected Files

- `src/config.rs`
- `src/loops/agent.rs`
- `src/loops/code.rs`
- `src/orchestrator.rs`
- `tests/common/mod.rs`
- `tests/hybrid_pipeline.rs`
- `TODO.md`

## Verification

```bash
cargo test --lib config && cargo test --lib loops::agent && cargo test --lib orchestrator && cargo test --test hybrid_pipeline && cargo test --test pipeline && cargo test --test test_titles && cargo clippy --all-targets -- -D warnings
```
