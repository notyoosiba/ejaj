# Proposal 26: Agent Loop Diagnostics, Tool/Sandbox Counters & Enriched `TrajectorySnapshot` (`src/loops/agent.rs`, `src/loops/mod.rs`, `src/loops/code.rs`, `src/orchestrator.rs`)

## Prerequisite

Depends on `T29` (`.plans/PROPOSAL-25-provider-token-usage-and-pricing-telemetry.md`).

## Problem

`TrajectorySnapshot` (`src/loops/agent.rs:152-160`) currently persists only six fields to `.ejaj/<timestamp>/trajectories/<phase>[-<N>].traj.json`: `phase`, `role`, `model`, `steps_taken`, `exit_status`, and `messages`. Four critical gaps hinder post-run analysis of agent loop behavior:

1. **`code.traj.json` / `code-<N>.traj.json` files are not linked to ticket IDs or attempt numbers (`src/loops/agent.rs:259-275, 1004-1064`, `src/orchestrator.rs:293-325`):**
   - In multi-ticket runs or retry rounds, numbered files (`code.traj.json`, `code-1.traj.json`, `code-2.traj.json`) omit which `ticket.id` was executed and whether the trajectory was Attempt 1 or Attempt 2 (handover retry).
2. **Compaction, overflow, truncation, and repetition interventions are invisible (`src/loops/agent.rs:801-1000, 1655-2040`):**
   - Tier 1 compactions (`> 400 KiB`), Tier 2 handover-summary turns (`> 200 KiB`), handover-summary validation failures, `force_compact_on_overflow` rescues, output-truncation recovery prompts (`MAX_TRUNCATION_RECOVERIES = 2`), and repeated-tool-call warnings/aborts (`3` / `5` identical turns) happen inside `run_agent_loop_inner` without structured counters.
3. **Tool, sandbox, spill, and control-handoff rejection rates require re-parsing multi-hundred-kilobyte `messages` arrays (`src/loops/agent.rs:545-755`):**
   - There are no structured counters for `execute_script` vs. `bash` usage, `SHELL_TIMEOUT` (120s) kills, QuickJS sandbox timeouts (`execution timed out`), `tools-output/` raw log spills (`> 16 KiB`), or `CallRunner::reject` rejections on `finish_plan` / `finish_spec` / `finish_tickets` / `finish_triage` / `finish_review` / `task_complete`.
4. **Loop wall time, HTTP retry counts/backoff, peak context size, and accumulated `TokenUsage` are not returned to callers (`src/loops/agent.rs:339-389`):**
   - `AgentAttempt` and `code::Attempt` carry only `stop`/`outcome`, `narration`, and `tool_activity`.

---

## Guardrails for Existing `ejaj` Tests & Clippy Policy

- **Preserve all existing `TrajectorySnapshot` top-level keys and skip rules (`src/loops/agent.rs:1004-1064, 8950-9040`, `tests/hybrid_pipeline.rs:922-960, 1096-1110`):**
  - Keep `phase`, `role`, `model`, `steps_taken`, `exit_status`, and `messages` with their exact names and JSON types.
  - Keep `save_trajectory` skipping `Phase::Plan` when `!matches!(stop, Stop::Handoff(_))`, skipping `None` `session_dir`, and skipping paths where `crate::tools::ejaj_ancestor(&base).is_none()`.
  - Serialize `ticket_id` and `attempt` with `#[serde(skip_serializing_if = "Option::is_none")]` so non-ticket phases (`plan`, `spec`, `tickets`, `triage`, `review`, `summary`) and direct loop unit tests omit them cleanly.
- **Preserve `InvocationContext::default()` and `InvocationContext::new(root, session_dir)` (`src/loops/agent.rs:259-275`, `src/orchestrator.rs:293-298`):**
  - Both existing constructors must compile unchanged with `ticket_id: None` and `attempt: None` so all 50+ existing call sites across `src/pipeline.rs` and `src/loops/*.rs` need zero edits.
  - Update the single struct literal `InvocationContext { root: Some(self.root), session_dir: self.session_dir, ticket_id: None, attempt: None }` in `Production::invocation` (`src/orchestrator.rs:294`).
- **Preserve `code::Attempt::new(outcome)` (`src/loops/code.rs:47-53`):**
  - Initialize `telemetry: LoopTelemetry::default()` in `Attempt::new(outcome)` so `Outcome::into()` and scripted `Lifecycle` test doubles (`src/orchestrator.rs:1501, 1940`) compile unchanged.
- **Enforce Clippy complexity (`<= 15`) and lint gates (`tests/guidance.rs`):**
  - `run_agent_loop_inner` (`src/loops/agent.rs:789`) and `prepare_for_send` (`src/loops/agent.rs:1655`) are already substantial functions! **Do not inline nested branching into `run_agent_loop_inner`.** Instead, encapsulate mutations as tiny 2–5 line methods on `LoopTelemetry` (`record_turn_context`, `record_http_attempt`, `record_http_retry`, `record_tool_run`, `record_tool_rejection`, etc.) so cognitive complexity in `run_agent_loop_inner` does not increase.
- **Enforce caller-intent test-title contract (`tests/test_titles.rs`):**
  - Name every new test `when_<action>_given_<context>_then_<outcome>` without any `INTERNAL_IDENTIFIERS` (`thinking_label`, `group_digits`, `provider_role`, `summarize`, `call_time`, `measured`, `new`, `with_giga`, etc.).
- **Scope tracker updates strictly to `TODO.md`:**
  - Mark `T30` `[x]` in `TODO.md` right before running the verification gate; do not edit `.plans/PROPOSAL-26-agent-loop-and-tool-telemetry-enrichment.md`.

---

## Proposed Design

### 1. `LoopTelemetry` Struct & Merge/Record Helpers (`src/loops/agent.rs`, re-exported from `src/loops/mod.rs`)

Define `LoopTelemetry` (`#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]`):

```rust
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LoopTelemetry {
    /// Wall-clock duration of the agent loop invocation in whole milliseconds
    /// (floored via `Duration::as_millis() as u64`).
    pub elapsed_ms: u64,
    /// Total provider HTTP send+decode attempts executed (retries and Tier 2
    /// handover-summary requests included).
    pub http_requests: usize,
    /// Transient HTTP/API retries scheduled via `retry::should_retry`.
    pub http_retries: usize,
    /// Total backoff duration slept across `http_retries`, in milliseconds.
    pub retry_backoff_ms: u64,
    /// Largest `prepared.context_bytes` sent across all turns in this loop.
    pub peak_context_bytes: usize,
    /// Accumulated provider-reported token usage across all decoded responses
    /// (Tier 2 handover summaries included).
    pub usage: crate::api::TokenUsage,
    /// Number of turns that ran Tier 1 history compaction (`> 400 KiB`).
    pub tier1_compactions: usize,
    /// Number of accepted Tier 2 tool-free handover-summary replacements.
    pub tier2_summaries: usize,
    /// Number of Tier 2 handover-summary requests that failed or were rejected
    /// by the `[HANDOVER SUMMARY]` validator.
    pub tier2_summary_failures: usize,
    /// Number of provider context-overflow errors recovered via
    /// `force_compact_on_overflow`.
    pub overflow_compactions: usize,
    /// Number of `TRUNCATION_RECOVERY_INSTRUCTION` prompts appended for
    /// output-truncated tool-free turns.
    pub truncation_recoveries: usize,
    /// Number of turns annotated with `REPEATED_TOOL_CALL_REDIRECT` (`>= 3`).
    pub repeated_turn_warnings: usize,
    /// Whether the loop aborted early on `REPEATED_TOOL_CALL_ABORT_THRESHOLD` (`>= 5`).
    pub repeated_turn_aborted: bool,
    /// Number of delivered tool results that carried the remaining-step warning
    /// (`MAX_STEPS - step <= STEP_WARN_REMAINING`).
    pub step_budget_warnings: usize,
    /// Number of `Phase::Plan` turns that suspended to wait for a user answer.
    pub user_question_turns: usize,
    /// Executed `bash` tool calls and outcomes.
    pub bash_calls: usize,
    pub bash_failures: usize,
    pub bash_timeouts: usize,
    pub bash_spills: usize,
    /// Executed `execute_script` tool calls and outcomes.
    pub script_calls: usize,
    pub script_failures: usize,
    pub script_timeouts: usize,
    /// Control-tool calls (`finish_plan`, `finish_spec`, `finish_tickets`,
    /// `finish_triage`, `finish_review`, `task_complete`) rejected by `Tool::parse`.
    pub control_tool_rejections: usize,
    /// Non-control tool calls rejected by `Tool::parse` (unknown tool name or
    /// malformed JSON arguments).
    pub tool_parse_rejections: usize,
}
```

Add `pub fn merge_from(&mut self, other: &LoopTelemetry)` using saturating addition on counters (`peak_context_bytes = self.peak_context_bytes.max(other.peak_context_bytes)`, `repeated_turn_aborted |= other.repeated_turn_aborted`, `self.usage.add_assign(other.usage)`). Re-export `pub use agent::LoopTelemetry;` from `src/loops/mod.rs`.

### 2. Small Helper Methods on `LoopTelemetry` (`src/loops/agent.rs`)

Keep all classification rules deterministic and flat:

1. **HTTP & Usage accounting (`send_turn_with_retry` at `src/loops/agent.rs:424`):**
   - Pass `telemetry: &mut LoopTelemetry` into `send_turn_with_retry`:
     - Increment `telemetry.http_requests += 1` on every `config.send(...)` attempt.
     - When `Ok(message)` is returned and `let Some(u) = message.usage`: `telemetry.usage.add_assign(u)`.
     - When `retry::should_retry(&error, attempt)` returns `Some(delay)`: increment `telemetry.http_retries += 1` and `telemetry.retry_backoff_ms = telemetry.retry_backoff_ms.saturating_add(delay.as_millis() as u64)`.
2. **Context & Compaction accounting (`prepare_for_send` at `src/loops/agent.rs:1655` and overflow branch at `src/loops/agent.rs:845`):**
   - Pass `telemetry: &mut LoopTelemetry` into `prepare_for_send` and `request_handover_summary`:
     - Update `telemetry.peak_context_bytes = telemetry.peak_context_bytes.max(prepared.context_bytes)` right before `prepare_for_send` returns `Ok(prepared)` (and after `force_compact_on_overflow` produces `compacted_prepared`).
     - Increment `telemetry.tier1_compactions += 1` whenever `compact_history(messages)?` succeeds inside `prepare_for_send`.
     - Inside `request_handover_summary` (`src/loops/agent.rs:1935`): increment `telemetry.http_requests += 1` (and retry count/usage inside `send_summary_with_retry`); if validation succeeds, increment `telemetry.tier2_summaries += 1`; if `request_handover_summary` errors in `prepare_for_send`, increment `telemetry.tier2_summary_failures += 1`.
     - In `run_agent_loop_inner` (`src/loops/agent.rs:847`): when `force_compact_on_overflow` returns `Ok(Some(compacted_prepared))`, increment `telemetry.overflow_compactions += 1`.
3. **Tool, Sandbox, Spill & Rejection accounting (`CallRunner` at `src/loops/agent.rs:510-755`):**
   - Hold `telemetry: &'b mut LoopTelemetry` on `CallRunner`:
     - In `run_command` (`src/loops/agent.rs:647`):
       - For `Tool::Bash(_)`: `telemetry.bash_calls += 1`. Inspect `result`:
         - If `result` text (Ok or Err) contains `SPILL_FOOTER_PREFIX` (`"[full output saved to"`), `telemetry.bash_spills += 1`.
         - If `Err(error)`: `telemetry.bash_failures += 1`; if `error.starts_with("[ERROR: command timed out")`, `telemetry.bash_timeouts += 1`.
       - For `Tool::ExecuteScript(_)`: `telemetry.script_calls += 1`. If `Err(error)`: `telemetry.script_failures += 1`; if `error.contains("execution timed out")`, `telemetry.script_timeouts += 1`.
     - In `reject` (`src/loops/agent.rs:705`):
       - Check `is_control_tool_name(call.function.name.as_str())` (`matches!(name, "finish_plan" | "finish_spec" | "finish_tickets" | "finish_triage" | "finish_review" | "task_complete")`).
       - Increment `telemetry.control_tool_rejections += 1` when true, else `telemetry.tool_parse_rejections += 1`.
     - In `deliver` (`src/loops/agent.rs:728`):
       - When `MAX_STEPS.saturating_sub(self.step) <= STEP_WARN_REMAINING`, increment `telemetry.step_budget_warnings += 1`.
4. **Loop Health & Elapsed Time (`run_agent_loop_capturing` / `run_agent_loop_inner` at `src/loops/agent.rs:760-1000`):**
   - Measure `crate::timing::measured(|| run_agent_loop_inner(...))` inside `run_agent_loop_capturing` and set `telemetry.elapsed_ms = elapsed.as_millis() as u64`.
   - Record `repeated_turn_aborted = true` on `abort_repeated_turn`, `truncation_recoveries += 1` when appending `TRUNCATION_RECOVERY_INSTRUCTION`, `repeated_turn_warnings += 1` on `redirect_repeated_turn`, and `user_question_turns += 1` when `waiting` returns `Stop::TurnEnd`.

### 3. Ticket Identity on `InvocationContext`, `AgentAttempt`, `code::Attempt` & `TrajectorySnapshot`

1. **`InvocationContext<'a>` (`src/loops/agent.rs:259-275`):**
   - Add `pub ticket_id: Option<&'a str>` and `pub attempt: Option<u8>`.
   - Add builder method:
     ```rust
     pub(crate) fn with_ticket_attempt(mut self, ticket_id: &'a str, attempt: u8) -> Self {
         self.ticket_id = Some(ticket_id);
         self.attempt = Some(attempt);
         self
     }
     ```
2. **Tag ticket attempts in `Production::work` (`src/orchestrator.rs:302-325`):**
   - Compute `let attempt_num = if handover.is_some() { 2 } else { 1 };` and pass `self.invocation().with_ticket_attempt(ticket.id.trim(), attempt_num)` to `session.run_ticket_attempt_in(...)`.
3. **Return `LoopTelemetry` on `AgentAttempt` (`src/loops/agent.rs:339`) and `code::Attempt` (`src/loops/code.rs:38`):**
   - Add `pub telemetry: LoopTelemetry` to both structs.
4. **Enrich `TrajectorySnapshot<'a>` (`src/loops/agent.rs:152-160, 1033`):**
   ```rust
   #[derive(serde::Serialize)]
   struct TrajectorySnapshot<'a> {
       phase: &'static str,
       role: &'static str,
       model: &'a str,
       #[serde(skip_serializing_if = "Option::is_none")]
       ticket_id: Option<&'a str>,
       #[serde(skip_serializing_if = "Option::is_none")]
       attempt: Option<u8>,
       steps_taken: usize,
       exit_status: &'static str,
       telemetry: &'a LoopTelemetry,
       messages: &'a [Message],
   }
   ```

### 4. Test Coverage

Add caller-intent (`when_<action>_given_<context>_then_<outcome>`) tests:
- `src/loops/agent.rs`:
  - Persisted `code.traj.json` with `with_ticket_attempt("t1", 1)` includes `"ticket_id": "t1"`, `"attempt": 1`, and `"telemetry"` counters while omitting `ticket_id`/`attempt` when `None`.
  - `LoopTelemetry` counts `http_requests`, `http_retries`, `usage` totals, `control_tool_rejections` vs `tool_parse_rejections`, `script_calls`/`script_failures`, `truncation_recoveries`, and `repeated_turn_warnings`/`repeated_turn_aborted`.
- `tests/hybrid_pipeline.rs`:
  - Assert `code.traj.json` carries `"attempt": 1` + `"ticket_id"` and `code-1.traj.json` carries `"attempt": 2` + the same `"ticket_id"`.

---

## Affected Files

- `src/loops/agent.rs`
- `src/loops/mod.rs`
- `src/loops/code.rs`
- `src/orchestrator.rs`
- `tests/hybrid_pipeline.rs`
- `TODO.md`

## Verification

```bash
cargo test --lib loops && cargo test --lib orchestrator && cargo test --test hybrid_pipeline && cargo test --test test_titles && cargo clippy --all-targets -- -D warnings
```
