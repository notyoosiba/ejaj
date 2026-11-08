# TODO — `.plans/` Implementation Status & Task DAG

Tracks the implementation status of all specifications and proposals in [`.plans/`](./.plans/README.md) as an ordered Directed Acyclic Graph (DAG) of implementation tasks.

---

## 1. `.plans/` Document Status Matrix

| Plan / Proposal | Status | Mapped DAG Tasks | Target Modules |
| --- | --- | --- | --- |
| [`PROPOSAL-3-non-code-test-expectations.md`](./.plans/PROPOSAL-3-non-code-test-expectations.md) | [x] Complete (T01, T02 done) | `T01`, `T02` | `src/tickets.rs`, `src/pipeline.rs`, `src/prompts/{plan,spec,tickets,code,review}.rs` |
| [`03-code-and-review-prompts.md`](./.plans/03-code-and-review-prompts.md) | [x] Complete (T02, T05, T04 done) | `T02`, `T05`, `T04` | `src/prompts/review.rs`, `src/prompts/code.rs` |
| [`PROPOSAL-8-internal-module-exports-in-review.md`](./.plans/PROPOSAL-8-internal-module-exports-in-review.md) | [x] Complete (T05 done) | `T05` | `src/prompts/review.rs` |
| [`PROPOSAL-4-no-artificial-red-probes.md`](./.plans/PROPOSAL-4-no-artificial-red-probes.md) | [x] Complete (T04 done) | `T04` | `src/prompts/code.rs` |
| [`01-plan-and-spec-prompts.md`](./.plans/01-plan-and-spec-prompts.md) | [x] Complete (T02, T03 done) | `T02`, `T03` | `src/prompts/plan.rs`, `src/prompts/spec.rs` |
| [`PROPOSAL-9-rollback-failed-ticket-and-cap-scope.md`](./.plans/PROPOSAL-9-rollback-failed-ticket-and-cap-scope.md) | [x] Complete (T04, T06 done) | `T04`, `T06` | `src/prompts/code.rs`, `src/prompts/tickets.rs` |
| [`02-tickets-and-triage-prompts.md`](./.plans/02-tickets-and-triage-prompts.md) | `[ ]` Pending (T02, T06 done; T07 pending) | `T02`, `T06`, `T07` | `src/prompts/tickets.rs`, `src/prompts/triage.rs` |
| [`PROPOSAL-1-triage-done-state.md`](./.plans/PROPOSAL-1-triage-done-state.md) | `[ ]` Pending | `T07`, `T08` | `src/triage.rs`, `src/prompts/triage.rs`, `src/pipeline.rs` |
| [`PROPOSAL-5-forward-triage-brief-to-code-agent.md`](./.plans/PROPOSAL-5-forward-triage-brief-to-code-agent.md) | `[ ]` Pending | `T09` | `src/triage.rs`, `src/orchestrator.rs`, `src/pipeline.rs` |
| [`PROPOSAL-7-gate-failure-excerpt-in-queue-report.md`](./.plans/PROPOSAL-7-gate-failure-excerpt-in-queue-report.md) | `[ ]` Pending | `T10` | `src/orchestrator.rs`, `src/pipeline.rs` |
| [`05-shell-execution-hardening.md`](./.plans/05-shell-execution-hardening.md) | `[ ]` Pending | `T11` | `src/tools.rs` |
| [`09-file-editing-and-patch-matching.md`](./.plans/09-file-editing-and-patch-matching.md) | `[ ]` Pending | `T12` | `src/script.rs` |
| [`12-http-retry-and-overload-resilience.md`](./.plans/12-http-retry-and-overload-resilience.md) | `[ ]` Pending | `T13` | `src/retry.rs` |
| [`07-in-loop-truncation-and-tool-recovery.md`](./.plans/07-in-loop-truncation-and-tool-recovery.md) | `[ ]` Pending | `T14`, `T15` | `src/api.rs`, `src/llm/`, `src/loops/agent.rs` |
| [`PROPOSAL-2-review-pass-retry.md`](./.plans/PROPOSAL-2-review-pass-retry.md) | `[ ]` Pending | `T16` | `src/pipeline.rs` |
| [`11-context-compaction-and-overflow-recovery.md`](./.plans/11-context-compaction-and-overflow-recovery.md) | `[ ]` Pending | `T17` | `src/loops/agent.rs` |
| [`08-conversation-trajectory-persistence.md`](./.plans/08-conversation-trajectory-persistence.md) | `[ ]` Pending | `T18` | `src/loops/agent.rs`, `src/tools.rs` |

### Completed Baseline Proposals (Already in `git` History)

- [x] **`PROPOSAL-10` — Stagnant Finding Circuit Breaker & 10-Round Review Cap (`5b4699a`)** (`src/pipeline.rs`, `src/review.rs`)
- [x] **`PROPOSAL-11` — Full-Suite Transition Gate Before Review (`5b4699a`)** (`src/pipeline.rs`, `src/specification.rs`)
- [x] **`PROPOSAL-12` — HTTP 529, Embedded 2xx Error, & Empty-Content Retry (`92e35ef`)** (`src/retry.rs`, `src/llm/anthropic.rs`)

---

## 2. Task Dependency Graph (DAG)

`T01` and `T02` implement [`PROPOSAL-3-non-code-test-expectations.md`](./.plans/PROPOSAL-3-non-code-test-expectations.md) first so the validator, pipeline, and prompts support `test_expectations: []` (`None.`) and never demand unit tests for markdown or tracker files (`TODO.md`, `.plans/*.md`, `README.md`). Next, Wave 1 (`T05` → `T04` → `T03` → `T06`) completes all phase-prompt upgrades (`src/prompts/*.rs`) so rebuilding `ejaj` gives all subsequent runtime and pipeline tasks (`T07`–`T18`) scoped Round 2+ re-reviews, no artificial red probes, and capped ticket scopes.

```text
[x] [T01] PROPOSAL-3a: Validator, transition_fix_ticket & ticket_kickoff optional test_expectations
      │
      ▼
[x] [T02] PROPOSAL-3b: Source-code-only test rules across Plan, Spec, Tickets & Review prompts
      │
      ▼
[x] [T05] 03 + PROPOSAL-8: REVIEW_SYSTEM_PROMPT upgrades — scoped Round 2+ re-review & pub(crate) exports (src/prompts/review.rs)
      │
      ▼
[x] [T04] 03 + PROPOSAL-4 + PROPOSAL-9: CODE_TDD_DISCIPLINE upgrades — no artificial red probes & tracker edits last (src/prompts/code.rs)
      │
      ▼
[x] [T03] 01: PLAN_SYSTEM_PROMPT & SPEC_SYSTEM_PROMPT upgrades (src/prompts/{plan,spec}.rs)
      │
      ▼
[x] [T06] 02 + PROPOSAL-9: TICKETS_SYSTEM_PROMPT scope cap & contracts (src/prompts/tickets.rs)
      ├──► [ ] [T07] PROPOSAL-1a + 02: TriageState::Done model & TRIAGE_SYSTEM_PROMPT (src/triage.rs, src/prompts/triage.rs)
      │          │
      │          ▼
      │        [ ] [T08] PROPOSAL-1b: Pipeline pre-completion & unblocking for Done tickets (src/pipeline.rs)
      │          │
      │          ▼
      │        [ ] [T09] PROPOSAL-5: Forward Triage notes & AgentBrief to Code kickoff (src/{triage,orchestrator,pipeline}.rs)
      │          │
      │          ▼
      │        [ ] [T10] PROPOSAL-7: Gate failure excerpts in queue-report.md (src/{orchestrator,pipeline}.rs)
      │          │
      │          └──────────────────────────┐
      ├──► [ ] [T11] 05: Shell process-group kill & timeout spill (src/tools.rs) ──────────────┐
      ├──► [ ] [T12] 09: CRLF tools.replace & grep line cap (src/script.rs)                    │
      ├──► [ ] [T13] 12: HTTP 408, permanent 429 fast-fail & retry-after hints (src/retry.rs) ─┼──┐
      └──► [ ] [T14] 07a: Output-token truncation detection across providers (src/api.rs, llm) │  │
                 │                                                                             │  │
                 ▼                                                                             │  │
               [ ] [T15] 07b: In-loop missing-tool/truncation recovery & loop guard (agent.rs) │  │
                 ├──────────────────────────┐                                                  │  │
                 ▼                          ▼                                                  │  │
               [ ] [T16] PROPOSAL-2:      [ ] [T17] 11: Compaction retry, Tier-1 soft fallback │  │
                 Outer retry for            & HTTP 400/413/422 overflow recovery (agent.rs) ◄──┼──┘
                 unattended passes          │                                                  │
                 (src/pipeline.rs)          ▼                                                  │
                                          [ ] [T18] 08: Conversation trajectory persistence ◄──┘
                                            (.traj.json in src/loops/agent.rs, src/tools.rs)
```

---

## 3. Ordered Task Backlog

### Wave 0: Root Foundation (`PROPOSAL-3-non-code-test-expectations.md`)

- [x] **`T01` — Optional `test_expectations` in Validator, `transition_fix_ticket`, & `ticket_kickoff`**
  - `depends_on`: `[]`
  - `unblocks`: `["T02"]`
  - `plans`: [`.plans/PROPOSAL-3-non-code-test-expectations.md`](./.plans/PROPOSAL-3-non-code-test-expectations.md)
  - `files`: `src/tickets.rs`, `src/pipeline.rs`, `src/prompts/code.rs`
  - `tasks`:
    - In `TicketSet::validate` (`src/tickets.rs`), allow empty or `["None."]` `test_expectations` (preserving `Ticket::has_content`) while requiring `has_bdd_test_name(entry)` on every non-blank, non-`None.` entry.
    - In `transition_fix_ticket` (`src/pipeline.rs`), replace the synthetic placeholder `when_running_full_test_suite_...` with `test_expectations: Vec::new()`.
    - In `ticket_kickoff` (`src/prompts/code.rs`), skip prepending `CODE_TDD_DISCIPLINE` when the rendered ticket contains `## Test Expectations\n\nNone.\n`.
  - `verification_gate`: `cargo test --lib tickets && cargo test --lib prompts::code && cargo test --lib pipeline && cargo test --test test_titles && cargo clippy --all-targets -- -D warnings`

- [x] **`T02` — Enforce Executable Source-Code-Only Testing Across Phase Prompts**
  - `depends_on`: `["T01"]`
  - `unblocks`: `["T05"]`
  - `plans`: [`.plans/PROPOSAL-3-non-code-test-expectations.md`](./.plans/PROPOSAL-3-non-code-test-expectations.md)
  - `files`: `src/prompts/plan.rs`, `src/prompts/spec.rs`, `src/prompts/tickets.rs`, `src/prompts/review.rs`
  - `tasks`:
    - In `PLAN_SYSTEM_PROMPT` (`src/prompts/plan.rs`), add the rule under `Rules for the document:` stating that automated tests apply exclusively to executable source code (never to documentation, `README.md`, `TODO.md`, `.plan/` files, or changelogs) and that obsolete non-code snapshot assertions should be removed rather than refreshed.
    - In `SPEC_SYSTEM_PROMPT` (`src/prompts/spec.rs`), scope Step 2 and `## 5. Testing Decisions & Seams` to executable source code only, instructing the agent to write `None.` under `### Test Seams`, `### Coverage Strategy`, and `### Prior Art` when a specification modifies only human-facing prose.
    - In `TICKETS_SYSTEM_PROMPT` (`src/prompts/tickets.rs`), fold documentation/`README.md`/`TODO.md`/`.plan/` updates into the accompanying behavioral ticket and allow `test_expectations: []` (`None.`) when a ticket modifies only documentation or markdown files.
    - In `REVIEW_SYSTEM_PROMPT` (`src/prompts/review.rs`), clarify that `keeper_coverage` and `boundary_matrix` audit executable source code only, forbid findings or fix tickets requesting unit tests for documentation/`README.md`/`TODO.md`/`.plan/` files, and allow `test_expectations: []` on fix tickets that add no new test cases.
  - `verification_gate`: `cargo test --lib prompts && cargo test --test test_titles && cargo clippy --all-targets -- -D warnings`

---

### Wave 1: Phase Prompt Upgrades (`T05` → `T04` → `T03` → `T06` — Rebuild `ejaj` Before Runtime Tasks)

- [x] **`T05` — `REVIEW_SYSTEM_PROMPT` Upgrades (Diff-First Inspection, Internal Exports, & Scoped Round 2+ Re-Review)**
  - `depends_on`: `[]` (`T02` complete)
  - `unblocks`: `["T04"]`
  - `plans`: [`.plans/03-code-and-review-prompts.md`](./.plans/03-code-and-review-prompts.md), [`.plans/PROPOSAL-8-internal-module-exports-in-review.md`](./.plans/PROPOSAL-8-internal-module-exports-in-review.md)
  - `files`: `src/prompts/review.rs`
  - `tasks`:
    - Preserve `REVIEW_MARKER` and all existing unit-test needles in `src/prompts/review.rs`.
    - Update Step 1 to start from `tools.diff()`, inspect modified/added files plus `AGENTS.md`/`README.md`, and inspect unchanged files outside the diff only for a named cross-cutting risk.
    - Update Step 2 (`sediment_and_coupling` and `code_and_scope`): check for tautological assertions, widened `pub` visibility for tests, and leftover debug probes/scratch files; clarify that internal cross-file exports (`pub(crate)` in Rust or non-entrypoint module `export`s in TS/JS not re-exported from the designated public entrypoint) are not unauthorized public API expansion (`PROPOSAL-8`).
    - Update Step 4: emit findings/fixes for blocking defects only, scope round 2+ re-reviews (`## Prior Review Rounds`) to verifying prior findings/fixes and checking the fix diff for regressions, and coalesce compatible fixes into the minimum number of fix tickets.
  - `verification_gate`: `cargo test --lib prompts::review && cargo test --test pipeline && cargo clippy --all-targets -- -D warnings`

- [x] **`T04` — `CODE_TDD_DISCIPLINE` Upgrades (Vertical Slices, No Artificial Red Probes, & Tracker Edits Last)**
  - `depends_on`: `["T05"]`
  - `unblocks`: `["T03"]`
  - `plans`: [`.plans/03-code-and-review-prompts.md`](./.plans/03-code-and-review-prompts.md), [`.plans/PROPOSAL-4-no-artificial-red-probes.md`](./.plans/PROPOSAL-4-no-artificial-red-probes.md), [`.plans/PROPOSAL-9-rollback-failed-ticket-and-cap-scope.md`](./.plans/PROPOSAL-9-rollback-failed-ticket-and-cap-scope.md)
  - `files`: `src/prompts/code.rs`
  - `tasks`:
    - Keep `CODE_SYSTEM_PROMPT` byte-identical so `tests/fixtures/llm_wire/*.json` remain valid.
    - Update `CODE_TDD_DISCIPLINE` in `src/prompts/code.rs`: work in vertical red-green slices at the public seam, never widen `pub` visibility on internal helpers solely for tests, replace obsolete coupled unit tests when consolidating behind a deeper seam, assert against independent known literals (anti-tautology), run focused test targets while iterating, never sabotage/stash/back up production files to force an artificial red failure when a new regression/boundary test already passes (`PROPOSAL-4`), and apply `TODO.md`/`.plan/*.md` tracker edits last right before `verification_gate` (reverting them if the gate fails, `PROPOSAL-9`).
  - `verification_gate`: `cargo test --lib prompts::code && cargo test --test llm_wire && cargo clippy --all-targets -- -D warnings`

- [x] **`T03` — Plan & Spec Prompt Upgrades (First-Turn Reconnaissance, Deep Modules, & Cross-Module Signatures)**
  - `depends_on`: `["T04"]`
  - `unblocks`: `["T06"]`
  - `plans`: [`.plans/01-plan-and-spec-prompts.md`](./.plans/01-plan-and-spec-prompts.md)
  - `files`: `src/prompts/plan.rs`, `src/prompts/spec.rs`
  - `tasks`:
    - In `PLAN_SYSTEM_PROMPT` (`src/prompts/plan.rs`, preserving `PLAN_MARKER` and the 4 `##` template headings): enforce prerequisite ordering across rounds, require first-turn `execute_script` reconnaissance + a 1–3 sentence summary of goal/current code/constraints before `Q1`, read `AGENTS.md` and `README.md` (following links), check for existing modules/redundancy before proposing new design options, and keep self-contained decision records without option letters.
    - In `SPEC_SYSTEM_PROMPT` (`src/prompts/spec.rs`, preserving all `## 1`..`## 7` and `###` headings): require reading `AGENTS.md` and `README.md` in Step 1, enforce deep module design with small public interfaces and real collaborators (edge-only mocking) in Step 2, and require exact cross-module function/type signatures in Step 3.
  - `verification_gate`: `cargo test --lib prompts::plan && cargo test --lib prompts::spec && cargo test --test pipeline && cargo clippy --all-targets -- -D warnings`

- [x] **`T06` — `TICKETS_SYSTEM_PROMPT` Upgrades (Per-Ticket Scope Cap & Cross-Ticket Signatures)**
  - `depends_on`: `["T03"]`
  - `unblocks`: `["T07", "T11", "T12", "T13", "T14"]`
  - `plans`: [`.plans/02-tickets-and-triage-prompts.md`](./.plans/02-tickets-and-triage-prompts.md), [`.plans/PROPOSAL-9-rollback-failed-ticket-and-cap-scope.md`](./.plans/PROPOSAL-9-rollback-failed-ticket-and-cap-scope.md)
  - `files`: `src/prompts/tickets.rs`
  - `tasks`:
    - Update Step 2 in `TICKETS_SYSTEM_PROMPT` to cap per-ticket scope at one primary user journey / entry point with a focused set of `test_expectations` (typically 3–8 BDD test cases in one test file, `PROPOSAL-9`) and batch small same-shape edits sharing one verification gate.
    - Require `prerequisites_assumed` to name the exact module paths, type names, and function signatures consumed from `blocked_by` tickets, and require `scope_of_changes` to name the exact public types and function signatures produced for `blocks` tickets (`02`).
  - `verification_gate`: `cargo test --lib prompts::tickets && cargo test --test pipeline && cargo clippy --all-targets -- -D warnings`

---

### Wave 2: Triage/Pipeline Flow & Independent Runtime Hardening (`T07`–`T15`)

- [ ] **`T07` — Add `TriageState::Done` Data Model & Upgrade `TRIAGE_SYSTEM_PROMPT`**
  - `depends_on`: `["T06"]`
  - `unblocks`: `["T08"]`
  - `plans`: [`.plans/PROPOSAL-1-triage-done-state.md`](./.plans/PROPOSAL-1-triage-done-state.md), [`.plans/02-tickets-and-triage-prompts.md`](./.plans/02-tickets-and-triage-prompts.md)
  - `files`: `src/triage.rs`, `src/prompts/triage.rs`, `src/tools.rs`, `TODO.md`
  - `tasks`:
    - Add `TriageState::Done` (wire `"done"`) in `src/triage.rs`, update `TriageState::ALL` to 5 states, add `pub fn authorizes_queue(self) -> bool` (`ReadyForAgent | Done`), and keep `is_ready(self)` true only for `ReadyForAgent` so `Done` carries audit `notes` and `blocker_explanation` without an `AgentBrief`.
    - Update `TRIAGE_SYSTEM_PROMPT` (`src/prompts/triage.rs`, preserving all existing test needles): add pairwise producer/consumer signature and self-consistency checks in Step 2 (`02`), require deterministic bug reproduction at the public seam under `bug` (`02`), and document `done` separately from `wontfix` under `## States` and `## Output rules` (`PROPOSAL-1`).
    - In `TODO.md`, mark `T07` complete (`[x]`) in the DAG and ordered backlog, mark `02-tickets-and-triage-prompts.md` `[x] Complete (T02, T06, T07 done)`, and update `PROPOSAL-1-triage-done-state.md` to `[ ]` Pending (T07 done; T08 pending) right before running `verification_gate`.
  - `verification_gate`: `cargo test --lib triage && cargo test --lib prompts::triage && cargo test --lib tools && cargo test --test test_titles && cargo clippy --all-targets -- -D warnings`

- [ ] **`T08` — Pipeline Queue Authorization & Pre-Completion for `TriageState::Done` Tickets**
  - `depends_on`: `["T07"]`
  - `unblocks`: `["T09"]`
  - `plans`: [`.plans/PROPOSAL-1-triage-done-state.md`](./.plans/PROPOSAL-1-triage-done-state.md)
  - `files`: `src/pipeline.rs`, `tests/pipeline.rs`, `TODO.md`
  - `tasks`:
    - Update `authorizes_queue` and `triage_blockers` in `src/pipeline.rs` to check `classification.state.authorizes_queue()`.
    - Partition `TriageState::Done` tickets into `progress.prior_completed` before `queue_and_review`, strip `Done` ticket IDs from remaining `ReadyForAgent` tickets' `blocked_by` lists, and when all draft tickets are `Done`, write `queue-status.json` and `queue-report.md` and proceed directly to `transition_gate` -> `review_round` -> `summary_pass`.
    - In `TODO.md`, mark `T08` complete (`[x]`) in the DAG and ordered backlog and mark `PROPOSAL-1-triage-done-state.md` `[x] Complete (T07, T08 done)` right before running `verification_gate`.
  - `verification_gate`: `cargo test --lib pipeline && cargo test --test pipeline && cargo test --test test_titles && cargo clippy --all-targets -- -D warnings`

- [ ] **`T09` — Forward Triage `notes` and `AgentBrief` to Code Agent Kickoff**
  - `depends_on`: `["T08"]`
  - `unblocks`: `["T10"]`
  - `plans`: [`.plans/PROPOSAL-5-forward-triage-brief-to-code-agent.md`](./.plans/PROPOSAL-5-forward-triage-brief-to-code-agent.md)
  - `files`: `src/triage.rs`, `src/orchestrator.rs`, `src/pipeline.rs`, `tests/pipeline.rs`, `TODO.md`
  - `tasks`:
    - Add `TriageReport::agent_context_for(&self, ticket_id: &str) -> Option<String>` in `src/triage.rs` to render `# Triage Audit` (`## Audit Notes` and `## Agent Brief`) for `ready-for-agent` tickets.
    - Store `triage: Option<TriageReport>` on `ReviewProgress` in `src/pipeline.rs`, pass `Option<&TriageReport>` into `Production` in `src/orchestrator.rs`, and append the triage audit block to the kickoff prompt in `Production::work` while keeping on-disk `tickets/<id>.md` unchanged.
    - In `TODO.md`, mark `T09` complete (`[x]`) in the DAG and ordered backlog and mark `PROPOSAL-5-forward-triage-brief-to-code-agent.md` `[x] Complete (T09 done)` right before running `verification_gate`.
  - `verification_gate`: `cargo test --lib triage && cargo test --lib orchestrator && cargo test --test pipeline && cargo test --test test_titles && cargo clippy --all-targets -- -D warnings`

- [ ] **`T10` — Include Verification Gate Failure Excerpts in `queue-report.md` and Retry Handovers**
  - `depends_on`: `["T09"]`
  - `unblocks`: `["T16"]`
  - `plans`: [`.plans/PROPOSAL-7-gate-failure-excerpt-in-queue-report.md`](./.plans/PROPOSAL-7-gate-failure-excerpt-in-queue-report.md)
  - `files`: `src/orchestrator.rs`, `src/pipeline.rs`, `tests/pipeline.rs`, `TODO.md`
  - `tasks`:
    - Add `gate_failure_excerpt(output: &str) -> String` (retaining up to the last 1,200 characters of trimmed output) and carry it on `Ending::GateFailed(String)` and `Ending::RetryExhausted(String)` in `src/orchestrator.rs`.
    - Render non-empty gate failure excerpts in `Report::markdown` (`src/orchestrator.rs`) so `Phase::Summary` and `queue_failures` / `queue_retry_detail` (`src/pipeline.rs`) include diagnostic context.
    - In `TODO.md`, mark `T10` complete (`[x]`) in the DAG and ordered backlog and mark `PROPOSAL-7-gate-failure-excerpt-in-queue-report.md` `[x] Complete (T10 done)` right before running `verification_gate`.
  - `verification_gate`: `cargo test --lib orchestrator && cargo test --lib pipeline && cargo test --test pipeline && cargo test --test test_titles && cargo clippy --all-targets -- -D warnings`

- [ ] **`T11` — Shell Execution Hardening (Process-Group Isolation, Partial Timeout Output, & Non-Interactive Env)**
  - `depends_on`: `["T06"]`
  - `unblocks`: `["T18"]`
  - `plans`: [`.plans/05-shell-execution-hardening.md`](./.plans/05-shell-execution-hardening.md)
  - `files`: `src/tools.rs`, `TODO.md`
  - `tasks`:
    - Configure `Command::new("bash")` with `process_group(0)` on Unix and kill the entire process group (`kill -s KILL -- -<pgid>`) on timeout before reaping the child and draining stream readers (`STREAM_DRAIN_GRACE = 200ms`).
    - Return `ShellOutcome` from `shell_output_with_timeout` carrying partial `stdout` and `stderr` captured before timeout; append non-empty partial output after the timeout header in `run_command_in` and `bash_in` (truncating command display at `BASH_FAILURE_COMMAND_BYTES` and spilling to `<session-dir>/tools-output/` when `> 16 KiB`).
    - Set non-interactive environment defaults (`GIT_TERMINAL_PROMPT=0`, `PAGER=cat`, `GIT_PAGER=cat`, `MANPAGER=cat`, `LESS=-R`, `CARGO_TERM_PROGRESS_WHEN=never`, `PIP_PROGRESS_BAR=off`, `TQDM_DISABLE=1`).
    - In `TODO.md`, mark `T11` complete (`[x]`) in the DAG and ordered backlog and mark `05-shell-execution-hardening.md` `[x] Complete (T11 done)` right before running `verification_gate`.
  - `verification_gate`: `cargo test --lib tools && cargo test --test test_titles && cargo clippy --all-targets -- -D warnings`

- [ ] **`T12` — CRLF-Safe `tools.replace`, Multi-Match Line Numbers, & `tools.grep` Line Cap**
  - `depends_on`: `["T06"]`
  - `unblocks`: `[]`
  - `plans`: [`.plans/09-file-editing-and-patch-matching.md`](./.plans/09-file-editing-and-patch-matching.md)
  - `files`: `src/script.rs`, `TODO.md`
  - `tasks`:
    - Reject no-op replacements (`old_text == new_text`) in `host_replace`.
    - Match against an LF-normalized view with Tier-1 exact matching and Tier-2 line-trimmed trailing horizontal whitespace fallback while preserving UTF-8 BOM (`\u{FEFF}`) and dominant line endings (`\r\n` vs `\n`).
    - Include 1-based match line numbers (`(at lines ...)` / `(first 5 at lines ...)`) when `matches > 1` while preserving existing error prefixes.
    - Cap individual matched lines in `tools.grep` at `GREP_LINE_MAX_BYTES = 2_000` on a UTF-8 character boundary.
    - In `TODO.md`, mark `T12` complete (`[x]`) in the DAG and ordered backlog and mark `09-file-editing-and-patch-matching.md` `[x] Complete (T12 done)` right before running `verification_gate`.
  - `verification_gate`: `cargo test --lib script && cargo test --test test_titles && cargo clippy --all-targets -- -D warnings`

- [ ] **`T13` — HTTP 408 Timeout Retry, Permanent 429 Quota Fast-Fail, & Retry-After Hints**
  - `depends_on`: `["T06"]`
  - `unblocks`: `["T17"]`
  - `plans`: [`.plans/12-http-retry-and-overload-resilience.md`](./.plans/12-http-retry-and-overload-resilience.md)
  - `files`: `src/retry.rs`, `TODO.md`
  - `tasks`:
    - Add HTTP `408` to retryable status codes in `should_retry`.
    - Classify permanent quota/billing exhaustion (`is_permanent_quota_error`, keeping `token_quota_exceeded` retryable) on HTTP `429` and `200..=299` so permanent billing errors fail fast.
    - Extract explicit retry-delay hints (`extract_retry_hint_secs`, clamped to `MAX_RATE_LIMIT_BACKOFF_SECS = 30`) from `429` messages and cap base exponential backoff at `MAX_BACKOFF_SECS = 10`.
    - In `TODO.md`, mark `T13` complete (`[x]`) in the DAG and ordered backlog and mark `12-http-retry-and-overload-resilience.md` `[x] Complete (T13 done)` right before running `verification_gate`.
  - `verification_gate`: `cargo test --lib retry && cargo test --test test_titles && cargo clippy --all-targets -- -D warnings`

- [ ] **`T14` — Provider Output-Token Truncation Detection (`Message.truncated`)**
  - `depends_on`: `["T06"]`
  - `unblocks`: `["T15"]`
  - `plans`: [`.plans/07-in-loop-truncation-and-tool-recovery.md`](./.plans/07-in-loop-truncation-and-tool-recovery.md)
  - `files`: `src/api.rs`, `src/llm/chat_completions.rs`, `src/llm/responses_api.rs`, `src/llm/anthropic.rs`, `TODO.md`
  - `tasks`:
    - Add `#[serde(skip)] pub truncated: bool` to `Message` in `src/api.rs` (initialized to `false` in `Message::text` and `Message::tool` so wire payloads and `context_bytes` stay unchanged).
    - Set `truncated = true` when `finish_reason == Some("length")` in `ChatProvider::decode` (`src/api.rs`, `src/llm/chat_completions.rs`), when `status == Some("incomplete")` and `incomplete_details["reason"] == Some("max_output_tokens")` in `ResponsesProvider::decode` (`src/llm/responses_api.rs`), and when `stop_reason == Some("max_tokens")` in `AnthropicProvider::decode` (`src/llm/anthropic.rs`, returning an empty `Message` with `truncated: true` when truncated before content/tool blocks arrive).
    - In `TODO.md`, mark `T14` complete (`[x]`) in the DAG and ordered backlog and update `07-in-loop-truncation-and-tool-recovery.md` to `[ ]` Pending (T14 done; T15 pending) right before running `verification_gate`.
  - `verification_gate`: `cargo test --lib api && cargo test --lib llm && cargo test --test test_titles && cargo clippy --all-targets -- -D warnings`

- [ ] **`T15` — In-Loop Missing-Tool / Truncation Recovery & Tool-Call Repetition Guard**
  - `depends_on`: `["T14"]`
  - `unblocks`: `["T16", "T17"]`
  - `plans`: [`.plans/07-in-loop-truncation-and-tool-recovery.md`](./.plans/07-in-loop-truncation-and-tool-recovery.md)
  - `files`: `src/loops/agent.rs`, `TODO.md`
  - `tasks`:
    - In `run_agent_loop_capturing` (`src/loops/agent.rs`), prompt up to `MAX_CONSECUTIVE_FORMAT_RECOVERIES = 2` times in-loop when an unattended phase omits a required tool call or when `assistant.truncated` is `true` before returning `Stop::TurnEnd` (preserving normal tool-free turns for non-truncated `Phase::Plan` and `Phase::Summary`).
    - Canonicalize tool calls per turn, append a loop-redirect notice (with canonical arguments preview capped at 500 UTF-8 bytes) to the last `Role::Tool` message at `TOOL_LOOP_WARN_THRESHOLD = 3` consecutive identical turns, and abort with `Stop::Failed(FailureKind::StepLimit)` at `TOOL_LOOP_ABORT_THRESHOLD = 5`.
    - In `TODO.md`, mark `T15` complete (`[x]`) in the DAG and ordered backlog and mark `07-in-loop-truncation-and-tool-recovery.md` `[x] Complete (T14, T15 done)` right before running `verification_gate`.
  - `verification_gate`: `cargo test --lib loops::agent && cargo test --test pipeline && cargo test --test test_titles && cargo clippy --all-targets -- -D warnings`

---

### Wave 3: Outer Pass Retries, Compaction Recovery, & Trajectory Persistence

- [ ] **`T16` — Outer Single-Pass Retry for Unattended Handoff Passes (`spec_pass`, `tickets_pass`, `triage_pass`, `review_round`)**
  - `depends_on`: `["T10", "T15"]`
  - `unblocks`: `[]`
  - `plans`: [`.plans/PROPOSAL-2-review-pass-retry.md`](./.plans/PROPOSAL-2-review-pass-retry.md)
  - `files`: `src/pipeline.rs`, `tests/pipeline.rs`, `TODO.md`
  - `tasks`:
    - Add `retries_after_unattended_stop(kind: Option<FailureKind>) -> bool` in `src/pipeline.rs` (`true` for `None` / `TurnEnd` and `Some(Malformed | StepLimit | ContextGuard)`, `false` for `Api | Transport`).
    - Retry `spec_pass`, `tickets_pass`, `triage_pass`, and `review_round` once in a fresh session (logging a `[WARN]` line via `status::shared().stderr_log`) before returning `RunTermination::ReviewIncomplete` or `self.downstream_retry(...)`.
    - In `TODO.md`, mark `T16` complete (`[x]`) in the DAG and ordered backlog and mark `PROPOSAL-2-review-pass-retry.md` `[x] Complete (T16 done)` right before running `verification_gate`.
  - `verification_gate`: `cargo test --lib pipeline && cargo test --test pipeline && cargo test --test test_titles && cargo clippy --all-targets -- -D warnings`

- [ ] **`T17` — Context Compaction Resilience, Tier-1 Soft Fallback, & HTTP 400/413/422 Overflow Recovery**
  - `depends_on`: `["T13", "T15"]`
  - `unblocks`: `["T18"]`
  - `plans`: [`.plans/11-context-compaction-and-overflow-recovery.md`](./.plans/11-context-compaction-and-overflow-recovery.md)
  - `files`: `src/loops/agent.rs`, `TODO.md`
  - `tasks`:
    - Retry transient failures during `request_handover_summary` via `send_summary_with_retry` (`retry::should_retry`).
    - Instruct `HANDOVER_SUMMARY_INSTRUCTION` to merge any earlier `[HANDOVER SUMMARY]` checkpoint into a single consolidated summary.
    - Split `SummaryError::SoftValidation` from `SummaryError::Fatal` in `prepare_for_send`, falling back to the Tier-1 compacted transcript when it fits within `CONTEXT_GUARD_BYTES` (4 MiB).
    - Detect context-overflow errors (`is_context_overflow_error` on HTTP `413` or HTTP `400`/`422` with overflow markers) below the 400 KiB watermark, compact once via `force_compact_on_overflow`, and retry the turn.
    - In `TODO.md`, mark `T17` complete (`[x]`) in the DAG and ordered backlog and mark `11-context-compaction-and-overflow-recovery.md` `[x] Complete (T17 done)` right before running `verification_gate`.
  - `verification_gate`: `cargo test --lib loops::agent && cargo test --test test_titles && cargo clippy --all-targets -- -D warnings`

- [ ] **`T18` — Per-Loop Conversation Trajectory Persistence (`.traj.json`)**
  - `depends_on`: `["T11", "T17"]`
  - `unblocks`: `[]`
  - `plans`: [`.plans/08-conversation-trajectory-persistence.md`](./.plans/08-conversation-trajectory-persistence.md)
  - `files`: `src/loops/agent.rs`, `src/tools.rs`, `TODO.md`
  - `tasks`:
    - Define `TrajectorySnapshot` (`phase`, `role`, `model`, `steps_taken`, `exit_status`, `messages`) and `Stop::exit_status_label` in `src/loops/agent.rs`.
    - When `invocation.session_dir` is `Some`, persist `<phase>.traj.json` (with `<phase>-<N>.traj.json` collision suffixes `1..10_000`) under `<session-dir>/trajectories/` on every loop exit path, calling `crate::tools::ignore_run_artifacts` and logging `[WARN]` on I/O errors without altering the returned `Stop`.
    - In `TODO.md`, mark `T18` complete (`[x]`) in the DAG and ordered backlog and mark `08-conversation-trajectory-persistence.md` `[x] Complete (T18 done)` right before running `verification_gate`.
  - `verification_gate`: `cargo test --lib loops::agent && cargo test --test pipeline && cargo test --test test_titles && cargo clippy --all-targets -- -D warnings`
