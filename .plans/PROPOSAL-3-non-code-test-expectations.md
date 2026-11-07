# Proposal 3: Restrict Automated Testing to Executable Source Code Across Plan, Spec, Tickets, Code, and Review

## Problem

Currently, `ejaj` forces automated unit tests onto non-code work (such as `TODO.md`, `.plan/*.md`, `README.md`, and documentation) because of gaps across the planning and validation boundaries:

1. **Plan & Spec prompts (`src/prompts/plan.rs`, `src/prompts/spec.rs`):**
   - Neither prompt states that automated tests apply only to executable source code. When a repository contains tracker files (`TODO.md`, `.plan/*.md`), the Plan and Spec agents frequently define "completion-contract test seams" and 5-dimension boundary matrices for markdown checklists and YAML frontmatter.
2. **Tickets boundary contradiction (`src/tickets.rs` vs `src/prompts/tickets.rs`):**
   - `bullets_markdown` (`src/tickets.rs:451-463`) and `TICKETS_SYSTEM_PROMPT` (`src/prompts/tickets.rs:75-76`) state that a field with no entries is written as `[]` and rendered as `None.` when a section does not apply.
   - However, `TicketSet::validate` (`src/tickets.rs:306-329`) unconditionally rejects any ticket whose `test_expectations` list is empty or `["None."]`.
   - Likewise, `transition_fix_ticket` (`src/pipeline.rs:1138-1141`) had to synthesize a dummy test expectation `when_running_full_test_suite_given_completed_queue_round_then_all_repository_tests_pass` just to pass `TicketSet::validate`.
3. **Code & Review prompts (`src/prompts/code.rs`, `src/prompts/review.rs`):**
   - `ticket_kickoff` (`src/prompts/code.rs:122-128`) unconditionally prepends `CODE_TDD_DISCIPLINE` even when a ticket has no test expectations, forcing the Code agent to write meta-tests for markdown files or spawn nested test runners inside unit tests.

---

## Guardrails for Existing `ejaj` Tests

- **Preserve `PLAN_MARKER` in `src/prompts/plan.rs`:** `tests/common/mod.rs` checks `pub const PLAN_MARKER: &str = "Interview the user to align on core architectural";`.
- **Preserve `Plan` and `Specification` markdown headings:** `src/plan_document.rs` and `src/specification.rs` validate exact section headings; when `## 5. Testing Decisions & Seams` subsections do not apply (e.g. a docs-only change), write the single line `None.` under each required `###` heading.
- **Preserve `Ticket::has_content` in `src/tickets.rs:291-305`:** A ticket with empty `test_expectations` still requires at least one non-blank entry across `out_of_scope`, `scope_of_changes`, `test_expectations`, or `acceptance_criteria`.

---

## Proposed Design

Enforce one consistent rule across **Plan**, **Spec**, **Tickets**, **Code**, and **Review**: **automated tests apply exclusively to executable source code, never to documentation, `README.md`, `TODO.md`, `.plan/`, or other human-facing prose.**

### 1. Plan Pass (`src/prompts/plan.rs`)

Add a rule under `Rules for the document:` in `PLAN_SYSTEM_PROMPT`:

```rust
- Automated tests apply exclusively to executable source code, never to documentation, `README.md`, `TODO.md`, `.plan/` files, changelogs, or other human-facing prose. Never propose or record decisions to add or refresh unit tests that assert the text or checklist status of markdown or tracking files; if existing tests assert obsolete live `TODO.md` or `.plan/` snapshots, plan to remove those non-code assertions rather than refreshing them.
```

### 2. Spec Pass (`src/prompts/spec.rs`)

In `SPEC_SYSTEM_PROMPT` Step 2 and `## 5. Testing Decisions & Seams`, specify that test seams, keeper categories, and boundary matrices apply only to executable source code:
- When a plan updates documentation or tracker files (`TODO.md`, `.plan/`, `README.md`) alongside source code, keep those file updates in `### Module Architecture` and exclude them from `## 5. Testing Decisions & Seams`.
- When a specification modifies only documentation or human-facing content and no executable source code, write `None.` under `### Test Seams`, `### Coverage Strategy`, and `### Prior Art`.

### 3. Tickets Pass & Validator (`src/prompts/tickets.rs`, `src/tickets.rs`, `src/pipeline.rs`)

1. **`TICKETS_SYSTEM_PROMPT` (`src/prompts/tickets.rs:55-63`):**
   - Fold documentation, `README.md`, `TODO.md`, and `.plan/` updates into the executable source-code ticket they accompany rather than emitting standalone tracker or documentation tickets.
   - Require `test_expectations` only for executable source-code behavior; when a ticket modifies only documentation or markdown files and no executable source code, pass `test_expectations: []` (which renders as `None.`).
2. **Allow Empty / `None.` `test_expectations` in `TicketSet::validate` (`src/tickets.rs:306-329`):**
   - Remove the `non_blank_tests.is_empty()` rejection (keeping `has_content` at lines 291-305) and validate `has_bdd_test_name(entry)` on every non-blank, non-`None.` entry when present:
     ```rust
     let non_blank_tests: Vec<&str> = ticket
         .test_expectations
         .iter()
         .map(|entry| entry.trim())
         .filter(|entry| !entry.is_empty() && !entry.eq_ignore_ascii_case(NONE))
         .collect();
     for entry in non_blank_tests {
         if !has_bdd_test_name(entry) {
             return Err(format!(
                 "[ERROR] ticket `{id}` has a `test_expectations` entry without a \
                  `when_<action>_given_<context>_then_<outcome>` test identifier: \
                  \"{entry}\"; every test expectation must name its test in lowercase \
                  snake_case `when_<action>_given_<context>_then_<outcome>` form"
             ));
         }
     }
     ```
3. **Remove Synthetic Placeholder in `transition_fix_ticket` (`src/pipeline.rs:1138-1141`):**
   - Pass `test_expectations: Vec::new()` instead of the dummy `when_running_full_test_suite_...` string.

### 4. Code & Review Passes (`src/prompts/code.rs`, `src/prompts/review.rs`)

1. **Skip `CODE_TDD_DISCIPLINE` When `## Test Expectations` Is `None.` (`src/prompts/code.rs:122-128`):**
   ```rust
   pub fn ticket_kickoff(ticket: &str) -> String {
       let has_test_expectations = !ticket.contains("## Test Expectations\n\nNone.\n");
       if ticket.starts_with("---\n") && has_test_expectations {
           format!("{CODE_TDD_DISCIPLINE}{CODE_TICKET_KICKOFF_PROMPT}{ticket}")
       } else {
           format!("{CODE_TICKET_KICKOFF_PROMPT}{ticket}")
       }
   }
   ```
2. **`REVIEW_SYSTEM_PROMPT` (`src/prompts/review.rs`):**
   - Clarify that `keeper_coverage` and `boundary_matrix` audit executable source-code behavior only; never emit findings or fix tickets requesting unit tests for documentation, `README.md`, `TODO.md`, or `.plan/` files, and allow `test_expectations: []` on fix tickets that do not add new test cases.

---

## Affected Files

- `src/prompts/plan.rs`
- `src/prompts/spec.rs`
- `src/prompts/tickets.rs`
- `src/tickets.rs`
- `src/prompts/code.rs`
- `src/prompts/review.rs`
- `src/pipeline.rs`

## Verification

```bash
cargo test --lib tickets && cargo test --lib prompts && cargo test --lib pipeline && cargo test --test test_titles && cargo clippy --all-targets -- -D warnings
```
