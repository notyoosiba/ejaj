# Proposal 15: Multi-Ticket Story Slicing, Per-Ticket Criteria Validation, and Triage Slice Alignment

## Problem

When `Phase::Spec` writes a specification that groups multiple independently gated modules or providers into a shared `UserStory` (for example, one story covering `chat_completions`, `responses_api`, and `anthropic` decoders), and `Phase::Tickets` slices the work into per-module tickets, three failure modes interact to stop the run in `Phase::Triage`:

1. **Singular story-to-ticket wording in `TICKETS_SYSTEM_PROMPT` (`src/prompts/tickets.rs:57-59`) and `missing_criteria_error` (`src/tools.rs:2026-2040`):**
   - `TICKETS_SYSTEM_PROMPT` defines `acceptance_criteria` as *"copied from the specification story it implements"*, and `missing_criteria_error` states *"every criterion must appear exactly in the `acceptance_criteria` of the ticket that implements its story"*.
   - When a single specification story spans multiple tickets and a `finish_tickets` call fails `spec_to_tickets` due to a typo in one criterion, the Tickets agent often repairs the error by appending all missing criteria from that story onto one ticket (including criteria for modules that the ticket lists in `out_of_scope`) and leaving the earlier typo'd or duplicate criteria behind.
2. **Unqualified tracker-file folding in `TICKETS_SYSTEM_PROMPT` (`src/prompts/tickets.rs:26`):**
   - Step 2 instructs the Tickets agent to *"Fold `README.md`, `TODO.md`, documentation, and `.plan/`/`.plans/` tracker updates into the executable-behavior ticket whose deliverable needs them"*.
   - During read-only `execute_script` reconnaissance, the Tickets agent can discover a `.plans/*.md` file in the repository and add it to a ticket's `scope_of_changes` even when `spec.md` only specified updating `TODO.md`.
3. **`Phase::Triage` blocks self-consistent slices over shared-story criterion overlap (`src/prompts/triage.rs:23`):**
   - When a draft ticket's `scope_of_changes`, `test_expectations`, and `verification_gate` form a self-contained, consistent slice of `spec.md`, but its copied `acceptance_criteria` includes a sibling ticket's verbatim criterion from the same shared specification story, `Phase::Triage` marks the ticket `needs-info` and aborts the entire pipeline run instead of scoping `AgentBrief.acceptance_criteria` to that ticket's slice.

---

## Guardrails for Existing `ejaj` Tests

- **Preserve all prompt-contract needles and forbidden-phrase invariants:**
  - Keep all existing substrings asserted by `tests/prompt_contract.rs`, `src/prompts/tickets.rs`, `src/prompts/triage.rs`, and `src/prompts/review.rs`.
  - In `SPEC_SYSTEM_PROMPT` (`src/prompts/spec.rs`), never introduce any substring forbidden by `tests/prompt_contract.rs:412, 451` (`"at most "`, `"no more than "`, `"maximum of "`, `"upper limit"`, `"authorization header"`, `"access control"`, `"permission check"`, `"authenticate"`).
  - In `TICKETS_SYSTEM_PROMPT` (`src/prompts/tickets.rs`), keep the 11 `- \`<field>\`: ` lines in their exact order (`src/prompts/tickets.rs:218-246`), keep `"expand, migrate, contract"` appearing once (`src/prompts/tickets.rs:247-253`), and keep the exact substring `"Fold \`README.md\`, \`TODO.md\`, documentation, and \`.plan/\`/\`.plans/\` tracker updates into the executable-behavior ticket whose deliverable needs them; never create standalone documentation or tracker tickets for those accompanying updates."` (`src/prompts/review.rs:278`).
  - In `TRIAGE_SYSTEM_PROMPT` (`src/prompts/triage.rs`), preserve all `REQUIRED_NEEDLES` and Step 2 substrings (`"A disagreement among scope, tests, gate and specification is a finding"`, `"inconsistency in the ticket"`, `"The notes of a classification are evidence"`, `"restatement of the title or the objective is not evidence"`).
- **Preserve `spec_to_tickets` unit tests in `src/tools.rs`:**
  - Keep `"missing"`, `"altered"`, story titles, and exact criterion quotes in `missing_criteria_error` (`src/tools.rs:4380-4443`).

---

## Proposed Design

### 1. Slice-Aligned User Stories & Explicit Tracker Paths in `SPEC_SYSTEM_PROMPT` (`src/prompts/spec.rs`)

In Step 3 of `SPEC_SYSTEM_PROMPT` (`src/prompts/spec.rs:25-30`), add guidance to structure user stories along independently verifiable vertical slices so each acceptance criterion applies to one cohesive slice rather than mixing requirements for separately gated modules into one criterion, and to name the exact documentation or tracker files (`TODO.md`, `README.md`, `.plan/`/`.plans/`) in `### Module Architecture` when they are modified:

```rust
3. **Define exact contracts and write implementation-ready stories and criteria:** Wherever producer and consumer modules depend on one another, including a single dependency, state the exact function and type signatures in `### Module Architecture` and `### Data Models & Contracts`, including every parameter type and every return type. Structure user stories along independently verifiable vertical slices, and keep each acceptance criterion scoped to one slice or module boundary rather than combining requirements for separately gated modules into one criterion. When documentation or tracker files are updated, name the exact target file paths in `### Module Architecture`. Each story and each acceptance criterion must
   be concrete enough for the tickets agent to slice into a ticket and for the code agent to verify.
```

### 2. Per-Ticket Criterion Scoping & Authorized Tracker Folding in `TICKETS_SYSTEM_PROMPT` (`src/prompts/tickets.rs`)

1. In Step 2 (`src/prompts/tickets.rs:26`), preserve the required needle and qualify it so the Tickets agent never invents tracker edits outside `spec.md`:
   ```rust
   `blocked_by`. Fold `README.md`, `TODO.md`, documentation, and `.plan/`/`.plans/` tracker updates into the executable-behavior ticket whose deliverable needs them; never create standalone documentation or tracker tickets for those accompanying updates. Fold setup, configuration, scaffolding, documentation, `README.md`, `TODO.md`, and `.plan/`/`.plans/` tracker updates into the behavioral ticket that needs them rather than creating standalone accompanying-work tickets. Only fold documentation, `README.md`, `TODO.md`, or `.plan/`/`.plans/` tracker files that the specification itself includes; never add documentation or tracker file edits that the specification did not specify.
   ```
2. Under `- acceptance_criteria:` (`src/prompts/tickets.rs:57-59`), clarify multi-ticket story distribution:
   ```rust
   - `acceptance_criteria`: the acceptance criteria this ticket makes true, copied from the
     specification story it implements without paraphrasing, shortening or rewording them and without
     a leading checkbox marker. When a specification story spans multiple tickets, copy into each
     ticket only the verbatim criteria from that story that this ticket's own `scope_of_changes` and
     `verification_gate` make true (while the union across the ticket set covers every criterion of
     the story); never copy a criterion for behaviour listed in this ticket's `out_of_scope`, and
     never leave duplicate or altered criteria from an earlier attempt.
   ```

### 3. Duplicate Criterion Rejection (`src/tickets.rs`) & Multi-Ticket `missing_criteria_error` (`src/tools.rs`)

1. In `TicketSet::validate` (`src/tickets.rs`), reject duplicate non-blank `acceptance_criteria` entries (after trimming and collapsing consecutive whitespace) within a single ticket:
   ```rust
   let mut seen_criteria: std::collections::HashSet<String> = std::collections::HashSet::new();
   for entry in &ticket.acceptance_criteria {
       let normalized = entry.split_whitespace().collect::<Vec<_>>().join(" ");
       if !normalized.is_empty() && !seen_criteria.insert(normalized) {
           return Err(format!(
               "[ERROR] ticket `{id}` repeats an `acceptance_criteria` entry: \"{}\"; \
                remove the duplicate criterion so each entry in a ticket is unique",
               entry.trim()
           ));
       }
   }
   ```
2. In `missing_criteria_error` (`src/tools.rs:2026-2040`), clarify that a story's criteria may be distributed across the tickets that implement its slices:
   ```rust
   if missing.len() == 1 {
       format!(
           "[ERROR] the ticket set is missing the specification criterion for story \"{}\": \
            \"{}\"; every criterion must appear exactly in the `acceptance_criteria` of the \
            ticket (or across the tickets) that implement its story, placing each criterion \
            only on the ticket whose scope implements it, so missing or altered criterion text \
            is rejected",
           missing[0].0, missing[0].1
       )
   } else {
       format!(
           "[ERROR] the ticket set is missing or altered {} specification criteria: {list}; \
            every criterion must appear exactly in the `acceptance_criteria` of the ticket (or \
            across the tickets) that implement its story, placing each criterion only on the \
            ticket whose scope implements it, so missing or altered criterion text is rejected",
           missing.len()
       )
   }
   ```

### 4. Narrow Shared-Story Criteria in `AgentBrief` During Triage (`src/prompts/triage.rs`)

In Step 2 of `TRIAGE_SYSTEM_PROMPT` (`src/prompts/triage.rs:23`), preserve all existing substrings and instruct the Triage agent that when a shared specification story spans multiple draft tickets and a ticket carries a sibling ticket's verbatim criterion from that shared story while its own `scope_of_changes`, `test_expectations`, and `verification_gate` define a self-contained slice of the specification, it should narrow the slice's `acceptance_criteria` in the `AgentBrief` and record the note rather than blocking the ticket as `needs-info` or `ready-for-human`:

```rust
Check that each ticket's `scope_of_changes`, `test_expectations`, and `verification_gate` agree with one another and with the specification: the scope must name the behavior the tests exercise, the gate must run those tests, and every claim must match the specification. A disagreement among scope, tests, gate and specification is a finding. When a shared specification story spans multiple draft tickets and a ticket carries a sibling ticket's verbatim criterion from that shared story while its own `scope_of_changes`, `test_expectations`, and `verification_gate` define a self-contained slice of the specification, narrow the slice's `acceptance_criteria` in the `Agent Brief` and record the audit note rather than blocking the ticket as `needs-info` or `ready-for-human`; reserve `needs-info` and `ready-for-human` for genuine specification gaps or codebase contradictions that cannot be resolved from the specification and repository. The notes of a classification are evidence:
```

---

## Affected Files

- `src/prompts/spec.rs`
- `src/prompts/tickets.rs`
- `src/prompts/triage.rs`
- `src/tickets.rs`
- `src/tools.rs`
- `tests/prompt_contract.rs`
- `TODO.md`

## Verification

```bash
cargo test --lib prompts && cargo test --lib tickets && cargo test --lib tools && cargo test --test prompt_contract && cargo test --test test_titles && cargo clippy --all-targets -- -D warnings
```
