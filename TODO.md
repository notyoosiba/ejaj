# TODO — `.plans/` Implementation Status & Task DAG

Tracks the implementation status of active proposals in [`.plans/`](./.plans/README.md) as an ordered Directed Acyclic Graph (DAG) of implementation tasks. All prior specifications and proposals (`01`–`12`, `PROPOSAL-1`–`PROPOSAL-14`, baseline `PROPOSAL-10`–`12`) have been completed, verified in `git` history (`T01`–`T19`), and removed from `.plans/`.

---

## 1. Active Proposals Status Matrix

| Proposal | Status | Mapped DAG Task | Target Modules |
| --- | --- | --- | --- |
| [`PROPOSAL-17-high-signal-bash-failure-digest.md`](./.plans/PROPOSAL-17-high-signal-bash-failure-digest.md) | `[x]` Complete (T20 done) | `T20` | `src/tools.rs` |
| [`PROPOSAL-16-ticket-test-cap-and-batched-tdd.md`](./.plans/PROPOSAL-16-ticket-test-cap-and-batched-tdd.md) | `[x]` Complete (T21 done) | `T21` | `src/prompts/tickets.rs`, `src/prompts/code.rs` |
| [`PROPOSAL-15-ticket-criteria-validation.md`](./.plans/PROPOSAL-15-ticket-criteria-validation.md) | `[x]` Complete (T22 done) | `T22` | `src/tickets.rs`, `src/tools.rs` |
| [`PROPOSAL-18-spec-and-tickets-story-slicing.md`](./.plans/PROPOSAL-18-spec-and-tickets-story-slicing.md) | `[x]` Complete (T23 done) | `T23` | `src/prompts/spec.rs`, `src/prompts/tickets.rs`, `tests/prompt_contract.rs` |
| [`PROPOSAL-19-triage-slice-criteria-narrowing.md`](./.plans/PROPOSAL-19-triage-slice-criteria-narrowing.md) | `[ ]` Pending | `T24` | `src/prompts/triage.rs`, `tests/prompt_contract.rs` |

---

## 2. Task Dependency Graph (DAG)

The pending tasks form a strictly ordered, serialized pipeline designed to stay well within the 100 tool-call ceiling per ticket:

1. **`T20` (Diagnostics First) — `[x]` Complete (T20 done):** Hardens `form_bash_digest` in `src/tools.rs` so any test or compiler failure in subsequent tasks receives an immediate, high-signal summary at the top of the digest instead of being buried under passing-test `[ERROR]` logs.
2. **`T21` (Discipline Second) — `[x]` Complete (T21 done):** Caps per-ticket test functions at 3–6 in `TICKETS_SYSTEM_PROMPT` and introduces batched TDD / early full-suite execution in `CODE_TDD_DISCIPLINE` so ticket execution avoids tool-call expansion.
3. **`T22` (Data Model Third) — `[x]` Complete (T22 done):** Enforces duplicate criterion rejection in `TicketSet::validate` and clarifies multi-ticket error messaging in `missing_criteria_error` before phase prompts teach models the rule.
4. **`T23` (Story Slicing Prompts Fourth):** Guides upstream authoring in `SPEC_SYSTEM_PROMPT` (vertical slices, explicit tracker paths) and `TICKETS_SYSTEM_PROMPT` (authorized tracker folding, verbatim slice criteria distribution).
5. **`T24` (Triage Brief Narrowing Fifth):** Guides downstream triage in `TRIAGE_SYSTEM_PROMPT` to narrow `Agent Brief` criteria for shared-story slices without blocking with `needs-info`.

```text
[T20] [x] Complete (T20 done) PROPOSAL-17: High-signal test/compiler failure extraction in form_bash_digest (src/tools.rs)
  │
  ▼
[T21] [x] Complete (T21 done) PROPOSAL-16: Consolidate boundary test_expectations & batch TDD slice runs (src/prompts/{tickets,code}.rs)
  │
  ▼
[T22] [x] Complete (T22 done) PROPOSAL-15: Duplicate criteria validation in TicketSet::validate & multi-ticket error (src/{tickets,tools}.rs)
  │
  ▼
[T23] [x] Complete (T23 done) PROPOSAL-18: Story slicing, per-ticket criteria scoping & authorized tracker folding (src/prompts/{spec,tickets}.rs)
  │
  ▼
[T24] PROPOSAL-19: Narrow shared-story criteria in Agent Brief during Triage (src/prompts/triage.rs)
```

---

## 3. Ordered Task Backlog

- [x] **`T20` — High-Signal Test and Compiler Failure Extraction in `form_bash_digest`**
  - `depends_on`: `[]`
  - `unblocks`: `["T21"]`
  - `plans`: [`.plans/PROPOSAL-17-high-signal-bash-failure-digest.md`](./.plans/PROPOSAL-17-high-signal-bash-failure-digest.md)
  - `files`: `src/tools.rs`, `TODO.md`
  - `tasks`:
    - In `src/tools.rs` (`diagnostic_text` / `is_diagnostic_line`), separate primary test/compiler failure blocks (`test ... ... FAILED`, `failures:`, `test result: FAILED`, `---- <test> stdout ----`, `panicked at`, `panic:`, `error[`, `error:`, `[error:`) from secondary `[ERROR]`/failure log lines while excluding `[WARN` lines, and order primary failure blocks ahead of secondary blocks so Cargo's failure summary is never crowded out by passing-test `[ERROR]` logs.
    - Bound multi-line diagnostic blocks at `DIAGNOSTIC_BLOCK_MAX_LINES = 25` lines and stop block continuation at routine harness/test progress lines (`#thinking `, `[INFO] `, `[WARN] `, `=== `, `test ... ... ok`), treat `test ... ... FAILED` as a single-line record, and cap individual diagnostic lines at `DIAGNOSTIC_LINE_MAX_BYTES = 512` UTF-8 bytes so a multi-kilobyte single-line panic payload cannot exhaust `BASH_DIGEST_DIAGNOSTIC_BYTES` (`8 KiB`).
    - In `TODO.md` only (do not edit `.plans/PROPOSAL-17-high-signal-bash-failure-digest.md`), mark `T20` complete (`[x]`) in the DAG and ordered backlog and update the `PROPOSAL-17-high-signal-bash-failure-digest.md` row in the `TODO.md` status matrix to `[x] Complete (T20 done)` right before running `verification_gate`.
  - `verification_gate`: `cargo test --lib tools && cargo test --test test_titles && cargo clippy --all-targets -- -D warnings`

- [x] **`T21` — Consolidate Per-Ticket Boundary `test_expectations` & Batch TDD Slice Execution (T21 done)**
  - `depends_on`: `["T20"]`
  - `unblocks`: `["T22"]`
  - `plans`: [`.plans/PROPOSAL-16-ticket-test-cap-and-batched-tdd.md`](./.plans/PROPOSAL-16-ticket-test-cap-and-batched-tdd.md)
  - `files`: `src/prompts/tickets.rs`, `src/prompts/code.rs`, `TODO.md`
  - `tasks`:
    - In `TICKETS_SYSTEM_PROMPT` (`src/prompts/tickets.rs`, preserving the 11 `- `<field>`: ` bullets in order, single-source `expand, migrate, contract`, and all existing test needles), instruct `- test_expectations:` to consolidate applicable 5-dimension boundary cases into table-driven `Edge-Case Tables` test functions rather than emitting separate duplicate per-dimension `Boundary Matrix` test functions for the same behavior, keeping each ticket to a compact set of distinct test functions (typically 3–6 `when_<action>_given_<context>_then_<outcome>` tests) and splitting into an additional ticket when more test functions are needed.
    - In `CODE_TDD_DISCIPLINE` (`src/prompts/code.rs`, keeping `CODE_SYSTEM_PROMPT` byte-identical and preserving `DISCIPLINE_LITERAL_PREFIX` and all existing test needles), update Step 1 and Step 9 to batch multiple `## Test Expectations` entries that exercise the same public seam and test harness into a single slice edit, and to run the full target test suite early (before tracker edits) so regressions in existing tests are caught and updated in one pass.
    - In `TODO.md` only (do not edit `.plans/PROPOSAL-16-ticket-test-cap-and-batched-tdd.md`), mark `T21` complete (`[x]`) in the DAG and ordered backlog and update the `PROPOSAL-16-ticket-test-cap-and-batched-tdd.md` row in the `TODO.md` status matrix to `[x] Complete (T21 done)` right before running `verification_gate`.
  - `verification_gate`: `cargo test --lib prompts::tickets && cargo test --lib prompts::code && cargo test --lib prompts::review && cargo test --test prompt_contract && cargo test --test llm_wire && cargo test --test test_titles && cargo clippy --all-targets -- -D warnings`

- [x] **`T22` — Duplicate Criteria Validation in `TicketSet::validate` & Multi-Ticket Error Format (T22 done)**
  - `depends_on`: `["T21"]`
  - `unblocks`: `["T23"]`
  - `plans`: [`.plans/PROPOSAL-15-ticket-criteria-validation.md`](./.plans/PROPOSAL-15-ticket-criteria-validation.md)
  - `files`: `src/tickets.rs`, `src/tools.rs`, `TODO.md`
  - `tasks`:
    - In `TicketSet::validate` (`src/tickets.rs`), reject duplicate non-blank `acceptance_criteria` entries (after trimming and collapsing consecutive whitespace) within the same ticket, returning an informative error naming the ticket and the duplicate criterion.
    - In `missing_criteria_error` (`src/tools.rs`), clarify that every criterion must appear in `acceptance_criteria` of the ticket (or across the tickets) that implement its story, placing each criterion only on the ticket whose scope implements it.
    - In `TODO.md` only (do not edit `.plans/PROPOSAL-15-ticket-criteria-validation.md`), mark `T22` complete (`[x]`) in the DAG and ordered backlog and update the `PROPOSAL-15-ticket-criteria-validation.md` row in the `TODO.md` status matrix to `[x] Complete (T22 done)` right before running `verification_gate`.
  - `verification_gate`: `cargo test --lib tickets && cargo test --lib tools && cargo test --test test_titles && cargo clippy --all-targets -- -D warnings`

- [x] **`T23` — Story Slicing, Per-Ticket Criteria Scoping & Authorized Tracker Folding in Spec & Tickets Prompts**
  - `depends_on`: `["T22"]`
  - `unblocks`: `["T24"]`
  - `plans`: [`.plans/PROPOSAL-18-spec-and-tickets-story-slicing.md`](./.plans/PROPOSAL-18-spec-and-tickets-story-slicing.md)
  - `files`: `src/prompts/spec.rs`, `src/prompts/tickets.rs`, `tests/prompt_contract.rs`, `TODO.md`
  - `tasks`:
    - In `SPEC_SYSTEM_PROMPT` (`src/prompts/spec.rs`, preserving all `tests/prompt_contract.rs` and `src/prompts/review.rs` needles and avoiding all forbidden phrases), update Step 3 to structure user stories along independently verifiable vertical slices with each acceptance criterion scoped to one slice or module boundary, and to name exact target file paths in `### Module Architecture` when documentation or tracker files are updated.
    - In `TICKETS_SYSTEM_PROMPT` (`src/prompts/tickets.rs`, preserving all existing test needles, the 11 field bullets in order, and single-source `expand, migrate, contract`), qualify Step 2 so tracker/documentation files (`README.md`, `TODO.md`, `.plan/`/`.plans/`) are folded only when the specification itself includes them, and clarify under `- acceptance_criteria:` that multi-ticket stories distribute only the verbatim criteria each ticket's `scope_of_changes` and `verification_gate` implement (never copying criteria for behavior in `out_of_scope` or leaving duplicate criteria).
    - Add prompt contract caller-intent test(s) in `tests/prompt_contract.rs` covering the new specification and ticket prompt rules.
    - In `TODO.md` only (do not edit `.plans/PROPOSAL-18-spec-and-tickets-story-slicing.md`), mark `T23` complete (`[x]`) in the DAG and ordered backlog and update the `PROPOSAL-18-spec-and-tickets-story-slicing.md` row in the `TODO.md` status matrix to `[x] Complete (T23 done)` right before running `verification_gate`.
  - `verification_gate`: `cargo test --lib prompts::spec && cargo test --lib prompts::tickets && cargo test --test prompt_contract && cargo test --test test_titles && cargo clippy --all-targets -- -D warnings`

- [ ] **`T24` — Narrow Shared-Story Criteria in Agent Brief During Triage**
  - `depends_on`: `["T23"]`
  - `unblocks`: `[]`
  - `plans`: [`.plans/PROPOSAL-19-triage-slice-criteria-narrowing.md`](./.plans/PROPOSAL-19-triage-slice-criteria-narrowing.md)
  - `files`: `src/prompts/triage.rs`, `tests/prompt_contract.rs`, `TODO.md`
  - `tasks`:
    - In `TRIAGE_SYSTEM_PROMPT` (`src/prompts/triage.rs`, preserving all `REQUIRED_NEEDLES` and existing test substrings), update Step 2 so that when a shared specification story spans multiple draft tickets and a ticket carries a sibling ticket's verbatim criterion from that shared story while its own `scope_of_changes`, `test_expectations`, and `verification_gate` define a self-contained slice, Triage narrows `acceptance_criteria` in the `Agent Brief` and records the audit note rather than blocking the run with `needs-info`.
    - Add prompt contract caller-intent test(s) in `tests/prompt_contract.rs` covering the Triage brief narrowing rule.
    - In `TODO.md` only (do not edit `.plans/PROPOSAL-19-triage-slice-criteria-narrowing.md`), mark `T24` complete (`[x]`) in the DAG and ordered backlog and update the `PROPOSAL-19-triage-slice-criteria-narrowing.md` row in the `TODO.md` status matrix to `[x] Complete (T24 done)` right before running `verification_gate`.
  - `verification_gate`: `cargo test --lib prompts::triage && cargo test --test prompt_contract && cargo test --test test_titles && cargo clippy --all-targets -- -D warnings`
