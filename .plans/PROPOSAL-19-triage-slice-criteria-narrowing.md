# Proposal 19: Narrow Shared-Story Criteria in Agent Brief During Triage (`src/prompts/triage.rs`)

## Problem

When a single specification story spans multiple draft tickets and each ticket's `scope_of_changes`, `test_expectations`, and `verification_gate` define a self-contained, consistent slice of the specification, `Phase::Triage` aborts the pipeline run if a ticket copies criteria that belong to a sibling ticket's slice:

- In `TRIAGE_SYSTEM_PROMPT` (`src/prompts/triage.rs:23`), Step 2 instructs the agent: *"Check that each ticket's `scope_of_changes`, `test_expectations`, and `verification_gate` agree with one another and with the specification... A disagreement among scope, tests, gate and specification is a finding."*
- When a ticket carries a sibling ticket's verbatim criterion from a shared story, Triage flags this overlap as an inconsistency and marks the ticket `needs-info`, blocking queue execution.
- Rather than aborting self-contained slices, Triage should narrow `AgentBrief.acceptance_criteria` to the criteria this ticket's scope actually implements, record the note as evidence, and keep the ticket `ready-for-agent`.

---

## Guardrails for Existing `ejaj` Tests

- **Preserve all prompt-contract needles in `TRIAGE_SYSTEM_PROMPT`:**
  - Preserve all `REQUIRED_NEEDLES` and Step 2 substrings in `src/prompts/triage.rs:85-111, 239-253, 298-311` (`"A disagreement among scope, tests, gate and specification is a finding"`, `"inconsistency in the ticket"`, `"The notes of a classification are evidence"`, `"restatement of the title or the objective is not evidence"`).
- **Scope tracker updates strictly to `TODO.md`:**
  - Update `TODO.md` only; do not edit `.plans/PROPOSAL-19-triage-slice-criteria-narrowing.md` during implementation.

---

## Proposed Design

### Narrow Shared-Story Criteria in `AgentBrief` During Triage (`src/prompts/triage.rs`)

In Step 2 of `TRIAGE_SYSTEM_PROMPT` (`src/prompts/triage.rs`), preserve all existing substrings and instruct the Triage agent that when a shared specification story spans multiple draft tickets and a ticket carries a sibling ticket's verbatim criterion while its own scope, tests, and gate define a self-contained slice, narrow the slice's `acceptance_criteria` in the `Agent Brief` and record the note, keeping the ticket `ready-for-agent`:

```rust
Check that each ticket's `scope_of_changes`, `test_expectations`, and `verification_gate` agree \
with one another and with the specification: the scope must name the behavior the tests exercise, \
the gate must run those tests, and every claim must match the specification. A disagreement among \
scope, tests, gate and specification is a finding. When a shared specification story spans multiple \
draft tickets and a ticket carries a sibling ticket's verbatim criterion from that shared story while \
its own `scope_of_changes`, `test_expectations`, and `verification_gate` define a self-contained slice \
of the specification, do not mark the ticket `needs-info` or raise a ticket inconsistency over that \
shared-story criterion overlap; instead, narrow the slice's `acceptance_criteria` in the `Agent Brief` \
to the criteria this ticket's scope implements and record the audit note, keeping the ticket \
`ready-for-agent`. Otherwise, an inconsistency in the ticket or an inconsistency between the ticket and \
the repository or the specification is a finding, and the ticket is marked `needs-info` when it leaves \
a question open or `wontfix` when it is out of scope. The notes of a classification are evidence: \
explain what was audited, which files were read, and why the category and state were chosen. A \
restatement of the title or the objective is not evidence.
```

---

## Affected Files

- `src/prompts/triage.rs`
- `tests/prompt_contract.rs`
- `TODO.md`

## Verification

```bash
cargo test --lib prompts::triage && cargo test --test prompt_contract && cargo test --test test_titles && cargo clippy --all-targets -- -D warnings
```
