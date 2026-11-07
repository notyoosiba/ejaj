# Proposal 4: Stop Artificial "Red-Probe" Sabotage When Tests Already Pass

## Problem

`CODE_TDD_DISCIPLINE` (`src/prompts/code.rs:104-117`) currently instructs the Code agent:

> `1. Author the outside-in tests listed in ## Test Expectations first...`
> `4. Run the test command with bash to observe the expected red failure before implementing production changes, then implement the production code and run the ticket's verification_gate until it exits 0.`

When the Code agent executes a **review-fix ticket (`r<round>-<n>-...`)**, a **retry attempt (`[RETRY HANDOVER]`)**, or a **boundary/regression test** whose production behavior was already implemented by an earlier ticket, the newly authored test **passes immediately (Green)** on the first run. Because rule 4 unconditionally demands observing a red failure first, the Code agent temporarily breaks or deletes production code (sometimes leaving backup files such as `.ejaj/*.bak`), runs the test suite just to manufacture a red failure, and then restores the file.

---

## Proposed Design

Update step 4 of `CODE_TDD_DISCIPLINE` in `src/prompts/code.rs:115-117` (coordinated with `03-code-and-review-prompts.md` and `PROPOSAL-9`) so the agent never sabotages existing production code to manufacture a red test failure:

- When new production behavior is still missing, run the focused test target with `bash` to confirm the new test fails (Red), then implement the minimal production code and re-run to confirm Green.
- If existing production code already satisfies a newly added regression or boundary test (for example on a retry handover or coverage fix ticket), keep the passing result and **never break, stash, or back up production files just to force a red failure**.

See `03-code-and-review-prompts.md` for the unified `CODE_TDD_DISCIPLINE` constant.

---

## Affected Files

- `src/prompts/code.rs`

## Verification

```bash
cargo test --lib prompts::code && cargo clippy --all-targets -- -D warnings
```
