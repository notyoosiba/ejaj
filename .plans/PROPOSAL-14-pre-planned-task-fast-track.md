# Proposal 14: Fast-Track Pre-Planned and Tracker-Backed Tasks in `PLAN_SYSTEM_PROMPT`

## Problem

`PLAN_SYSTEM_PROMPT` (`src/prompts/plan.rs:5,7,45`) currently assumes every request begins as an open-ended idea that requires at least one interview round:

1. **Unconditional reuse/extend/replace question (`src/prompts/plan.rs:5`):**
   > *"Before you offer design options, check whether the requested behaviour or a close equivalent already exists in the repository. When it does, your first round must cite the existing module and its path and ask whether to reuse it, extend it or replace it; never propose a parallel design that ignores the code already there."*

2. **First-round `Q1` framing (`src/prompts/plan.rs:7`):**
   > *"Open your first round with a brief (1 to 3 sentence) pre-question summary, before `Q1`..."*

When the user runs `ejaj` against a pre-planned task in `TODO.md` or `.plans/*.md`, the design interview has already happened in an earlier planning session: the task document already specifies the target files, function and type signatures, integration approach (whether to reuse, extend, or replace existing code), and `verification_gate`.

Because `PLAN_SYSTEM_PROMPT` does not treat explicit specifications in the user prompt or referenced repository task/proposal files as **already-settled decisions**, and unconditionally requires asking whether to reuse, extend, or replace any existing module it touches, the Plan agent manufactures a redundant Turn-1 confirmation question (`Q1 - How should T08 fit the existing flow? A) Extend... B) ... C) Replace...`) on every run instead of calling `finish_plan` immediately after reconnaissance.

---

## Proposed Design

Update `PLAN_SYSTEM_PROMPT` (`src/prompts/plan.rs`) so `Phase::Plan` acts as a **zero-round normalizer and code verifier** when given pre-planned work, while keeping its full interview workflow for open-ended requests:

1. **Treat explicit prompt and tracker/proposal specifications as settled decisions:**
   - Concrete design choices, target paths, and integration modes explicitly stated in the user's request or in a repository task, tracker, or proposal file it references (`TODO.md`, `.plan/`, `.plans/`) count as decisions the user has already settled, provided read-only `execute_script` reconnaissance confirms they match the live codebase.
   - Never ask the user to re-confirm an integration choice or design decision that the request or referenced task/proposal file already settles and the codebase supports.

2. **Qualify the "reuse, extend, or replace" question:**
   - Keep the existing needle (`Before you offer design options, check whether the requested behaviour or a close equivalent already exists in the repository. When it does, your first round must cite the existing module and its path and ask whether to reuse it, extend it or replace it`), and qualify it so that when the request or a referenced repository task/proposal file already settles whether to reuse, extend, or replace that module and reconnaissance finds no contradiction with the live code, the agent records that settled choice directly without asking a redundant confirmation question.

3. **Allow Turn-1 `finish_plan` after reconnaissance when no blocker remains:**
   - Keep read-only `execute_script` reconnaissance mandatory on Turn 1 for all code-related requests so stale `TODO.md` items or drifted signatures are still caught.
   - When a referenced task or proposal file already specifies the target module, how to extend or modify it, and the constraints to honour, and read-only reconnaissance finds no contradiction with the repository, skip `Q1` and call `finish_plan` immediately in that first turn.
   - Ask a question on pre-planned work **only** when reconnaissance discovers a concrete contradiction with the live codebase, an unstated blocking prerequisite, or an ambiguity as to which task to execute.

---

## Exact Updates to `src/prompts/plan.rs`

1. **In paragraph 2 (`src/prompts/plan.rs:5`), preserve all existing `tests/prompt_contract.rs` substrings while qualifying pre-settled task/proposal choices:**

```rust
Repository first. For a code-related request -- one that touches existing code or references repository files -- complete read-only repository reconnaissance before you ask your first question: read `AGENTS.md` and `README.md` when present, follow the relevant repository links they contain, and check how the affected behaviour works today before proposing a design. When the user's request references a task, tracker, or proposal file in the repository (such as `TODO.md`, `.plan/`, or `.plans/`), inspect that file and the target modules it names during reconnaissance and treat the concrete design choices, target paths, and integration mode it specifies as decisions the user has already settled unless they contradict the live codebase. Before you offer design options, check whether the requested behaviour or a close equivalent already exists in the repository. When it does, your first round must cite the existing module and its path and ask whether to reuse it, extend it or replace it whenever the request or referenced plan has not already settled that choice; never propose a parallel design that ignores the code already there, and never ask the user to re-confirm an integration choice that the request or referenced plan already specifies and the codebase supports. Finding facts is your job, never the user's: use the read-only `execute_script` inspection described below.
```

2. **In paragraph 5 (`src/prompts/plan.rs:45`), preserve all existing substrings while making Turn-1 `finish_plan` explicit for pre-planned requests:**

```rust
Do not prolong the interview. The session is complete as soon as the minimal viable design is defined (usually up to a few rounds, or immediately on the first turn after reconnaissance when the user's request or a referenced repository plan/task document already settles the design and matches the codebase). The moment no genuinely blocking decision remains, call the `finish_plan` tool immediately, in that same turn: do not summarize the plan first and do not wait for a separate confirmation turn, because a confirmation of decisions the user has already settled is redundant. When a referenced task or proposal file already specifies the target module, how to extend or modify it, and the constraints to honour, and read-only reconnaissance finds no contradiction with the repository, call `finish_plan` in that first turn without asking `Q1`. Ask the user and wait, as in every round above, only while a genuinely blocking decision is still open; never hand over a design that still depends on an unanswered question. A genuinely unresolved blocking decision is never silently treated as settled. Ask the user about it and wait; never hand off a plan that still depends on it. Do not implement anything yourself. If the user overrides or corrects an option, revise only what changed, then call `finish_plan` immediately once nothing blocking remains.
```

---

## Contract Test Updates (`tests/prompt_contract.rs`)

Add a caller-intent contract test in `tests/prompt_contract.rs`:

- `when_starting_given_preplanned_task_then_plan_prompt_treats_documented_choices_as_settled_and_hands_off`: asserts that `PLAN_SYSTEM_PROMPT` instructs the agent to inspect referenced `TODO.md`/`.plan/`/`.plans/` files during reconnaissance, treat their concrete design choices, target paths, and integration mode as already settled unless they contradict the live codebase, never ask the user to re-confirm an integration choice that the request or referenced plan already specifies, and call `finish_plan` in the first turn without asking `Q1` when reconnaissance finds no contradiction.

---

## Affected Files

- `src/prompts/plan.rs`
- `tests/prompt_contract.rs`

## Verification

```bash
cargo test --lib prompts && cargo test --test prompt_contract && cargo test --test pipeline && cargo test --test test_titles && cargo clippy --all-targets -- -D warnings
```
