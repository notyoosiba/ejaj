# Proposal 23: In-Memory `[DOWNSTREAM RETRY]` Scope Preservation & Live-Diff Recovery Guidance (`src/prompts/plan.rs`, `tests/prompt_contract.rs`)

## Problem

Within a running `ejaj` session, when an unattended pass stops in `spec`, `tickets`, `triage`, `queue`, `transition_gate`, or `review` without completing all work, `Pipeline::downstream_retry` (`src/pipeline.rs:1334-1344`) rewrites `plan_agent`'s in-memory handover to `{plan_markdown}\n[DOWNSTREAM RETRY]\nphase: <phase>\noutcome: <detail>\n` (`src/loops/plan.rs:118-138`), and `Pipeline::finish` (`src/pipeline.rs:1322`) sets `self.dirty_from_retry = true` in memory when stopping after queue execution. Two prompt gaps undermine recovery on the next `plan>>` turn:

1. **Silent scope collapse on targeted blocker replies (`src/loops/plan.rs:118-138`, `src/prompts/plan.rs`):**
   - `replace_handover` clears earlier interview turns and leaves `[system, assistant(Plan + [DOWNSTREAM RETRY]), user(reply)]`.
   - Because `PLAN_SYSTEM_PROMPT` contains no `[DOWNSTREAM RETRY]` contract, a narrow user reply fixing only the reported blocker (for example, adjusting one slice boundary) can cause the Plan agent to treat that delta reply as the entire task scope and call `finish_plan` with a partial `Plan` that silently drops every unaffected decision.
2. **`PLAN_SYSTEM_PROMPT` omits `tools.diff()` and live-tree `[DOWNSTREAM RETRY]` recovery guidance (`src/prompts/plan.rs:34-38`):**
   - Although `Phase::Plan` already exposes `tools.diff()` (`src/script.rs:2128`), `PLAN_SYSTEM_PROMPT` only lists `tools.list`, `tools.grep`, and `tools.read`, and never instructs the Plan agent to inspect uncommitted Git working-tree changes (`tools.diff()`) left by an incomplete in-process queue run so valid work stays preserved in the revised `Plan`.

---

## Guardrails for Existing `ejaj` Tests & Architecture

- **Never rely on `.ejaj/` state or existence:**
  - Files under `.ejaj/` are write-only historical records, never runtime progress or state. Do not read `.ejaj/` on startup (`Pipeline::new`), do not persist or reconstruct `dirty_from_retry` across process restarts, and do not instruct `PLAN_SYSTEM_PROMPT` to list, grep, or read `.ejaj/` during `[DOWNSTREAM RETRY]`. All retry context comes strictly from the in-memory `[DOWNSTREAM RETRY]` handover message and live repository inspection (`tools.diff()`, `tools.list()`, `tools.grep()`, `tools.read()`).
- **Preserve the exact `[DOWNSTREAM RETRY]` handover format, 3-message transcript shape, and 600-byte `MAX_RETRY_DETAIL_BYTES` cap (`src/loops/plan.rs:20-25, 118-175`):**
  - `tests/pipeline.rs:12355-12428` and `tests/support/retry_tests.rs:1283-1314` assert `[system, assistant(plan + [DOWNSTREAM RETRY]), user]`, the exact 4-line `[DOWNSTREAM RETRY]\nphase: <phase>\noutcome: <outcome>\n` shape, and `MAX_RETRY_DETAIL_BYTES = 600`. Do not alter `replace_handover` or `MAX_RETRY_DETAIL_BYTES` in `src/loops/plan.rs`.
- **Preserve `Object.keys(tools)` for `Phase::Plan` (`src/script.rs:2128`):**
  - Read-only phases already expose `["diff", "grep", "list", "read"]`; no changes to `src/script.rs` are required.
- **Preserve all `PLAN_SYSTEM_PROMPT` contract needles, forbidden-phrase checks, and caller-intent test titles (`tests/prompt_contract.rs`, `src/prompts/review.rs`, `tests/test_titles.rs`):**
  - Keep `OPENING_MARKER`, `PLAN_TEMPLATE_HEADINGS` in exact order, and all required substrings in `tests/prompt_contract.rs:62-238` and `src/prompts/review.rs:210-256, 421-546`.
  - Never introduce any forbidden phrase (`"1 to 4 critical questions"`, `"authorization header"`, `"access control"`, `"permission check"`, `"authenticate"`, `"HTTP 401"`, `"HTTP 403"`, `"payload validation rule"`, `"access-control rule"`, `"numeric limit"`, `"maximum number of test expectations"`).
  - Name every new test `when_<action>_given_<context>_then_<outcome>` without banned `INTERNAL_IDENTIFIERS`.

---

## Proposed Design

### 1. Document `tools.diff()` in `PLAN_SYSTEM_PROMPT` (`src/prompts/plan.rs`)

In the `execute_script` read-only inspection bullet list (`src/prompts/plan.rs:34-38`):
- Add `- To inspect uncommitted working-tree changes, use: tools.diff()` alongside `tools.list(glob?)`, `tools.grep(pattern, glob?)`, and `tools.read(path, startLine?, endLine?)`.

### 2. `[DOWNSTREAM RETRY]` Scope-Preservation & Live-Tree Recovery Rules (`src/prompts/plan.rs`)

Add focused `[DOWNSTREAM RETRY]` recovery rules to `PLAN_SYSTEM_PROMPT` (`src/prompts/plan.rs`):
- When the prior assistant turn ends with `[DOWNSTREAM RETRY]`, the markdown above `[DOWNSTREAM RETRY]` is the active agreed `Plan` from the previous pass in this session, and `phase` and `outcome` report why that pass stopped.
- Treat the user's reply as resolving the reported downstream blocker against that active `Plan`, not as a standalone replacement goal: **preserve every unaffected Decision, Default, and Constraint from the active `Plan`** unless the user explicitly asks to remove, replace, or narrow the overall goal.
- Before asking a question or calling `finish_plan`, use `execute_script` (`tools.diff()`, `tools.list()`, `tools.grep()`, `tools.read()`) against the live repository to inspect any uncommitted working-tree changes left by an incomplete in-process run and the affected source modules, preserve valid work already present in the working tree, revise the `Plan` to resolve the downstream blocker, and call `finish_plan` immediately when no genuinely blocking human question remains.

### 3. Prompt Contract Coverage (`tests/prompt_contract.rs`)

- Add caller-intent (`when_<action>_given_<context>_then_<outcome>`) tests in `tests/prompt_contract.rs` asserting that `PLAN_SYSTEM_PROMPT` documents `tools.diff()`, treats the markdown above `[DOWNSTREAM RETRY]` as the active agreed `Plan`, preserves unaffected decisions/defaults/constraints unless the user explicitly narrows or replaces the goal, and inspects live working-tree changes via `tools.diff()` without referencing `.ejaj/` artifacts.

---

## Affected Files

- `src/prompts/plan.rs`
- `tests/prompt_contract.rs`

## Verification

```bash
cargo test --lib prompts::plan && cargo test --lib prompts::review && cargo test --test prompt_contract && cargo test --test test_titles && cargo clippy --all-targets -- -D warnings
```
