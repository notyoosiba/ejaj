# Proposal 18: Story Slicing, Per-Ticket Criteria Scoping, & Authorized Tracker Folding (`src/prompts/{spec,tickets}.rs`)

## Problem

When `Phase::Spec` groups multiple independently testable modules into a single `UserStory`, two prompt contracts lead `Phase::Tickets` to generate bloated or misaligned tickets:

1. **Unscoped stories and missing tracker paths in `SPEC_SYSTEM_PROMPT` (`src/prompts/spec.rs:25-30`):**
   - Step 3 does not instruct the agent to structure user stories along independently verifiable vertical slices.
   - When a specification calls for updating `README.md`, `TODO.md`, or tracker files, `### Module Architecture` does not require naming the exact target file paths.

2. **Unqualified tracker folding and singular story wording in `TICKETS_SYSTEM_PROMPT` (`src/prompts/tickets.rs:26, 57-59`):**
   - Step 2 instructs the agent to *"Fold `README.md`, `TODO.md`, documentation, and `.plan/`/`.plans/` tracker updates into the executable-behavior ticket whose deliverable needs them"*. During read-only reconnaissance, the Tickets agent discovers tracker files and adds them to `scope_of_changes` even when `spec.md` never specified updating them.
   - Under `- acceptance_criteria:`, the prompt states criteria are *"copied from the specification story it implements"*. When one story spans multiple tickets, it fails to explain how verbatim criteria should be distributed across each ticket's self-contained slice.

---

## Guardrails for Existing `ejaj` Tests

- **Preserve all prompt-contract needles and forbidden-phrase invariants:**
  - Keep all existing substrings asserted by `tests/prompt_contract.rs`, `src/prompts/tickets.rs`, and `src/prompts/review.rs`.
  - In `SPEC_SYSTEM_PROMPT` (`src/prompts/spec.rs`), never introduce any substring forbidden by `tests/prompt_contract.rs:412, 451` (`"at most "`, `"no more than "`, `"maximum of "`, `"upper limit"`, `"authorization header"`, `"access control"`, `"permission check"`, `"authenticate"`).
  - In `TICKETS_SYSTEM_PROMPT` (`src/prompts/tickets.rs`), keep the 11 `- \`<field>\`: ` lines in their exact order (`src/prompts/tickets.rs:218-246`), keep `"expand, migrate, contract"` appearing once (`src/prompts/tickets.rs:247-253`), and keep the exact substring `"Fold \`README.md\`, \`TODO.md\`, documentation, and \`.plan/\`/\`.plans/\` tracker updates into the executable-behavior ticket whose deliverable needs them; never create standalone documentation or tracker tickets for those accompanying updates."`.
- **Scope tracker updates strictly to `TODO.md`:**
  - Update `TODO.md` only; do not edit `.plans/PROPOSAL-18-spec-and-tickets-story-slicing.md` during implementation.

---

## Proposed Design

### 1. Slice-Aligned User Stories & Explicit Tracker Paths in `SPEC_SYSTEM_PROMPT` (`src/prompts/spec.rs`)

In Step 3 of `SPEC_SYSTEM_PROMPT` (`src/prompts/spec.rs`), add guidance to structure user stories along independently verifiable vertical slices so each acceptance criterion applies to one cohesive slice, and name exact documentation or tracker files (`TODO.md`, `README.md`, `.plan/`/`.plans/`) in `### Module Architecture` when they are modified:

```rust
3. **Define exact contracts and write implementation-ready stories and criteria:** Wherever producer \
and consumer modules depend on one another, including a single dependency, state the exact function \
and type signatures in `### Module Architecture` and `### Data Models & Contracts`, including every \
parameter type and every return type. Structure user stories along independently verifiable vertical \
slices, and keep each acceptance criterion scoped to one slice or module boundary rather than combining \
requirements for separately gated modules into a single criterion. When documentation, `README.md`, \
`TODO.md`, or `.plan/`/`.plans/` tracker updates accompany the changes, specify those exact target \
file paths in `### Module Architecture`. Write every acceptance criterion as an outside-in, \
machine-verifiable condition: avoid vague aspirations (\"clean code\", \"robust\") and write criteria that \
be concrete enough for the tickets agent to slice into a ticket and for the code agent to verify.
```

### 2. Authorized Tracker Folding & Verbatim Slice Criteria in `TICKETS_SYSTEM_PROMPT` (`src/prompts/tickets.rs`)

1. In Step 2, preserve the required needle and qualify it so the Tickets agent only folds tracker edits when the specification calls for them:

```rust
`blocked_by`. Fold `README.md`, `TODO.md`, documentation, and `.plan/`/`.plans/` tracker updates \
into the executable-behavior ticket whose deliverable needs them; never create standalone documentation \
or tracker tickets for those accompanying updates. Fold setup, configuration, scaffolding, documentation, \
`README.md`, `TODO.md`, and `.plan/`/`.plans/` tracker updates into the behavioral ticket that needs \
them rather than creating standalone accompanying-work tickets. Only fold documentation, `README.md`, \
`TODO.md`, or `.plan/`/`.plans/` tracker updates into a ticket when the specification itself calls for \
updating them; do not invent tracker or documentation tasks that the specification omits.
```

2. Under `- acceptance_criteria:`, clarify multi-ticket story distribution:

```rust
- `acceptance_criteria`: the acceptance criteria this ticket makes true, copied from the
  specification story it implements without paraphrasing, shortening or rewording them and without
  a leading checkbox marker. When a specification story spans multiple tickets, copy into each
  ticket only the verbatim criteria from that story that this ticket's own `scope_of_changes` and
  `verification_gate` make true (while the union across the ticket set covers every criterion of
  the story); never copy a criterion for behaviour listed in this ticket's `out_of_scope`, and
  never leave duplicate or altered criteria from an earlier attempt.
```

---

## Affected Files

- `src/prompts/spec.rs`
- `src/prompts/tickets.rs`
- `tests/prompt_contract.rs`
- `TODO.md`

## Verification

```bash
cargo test --lib prompts::spec && cargo test --lib prompts::tickets && cargo test --test prompt_contract && cargo test --test test_titles && cargo clippy --all-targets -- -D warnings
```
