# Proposal 1: Add `done` State to Triage for Already-Implemented Tickets

## Problem

`TriageState::Wontfix` (`src/triage.rs:94-98` and `src/prompts/triage.rs:45`) currently conflates two opposite conditions under one wire value (`wontfix`):
1. A ticket whose target behavior is **already implemented and covered** in the repository.
2. A ticket that is **out of scope, contradictory, or invalid** and must not be executed.

Because `authorizes_queue(&report)` (`src/pipeline.rs:960-966`) requires every draft ticket to be `ready-for-agent`, any ticket classified as `wontfix` blocks the entire draft set from running (`Step::Retry`). When an earlier run implements feature code without finishing downstream or tracker tickets, subsequent runs slice the feature into implementation tickets plus a final ticket, Triage marks the already-implemented tickets `wontfix` ("already implemented") and the remaining ticket `ready-for-agent`, and `ejaj` refuses to run the remaining ticket.

---

## Proposed Design

### 1. Add `TriageState::Done` (`src/triage.rs`)

Add `Done` (wire value `"done"`) to `TriageState`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum TriageState {
    /// The ticket is self-contained: the code agent can implement it now, so
    /// the classification carries an [AgentBrief] and no blocker explanation.
    ReadyForAgent,
    /// The ticket's behavior is already implemented and covered in the
    /// repository, so no code agent needs to run it and its dependents are
    /// unblocked. Carries a blocker explanation stating what existing
    /// implementation satisfies it, and no brief.
    Done,
    /// The ticket needs a human decision or human work before an agent can run
    /// it, so the classification carries a blocker explanation and no brief.
    ReadyForHuman,
    /// The ticket is under-specified and needs an answer from a human before an
    /// agent can run it, so the classification carries a blocker explanation and
    /// no brief.
    NeedsInfo,
    /// The ticket is out of scope or invalid, so this run will not execute it:
    /// the classification carries a blocker explanation and no brief.
    Wontfix,
}
```

- Update `TriageState::ALL` (to `[TriageState; 5]`) and `TriageState::wire(self)` to include `TriageState::Done => "done"`.
- Add `pub fn authorizes_queue(self) -> bool` returning `matches!(self, TriageState::ReadyForAgent | TriageState::Done)`.
- Keep `is_ready(self)` returning `matches!(self, TriageState::ReadyForAgent)` so `Done` continues to carry audit `notes` and `blocker_explanation` (naming the existing implementation and tests that satisfy it) without requiring a redundant `AgentBrief`.

### 2. Update `TRIAGE_SYSTEM_PROMPT` (`src/prompts/triage.rs`)

Split `done` and `wontfix` under `## States (the state field)`:

- `done`: the ticket is already implemented and covered in the repository, so no code agent needs to run it and any downstream ticket depending on it is unblocked. This state carries a `blocker_explanation` naming the existing implementation and tests that satisfy it, and no `AgentBrief`.
- `wontfix`: the ticket is out of scope or invalid for this run, so this run must not execute it. This state carries a `blocker_explanation` naming why, and no `AgentBrief`.

### 3. Unblock Downstream `ready-for-agent` Tickets in the Pipeline (`src/pipeline.rs`)

1. **Queue Authorization (`src/pipeline.rs:447, 861-885, 960-966`)**:
   - Update `authorizes_queue(report: &TriageReport) -> bool` to check `classification.state.authorizes_queue()` (`ReadyForAgent` or `Done`).
   - Update `triage_blockers(report: &TriageReport)` to filter by `!classification.state.authorizes_queue()` so `Done` tickets are never reported as blockers.
2. **Pre-completing `Done` Tickets Before `queue_and_review` (`src/pipeline.rs:463-543`)**:
   - Partition the validated `TicketSet` using the `TriageReport`:
     - Tickets classified as `TriageState::Done` are placed into `progress.prior_completed`.
     - Tickets classified as `TriageState::ReadyForAgent` remain in `progress.current_set`, with any `Done` ticket IDs removed from their `blocked_by` lists.
   - If `progress.current_set.tickets` is non-empty, run the queue loop as normal over the remaining `ReadyForAgent` tickets while preserving `progress.prior_completed` in `queue-status.json` and `queue-report.md`.
   - If *every* draft ticket in the set is classified `TriageState::Done`, write the resolved `queue-status.json` and `queue-report.md` from `progress.prior_completed` and proceed directly to `transition_gate` -> `review_round` -> `summary_pass`.

---

## Affected Files

- `src/triage.rs`
- `src/prompts/triage.rs`
- `src/pipeline.rs`
- `tests/pipeline.rs`

## Verification

```bash
cargo test --lib triage && cargo test --lib prompts::triage && cargo test --test pipeline && cargo clippy --all-targets -- -D warnings
```
