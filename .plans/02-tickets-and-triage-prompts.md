# 02: Tickets and Triage Prompt Upgrades (`src/prompts/tickets.rs`, `src/prompts/triage.rs`)

This document specifies focused upgrades for `TICKETS_SYSTEM_PROMPT` in `src/prompts/tickets.rs` and `TRIAGE_SYSTEM_PROMPT` in `src/prompts/triage.rs`, integrating `PROPOSAL-1`, `PROPOSAL-3`, and `PROPOSAL-9`.

## Guardrails for Existing `ejaj` Tests

- **Preserve `TicketSet` JSON schema:** `finish_tickets` in `src/tickets.rs` validates `id`, `blocked_by`, `blocks`, `title`, `objective`, `acceptance_criteria`, `prerequisites_assumed`, `out_of_scope`, `scope_of_changes`, `test_expectations`, and `verification_gate`.
- **Keep Step 3 in `TICKETS_SYSTEM_PROMPT` as the single source of truth for expand-migrate-contract:** Do not duplicate expand-contract rules inside `scope_of_changes`.
- **Preserve all unit-test needles in `TRIAGE_SYSTEM_PROMPT`:** `src/prompts/triage.rs` tests that `TRIAGE_SYSTEM_PROMPT` contains `finish_triage`, `read-only`, `execute_script`, `tools.list`, `tools.grep`, `tools.read`, `never ask`, `evidence`, `bug`, `enhancement`, `ready-for-agent`, `ready-for-human`, `needs-info`, `wontfix`, `Agent Brief`, `target_behavior`, `acceptance_criteria`, `out_of_scope`, `blocked_by`, `blocker_explanation`, `ticket_id`, `ticket_title`, `notes`, and `original ticket`.

---

## Part 1: `src/prompts/tickets.rs` (`TICKETS_SYSTEM_PROMPT`)

### Improvements Checklist

1. **Task Right-Sizing, Per-Ticket Scope Cap (`PROPOSAL-9`), & Docs Folding (`PROPOSAL-3`):**
   - Right-size every ticket as a focused vertical slice touching at most one primary user journey / entry point with a bounded set of `test_expectations` (typically 3-8 BDD test cases in one test file), rather than bundling 15+ test expectations across multiple suites into one ticket.
   - Fold setup, configuration, scaffolding, documentation, `README.md`, `TODO.md`, and `.plan/` updates into the behavioral ticket whose deliverable needs them rather than emitting standalone scaffolding or docs-only tickets.
   - Batch small same-shape edits across multiple files into one ticket when they share a single test cycle and verification gate.
2. **Explicit Cross-Ticket Producer/Consumer Interface Contracts (`prerequisites_assumed` & `scope_of_changes`):**
   - Because each ticket executes in an isolated fresh context window, require `prerequisites_assumed` to state the exact module paths, type names, and function signatures consumed from `blocked_by` tickets, and require `scope_of_changes` to state the exact public signatures produced for downstream `blocks` tickets.
3. **Executable Source-Code-Only `test_expectations` (`PROPOSAL-3`):**
   - Write `test_expectations` only for executable source-code behavior (never for documentation, `README.md`, `TODO.md`, or `.plan/` files). When a ticket modifies only documentation or human-facing markdown and no executable source code, pass `test_expectations: []` (which renders as `None.`).

### Exact Updates to `TICKETS_SYSTEM_PROMPT` in `src/prompts/tickets.rs`

1. Replace Step 2 under `## Workflow` in `TICKETS_SYSTEM_PROMPT`:

```rust
2. **Slice the specification into right-sized vertical slices:** each ticket is a narrow,
   self-contained path through every layer it touches -- a tracer bullet that carries its own
   red-green test cycle and is independently verifiable on its own. Never slice by layer or by
   module: a ticket that changes only one module, only one file or only one layer is not a ticket,
   because the behaviour is not verifiable until some later ticket lands. Size each ticket for one
   fresh context window: touch at most one primary user journey or entry point per ticket with a
   focused set of `test_expectations` (typically 3 to 8 test cases in one test file) rather than
   bundling multi-view features or 15+ test cases into a single monolithic ticket. Fold setup,
   configuration, documentation, `README.md`, `TODO.md`, and `.plan/` updates into the ticket
   whose deliverable needs them, and batch small same-shape edits that share one verification gate.
   Order the tickets in implementation order and name the dependency edges between them: a ticket
   that builds on another names that id in its `blocked_by`.
```

2. Replace the `prerequisites_assumed`, `scope_of_changes`, and `test_expectations` bullets in `TICKETS_SYSTEM_PROMPT`:

```rust
- `prerequisites_assumed`: what the ticket takes for granted about the tree, including the existing
  files and modules it builds on and the exact module paths, type names, and function signatures it
  consumes from any ticket in its `blocked_by` list.
- `out_of_scope`: what this ticket must not touch.
- `scope_of_changes`: the behaviour the ticket makes work, the target files and modules it
  touches, the exact public types and function signatures it produces for any downstream ticket in
  its `blocks` list, and any obsolete internal helpers or coupled unit tests it removes when
  consolidating code behind a deeper public seam.
- `test_expectations`: the outside-in test cases the ticket adds or updates for executable
  source-code behaviour at the public seam, structured by the 4 keeper categories (Core
  Entry-Point Behaviors, Edge-Case Tables, Contract & Invariant Checks, Regression Tests) and the
  5-dimension boundary matrix (Empty/Zero/Nil, Minimum valid boundary, Maximum limit/Upper
  boundary, Malformed/Invalid payload, Boundary unauthorized/Forbidden access), with strict
  edge-only mocking (never mock internal classes, services or local persistence). Write
  `test_expectations` only for executable source-code behaviour -- never for documentation,
  `README.md`, `TODO.md`, `.plan/` files, or other human-facing prose. Every ticket that changes
  executable source code must carry at least one non-blank entry in `test_expectations`, and every
  entry must name its test in lowercase `snake_case` `when_<action>_given_<context>_then_<outcome>`
  form (without internal function or class names in the test title). When a ticket modifies only
  documentation or human-facing content and no executable source code, pass `[]` (which renders as
  `None.`).
```

---

## Part 2: `src/prompts/triage.rs` (`TRIAGE_SYSTEM_PROMPT`)

### Improvements Checklist

1. **Pairwise Producer/Consumer & Self-Consistency Audit (Step 2):**
   - For every ticket with `blocked_by` dependencies, verify that the files, types, and function signatures assumed in `prerequisites_assumed` match what its upstream blocker tickets specify in `scope_of_changes`, and verify that each ticket's `scope_of_changes`, `test_expectations`, and `verification_gate` agree with one another and with the specification.
2. **Deterministic Bug Reproduction Check Under `## Categories` (`bug`):**
   - When classifying a draft ticket as `bug`, verify that the ticket's `test_expectations` and `acceptance_criteria` specify a deterministic reproduction of the broken symptom at the public seam.
3. **Separate `done` from `wontfix` Under `## States` (`PROPOSAL-1`):**
   - When implementing `PROPOSAL-1`, document `done` (already implemented and covered in the repository, unblocking dependents) separately from `wontfix` (out of scope or invalid for this run).

### Exact Updates to `TRIAGE_SYSTEM_PROMPT` in `src/prompts/triage.rs`

1. Update Step 2 under `## Workflow` in `TRIAGE_SYSTEM_PROMPT`:

```rust
2. **Audit every draft ticket against what you found:** the draft set itself, the specification it was sliced from, and the repository. For every ticket with `blocked_by` dependencies, check pairwise producer/consumer agreement: verify that the files, types, and function signatures the ticket assumes in `prerequisites_assumed` match what its upstream blocker tickets produce in `scope_of_changes`, and verify that each ticket's own `scope_of_changes`, `test_expectations`, and `verification_gate` agree with one another and with the specification. The notes of a classification are evidence: each note says what was audited and what that showed -- a file, a seam, a producer/consumer signature check, an existing implementation, a missing definition, a contradiction in the ticket. At least one note must be non-blank, and a restatement of the title or the objective is not evidence.
```

2. Update the `- bug:` bullet under `## Categories (the category field)` in `TRIAGE_SYSTEM_PROMPT`:

```rust
- bug: the ticket corrects behavior that already exists, including a regression, so the behavior it fixes is already specified and its test expectations must lock down a deterministic reproduction of the exact broken symptom at the public seam.
```

---

## Verification

```bash
cargo test --lib prompts::tickets && cargo test --lib prompts::triage && cargo test --test pipeline && cargo clippy --all-targets -- -D warnings
```
