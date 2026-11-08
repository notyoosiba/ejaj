# Proposal 16: Cap Per-Ticket Test Functions and Batch TDD Execution (`src/prompts/tickets.rs`, `src/prompts/code.rs`)

## Problem

In `.ejaj/20261108-181746`, a single ticket (`16-retry-unattended-handoff-passes.md`) consumed 98+ tool-call steps on `deepseek-flash` before hitting a provider API error on step 99. Two prompt contracts caused a small pipeline change (~70 lines in `src/pipeline.rs`) to expand into a 98-step code session:

1. **Duplicate Keeper + Boundary Matrix `test_expectations` in `TICKETS_SYSTEM_PROMPT` (`src/prompts/tickets.rs:66-70`):**
   - `- test_expectations:` instructs the Tickets agent to structure expectations *"by the 4 keeper categories (Core Entry-Point Behaviors, Edge-Case Tables, Contract & Invariant Checks, Regression Tests) and the 5-dimension boundary matrix (Empty/Zero/Nil, Minimum valid boundary, Maximum limit/Upper boundary, Malformed/Invalid payload, Boundary unauthorized/Forbidden access)"*.
   - Because it does not tell the Tickets agent to fold the 5 boundary dimensions into the `Edge-Case Tables` test(s), the model mechanically emits 4–7 Keeper test functions **plus** 4–5 separate Boundary Matrix test functions (9–11 distinct `when_<action>_given_<context>_then_<outcome>` test functions in a single ticket) that test the same behavior twice.
2. **One-test-at-a-time Red-Green loops and late full-suite execution in `CODE_TDD_DISCIPLINE` (`src/prompts/code.rs:206-239`):**
   - Rules 1, 6, and 7 instruct the Code agent to work in one slice at a time, author a test, run a focused `bash` `cargo test`, implement the minimal change, and rerun the focused test before moving to the next expectation.
   - Across 10–11 test expectations in large test files (`tests/pipeline.rs` is 14,700+ lines; `tests/common/mod.rs` is 3,400+ lines), running a separate read-edit-compile-test loop per individual `#[test]` function burns 70–90 steps before the agent ever runs the full test binary (`cargo test --test pipeline`).
   - When the full test binary finally ran at step 98, 5 existing `HandoverTransport` tests in `tests/pipeline.rs` failed due to updated request counts, leaving only 2 steps before `MAX_STEPS = 100` and pushing the transcript past the provider's context limit.

---

## Guardrails for Existing `ejaj` Tests

- **Do not edit `CODE_SYSTEM_PROMPT` in `src/prompts/code.rs`:**
  - `tests/llm_wire.rs` replays recorded fixtures against `CODE_SYSTEM_PROMPT`. Keep `CODE_SYSTEM_PROMPT` byte-identical and make all Code-phase changes in `CODE_TDD_DISCIPLINE`.
- **Preserve all required substrings in `TICKETS_SYSTEM_PROMPT` (`src/prompts/tickets.rs` and `src/prompts/review.rs`):**
  - Keep the 11 `- `<field>`: ` bullets in exact order (`src/prompts/tickets.rs:218-246`).
  - Keep `"expand, migrate, contract"` appearing once (`src/prompts/tickets.rs:247-253`).
  - Keep all existing substrings asserted by `src/prompts/tickets.rs:153-216` and `src/prompts/review.rs:229-235, 275-281, 427-439, 464-470, 482-488, 501-508, 521-545`, and never introduce any forbidden phrase checked in `src/prompts/review.rs:532-540` (`"HTTP 401"`, `"HTTP 403"`, `"permission check"`, `"access control"`, `"65535"`).
- **Preserve all required substrings in `CODE_TDD_DISCIPLINE` (`src/prompts/code.rs`):**
  - Keep `DISCIPLINE_LITERAL_PREFIX` (`"Follow a strict Red-First TDD loop for this ticket:\n"`) and all substrings asserted in `src/prompts/code.rs:672-686, 721-732, 893-911, 925-932`.

---

## Proposed Design

### 1. Consolidate Boundary Matrix Cases into `Edge-Case Tables` in `TICKETS_SYSTEM_PROMPT` (`src/prompts/tickets.rs`)

In `- test_expectations:` (`src/prompts/tickets.rs:66-71`), instruct the Tickets agent to fold the 5 boundary dimensions into the `Edge-Case Tables` test function(s) rather than emitting a second set of duplicate per-dimension `Boundary Matrix` test functions, keeping a single ticket to 3–6 distinct `#[test]` functions (and splitting into another ticket when a feature genuinely needs more):

```rust
- `test_expectations`: the outside-in test cases the ticket adds or updates for executable source-code behavior, structured by the 4
  keeper categories (Core Entry-Point Behaviors, Edge-Case Tables, Contract & Invariant Checks,
  Regression Tests) and the 5-dimension boundary matrix (Empty/Zero/Nil, Minimum valid boundary,
  Maximum limit/Upper boundary, Malformed/Invalid payload, Boundary unauthorized/Forbidden access),
  with strict edge-only mocking (never mock internal classes, services or local persistence). Consolidate the applicable 5-dimension boundary cases into table-driven `Edge-Case Tables` test functions rather than emitting separate duplicate per-dimension `Boundary Matrix` test functions for the same behavior, keeping each ticket to a compact set of distinct test functions (typically 3–6 `when_<action>_given_<context>_then_<outcome>` tests) and splitting into an additional ticket when more test functions are genuinely needed. Write test expectations only for executable source-code behavior -- never for documentation, `README.md`, `TODO.md`, `.plan/`/`.plans/` tracker files, or other human-facing prose. Every ticket that changes executable source code must carry at least one non-blank entry in `test_expectations`, and every substantive entry must name its test in lowercase `snake_case` `when_<action>_given_<context>_then_<outcome>` form (using domain-level BDD terminology, never internal function or class names in the test title). A documentation-only or markdown-only ticket that modifies only documentation or markdown files and no executable source code may pass `test_expectations: []`, which renders as `None.`; test expectations are required only for executable source-code behavior.
```

### 2. Batch Cohesive Seam Tests & Run the Full Target Suite Early in `CODE_TDD_DISCIPLINE` (`src/prompts/code.rs`)

In `CODE_TDD_DISCIPLINE` (`src/prompts/code.rs:206-239`), preserve every existing required needle while instructing the Code agent to:
1. Batch initial reconnaissance into 1–2 `execute_script` calls and group expectations that share the same public seam and test harness into a single cohesive slice (authoring those `#[test]` functions and shared test-helper updates in one `execute_script` edit rather than running a separate compile-and-test loop per individual `#[test]` function).
2. Run the full target test binary (`verification_gate` or `cargo test --test <binary>`) as soon as the production change and new tests pass, so any regressions in existing callers or fixtures are caught and fixed before making final tracker edits:

```rust
1. Work in one focused vertical slice at a time at the module's public seam rather than writing \
every test before implementation: for each slice, author the relevant outside-in expectation from \
`## Test Expectations` using exact `when_<action>_given_<context>_then_<outcome>` test titles \
(never put internal function or class names in test titles) and exercise behavior through the \
public seam rather than requiring tests of internal helpers. When multiple `## Test Expectations` \
entries exercise the same public seam and test harness, batch those test functions and their shared \
fixture updates into a single slice edit rather than running a separate edit-and-compile cycle per \
individual test function.\n\
...
9. After implementing the production code, run the ticket's `verification_gate` with `bash` until \
it exits successfully. Run the full target test suite early (before tracker edits) so any \
regressions in existing tests are caught and updated in one pass.\n\
```

---

## Affected Files

- `src/prompts/tickets.rs`
- `src/prompts/code.rs`
- `TODO.md`

## Verification

```bash
cargo test --lib prompts::tickets && cargo test --lib prompts::code && cargo test --lib prompts::review && cargo test --test prompt_contract && cargo test --test llm_wire && cargo test --test test_titles && cargo clippy --all-targets -- -D warnings
```
