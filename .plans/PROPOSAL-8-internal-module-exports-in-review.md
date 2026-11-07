# Proposal 8: Distinguish Internal Cross-File Module Exports from Public API Surface in Review

## Problem

When the Code agent extracts an internal helper into a sibling module within the repository, languages like TypeScript/ES modules (`export const ...`) and Rust (`pub(crate) fn ...` / `pub mod ...`) require an export keyword for cross-file imports inside the same package. Without explicit guidance, the Review agent can misclassify internal cross-file exports as "unauthorized public API expansion" under `code_and_scope` and repeatedly fail review until the review circuit breaker trips.

---

## Proposed Design

Update `REVIEW_SYSTEM_PROMPT` (`src/prompts/review.rs`) under `2. Complete the Pre-Flight Audit Scorecard` -> `code_and_scope` (integrated into `03-code-and-review-prompts.md`):

- Clarify that internal helpers exported solely for cross-file imports within the repository (such as `pub(crate)` items in Rust or non-entrypoint module `export`s in TypeScript/JavaScript that are not re-exported from the package's public entrypoint or the specified public module under test) do **not** constitute unauthorized public API expansion.
- Reserve public-API scope findings for changes that alter or expand the signatures/exports of the specification's designated public entrypoints.

---

## Affected Files

- `src/prompts/review.rs`

## Verification

```bash
cargo test --lib prompts::review && cargo clippy --all-targets -- -D warnings
```
