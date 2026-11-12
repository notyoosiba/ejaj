# Proposal 23: Dirty-Tree Incomplete Session Resume & `[DOWNSTREAM RETRY]` Recovery Guidance (`src/pipeline.rs`, `src/prompts/plan.rs`, `tests/prompt_contract.rs`)

## Problem

When a pipeline run stops in `spec`, `tickets`, `triage`, `queue`, `transition_gate`, or `review` without completing all work, two gaps prevent smooth recovery of the ongoing work with the `giga` Plan agent:

1. **Restarting `ejaj` after a failed queue is blocked by `git::preflight` (`src/pipeline.rs:153, 167, 294-300`, `src/git.rs:113-126`):**
   - Within a single process, `Pipeline::finish` sets `self.dirty_from_retry = true` (`src/pipeline.rs:925`), which switches `preflight_root()` from `git::preflight(&self.root)` to `git::preflight_repository(&self.root)` so the next `plan>>` turn can run over the uncommitted working-tree changes left by the incomplete queue.
   - However, `dirty_from_retry` is only held in memory. If `ejaj` exits, crashes, or is restarted after a failed run left uncommitted changes in the working tree (such as `.ejaj/20261108-181746`), `Pipeline::new` starts with `dirty_from_retry: false` and an empty `plan_agent` transcript, causing `preflight_root()` to fail fatally with `the project root ... has uncommitted changes`.
2. **`PLAN_SYSTEM_PROMPT` lacks recovery instructions for `[DOWNSTREAM RETRY]` (`src/prompts/plan.rs`):**
   - When `Pipeline::downstream_retry` (`src/pipeline.rs:937-946`) seeds the Plan transcript with `[DOWNSTREAM RETRY]\nphase: <phase>\noutcome: <detail>\n`, `PLAN_SYSTEM_PROMPT` has no instructions telling the Plan (`giga`) agent to inspect the uncommitted working-tree diff (`tools.diff()`) and the session artifacts so it can preserve already-completed slices and resolve the downstream blocker in the revised `Plan`.

---

## Guardrails for Existing `ejaj` Tests

- **Preserve the exact `[DOWNSTREAM RETRY]` handover format and 600-byte `MAX_RETRY_DETAIL_BYTES` cap (`src/loops/plan.rs:20-25, 118-175`):**
  - `tests/pipeline.rs:12362-12428` and `tests/support/retry_tests.rs:1283-1314` assert the exact 4-line `[DOWNSTREAM RETRY]\nphase: <phase>\noutcome: <outcome>\n` shape and `MAX_RETRY_DETAIL_BYTES = 600`. Do not alter `replace_handover` or `MAX_RETRY_DETAIL_BYTES` in `src/loops/plan.rs`.
- **Keep `git::preflight` strict when no incomplete `.ejaj/<timestamp>/` session exists (`tests/pipeline.rs`):**
  - A repository with uncommitted changes and **no** incomplete `.ejaj/<timestamp>/` session directory under `root.join(".ejaj")` must continue to fail `preflight_root()` with `Step::GitFailed { stage: "preflight", ... }`.
- **Preserve all `PLAN_SYSTEM_PROMPT` contract needles and forbidden-phrase checks (`tests/prompt_contract.rs`, `src/prompts/review.rs`):**
  - Keep `OPENING_MARKER`, `PLAN_TEMPLATE_HEADINGS` in exact order, and all required substrings in `tests/prompt_contract.rs:62-238` and `src/prompts/review.rs:210-256, 421-546`.
  - Never introduce any forbidden phrase (`"1 to 4 critical questions"`, `"authorization header"`, `"access control"`, `"permission check"`, `"authenticate"`, `"HTTP 401"`, `"HTTP 403"`, `"payload validation rule"`, `"access-control rule"`, `"numeric limit"`, `"maximum number of test expectations"`).

---

## Proposed Design

### 1. Resume Incomplete `.ejaj/<timestamp>/` Sessions Across Process Restarts (`src/pipeline.rs`)

In `Pipeline::new(config: &Config, scope: &str, root: impl AsRef<Path>) -> Pipeline` (`src/pipeline.rs:161-169`):

1. Inspect `root.join(".ejaj")` for the lexically newest session subdirectory `<timestamp>` that contains a valid `plan.md` (`Plan::from_markdown`).
2. Classify that newest session as **incomplete** when:
   - `tickets/queue-status.json` exists and contains any ticket with status `"failed"` or `"in_progress"`, **or**
   - `plan.md` exists and `change-summary.md` does not exist (the run stopped in `spec`, `tickets`, `triage`, `queue`, or `review` before producing a summary), **or**
   - `review.md` exists and its last `Verdict:` line is `Needs fixes`.
3. When the newest `.ejaj/<timestamp>/` session is incomplete:
   - Set `dirty_from_retry = true` on the new `Pipeline` instance so `preflight_root()` uses `git::preflight_repository(&self.root)` and permits resuming over the uncommitted changes that session left behind.
   - Reconstruct the returning `(phase, detail)` from the session's artifacts (`queue_retry_detail` from `queue-report.md` when present, `triage_blockers` from `ticket-triage.md` when present, or the last stage reached) and call `plan_agent.record_downstream_retry(&plan, phase, detail)` so the restarted `plan>>` session begins with the agreed `Plan` and `[DOWNSTREAM RETRY]` context already loaded.

### 2. `[DOWNSTREAM RETRY]` Recovery Guidance in `PLAN_SYSTEM_PROMPT` (`src/prompts/plan.rs`)

Add focused recovery guidance to `PLAN_SYSTEM_PROMPT` (`src/prompts/plan.rs`):

- When the conversation transcript contains `[DOWNSTREAM RETRY]`, a previous pipeline pass stopped at the named `phase` and `outcome`.
- Before asking a question or calling `finish_plan`, use `execute_script` (`tools.diff()`, `tools.list(".ejaj/**")`, `tools.read(...)`) to inspect the current working-tree diff and the latest `.ejaj/<timestamp>/` session artifacts (`ticket-triage.md`, `queue-report.md`, `review.md`, `tickets/*.md`).
- Preserve valid work already present in the working tree, resolve the downstream blocker in the revised `Plan`'s decisions and constraints (such as reconciling dependency/scope boundaries, clarifying interface signatures, or addressing a failed verification gate), and call `finish_plan` immediately when no genuinely blocking human question remains.

---

## Affected Files

- `src/pipeline.rs`
- `src/prompts/plan.rs`
- `tests/prompt_contract.rs`

## Verification

```bash
cargo test --lib pipeline && cargo test --lib prompts::plan && cargo test --test prompt_contract && cargo test --test pipeline && cargo test --test test_titles && cargo clippy --all-targets -- -D warnings
```
