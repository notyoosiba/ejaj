# Proposal 31: Unchanged-Plan Specification Reuse on Clean-Tree Downstream Retries (`src/plan_document.rs`, `src/specification.rs`, `src/pipeline.rs`)

## Problem

When an unattended downstream pass fails before the queue mutates the working tree—such as `tickets` ending early or `triage` rejecting a draft ticket set (`src/pipeline.rs:295-370, 619-785`)—`Pipeline::ask` returns `Step::Retry` to `plan>>` with `dirty_from_retry == false`:

1. **Redundant `spec` regeneration cost and latency:**
   - `Pipeline::downstream_retry` (`src/pipeline.rs:1185-1195`) preserves the agreed `Plan` inside `plan_agent`.
   - When the user re-submits at `plan>>` and `plan_agent` hands off the **exact same `Plan`**, `Pipeline::ask` unconditionally runs a fresh `spec_pass` (`src/pipeline.rs:342`) before reaching `tickets_pass`.
   - In `giga` and `hybrid` modes, `spec_pass` is often a multi-minute, high-token reasoning pass producing a 25–30 KB `spec.md`, even though neither the `Plan` nor the repository working tree changed since the previous attempt.
2. **Additional stochastic surface on retry:**
   - Re-generating `Specification` when only `tickets` slicing failed introduces unnecessary drift in story wording and interface signatures on retry.

---

## Guardrails for Existing `ejaj` Tests & Clippy Policy

- **Invalidate cached `Specification` whenever `Plan` or the working tree changes (`src/pipeline.rs`):**
  - Reuse is valid **only** when all three hold:
    1. `!self.dirty_from_retry` (no uncommitted queue/code changes landed since the spec was generated),
    2. `git::preflight(&self.root)` just succeeded at the top of `Pipeline::ask`, and
    3. The newly handed-off `Plan` equals the cached `Plan` (`cached_plan == plan`).
  - Clear the cache immediately whenever `dirty_from_retry` becomes `true`, `plan != cached_plan`, or `Pipeline::ask` reaches `Step::Complete`.
- **Always write `spec.md` into the new `.ejaj/<timestamp>/` directory (`src/pipeline.rs:584-591`):**
  - Every post-handoff session directory remains self-contained (`plan.md`, `spec.md`, `tickets/*.md`, `ticket-triage.md`, `metrics.json`). Reusing the in-memory `Specification` must still call `write_artifact(dir, "spec.md", &specification.to_markdown(), "spec", "spec written to")?` and preserve `Step::WriteFailed { artifact: "spec", .. }` on I/O failure.
- **Preserve `RunMetrics` contract (`src/metrics.rs:158-160`):**
  - `RunMetrics.spec_pass` is `Option<PassMetrics>` with `#[serde(skip_serializing_if = "Option::is_none")]`. On a reused specification where no `spec` LLM pass ran in this invocation, leave `self.spec_metrics = None` (or record zero-duration `PassOutcome::Handoff` only if explicitly specified; `None` accurately reflects that `trajectories/` in the new session directory has no `spec` trajectory).
- **Enforce Clippy complexity (`<= 15`) and caller-intent test titles (`tests/guidance.rs`, `tests/test_titles.rs`):**
  - Keep `resolve_specification(&mut self, plan: &Plan, dir: &Path) -> Result<Specification, Step>` small and name all new tests `when_<action>_given_<context>_then_<outcome>` without banned `INTERNAL_IDENTIFIERS`.

---

## Proposed Design

### 1. Derive `Clone` and `Eq` on `Plan` and `Specification` (`src/plan_document.rs`, `src/specification.rs`)

- Add `Clone, Eq` to `Plan` and `PlanDecision` in `src/plan_document.rs:34-60`.
- Add `Clone, Eq` to `Specification`, `UserStory`, `ImplementationDecisions`, `TestingDecisions`, and `DeferredItems` in `src/specification.rs:109-195`.

### 2. Cache `(Plan, Specification)` Across Clean-Tree Retries (`src/pipeline.rs`)

1. Add `cached_spec: Option<(Plan, Specification)>` to `Pipeline` (`src/pipeline.rs:170-210`), initialized to `None` in `Pipeline::new_with_clock`.
2. Extract `fn specification_for_plan(&mut self, plan: &Plan, dir: &Path) -> Result<Specification, Step>`:
   - If `!self.dirty_from_retry` and `self.cached_spec.as_ref().is_some_and(|(cached_plan, _)| cached_plan == plan)`:
     - Clone `specification` from `self.cached_spec`.
     - Emit an informational status line (`[INFO] Reusing specification from unchanged plan; spec written to ...`) or write `spec.md` directly via `write_artifact(dir, "spec.md", &specification.to_markdown(), "spec", "spec written to")?`.
     - Return `Ok(specification)` without invoking `spec::to_spec_in`.
   - Otherwise:
     - Clear `self.cached_spec = None`.
     - Run `let specification = self.spec_pass(plan, dir)?;`.
     - Store `self.cached_spec = Some((plan.clone(), specification.clone()));`.
     - Return `Ok(specification)`.
3. Clear `self.cached_spec = None` when:
   - `self.dirty_from_retry` is set to `true` in `finish` (`src/pipeline.rs`), or
   - `Step::Complete` is reached.

### 3. Test Coverage

Add integration tests in `tests/pipeline.rs` and `tests/hybrid_pipeline.rs`:
- `when_retrying_after_triage_refusal_given_unchanged_plan_and_clean_tree_then_reuses_specification_without_second_spec_call`: verifies `spec` provider transport is called once across the two `Pipeline::ask` calls, `spec.md` is still written in the second session directory, and the second `tickets` + `triage` passes proceed to completion.
- `when_retrying_after_triage_refusal_given_revised_plan_then_runs_fresh_spec_pass`: changing a decision or constraint in `Plan` invalidates the cached `Specification` and invokes `spec` again.
- `when_retrying_after_incomplete_queue_given_dirty_tree_then_runs_fresh_spec_pass`: when `dirty_from_retry` is `true`, even an identical `Plan` runs a fresh `spec_pass` against the mutated repository state.

---

## Affected Files

- `src/plan_document.rs`
- `src/specification.rs`
- `src/pipeline.rs`
- `tests/pipeline.rs`
- `tests/hybrid_pipeline.rs`

## Verification

```bash
cargo test --lib plan_document && cargo test --lib specification && cargo test --lib pipeline && cargo test --test pipeline && cargo test --test hybrid_pipeline && cargo test --test test_titles && cargo clippy --all-targets -- -D warnings
```
