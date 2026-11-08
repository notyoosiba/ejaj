# Proposal 2: Retry Unattended Handoff Passes (`review_round`, `spec_pass`, `tickets_pass`, `triage_pass`) Once on Transient Stops

## Problem

In `src/pipeline.rs`, all four unattended structured-handoff passes run once with no outer retry when the model stops with `TurnEnd` or `Failed(FailureKind::Malformed | FailureKind::StepLimit | FailureKind::ContextGuard)`:
- `Pipeline::spec_pass` (`src/pipeline.rs:288-333`)
- `Pipeline::tickets_pass` (`src/pipeline.rs:336-377`)
- `Pipeline::triage_pass` (`src/pipeline.rs:381-428`)
- `Pipeline::review_round` (`src/pipeline.rs:679-714`)

Unlike the code agent queue (`orchestrator::work_with_retry` in `src/orchestrator.rs:645-706`), a single early stop from any of these unattended passes immediately aborts the pipeline run:
- In `spec_pass`, `tickets_pass`, or `triage_pass`, it drops back to `plan>>` even when a fresh retry succeeds immediately.
- In `review_round`, it terminates the entire run as `RunTermination::ReviewIncomplete`, skips `git::finalize`, and leaves all completed ticket changes uncommitted in the working tree.

---

## Relationship to `07-in-loop-truncation-and-tool-recovery.md`

- **Layer 1 (In-Loop Recovery, `07`):** Inside `run_agent_loop_capturing` (`src/loops/agent.rs`), if an unattended turn omits a tool call or hits `max_tokens`, the loop appends a corrective user message and continues up to `MAX_CONSECUTIVE_FORMAT_RECOVERIES = 2` times without losing the in-memory transcript.
- **Layer 2 (Outer Pass Retry, this proposal):** If a pass still exits with `TurnEnd`, `FailureKind::Malformed` (an unparseable provider HTTP payload), `FailureKind::StepLimit`, or `FailureKind::ContextGuard`, `Pipeline` retries the pass **once** in a fresh transcript before failing.

---

## Proposed Design

### 1. Shared Retry Eligibility Helper (`src/pipeline.rs`)

Keep `FailureKind::Api` and `FailureKind::Transport` non-retryable at the pass level because `agent::run_agent_loop_in` already exhausts transport/API retries via `crate::retry::should_retry`:

```rust
fn retries_after_unattended_stop(kind: Option<FailureKind>) -> bool {
    match kind {
        // TurnEnd (None) or a bounded/malformed model stop earns one fresh retry.
        None => true,
        Some(
            FailureKind::Malformed | FailureKind::StepLimit | FailureKind::ContextGuard,
        ) => true,
        // Transport and API errors already exhausted crate::retry::should_retry.
        Some(FailureKind::Api | FailureKind::Transport) => false,
    }
}
```

### 2. Retry Once in `review_round`, `spec_pass`, `tickets_pass`, and `triage_pass` (`src/pipeline.rs`)

When the first attempt in `review_round`, `spec_pass`, `tickets_pass`, or `triage_pass` returns `TurnEnd` (`None`) or `Failed(kind)` where `retries_after_unattended_stop` is `true`:
1. Log a `[WARN]` line to `status::shared().stderr_log` noting the early stop reason and that the pass is being retried once in a fresh session.
2. Run the pass (`review::audit_in`, `spec::to_spec_in`, `tickets::to_tickets_in`, or `triage::audit_in`) a second time with a fresh transcript **inside the same `status::timed(...)` interval** so each pass still reports exactly one timing line.
3. If the second attempt hands over a valid artifact, continue normally; if it fails again, return the existing termination (`RunTermination::ReviewIncomplete` or `self.downstream_retry(...)`).

---

## Guardrails for Existing `ejaj` Tests

- **Keep the retry inside the single `status::timed(...)` call for each pass:**
  - `when_phase_is_not_invoked_given_early_pipeline_return_then_emits_no_phase_line` (`tests/pipeline.rs:9291`) runs `"triage-turn-end"` (`common::TriageScript::TurnEnd`) and asserts `phase_labels(&transcript) == ["plan-agent ask", "spec pass", "tickets pass", "triage pass"]`. Running the retry inside the `status::timed("triage pass", || ...)` closure (just as `summary_pass` runs its corrections inside one `status::timed("summary pass", || ...)` closure) preserves that single timing line.
- **Update the `TurnEnd` request count in `when_triaging_tickets_given_missing_or_invalid_handoff_then_returns_retry_without_queue` (`tests/pipeline.rs:2461-2484`):**
  - Because `common::TriageScript::TurnEnd` is retried once while `common::TriageScript::ApiError` is not retried, update `tests/pipeline.rs:2478` to assert `5` requests (3 handoff steps + 2 triage attempts) for `common::TriageScript::TurnEnd` and `4` requests (3 handoff steps + 1 refused triage attempt) for `common::TriageScript::ApiError`.
- **Scope tracker updates strictly to `TODO.md`:**
  - Update `TODO.md` only; do not edit `.plans/PROPOSAL-2-review-pass-retry.md` during implementation.

---

## Affected Files

- `src/pipeline.rs`
- `tests/pipeline.rs`
- `TODO.md`

## Verification

```bash
cargo test --lib pipeline && cargo test --test pipeline && cargo test --test test_titles && cargo clippy --all-targets -- -D warnings
```
