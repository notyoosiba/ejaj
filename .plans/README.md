# ejaj Upgrades and Proposals

This directory contains standalone specifications and targeted proposals for upgrading `ejaj`.
Completed items (`01`–`12`, `PROPOSAL-1`–`PROPOSAL-14`, `PROPOSAL-10`–`12`) have been implemented, verified in `git` history, and removed from the active directory.

## Active Proposals

1. **[`PROPOSAL-17-high-signal-bash-failure-digest.md`](./PROPOSAL-17-high-signal-bash-failure-digest.md)**: Prioritize test-runner and compiler failure blocks (`failures:`, `---- <test> stdout ----`, `panicked at`, `test ... FAILED`, `error[`) over passing-test `[ERROR]` logs, stop block expansion at harness progress lines, and cap per-line length in `diagnostic_text` (`src/tools.rs`).
2. **[`PROPOSAL-16-ticket-test-cap-and-batched-tdd.md`](./PROPOSAL-16-ticket-test-cap-and-batched-tdd.md)**: Consolidate 5-dimension boundary cases into `Edge-Case Tables` test functions in `TICKETS_SYSTEM_PROMPT` (`3–6` distinct test functions per ticket) and batch same-seam test authoring and early full-suite runs in `CODE_TDD_DISCIPLINE` (`src/prompts/tickets.rs`, `src/prompts/code.rs`).
3. **[`PROPOSAL-15-ticket-criteria-validation.md`](./PROPOSAL-15-ticket-criteria-validation.md)**: Reject duplicate non-blank `acceptance_criteria` entries within a ticket in `TicketSet::validate`, and clarify in `missing_criteria_error` that criteria may be distributed across the tickets implementing slices of a shared story (`src/tickets.rs`, `src/tools.rs`).
4. **[`PROPOSAL-18-spec-and-tickets-story-slicing.md`](./PROPOSAL-18-spec-and-tickets-story-slicing.md)**: Structure user stories along independently verifiable vertical slices in `SPEC_SYSTEM_PROMPT`, name exact documentation/tracker file paths in `### Module Architecture`, qualify tracker folding to specification-authorized edits in `TICKETS_SYSTEM_PROMPT`, and clarify verbatim slice criteria distribution under `- acceptance_criteria:` (`src/prompts/{spec,tickets}.rs`).
5. **[`PROPOSAL-19-triage-slice-criteria-narrowing.md`](./PROPOSAL-19-triage-slice-criteria-narrowing.md)**: In `TRIAGE_SYSTEM_PROMPT` Step 2, instruct Triage to narrow `acceptance_criteria` in the `Agent Brief` for self-contained slices of a shared story rather than blocking with `needs-info` over sibling-ticket criterion overlap (`src/prompts/triage.rs`).

## Recommended Implementation Order (See [`TODO.md`](../TODO.md) for the Full Task DAG)

1. **Diagnostics First (`T20`):** `PROPOSAL-17` (harden `form_bash_digest` in `src/tools.rs` so subsequent tasks receive immediate, high-signal failure summaries if tests or compilation fail).
2. **Execution Discipline Second (`T21`):** `PROPOSAL-16` (cap test expectations at 3–6 distinct test functions and introduce batched TDD / early test suite execution in `src/prompts/{tickets,code}.rs`).
3. **Data Model Validation Third (`T22`):** `PROPOSAL-15` (enforce duplicate criterion rejection in `TicketSet::validate` and clarify multi-ticket error formatting in `src/tools.rs` before prompts teach models the rule).
4. **Story Slicing Prompts Fourth (`T23`):** `PROPOSAL-18` (align Spec and Tickets prompts with vertical story slicing, explicit tracker paths, authorized tracker folding, and verbatim criteria distribution).
5. **Triage Brief Alignment Fifth (`T24`):** `PROPOSAL-19` (align Triage agent prompt to narrow `Agent Brief` criteria for shared-story slices without blocking with `needs-info`).
