# Proposal 32: Bounded `tickets ↔ triage` Self-Repair Pass (`src/loops/tickets.rs`, `src/prompts/tickets.rs`, `src/pipeline.rs`, `src/metrics.rs`)

## Problem

`ejaj` already encloses two downstream boundaries in bounded, unattended self-repair loops:
- `queue ↔ review` (`src/pipeline.rs`): `review` emits targeted fix tickets back into the queue rather than aborting to `plan>>`.
- `summary` (`src/loops/summary.rs`): up to 4 candidate/canonicalizer correction turns run inside one summary phase.

Between `tickets` and `triage`, however, **zero** repair turns exist (`src/pipeline.rs:346-360, 745-785`):
- Whenever `triage_pass` returns a `TriageReport` with `!authorizes_queue(&report)`, the pipeline immediately aborts to `plan>>` with `Step::Retry`.
- When the blocker is a purely internal ticket-slicing defect—such as a missing `blocked_by` edge, a mismatch between `test_expectations` and `verification_gate`, or a signature mismatch between producer `scope_of_changes` and consumer `prerequisites_assumed`—no human decision is actually missing: `Specification` and the repository already contain every answer needed to fix the draft `TicketSet`.

---

## Guardrails for Existing `ejaj` Tests & Clippy Policy

- **Never auto-repair genuine human/scope blockers (`src/triage.rs:95-114`):**
  - If **any** ticket in `TriageReport` is classified `TriageState::NeedsInfo` (genuinely under-specified question for a human) or `TriageState::Wontfix` (invalid/out-of-scope work), skip ticket repair and return `Err(self.downstream_retry(plan, "triage", triage_blockers(&report)))` immediately—exactly as existing `tests/pipeline.rs` triage-refusal tests expect.
  - Trigger repair **only** when `!authorizes_queue(&report)` and every non-authorizing ticket has `state == TriageState::ReadyForHuman` (or introduce an explicit `is_ticket_repairable(&report)` predicate).
  - *Compatibility note:* Audit existing `tests/pipeline.rs` tests that script `ReadyForHuman` triage outcomes with single-shot scripted transports; either gate repair on transport/repair availability or update/scope the repair trigger (for example, adding a dedicated `TriageState` or keeping repair bounded so a `TurnEnd`/`Failed` repair falls back cleanly to `downstream_retry(plan, "triage", ...)` with the original or latest triage blockers!).
- **Strict bound of at most 1 repair round per `Pipeline::ask`:**
  - Never loop indefinitely: run at most **1** initial `tickets + triage` pair and **1** repair `tickets + triage` pair per post-handoff invocation. If the post-repair `TriageReport` still fails `authorizes_queue`, emit the existing `[WARN] Triage did not mark every draft ticket ready-for-agent...` line and return `Step::Retry`.
- **Clean stale `tickets/*.md` files before writing repaired tickets (`src/pipeline.rs`):**
  - `src/pipeline.rs:566-567` requires `tickets/` to hold *"one markdown file per ticket id and nothing else"*. If the repaired `TicketSet` renames, splits, or merges ticket IDs, remove prior `tickets/*.md` files from `tickets_dir` before calling `write_tickets(&tickets_dir, &repaired.tickets)`.
- **Preserve all `TICKETS_SYSTEM_PROMPT` contract needles (`src/prompts/tickets.rs`, `tests/prompt_contract.rs`):**
  - Keep `TICKETS_SYSTEM_PROMPT` and `TICKETS_KICKOFF_PROMPT` intact; add a separate `TICKETS_REPAIR_KICKOFF_PROMPT` for the repair invocation.
- **Enforce Clippy complexity (`<= 15`) and caller-intent test titles (`tests/guidance.rs`, `tests/test_titles.rs`):**
  - Extract small helpers in `src/pipeline.rs` and name all new tests `when_<action>_given_<context>_then_<outcome>`.

---

## Proposed Design

### 1. Repair Entry Point in `src/loops/tickets.rs` and `src/prompts/tickets.rs`

1. Add `pub const TICKETS_REPAIR_KICKOFF_PROMPT: &str` in `src/prompts/tickets.rs`:
   - Instructs the Tickets agent that Triage audited its previous draft `TicketSet` against the `Specification` and repository and found blockers.
   - Directs it to fix only the blocked tickets' dependency declarations, `prerequisites_assumed`, `scope_of_changes`, `test_expectations`, or `verification_gate` against the unchanged `Specification` and live codebase, keeping already `ready-for-agent` / `done` slices intact unless fixing the contract requires updating a producer/consumer pair, then call `finish_tickets` with the complete corrected `TicketSet`.
2. Add `pub(crate) fn repair_tickets_in(config: &Config, scope: &str, specification: &Specification, previous: &TicketSet, triage: &TriageReport, invocation: InvocationContext<'_>) -> Outcome` in `src/loops/tickets.rs`:
   - Seeds a fresh `Phase::Tickets` transcript with `TICKETS_SYSTEM_PROMPT + scope` and a user message containing `TICKETS_REPAIR_KICKOFF_PROMPT`, `specification.to_markdown()`, the rendered previous draft tickets, and `triage.to_markdown(previous)`.

### 2. Single Bounded Repair Round in `Pipeline` (`src/pipeline.rs`)

In `Pipeline::ask` / a helper `tickets_and_triage_with_repair`:

1. Run initial `tickets_pass` → `write_tickets` → `triage_pass` (writing `ticket-triage.md`).
2. If `authorizes_queue(&report)` is `true`, return `Ok((set, report))`.
3. If `!is_repairable_by_tickets(&report)` (e.g., any classification is `NeedsInfo` or `Wontfix`):
   - Emit the existing stdout triage markdown + stderr `[WARN]` and return `Err(self.downstream_retry(plan, "triage", triage_blockers(&report)))`.
4. Otherwise, log `[INFO] Triage reported repairable ticket blockers; running one bounded tickets repair pass.` and invoke `tickets::repair_tickets_in` (wrapped in `status::timed("tickets repair pass", ...)`):
   - If `repair_tickets_in` does not hand off a `TicketSet`, fall back to reporting the initial `report` and returning `Err(self.downstream_retry(plan, "triage", triage_blockers(&report)))`.
   - If `repair_tickets_in` hands off `repaired_set`: delete old `*.md` files in `tickets_dir`, call `write_tickets(&tickets_dir, &repaired_set.tickets)?`, and run a second `triage_pass` (overwriting `ticket-triage.md` with the final audit).
   - If the second `TriageReport` authorizes the queue, proceed directly to `queue_and_review`; otherwise emit the existing `[WARN]` and return `Err(self.downstream_retry(plan, "triage", triage_blockers(&second_report)))`.

### 3. Metrics Accounting (`src/metrics.rs`, `src/pipeline.rs`)

- Accumulate elapsed time across `tickets` + `triage` passes (or add optional `repair_attempted: bool` / `repair_succeeded: bool` fields with `#[serde(default, skip_serializing_if = ...)]` on `TriageMetrics` if schema v2 backwards compatibility is kept, or bump `METRICS_SCHEMA_VERSION` consistently across `src/metrics.rs` tests). Trajectory-based `provider_roles` token/request totals already aggregate every `.traj.json` file automatically.

### 4. Test Coverage

- `src/loops/tickets.rs`: unit test verifying `repair_tickets_in` forwards `Specification`, prior draft tickets, and `TriageReport` markdown under `Phase::Tickets`.
- `tests/pipeline.rs` & `tests/hybrid_pipeline.rs`:
  - `when_triage_flags_ready_for_human_dependency_gap_given_valid_repair_handoff_then_queue_runs_and_completes`: reproduces the exact Ticket 04 scenario end-to-end without returning `Step::Retry`, verifies stale `tickets/*.md` cleanup, and checks `hybrid` provider routing (`giga` for repair + re-triage, `mid` for code).
  - `when_triage_flags_needs_info_or_second_triage_still_blocks_given_draft_set_then_returns_to_plan_prompt`: confirms `NeedsInfo`/`Wontfix` never trigger repair and a second blocked triage stops after 1 repair attempt.

---

## Affected Files

- `src/prompts/tickets.rs`
- `src/loops/tickets.rs`
- `src/pipeline.rs`
- `src/metrics.rs`
- `tests/pipeline.rs`
- `tests/hybrid_pipeline.rs`

## Verification

```bash
cargo test --lib loops::tickets && cargo test --lib prompts::tickets && cargo test --lib pipeline && cargo test --test pipeline && cargo test --test hybrid_pipeline && cargo test --test test_titles && cargo clippy --all-targets -- -D warnings
```
