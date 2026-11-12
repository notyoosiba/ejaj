# Proposal 27: Queue Time Attribution Breakdown, Transition-Gate Timing & Per-Ticket Attempt Metrics (`src/timing.rs`, `src/orchestrator.rs`, `src/pipeline.rs`)

## Prerequisite

Depends on `T30` (`.plans/PROPOSAL-26-agent-loop-and-tool-telemetry-enrichment.md`).

## Problem

Three gaps hide where queue time and ticket retries are spent:

1. **`QueueContributions` sub-buckets are computed and then discarded (`src/timing.rs:86-143`, `src/pipeline.rs:639-667`):**
   - `QueueContributions` already accumulates `model_attempts`, `executed_tools`, and `verification_gates` (currently marked `#[allow(dead_code)]` at `src/timing.rs:92`), yet `Pipeline::run_queue_round` only calls `contributions.residual(elapsed)` to print the console `overhead` line and drops all three sub-buckets.
2. **Per-ticket Attempt 1 vs. Attempt 2 outcomes, DAG depth, and `hybrid` escalation win rates are lost (`src/orchestrator.rs:738-835, 1056-1160`):**
   - `orchestrator::QueueResult` (`src/orchestrator.rs:227-235`) returns only `outcome` and the rendered `report` string.
   - Once a ticket finishes, `ejaj` forgets whether it passed on Attempt 1 (`Declared`) or required Attempt 2 (`retry_once`), why Attempt 1 failed (`task_complete` + gate failure vs. `TurnEnd` vs. `StepLimit` vs. `ContextGuard`), whether `Mode::Hybrid` escalated Attempt 2 from `mid` to `giga`, how many verification gates ran, and the ticket's initial downstream dependency chain (`Scheduler::chain`).
3. **`Pipeline::transition_gate` is untimed (`src/pipeline.rs:672-694`):**
   - Full-suite verification (`cargo test` / `npm test`) between queue resolution and review runs through `crate::tools::run_command_in(&self.root, command)` without recording its duration.

---

## Guardrails for Existing `ejaj` Tests & Clippy Policy

- **CRITICAL — Do NOT add or rename any stdout `elapsed_log` lines (`tests/pipeline.rs:7876-7884, 9745-9770`, `tests/hybrid_pipeline.rs:501`):**
  - `PHASE_LABELS` (`tests/pipeline.rs:7876`) and `when_queue_round_finishes_given_timing_reports_then_preserves_artifacts_and_outcome` (`tests/pipeline.rs:9757-9763`) assert the exact stdout elapsed labels:
    `["ticket-queue round 1", "ticket-queue round 1 overhead"]` and forbid any extra phase or round elapsed lines!
  - Measure `Pipeline::transition_gate` using `crate::timing::measured(|| crate::tools::run_command_in(&self.root, command))` (which writes **nothing** to stdout/stderr) and return `QueueTimingBreakdown` / `TicketExecutionMetrics` as structured data rather than printing new `[INFO] ... elapsed:` lines.
- **Preserve public `orchestrator::QueueResult` fields and `Lifecycle` test doubles (`src/orchestrator.rs:227-255, 1495-1520, 1935-1960`):**
  - Keep `pub outcome: Outcome` and `pub report: String` on `QueueResult`; add `pub(crate) round_metrics: QueueRoundMetrics` (defaulted in `record_precompleted` and non-timed callers) so existing external callers in `tests/` that read `result.outcome` and `result.report` work without modification.
  - Add a default method `fn attempt_role(&self, _is_retry: bool) -> &'static str { "mid" }` on `trait Lifecycle` (`src/orchestrator.rs:240`), overridden only on `impl Lifecycle for Production<'_>` (`src/orchestrator.rs:302`), so scripted `Lifecycle` doubles in unit tests need **zero** edits.
- **Enforce caller-intent test-title contract (`tests/test_titles.rs`):**
  - Every `#[test]` in `src/timing.rs` is checked by `when_describing_timing_behavior_given_operation_tests_then_uses_domain_level_titles` (`tests/test_titles.rs:221-240`)!
  - Every new test in `src/timing.rs`, `src/orchestrator.rs`, and `src/pipeline.rs` must follow `when_<action>_given_<context>_then_<outcome>` and must **never** include `measured`, `call_time`, `provider_role`, `new`, `with_giga`, or any other entry in `INTERNAL_IDENTIFIERS`.
- **Enforce Clippy complexity (`<= 15`) and lint gates (`tests/guidance.rs`):**
  - Keep `work_with_retry`, `retry_once`, and `run_queue_with_prior_contributions` decomposed into small helpers so every function stays at or below cognitive complexity 15.
- **Scope tracker updates strictly to `TODO.md`:**
  - Mark `T31` `[x]` in `TODO.md` right before running the verification gate; do not edit `.plans/PROPOSAL-27-queue-contributions-and-attempt-metrics.md`.

---

## Proposed Design

### 1. `QueueTimingBreakdown` on `QueueContributions` (`src/timing.rs`)

1. Remove `#[allow(dead_code)]` from `impl QueueContributions` (`src/timing.rs:92`).
2. Add `QueueTimingBreakdown` in `src/timing.rs`:
   ```rust
   /// Serializable millisecond breakdown of one queue round's wall time.
   #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
   pub struct QueueTimingBreakdown {
       pub total_ms: u64,
       pub model_attempts_ms: u64,
       pub executed_tools_ms: u64,
       pub verification_gates_ms: u64,
       pub overhead_ms: u64,
   }
   ```
3. Add `pub(crate) fn breakdown(&self, total_elapsed: Duration) -> QueueTimingBreakdown` on `QueueContributions`, converting `total_elapsed`, `self.model_attempts()`, `self.executed_tools()`, `self.verification_gates()`, and `self.residual(total_elapsed)` via `Duration::as_millis() as u64`.

### 2. Per-Ticket Execution & Escalation Metrics (`src/orchestrator.rs`)

1. Define `TicketExecutionMetrics` and `QueueRoundMetrics` in `src/orchestrator.rs`:
   ```rust
   #[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
   pub struct TicketExecutionMetrics {
       pub ticket_id: String,
       pub blocked_by_count: usize,
       /// Initial downstream chain length in this round's DAG (1 = leaf ticket).
       pub downstream_chain_len: usize,
       /// Number of code-agent attempts executed (`0`, `1`, or `2`).
       pub attempts_run: u8,
       #[serde(skip_serializing_if = "Option::is_none")]
       pub attempt1_role: Option<String>,
       /// Why Attempt 1 stopped: `"complete_gate_passed"`, `"complete_gate_failed"`,
       /// `"turn_end"`, `"step_limit"`, `"context_guard"`, `"api_error"`,
       /// `"transport_error"`, or `"malformed_response"`.
       #[serde(skip_serializing_if = "Option::is_none")]
       pub attempt1_outcome: Option<String>,
       #[serde(skip_serializing_if = "Option::is_none")]
       pub attempt2_role: Option<String>,
       /// True when Attempt 1 ran on `"mid"` and Attempt 2 escalated to `"giga"`.
       pub escalated_to_giga: bool,
       pub gate_runs: u8,
       pub gate_elapsed_ms: u64,
       /// `"completed"`, `"failed"`, `"blocked"`, or `"pending"`.
       pub final_status: String,
       /// `"declared"`, `"gate_accepted"`, `"gate_failed"`, `"retry_exhausted"`,
       /// `"agent_failure"`, `"blocked"`, or `"never_started"`.
       pub final_ending: String,
       #[serde(skip_serializing_if = "Option::is_none")]
       pub blocked_by_failed_ticket: Option<String>,
       /// Merged loop telemetry across Attempt 1 and (when run) Attempt 2.
       pub telemetry: crate::loops::LoopTelemetry,
   }

   #[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
   pub struct QueueRoundMetrics {
       pub timing: crate::timing::QueueTimingBreakdown,
       pub tickets: Vec<TicketExecutionMetrics>,
   }
   ```
2. **Role resolution on `Lifecycle` (`src/orchestrator.rs:240, 302`):**
   - Add `fn attempt_role(&self, _is_retry: bool) -> &'static str { "mid" }` to `trait Lifecycle`.
   - Implement on `Production<'_>`:
     ```rust
     fn attempt_role(&self, is_retry: bool) -> &'static str {
         if is_retry {
             self.config.code_retry_config().provider_role(crate::tools::Phase::Code)
         } else {
             self.config.provider_role(crate::tools::Phase::Code)
         }
     }
     ```
3. **Capture gate duration and count in `invoke_gate` (`src/orchestrator.rs:1018`):**
   - Return `(Result<(), String>, Duration)` from `invoke_gate` (or pass `&mut TicketExecutionMetrics`) so each ticket records `gate_runs += 1` and `gate_elapsed_ms = gate_elapsed_ms.saturating_add(elapsed.as_millis() as u64)` without changing the stdout `elapsed_log` line!
4. **Capture Attempt 1 & Attempt 2 outcomes in `work_with_retry` / `retry_once` (`src/orchestrator.rs:738-835`):**
   - Compute initial `downstream_chain_len = scheduler.chain(index)` for every ticket in `set.tickets` right after `let mut scheduler = Scheduler::new(set);` (before any ticket settles).
   - In `work_with_retry`, populate `attempts_run = 1`, `attempt1_role = Some(lifecycle.attempt_role(false).to_string())`, `telemetry.merge_from(&first.telemetry)`, and `attempt1_outcome`.
   - When entering `retry_once`, set `attempts_run = 2`, `let role2 = lifecycle.attempt_role(true);`, `attempt2_role = Some(role2.to_string())`, `escalated_to_giga = metrics.attempt1_role.as_deref() == Some("mid") && role2 == "giga"`, and `telemetry.merge_from(&second.telemetry)`.
   - After the `while scheduler.remaining() > 0` loop settles blocked tickets (`Ending::Blocked(blocker)`), populate `final_status`, `final_ending` (canonical snake_case tag), and `blocked_by_failed_ticket` for every ticket in `set.tickets`.
   - Store `pub(crate) round_metrics: QueueRoundMetrics` on `QueueResult`. In `Pipeline::run_queue_round` (`src/pipeline.rs:639-667`), set `result.round_metrics.timing = contributions.breakdown(elapsed);`.

### 3. Measure `Pipeline::transition_gate` Duration (`src/pipeline.rs:672-694`)

1. Add `pub transition_gate_runs: usize` and `pub transition_gate_elapsed_ms: u64` to `ReviewProgress` (`src/pipeline.rs`).
2. In `Pipeline::transition_gate` (`src/pipeline.rs:682`), wrap `crate::tools::run_command_in(&self.root, command)` in `crate::timing::measured(...)`:
   - Increment `progress.transition_gate_runs += 1`.
   - Accumulate `progress.transition_gate_elapsed_ms = progress.transition_gate_elapsed_ms.saturating_add(elapsed.as_millis() as u64)`.
   - Keep all stdout/stderr logs identical to today.

### 4. Test Coverage

Add caller-intent (`when_<action>_given_<context>_then_<outcome>`) tests:
- `src/timing.rs`: `QueueContributions::breakdown` preserves sub-bucket millisecond totals and saturates `overhead_ms` at `0` when attributed parts exceed `total_elapsed`.
- `src/orchestrator.rs`:
  - First-attempt `Declared` records `attempts_run == 1`, `attempt1_outcome == Some("complete_gate_passed")`, `gate_runs == 1`, and `downstream_chain_len`.
  - Handover retry after failed first gate records `attempts_run == 2`, `attempt1_outcome == Some("complete_gate_failed")`, `gate_runs == 2`, and `final_ending == "declared"` or `"gate_accepted"`.
  - Failed root ticket blocking a dependent records `final_ending == "blocked"` and `blocked_by_failed_ticket == Some(root_id)` with `attempts_run == 0`.
- `tests/hybrid_pipeline.rs` or `src/orchestrator.rs`: `Mode::Hybrid` retry sets `attempt1_role == Some("mid")`, `attempt2_role == Some("giga")`, and `escalated_to_giga == true`.

---

## Affected Files

- `src/timing.rs`
- `src/orchestrator.rs`
- `src/pipeline.rs`
- `TODO.md`

## Verification

```bash
cargo test --lib timing && cargo test --lib orchestrator && cargo test --lib pipeline && cargo test --test pipeline && cargo test --test hybrid_pipeline && cargo test --test test_titles && cargo clippy --all-targets -- -D warnings
```
