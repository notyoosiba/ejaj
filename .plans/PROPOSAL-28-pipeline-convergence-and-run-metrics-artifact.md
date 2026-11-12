# Proposal 28: Pipeline Convergence Metrics & `.ejaj/<timestamp>/metrics.json` Run Summary Artifact (`src/metrics.rs`, `src/lib.rs`, `src/loops/plan.rs`, `src/loops/summary.rs`, `src/pipeline.rs`)

## Prerequisite

Depends on `T31` (`.plans/PROPOSAL-27-queue-contributions-and-attempt-metrics.md`).

## Problem

Even with loop trajectories and per-round queue metrics in memory, `ejaj` lacks a single structured run summary artifact under `.ejaj/<timestamp>/metrics.json` capturing end-to-end execution, quality-gate convergence, and cost:

1. **Fresh-session retry rates across `spec`, `tickets`, `triage`, and `review` are unrecorded (`src/pipeline.rs:47-68`):**
   - `attempt_with_fresh_session_retry` logs a `[WARN]` line on stderr when Attempt 1 fails or ends early, then runs Attempt 2—without recording which phases needed a fresh-session retry or whether Attempt 2 rescued the pass.
2. **Triage, Transition-Gate vs. Local-Gate regressions, and Review convergence are scattered (`src/pipeline.rs:433-858`):**
   - Analyzing how often triage blocks draft sets, how often tickets pass local gates yet fail `transition_gate` (`cargo test` / `npm test`), how many review rounds and fix tickets a run took, or whether `MAX_STALLED_REVIEW_ROUNDS` tripped requires scraping multiple Markdown artifacts.
3. **Summary canonicalizer `pass@k` (`1..=4`) and rejection categories are discarded (`src/loops/summary.rs:124-182`):**
   - `summary_pass` runs up to `MAX_CANDIDATES = 4` turns in one transcript when `canonicalize_with_ticket_ids` rejects an answer, discarding how many candidates were needed and which format rule rejected earlier candidates.
4. **Role-level token usage and estimated USD cost are not rolled up across the run:**
   - There is no single JSON document aggregating `TokenUsage` and estimated USD cost per role (`mid`, `giga`, `smoll`) and across the whole run on both successful (`Step::Complete`) and incomplete/failed post-handoff exits.

---

## Guardrails for Existing `ejaj` Tests & Clippy Policy

- **CRITICAL — Persist `metrics.json` ONLY after `plan` handoff and ONLY when `dir` is under `.ejaj/` (`src/loops/agent.rs:1013-1026`, `src/pipeline.rs:172-240`):**
  - Never write `metrics.json` on `preflight_root()` failures or when `plan_handoff` returns `TurnEnd` / `Failed` before `finish_plan` (before `plan.md` is written, `dir` must not be created).
  - Resolve `let base = if dir.is_absolute() { dir.to_path_buf() } else { self.root.join(dir) };` and return `Ok(())` immediately when `crate::tools::ejaj_ancestor(&base).is_none()`.
    - Why: legacy tests in `tests/pipeline.rs` pass non-`.ejaj` directories inside temporary Git repositories; gating on `ejaj_ancestor(&base).is_some()` (exact same contract as `save_trajectory` in `src/loops/agent.rs:1024`) guarantees `metrics.json` is never staged by `git add --all` in non-`.ejaj` test paths, while every real `.ejaj/<timestamp>/` session writes `.ejaj/<timestamp>/metrics.json` under `.ejaj/.gitignore`!
  - Persistence is best-effort: on I/O failure, log `[WARN] could not persist run metrics: {error}` via `status::shared().stderr_log(...)` and **never** convert a `Step` into `Step::WriteFailed`.
- **CRITICAL — Do NOT place anything inside `.ejaj/<timestamp>/tickets/` (`tests/pipeline.rs:65, 1485`, `tests/hybrid_pipeline.rs:49`):**
  - Integration tests assert `fs::read_dir(dir.join("tickets"))`; write `metrics.json` directly at `base.join("metrics.json")` beside `plan.md` and `queue-report.md`.
- **CRITICAL — Preserve `src/loops/summary.rs` test-title invariant (`tests/test_titles.rs:163-179`):**
  - `when_renaming_given_rewritten_summary_routing_test_then_uses_domain_level_titles` asserts that **at most one** `#[test]` function in `src/loops/summary.rs` has a name containing **both** `"provider"` AND `"summary"`! Never add a test in `src/loops/summary.rs` whose name contains both words (e.g., use `when_canonicalizing_change_note_given_...` or avoid `"provider"`), and never use `"summarize"` (forbidden by `INTERNAL_IDENTIFIERS` in `tests/test_titles.rs:26`).
- **Preserve public `summary::summarize` signature (`src/loops/summary.rs:84-103`):**
  - Keep `pub fn summarize(...) -> Outcome` unchanged; add `pub(crate) fn summarize_with_metrics_in(...) -> (Outcome, SummaryPassMetrics)` (or carry metrics out of `summarize_in`) for `Pipeline::summary_pass`.
- **Enforce Clippy complexity (`<= 15`) and lint gates (`tests/guidance.rs`):**
  - Put all metric data types, summary-rejection classification, trajectory-usage rollup, and serialization helpers in a dedicated new module `src/metrics.rs` (`pub mod metrics;` in `src/lib.rs`) so functions in `src/pipeline.rs` and `src/loops/summary.rs` stay concise and below cognitive complexity 15.
- **Scope tracker updates strictly to `TODO.md`:**
  - Mark `T32` `[x]` in `TODO.md` right before running the verification gate; do not edit `.plans/PROPOSAL-28-pipeline-convergence-and-run-metrics-artifact.md`.

---

## Proposed Design

### 1. New `src/metrics.rs` Schema (`src/lib.rs`, `src/metrics.rs`)

Declare `pub mod metrics;` in `src/lib.rs` with clean `Serialize + Deserialize` structs:

```rust
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RunMetrics {
    pub mode: String,
    pub mid_model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub giga_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub smoll_model: Option<String>,
    /// `"complete"`, `"retry:spec"`, `"retry:tickets"`, `"retry:triage"`,
    /// `"retry:queue"`, `"retry:transition_gate"`, `"retry:review"`,
    /// `"summary_failed"`, `"write_failed:<artifact>"`, or `"git_failed:<stage>"`.
    pub terminal_outcome: String,
    pub total_elapsed_ms: u64,
    /// Number of `plan::Session::ask_in` calls required to reach `finish_plan`
    /// (1 when the first prompt handed off immediately).
    pub plan_interview_turns: usize,
    pub plan_elapsed_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spec: Option<UnattendedPassMetrics>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tickets_pass: Option<UnattendedPassMetrics>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub triage: Option<TriagePassMetrics>,
    pub queue_rounds: Vec<crate::orchestrator::QueueRoundMetrics>,
    pub transition_gate: TransitionGateMetrics,
    pub review_rounds: Vec<ReviewRoundMetrics>,
    pub review_stalled_breaker_tripped: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<SummaryPassMetrics>,
    /// Aggregated token usage and optional USD cost per provider role
    /// (`"mid"`, `"giga"`, `"smoll"`), summed from `<dir>/trajectories/*.traj.json`.
    pub role_usage: std::collections::BTreeMap<String, RoleUsageSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_estimated_cost_usd: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnattendedPassMetrics {
    pub elapsed_ms: u64,
    pub fresh_session_retried: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_attempt_failure: Option<String>,
    pub succeeded: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TriagePassMetrics {
    pub pass: UnattendedPassMetrics,
    pub draft_tickets: usize,
    pub ready_tickets: usize,
    pub done_tickets: usize,
    pub blocked_tickets: usize,
    pub authorized_queue: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransitionGateMetrics {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    pub runs: usize,
    pub failures: usize,
    pub total_elapsed_ms: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewRoundMetrics {
    pub round: usize,
    pub pass: UnattendedPassMetrics,
    /// `"pass"`, `"needs_fixes"`, or `"incomplete"`.
    pub verdict: String,
    pub findings_count: usize,
    pub fix_tickets_emitted: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SummaryPassMetrics {
    pub elapsed_ms: u64,
    pub candidates_generated: usize,
    pub rejection_categories: Vec<String>,
    pub accepted: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RoleUsageSummary {
    pub invocations: usize,
    pub steps_taken: usize,
    pub http_requests: usize,
    pub http_retries: usize,
    pub usage: crate::api::TokenUsage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_cost_usd: Option<f64>,
}
```

### 2. Capturing Phase, Retry, and Summary Metrics (`src/loops/plan.rs`, `src/loops/summary.rs`, `src/pipeline.rs`)

1. **Plan interview turn counter (`src/loops/plan.rs`):**
   - Add `turns_since_handoff: usize` on `plan::Session` (`0` in `Session::new`), increment `self.turns_since_handoff += 1` at the start of `ask_in`, expose `pub(crate) fn turns_since_handoff(&self) -> usize`, and reset `self.turns_since_handoff = 0` inside `accept_handoff`.
2. **Instrument `attempt_with_fresh_session_retry` (`src/pipeline.rs:47-68`):**
   - Add `attempt_with_fresh_session_retry_tracked` returning `(O, bool, Option<String>)` (`(outcome, retried, first_failure_reason)`) so `spec_pass`, `tickets_pass`, `triage_pass`, and `review_round` populate `UnattendedPassMetrics` cleanly while keeping exact stderr warning output.
3. **Classify summary canonicalizer rejections (`src/loops/summary.rs:140-182`, `src/metrics.rs`):**
   - In `summary_pass`, record `candidates_generated = candidate` (`1..=4`) and push `classify_summary_rejection(&reason)` whenever `canonicalize_with_ticket_ids` returns `Err(reason)`:
     - `reason.contains("answer is empty")` -> `"empty"`
     - `reason.contains("Markdown fence")` -> `"markdown_fence"`
     - `reason.contains("generated ticket id")` -> `"generated_ticket_id"`
     - `reason.contains("Refs:")` -> `"refs_footer"`
     - `reason.contains("footer")` -> `"invalid_footer"`
     - `reason.contains("blank line")` -> `"missing_blank_line"`
     - `reason.contains("characters; the limit")` or `reason.contains("characters long")` -> `"line_over_limit"`
     - `reason.contains("body")` -> `"empty_body"`
     - otherwise -> `"invalid_header_or_preamble"`.
4. **Roll up `role_usage` and `total_estimated_cost_usd` (`src/metrics.rs`):**
   - Write `collect_role_usage(base_dir: &Path, config: &Config) -> (BTreeMap<String, RoleUsageSummary>, Option<f64>)` in `src/metrics.rs`:
     - Read all `*.traj.json` files in `base_dir.join("trajectories")` (sorted lexically; ignore missing directory).
     - Sum `invocations`, `steps_taken`, `telemetry.http_requests`, `telemetry.http_retries`, and `telemetry.usage` grouped by `role` (`"mid"`, `"giga"`, `"smoll"`).
     - Price each role using `TokenUsage::estimate_cost_usd` with that role's configured rates on `Config` (`Phase::Code` on `Mode::Mid` or direct role rate helper `Config::role_rates(role: &str) -> (Option<f64>, Option<f64>, Option<f64>)`).
     - `total_estimated_cost_usd` is `Some(sum)` when at least one role with invocations is priced, else `None`.
5. **Persist `.ejaj/<timestamp>/metrics.json` at every post-handoff exit of `Pipeline::ask` (`src/pipeline.rs:195-295`):**
   - Once `plan_handoff` returns `Ok(plan)`, initialize a `RunMetricsBuilder` with `started = Instant::now()`, `plan_interview_turns`, and `plan_elapsed_ms`.
   - Before returning from any post-handoff path (`spec`, `tickets`, `triage`, `queue_and_review`, `summary_pass`, `record_run`, `finish`, or `write_artifact` failure after `plan.md`), finalize `terminal_outcome` and `total_elapsed_ms` and call `metrics::save_run_metrics(&self.root, dir, &self.config, builder)`.

### 3. Test Coverage

Add caller-intent (`when_<action>_given_<context>_then_<outcome>`) tests:
- `src/metrics.rs`:
  - `classify_summary_rejection` maps each validator failure message to its deterministic category.
  - `collect_role_usage` sums `mid`, `giga`, and `smoll` trajectory usage and computes `estimated_cost_usd` + `total_estimated_cost_usd`.
  - `save_run_metrics` writes `metrics.json` when `dir` is under `.ejaj/` and is a no-op when `dir` has no `.ejaj` ancestor.
- `src/loops/summary.rs`:
  - Multi-candidate correction pass reports `candidates_generated == 2` and records the first candidate's rejection category before accepting the corrected summary (remember: do not put both `"provider"` and `"summary"` in the test title!).
- `tests/hybrid_pipeline.rs` or `tests/pipeline.rs`:
  - Full `.ejaj/<timestamp>/` pipeline run writes `.ejaj/<timestamp>/metrics.json` with `terminal_outcome == "complete"`, populated `spec`/`tickets_pass`/`triage`/`queue_rounds`/`review_rounds`/`summary`, and `role_usage` entries for `"giga"`, `"mid"`, and `"smoll"`.
  - Incomplete post-handoff run (e.g., blocked triage or failed queue) still writes `.ejaj/<timestamp>/metrics.json` with `"retry:triage"` / `"retry:queue"`, while a `TurnEnd` plan question before handoff writes nothing.

---

## Affected Files

- `src/lib.rs`
- `src/metrics.rs`
- `src/config.rs`
- `src/loops/plan.rs`
- `src/loops/summary.rs`
- `src/pipeline.rs`
- `tests/hybrid_pipeline.rs`
- `TODO.md`

## Verification

```bash
cargo test --lib metrics && cargo test --lib loops && cargo test --lib pipeline && cargo test --test pipeline && cargo test --test hybrid_pipeline && cargo test --test test_titles && cargo clippy --all-targets -- -D warnings
```
