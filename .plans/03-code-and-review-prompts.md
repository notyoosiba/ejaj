# 03: Code and Review Prompt Upgrades (`src/prompts/code.rs`, `src/prompts/review.rs`)

This document specifies focused upgrades for `CODE_TDD_DISCIPLINE` in `src/prompts/code.rs` and `REVIEW_SYSTEM_PROMPT` in `src/prompts/review.rs`, integrating `PROPOSAL-3`, `PROPOSAL-4`, `PROPOSAL-8`, and `PROPOSAL-9`.

## Guardrails for Existing `ejaj` Tests

- **Leave `CODE_SYSTEM_PROMPT` untouched in `src/prompts/code.rs`:**
  - Placing slice-by-slice TDD, targeted inner-loop test execution, no-artificial-red-probe (`PROPOSAL-4`), and tracker-edits-last (`PROPOSAL-9`) rules inside `CODE_TDD_DISCIPLINE` keeps `CODE_SYSTEM_PROMPT` byte-identical.
  - Because `tests/llm_wire.rs` sends bare prompts without `CODE_TDD_DISCIPLINE`, leaving `CODE_SYSTEM_PROMPT` unchanged avoids invalidating or re-recording any `tests/fixtures/llm_wire/*.json` fixtures.
- **Preserve `REVIEW_MARKER` and existing unit-test needles in `REVIEW_SYSTEM_PROMPT`:**
  - `tests/common/mod.rs` checks `const REVIEW_MARKER: &str = "Audit the completed ticket queue against the technical specification and test design blueprint";`.
  - `src/prompts/review.rs` unit tests assert that `REVIEW_SYSTEM_PROMPT` contains `keeper_coverage`, `sediment_and_coupling`, `inferred_assumptions`, `boundary_matrix`, `code_and_scope`, `when_<action>_given_<context>_then_<outcome>`, `explicit per-dimension boundary tests named in the executed tickets' `test_expectations```, `never reject a suite solely for using the explicit per-dimension`, `reuse that earlier finding's exact `id` in `findings``, and `finish_review`.

---

## Part 1: `src/prompts/code.rs` (`CODE_TDD_DISCIPLINE`)

### Improvements Checklist

1. **Incremental Vertical Slices & Public-Seam Encapsulation (Item 1):**
   - Drive implementation in vertical red-green slices at the module's public seam rather than writing all tests in bulk up front.
   - Keep internal helpers private instead of widening `pub` visibility solely for unit tests, and replace obsolete unit tests coupled to removed internals with seam-level tests.
2. **Real Collaborators, Public Read Interface, & Anti-Tautology Assertions (Item 2):**
   - Exercise in-process and local-substitutable dependencies with real collaborators or local stand-ins (restricting test doubles to non-deterministic external boundaries).
   - Verify state through the module's public read interface rather than side-channel storage or private state inspection, and assert against independent known literals rather than recomputing expected values with the production formula.
3. **Targeted Test Execution, No Artificial Red Probes (`PROPOSAL-4`), & Tracker Edits Last (`PROPOSAL-9`) (Item 4):**
   - While iterating on each slice, run the focused test target with `bash` (e.g. filtering by test name) rather than running the full workspace suite after every edit.
   - If existing production code already satisfies a newly added regression or boundary test (for example on a retry handover or coverage fix ticket), keep the passing result and **never break, stash, or back up production files just to force a red failure**.
   - Once all slices pass, remove any temporary debug probes or scratch files, update any tracker/status files (`TODO.md`, `.plan/*.md`) last right before running the full `verification_gate` command, and revert tracker status edits if the gate fails.

### Exact Update to `CODE_TDD_DISCIPLINE` in `src/prompts/code.rs`

```rust
/// Red-first TDD and test-design discipline appended to rendered ticket
/// kickoffs in the ticket queue.
pub const CODE_TDD_DISCIPLINE: &str = "\
Follow a strict Red-First TDD loop for this ticket:\n\
1. Work in vertical red-green slices at the module's public seam rather than writing all tests in \
bulk up front: author the outside-in tests listed in `## Test Expectations` using exact \
`when_<action>_given_<context>_then_<outcome>` test titles (never put internal function or class \
names in test titles, and never widen `pub` visibility on internal helpers solely for tests). \
When consolidating shallow internals behind a deeper public seam, replace obsolete unit tests \
coupled to removed internals with seam-level tests.\n\
2. Enforce strict edge-only mocking and anti-tautology: never mock internal classes, services, or \
local persistence layers, and never introspect private state; restrict test doubles strictly to \
non-deterministic external boundaries, verify state through the module's public read interface, \
and assert against independent known literals rather than recomputing expected values with the \
production formula.\n\
3. Cover the ticket's applicable keeper categories (Core Entry-Point Behaviors, Edge-Case Tables \
across Empty/Zero/Nil, Minimum valid, Maximum limit, Malformed/Invalid, and Unauthorized/Forbidden \
boundaries, Contract & Invariant Checks, and Regression Tests).\n\
4. While iterating on each slice, run the focused test target with `bash` before editing \
production code: when new production behavior is still missing, confirm the new test fails (Red), \
then write only the minimal production code needed and re-run the focused test to confirm Green; \
if existing production code already satisfies a newly added regression or boundary test (for \
example on a retry handover or coverage fix ticket), keep the passing result and never break, \
stash, or back up production files just to force a red failure. Once all slices pass, remove any \
temporary debug probes or scratch files, apply any tracker or status file updates (`TODO.md`, \
`.plan/*.md`) last, and run the ticket's full `verification_gate` until it exits 0 (reverting \
tracker status edits if the gate fails).\n\n";
```

---

## Part 2: `src/prompts/review.rs` (`REVIEW_SYSTEM_PROMPT`)

### Improvements Checklist

1. **Diff-First Inspection Budget & Named-Risk Codebase Reads (Step 1):**
   - Start from `tools.diff()` as the primary view of the change set, and read modified or added files (plus `AGENTS.md` and `README.md` when present).
   - Inspect unchanged files outside the diff only to evaluate a concrete cross-cutting risk you can name.
2. **Source-Code-Only Testing (`PROPOSAL-3`), Anti-Tautology, & Internal Module Exports (`PROPOSAL-8`) (Step 2):**
   - Clarify in `keeper_coverage` and `boundary_matrix` that automated tests audit executable source-code behavior only (never demand unit tests for documentation, `README.md`, `TODO.md`, or `.plan/` files).
   - Check in `sediment_and_coupling` for tautological assertions, widened `pub` visibility for tests, and leftover debug probes or scratch files.
   - Clarify in `code_and_scope` that internal helpers exported solely for cross-file imports within the repository (`pub(crate)` in Rust or non-entrypoint module `export`s in TypeScript/JavaScript that are not re-exported from the designated public entrypoint) do not constitute unauthorized public API expansion.
3. **Code Review Loop Convergence: Blocking-Only, Scoped Re-Review on Round 2+, & Fix-Ticket Consolidation (Step 4):**
   - Emit `findings` and `fixes` tickets only for blocking defects.
   - On review round 2+ (when `## Prior Review Rounds` is present), scope the audit to verifying that prior findings and fix tickets were resolved and checking the fix diff for new blocking regressions.
   - Coalesce compatible fixes into the minimum number of fix tickets so the fix queue does not rebuild context and re-run the verification gate per finding.

### Exact Updates to `REVIEW_SYSTEM_PROMPT` in `src/prompts/review.rs`

```rust
1. **Inspect the repository (read-only) with `execute_script`:** start with `tools.diff()` as your \
primary view of the change set, and use `tools.read(path, startLine?, endLine?)` to inspect the \
modified and added production and test files (plus `AGENTS.md` and `README.md` when present, \
following any links they contain to project standards or architectural decisions). Do not crawl \
unchanged files across the broader codebase by default: use `tools.list(glob?)` or \
`tools.grep(pattern, glob?)` outside the diff only to evaluate a concrete cross-cutting risk you \
can name (such as a changed function or trait contract, shared state, or existing test seam). \
Batch related reads and searches into one script call. Never write, build or test.
```

```rust
// Under Step 2 (`sediment_and_coupling` and `code_and_scope`):
   - `sediment_and_coupling`: confirm zero internal mocks (internal classes, services, and local \
persistence layers are never mocked; test doubles are restricted strictly to non-deterministic \
external boundaries), no tautologies (tests must assert against independent known literals, never \
recomputing the production formula), no private state introspection or `pub` visibility widened on \
internal helpers solely for tests, and no leftover debug probes or scratch files.
   - `code_and_scope`: verify that every ticket's acceptance criteria, scope of changes, out-of-scope \
boundaries, verification gate, the specification's `### ADR Compliance`, and any project rules \
documented in or linked from `AGENTS.md` or `README.md` were honoured without specification drift. \
Internal helpers exported solely for cross-file imports within the repository (such as `pub(crate)` \
items in Rust or non-entrypoint module `export`s in TypeScript/JavaScript that are not re-exported \
from the designated public entrypoint) do not constitute unauthorized public API expansion.
```

```rust
// Under Step 4 (`Decide the verdict and hand over with finish_review`):
4. **Decide the verdict and hand over with `finish_review`:**
   - **Blocking-only rule:** only emit `findings` and `fixes` tickets for blocking defects \
(missing or broken specification/ticket acceptance criteria, breaches of rules documented in or \
linked from `AGENTS.md` or `README.md`, unrequested public entrypoint expansion or scope creep, \
missing keeper or 5-dimension boundary coverage for executable source code, internal mocks, \
private state introspection, `pub` visibility widened on internal helpers solely for tests, \
tautological assertions, leftover debug probes or scratch files, or invalid \
`when_<action>_given_<context>_then_<outcome>` test names). Never emit `findings` or `fixes` \
tickets for subjective style nitpicks or to request unit tests for documentation, `README.md`, \
`TODO.md`, or `.plan/` files.
   - **Scoped re-review on round 2+:** when `## Prior Review Rounds` contains an earlier review \
round, scope your audit to verifying that every finding and fix ticket from the previous round was \
resolved in substance and checking the code touched by the fix round for new blocking regressions; \
do not wander into untouched code to raise new findings that were not introduced by the fix round.
   - **When no blocking defects remain:** pass `findings: []` and `fixes: []`. The pipeline will \
write `review.md` and proceed to the change summary.
   - **When blocking defects remain:** list the highest-priority blocking defects in `findings` \
(each citing the exact file and symbol/hunk alongside the violated specification, ticket, or \
`AGENTS.md`-linked rule) and emit the ordered `fixes` tickets that resolve them. Coalesce \
compatible fixes into the minimum number of fix tickets (prefer a single consolidated fix ticket \
covering all blocking findings when they fit in one context window, rather than one fix ticket per \
finding). When a finding from any prior review round is still unresolved, or when a later fix \
ticket breaks behavior that an earlier finding covered, reuse that earlier finding's exact `id` in \
`findings` (while giving the new fix ticket a fresh `r<round>-<n>-<slug>` ticket `id`); assign a \
new `finding.id` only to a defect not reported in any prior review round. Every finding must be \
covered in substance by at least one fix ticket, and every fix ticket must satisfy all ticket \
rules:
```

---

## Verification

```bash
cargo test --lib prompts::code && cargo test --lib prompts::review && cargo test --test pipeline && cargo clippy --all-targets -- -D warnings
```
