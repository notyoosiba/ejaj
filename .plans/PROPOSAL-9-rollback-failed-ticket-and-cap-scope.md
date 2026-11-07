# Proposal 9: Cap Monolithic Ticket Scope and Order Tracker Edits Last Before Verification Gate

## Problem

Two failure modes compound each other when a ticket fails mid-queue:

1. **Oversized single-ticket slicing:**
   When the Tickets agent bundles a multi-surface feature (e.g. 3 UI views + 15-18 BDD test cases across 3 test files) into a single ticket, slow test suites and cross-file interactions cause the Code agent to exhaust its 100-step budget (`MAX_STEPS = 100`).
2. **Failed tickets leave premature `TODO.md` / `.plan/` completion markers in the working tree:**
   When a ticket includes updating tracker/status files (`TODO.md`, `.plan/*.md`), the Code agent often marks `TODO.md` complete (`- [x]`) *before* running the verification gate. If the gate or review subsequently fails, the working tree falsely marks an unfinished task as done.

---

## Guardrails for Existing `ejaj` Tests

- **Do not edit `CODE_SYSTEM_PROMPT` in `src/prompts/code.rs`:** `tests/llm_wire.rs` records exact wire requests containing `CODE_SYSTEM_PROMPT`. Place the tracker-ordering instruction inside `CODE_TDD_DISCIPLINE` (or `CODE_TICKET_KICKOFF_PROMPT`) so `CODE_SYSTEM_PROMPT` stays byte-identical and `tests/fixtures/llm_wire/*.json` fixtures remain valid without re-recording.

---

## Proposed Design

### 1. Bound Per-Ticket Test & Surface Scope in `TICKETS_SYSTEM_PROMPT` (`src/prompts/tickets.rs`)

Under `## Workflow` -> `2. Slice the specification into vertical slices` (integrated into `02-tickets-and-triage-prompts.md`):
- Require the Tickets agent to split multi-surface features so a single ticket touches at most one primary user journey / entry point and carries a focused set of `test_expectations` (typically 3-8 BDD test cases in one test file), rather than bundling 15+ test expectations across multiple test suites into one ticket.

### 2. Order Tracker Updates Last in `CODE_TDD_DISCIPLINE` (`src/prompts/code.rs`)

In `CODE_TDD_DISCIPLINE` step 4 (`src/prompts/code.rs:115-117`, integrated into `03-code-and-review-prompts.md`):
- Instruct the Code agent that when a ticket includes updating tracker or status files (`TODO.md`, `.plan/*.md`), it must implement and verify all source and test changes **first**, update the tracker files only immediately before running the final `verification_gate`, and revert those tracker status edits if the gate fails.

---

## Affected Files

- `src/prompts/tickets.rs`
- `src/prompts/code.rs`

## Verification

```bash
cargo test --lib prompts::tickets && cargo test --lib prompts::code && cargo clippy --all-targets -- -D warnings
```
