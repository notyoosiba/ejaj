# Proposal 24: Phase Handoff Isolation & Slice-Scoped Verification Gates (`src/prompts/plan.rs`, `src/prompts/spec.rs`, `src/prompts/tickets.rs`, `src/prompts/triage.rs`, `src/tickets.rs`, `tests/prompt_contract.rs`)

## Problem

`ejaj`'s pipeline architecture (`src/main.rs:362-367`, `src/pipeline.rs:200-249, 538-608, 672-694`) separates responsibilities across phases by typed handoff objects and distinct verification scopes:

- **`Phase::Plan`** interviews the user (and digests any referenced `.plan/` / `.plans/` task document) into a self-contained `Plan`.
- **`Phase::Spec`** turns only that `Plan` + live codebase state into architectural module boundaries, cross-module signatures, acceptance criteria, and test seams.
- **`Phase::Tickets`** slices the `Specification` into focused vertical-slice `Ticket`s, each carrying a **narrow, slice-scoped** `verification_gate` covering that ticket's own `test_expectations`.
- **`Phase::Triage`** audits draft tickets against `spec.md` and the codebase before execution.
- **`Pipeline::transition_gate` & `Phase::Review`** run the repository-wide test suite (`cargo test` / `npm test`) and repository-wide lint/specification checks after the narrow ticket gates pass.

In practice (observed in `.ejaj/20261112-075130/`), three prompt gaps and one missing handoff validation allow those boundaries to collapse and falsely halt runs at `triage` with `needs-info`:

1. **`Phase::Spec` and `Phase::Tickets` read `.plan/` / `.plans/` proposal files as shadow specifications (`src/prompts/spec.rs:11-16`, `src/prompts/tickets.rs:11-15`):**
   - Although `src/prompts/spec.rs:4-5` states *"the confirmed plan is its only input"*, `SPEC_SYSTEM_PROMPT` exposes `tools.list()` and `tools.read()` with no rule forbidding `spec` (or `tickets`) from reading `.plan/` / `.plans/` proposal files.
   - In `.ejaj/20261112-075130/trajectories/spec.traj.json` (Step 1), `spec` listed the repository, read `.plans/T25-transition-gate-diagnostic-excerpt.md` directly, and copied its end-of-task verification chain (`cargo test --lib tools && cargo test --lib pipeline && cargo test --test pipeline && cargo test --test test_titles && cargo clippy --all-targets -- -D warnings`) into `spec.md` under `### Coverage Strategy` — even though `plan.md` never mentioned Clippy or shell commands.
2. **`SPEC_SYSTEM_PROMPT` lets `spec.md` prescribe repository-wide shell commands (`src/prompts/spec.rs:17-24, 107-116`):**
   - `## 5. Testing Decisions & Seams` is meant to define architectural test boundaries, keeper categories, zero-internal-mock rules, and boundary cases — not shell commands. When `spec.md` embeds a multi-target/Clippy command chain, downstream passes face conflicting instructions between `spec.md`'s broad command and `Phase::Tickets`'s narrow per-ticket gate policy.
3. **`Phase::Tickets` can emit a `when_*` prefix `verification_gate` that filters out tests listed in its own `test_expectations` (`src/prompts/tickets.rs:66-85`, `src/tickets.rs:327-345`):**
   - `TICKETS_SYSTEM_PROMPT` instructs the tickets agent to use a narrow test selection that *"would fail before that behaviour exists"*, without requiring `verification_gate` to execute **every** `when_<action>_given_<context>_then_<outcome>` test named in that ticket's `test_expectations`.
   - In `.ejaj/20261112-075130/tickets/1-transition-gate-diagnostic-excerpts.md`, `tickets` listed an existing test (`when_creating_repair_ticket_given_full_suite_transition_failure_then_returns_valid_ticket_without_expectations`) alongside three new `when_transition_gate_fails_*` tests in `test_expectations`, then set `verification_gate` to `cargo test --lib when_transition_gate_fails` — skipping its own listed repair-ticket test (and in Cargo, filtering to not-yet-existing test names runs `0 tests` and exits `0` anyway).
   - `TicketSet::validate` (`src/tickets.rs:185-345`) does not reject a `when_*`-filtered `cargo test` gate that excludes a `when_*` test identifier in the same ticket's `test_expectations`, so `finish_tickets` accepts the self-contradictory ticket instead of letting the tickets agent self-correct in-loop.
4. **`TRIAGE_SYSTEM_PROMPT` conflates narrow slice gates with repository-wide coverage (`src/prompts/triage.rs:23`):**
   - Step 2 states *"the gate must run those tests, and every claim must match the specification. A disagreement among scope, tests, gate and specification is a finding."* Without an explicit boundary stating that per-ticket gates are slice-scoped while full-suite and linter runs belong to `transition_gate` and `review`, triage flags a narrow per-ticket `verification_gate` whenever `spec.md` mentions broader repository test or lint coverage.

---

## Guardrails for Existing `ejaj` Tests

- **Preserve all `PLAN_SYSTEM_PROMPT` and `SPEC_SYSTEM_PROMPT` markers, headings, and forbidden-phrase invariants (`tests/prompt_contract.rs:14-498`, `src/prompts/review.rs:180-269, 421-546`):**
  - Keep `OPENING_MARKER`, `PLAN_TEMPLATE_HEADINGS`, `SPEC_OPENING_MARKER`, `SPEC_KICKOFF_MARKER`, `SPEC_TEMPLATE_HEADINGS`, and all required literal substrings in `tests/prompt_contract.rs`.
  - Never introduce case-insensitive `"at most "`, `"no more than "`, `"maximum of "`, or `"upper limit"` into `SPEC_SYSTEM_PROMPT` (`tests/prompt_contract.rs:446-454`).
  - Never introduce any forbidden substring across `PLAN_SYSTEM_PROMPT`, `SPEC_SYSTEM_PROMPT`, `TICKETS_SYSTEM_PROMPT`, or `REVIEW_SYSTEM_PROMPT`: `"1 to 4 critical questions"`, `"authorization header"`, `"access control"`, `"permission check"`, `"authenticate"`, `"HTTP 401"`, `"HTTP 403"`, `"payload validation rule"`, `"access-control rule"`, `"numeric limit"`, `"maximum number of test expectations"` (`tests/prompt_contract.rs:204, 227-232, 486-491`, `src/prompts/review.rs:532-540`).
- **Preserve `TICKETS_SYSTEM_PROMPT` schema slicing and Step 2 / Step 3 contracts (`src/prompts/tickets.rs:157-301`, `tests/prompt_contract.rs:500-603`):**
  - Keep the 11 `- `<field>`: ` bullets between `"with exactly these fields:"` and `"The `verification_gate` must demonstrate"` in exact `TICKETS_FIELDS_IN_ORDER` order.
  - Keep `TICKETS_TRACKER_FOLDING_NEEDLE` in Step 2, `"Use a narrow test selection (\`cargo test <module>::tests\`)"`, and `TICKETS_SYSTEM_PROMPT.matches("expand, migrate, contract").count() == 1`.
- **Preserve `TRIAGE_SYSTEM_PROMPT` Step 2 and `REQUIRED_NEEDLES` contracts (`src/prompts/triage.rs:85-395`, `tests/prompt_contract.rs:605-725`):**
  - Keep all `REQUIRED_NEEDLES`, `TRIAGE_LEGACY_STEP_TWO_NEEDLES` (`"A disagreement among scope, tests, gate and specification is a finding"`, `"inconsistency in the ticket"`, `"The notes of a classification are evidence"`, `"restatement of the title or the objective is not evidence"`), and `step_two.matches("narrow that ticket's \`acceptance_criteria\`").count() == 1`.
  - Never introduce `"Always narrow"`, `"always narrow"`, or `"narrow every ticket's \`acceptance_criteria\`"`.
- **Preserve existing `TicketSet::validate` acceptance of module/target gates and shell checks (`src/tickets.rs`, `tests/common/mod.rs`):**
  - Module/target gates (`"cargo test tickets"`, `"cargo test triage::tests"`, `"cargo test --lib pipeline::tests"`, `"cargo test"`), build checks (`"cargo check"`), and shell commands (`"if [ ... ]; then exit 1; fi"`) must continue to pass `TicketSet::validate` unchanged.
- **Preserve blocked-triage pipeline control flow (`tests/pipeline.rs:1975-2250`):**
  - Keep `Pipeline::triage_pass` returning `Step::Retry` immediately on non-ready classifications (preserving the 4-call transport invariant in `tests/pipeline.rs:2005-2009`).
- **Preserve caller-intent test-title rules (`tests/test_titles.rs`):**
  - Name every new test `when_<action>_given_<context>_then_<outcome>` without internal function or type identifiers.
- **Scope tracker updates strictly to `TODO.md`:**
  - Mark the `T28` row `[x]` in `TODO.md` right before running the verification gate; do not edit `.plans/PROPOSAL-24-phase-handoff-isolation-and-slice-gates.md`.

---

## Proposed Design

### 1. Upstream Handoff Isolation Against `.plan/` / `.plans/` Leakage (`src/prompts/plan.rs`, `src/prompts/spec.rs`, `src/prompts/tickets.rs`)

1. **`PLAN_SYSTEM_PROMPT` (`src/prompts/plan.rs`):**
   - Clarify that when a request references an upstream `.plan/` or `.plans/` task or proposal document that is not itself being modified, the Plan agent must digest all settled architectural choices, defaults, and constraints directly into the self-contained `Plan` prose without citing `.plan/` or `.plans/` proposal paths as pointers or copying verification shell commands into the `Plan`.
2. **`SPEC_SYSTEM_PROMPT` (`src/prompts/spec.rs` Workflow Step 1):**
   - State explicitly that the confirmed `Plan` reproduced in the kickoff is the sole source of task requirements and scope.
   - During read-only reconnaissance, inspect live repository code, tests, manifests, `AGENTS.md`, and `README.md` (and read a specific tracker file such as `TODO.md` or a `.plan/` / `.plans/` file **only** when the confirmed `Plan` explicitly directs modifying that exact file path and its current structure must be matched).
   - Never read `.plan/` or `.plans/` proposal or task documents to import extra requirements, verification commands, test names, or design choices that the confirmed `Plan` omitted.
3. **`TICKETS_SYSTEM_PROMPT` (`src/prompts/tickets.rs` Workflow Step 1):**
   - State explicitly that the `Specification` reproduced in the kickoff and live codebase/test files are the sole inputs; never read `.plan/` or `.plans/` proposal or task documents to import requirements, test expectations, or verification commands (inspecting a tracker file only when `spec.md` explicitly calls for updating that exact file).

### 2. Keep `spec.md` Testing Strategy Architectural, Not Shell-Gate Prescriptive (`src/prompts/spec.rs`)

In `SPEC_SYSTEM_PROMPT` (`src/prompts/spec.rs` Workflow Step 2 and `### Coverage Strategy` template guidance):

- Clarify that `## 5. Testing Decisions & Seams` specifies architectural test boundaries, keeper categories, zero-internal-mock rules, boundary cases, and caller-intent naming — **never** shell command lines (`cargo test ...`, `npm test`, `cargo clippy ...`) or per-ticket / repository-wide verification commands, because narrow slice gates belong to `Phase::Tickets` while full-suite test execution and repository-wide lint/review checks belong to the harness transition gate and `Phase::Review`.

### 3. Slice-Scoped `verification_gate` Alignment & Deterministic Handoff Validation (`src/prompts/tickets.rs`, `src/tickets.rs`)

1. **`TICKETS_SYSTEM_PROMPT` (`src/prompts/tickets.rs:66-85`):**
   - Clarify in `test_expectations` that entries list the executable-source `when_<action>_given_<context>_then_<outcome>` tests this ticket adds or updates (do not pad `test_expectations` with unchanged pre-existing tests unless this ticket updates them or exercises them under the same module gate).
   - Clarify in the `verification_gate` guidance after `"The \`verification_gate\` must demonstrate..."`:
     - Keep `verification_gate` slice-scoped to the module or integration target this ticket touches (e.g., `cargo test --lib <module>::tests` or `cargo test --test <target>`), never bundling repository-wide multi-suite chains or `cargo clippy` checks that belong to `transition_gate` and `review`.
     - Require `verification_gate` to execute **every** `when_<action>_given_<context>_then_<outcome>` test identifier named in this ticket's `test_expectations`; prefer module-level selection (`cargo test --lib <module>::tests`) over `when_*` test-name prefix filters (which can skip sibling expectations in `test_expectations` and pass vacuously with `0 filtered` before new tests exist).
2. **Deterministic filter mismatch check in `TicketSet::validate` (`src/tickets.rs:327-345`):**
   - Extract all `when_<action>_given_<context>_then_<outcome>` identifiers from `ticket.test_expectations`.
   - Inspect `cargo test` subcommands in `ticket.verification_gate` (split on `&&`, `;`, `||`):
     - When `verification_gate` contains one or more `cargo test` invocations and **every** `cargo test` invocation restricts execution via `when_*` positional filter token(s) (rather than an unfiltered target or module filter such as `pipeline::tests` or `tickets`), verify that every `when_*` identifier extracted from `ticket.test_expectations` contains at least one of those `when_*` filter tokens as a substring.
     - If any expected `when_*` test identifier is excluded by all `when_*` gate filters, return a deterministic validation error:
       `[ERROR] ticket \`<id>\` has a \`verification_gate\` filter that skips \`<test_id>\` from \`test_expectations\`; widen \`verification_gate\` to the module or target (for example \`cargo test --lib <module>::tests\`) so it executes every test named in \`test_expectations\`.`
   - Because `finish_tickets` runs `TicketSet::validate` inside the tickets tool loop (`src/tools.rs`), the tickets agent receives this error immediately and widens the gate before handing off to `Phase::Triage`.

### 4. Slice-Scoped Gate Auditing in `TRIAGE_SYSTEM_PROMPT` (`src/prompts/triage.rs`)

In `TRIAGE_SYSTEM_PROMPT` Workflow Step 2 (`src/prompts/triage.rs:23`), while preserving every existing `TRIAGE_LEGACY_STEP_TWO_NEEDLES` and shared-story narrowing rule verbatim:

- Clarify that each ticket's `verification_gate` is intentionally narrow and slice-scoped: it must run every `when_<action>_given_<context>_then_<outcome>` test named in that ticket's own `test_expectations` (so a gate filter that omits one of the ticket's own listed `test_expectations` remains an inconsistency in the ticket), whereas a narrow module- or target-level `verification_gate` does **not** conflict with broader repository-wide test or lint coverage mentioned in the specification (because the harness runs the full repository test suite in its transition gate and repository-wide checks in review after the ticket queue).

### 5. Test Coverage (`src/tickets.rs`, `src/prompts/tickets.rs`, `src/prompts/triage.rs`, `tests/prompt_contract.rs`)

Add focused caller-intent (`when_<action>_given_<context>_then_<outcome>`) tests covering:

- **`src/tickets.rs`:**
  - Rejecting a ticket whose `verification_gate` uses a `when_*` filter (`cargo test --lib when_transition_gate_fails`) that skips another `when_*` identifier in `test_expectations` (`when_creating_repair_ticket_given_full_suite_transition_failure_then_returns_valid_ticket_without_expectations`), naming the ticket id and skipped test identifier in the `[ERROR]` message.
  - Accepting module/target gates (`cargo test --lib pipeline::tests`, `cargo test tickets`, `cargo test --test pipeline`), compound gates where a module command covers all listed expectations, and matching `when_*` filters that cover every listed expectation.
- **`tests/prompt_contract.rs`, `src/prompts/tickets.rs`, `src/prompts/triage.rs`:**
  - `SPEC_SYSTEM_PROMPT` and `TICKETS_SYSTEM_PROMPT` forbid importing unstated requirements or verification commands from `.plan/` / `.plans/` proposal files, and `SPEC_SYSTEM_PROMPT` excludes shell command gates from `## 5. Testing Decisions & Seams`.
  - `TICKETS_SYSTEM_PROMPT` requires `verification_gate` to stay slice-scoped while executing every `when_*` test named in `test_expectations`.
  - `TRIAGE_SYSTEM_PROMPT` audits `verification_gate` against the ticket's own `test_expectations` without flagging narrow slice gates against repository-wide transition/review coverage.

---

## Affected Files

- `src/prompts/plan.rs`
- `src/prompts/spec.rs`
- `src/prompts/tickets.rs`
- `src/prompts/triage.rs`
- `src/tickets.rs`
- `tests/prompt_contract.rs`
- `TODO.md`

## Verification

- Per-slice ticket gates stay narrow and run every `when_<action>_given_<context>_then_<outcome>` test listed in that ticket's `test_expectations` (for example `cargo test --lib tickets` for `src/tickets.rs`, and `cargo test --lib prompts && cargo test --test prompt_contract` for prompt contract slices); do not copy shell command chains into `spec.md`.
- Repository-wide verification (handled by the harness transition gate and review pass):

```bash
cargo test --lib tickets && cargo test --lib prompts && cargo test --test prompt_contract && cargo test --test test_titles && cargo clippy --all-targets -- -D warnings
```
