# Proposal 15: Duplicate Criteria Validation & Multi-Ticket Error Formatting (`src/tickets.rs`, `src/tools.rs`)

## Problem

When `finish_tickets` evaluates draft tickets against specification stories, two validation failure modes interact to confuse ticket generation:

1. **Silent acceptance of duplicate criteria within a ticket (`src/tickets.rs`):**
   - When a draft ticket attempts to repair a criteria validation error, models frequently append duplicate copies of the same criterion to a ticket's `acceptance_criteria` list.
   - `TicketSet::validate` does not reject duplicate criteria within a single ticket, allowing redundant entries to persist into ticket execution and audit.

2. **Misleading singular error message in `missing_criteria_error` (`src/tools.rs:2026-2040`):**
   - The current error message states: *"every criterion must appear exactly in the `acceptance_criteria` of the ticket that implements its story"*.
   - When a story is sliced across multiple tickets, this singular phrasing suggests that all criteria must be placed on a single ticket, leading models to copy sibling tickets' criteria onto one ticket (even for modules listed in `out_of_scope`).

---

## Guardrails for Existing `ejaj` Tests

- **Preserve `spec_to_tickets` unit tests in `src/tools.rs`:**
  - Keep `"missing"`, `"altered"`, story titles, and exact criterion quotes in `missing_criteria_error` (`src/tools.rs:4380-4443`).
- **Scope tracker updates strictly to `TODO.md`:**
  - Update `TODO.md` only; do not edit `.plans/PROPOSAL-15-ticket-criteria-validation.md` during implementation.

---

## Proposed Design

### 1. Duplicate Criterion Rejection in `TicketSet::validate` (`src/tickets.rs`)

Reject duplicate non-blank `acceptance_criteria` entries (after trimming and collapsing consecutive whitespace) within a single ticket:

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

### 2. Multi-Ticket Story Distribution Error in `missing_criteria_error` (`src/tools.rs`)

Clarify that a story's criteria may be distributed across the tickets that implement its slices:

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

---

## Affected Files

- `src/tickets.rs`
- `src/tools.rs`
- `TODO.md`

## Verification

```bash
cargo test --lib tickets && cargo test --lib tools && cargo test --test test_titles && cargo clippy --all-targets -- -D warnings
```
