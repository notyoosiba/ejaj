# Proposal 29: Multi-Run Corpus Aggregation & Offline `ejaj --metrics` Scorecard (`src/metrics.rs`, `src/main.rs`, `tests/config_cli.rs`)

## Prerequisite

Depends on `T32` (`.plans/PROPOSAL-28-pipeline-convergence-and-run-metrics-artifact.md`).

## Problem

Once `.ejaj/<timestamp>/metrics.json` (`T32`) lands on disk per run, operators and self-diagnostic agents still lack a built-in way to aggregate metrics across historical `.ejaj/*/` runs to spot systemic implementation bottlenecks:

1. **No cross-run aggregation seam (`src/metrics.rs`):**
   - Comparing `mid`, `giga`, and `hybrid` ROI (mean USD and wall time per completed ticket), first-pass vs. handover-retry rescue rates, `mid -> giga` escalation conversion, prompt cache hit rates, control-tool rejection hotspots, local-vs-transition-gate regression rates, and summary `pass@1` rates requires writing ad-hoc scripts over individual `metrics.json` files.
2. **No offline CLI scorecard (`src/main.rs:340-355`):**
   - Starting `ejaj` always invokes `configured()` (`src/main.rs:343`), which requires valid `LLM_SMOLL_*` (and `LLM_GIGA_*` in `hybrid`/`giga`) environment variables before opening the interactive `plan>>` REPL. There is no offline, zero-key `ejaj --metrics [dir]` command to print a corpus scorecard on a developer laptop or CI artifact directory.

---

## Guardrails for Existing `ejaj` Tests & Clippy Policy

- **Preserve zero-argument CLI startup behavior (`src/main.rs:340-360`, `tests/config_cli.rs`):**
  - When `std::env::args().skip(1)` is empty, `main()` must execute `load_dotenv()`, `configured()`, `enter_project_root()`, `line_editor()`, and `run_repl(...)` in the **exact** existing order so every startup and fatal-config test in `tests/config_cli.rs` passes unchanged.
- **Tolerant corpus scanning (`src/metrics.rs`):**
  - A missing `.ejaj` directory or a directory with zero `metrics.json` files is valid (renders `"0 runs analyzed"` rather than failing).
  - Unreadable or malformed `<session>/metrics.json` files increment `skipped_files: usize` rather than aborting the whole report.
  - Only unknown CLI flags (e.g., `ejaj --unknown`) or extra positional arguments past `--metrics [dir]` print one `[ERROR] ...` line to stderr and exit with status `1`.
- **Enforce Clippy complexity (`<= 15`) and lint gates (`tests/guidance.rs`):**
  - Decompose aggregation and Markdown table formatting in `src/metrics.rs` into small per-section helpers (`aggregate_mode_stats`, `aggregate_ticket_funnel`, `aggregate_loop_friction`, `render_markdown_report`) so every function stays well under cognitive complexity 15.
- **Enforce caller-intent test-title contract (`tests/test_titles.rs`):**
  - Name every new test `when_<action>_given_<context>_then_<outcome>` without any `INTERNAL_IDENTIFIERS` (`thinking_label`, `group_digits`, `provider_role`, `summarize`, `call_time`, `measured`, `new`, `with_giga`, etc.).
- **Scope tracker updates strictly to `TODO.md`:**
  - Mark `T33` `[x]` in `TODO.md` right before running the verification gate; do not edit `.plans/PROPOSAL-29-corpus-metrics-aggregation-and-cli-report.md`.

---

## Proposed Design

### 1. Corpus Aggregation Model (`src/metrics.rs`)

Add `CorpusReport` and `pub fn aggregate_ejaj_dir(ejaj_dir: &Path) -> CorpusReport` in `src/metrics.rs`:

1. **Discovery:**
   - If `!ejaj_dir.exists()`, return `CorpusReport::default()`.
   - Read child entries of `ejaj_dir` sorted lexically by directory name; for each directory containing `metrics.json`, deserialize `RunMetrics` (`skipped_files += 1` on read/JSON error).
2. **Aggregated dimensions (`CorpusReport`):**
   - **Run & Mode Summary (`BTreeMap<String, ModeSummary>` keyed by `"mid"`, `"giga"`, `"hybrid"`):**
     - `runs_total`, `runs_completed`, `tickets_completed`, `total_elapsed_ms`, `priced_runs`, `total_cost_usd: f64`.
     - Derived: completion rate (`%`), mean wall time per completed run, and mean cost per completed ticket (`total_cost_usd / tickets_completed` across priced runs).
   - **Terminal Outcome Breakdown (`BTreeMap<String, usize>`):**
     - Counts of `"complete"`, `"retry:triage"`, `"retry:queue"`, `"retry:transition_gate"`, `"retry:review"`, `"summary_failed"`, `"git_failed:*"`, etc.
   - **Role Token & Cache Efficiency (`BTreeMap<String, RoleCorpusSummary>` keyed by `"mid"`, `"giga"`, `"smoll"`):**
     - Summed `invocations`, `steps_taken`, `http_requests`, `http_retries`, `usage: TokenUsage`, `estimated_cost_usd: Option<f64>`, and derived prompt cache hit rate (`cached_input_tokens / input_tokens`).
   - **Ticket Queue, Handover & Escalation Funnel (`TicketFunnelSummary`):**
     - `tickets_total`, `attempt1_completed` (first-pass resolution count), `attempt2_triggered`, `attempt2_rescued` (overall handover rescue count), `attempt1_failures_by_cause: BTreeMap<String, (usize, usize)>` (`cause -> (triggered, rescued)`), `hybrid_escalations_triggered`, `hybrid_escalations_rescued`, and `blocked_downstream_tickets`.
   - **Quality Gate & Convergence Summary (`ConvergenceSummary`):**
     - `fresh_session_retries_by_phase: BTreeMap<String, (usize, usize)>` (`phase -> (retried, succeeded)` for `spec`, `tickets`, `triage`, `review`).
     - `triage_runs`, `triage_blocked_runs`, `triage_precompleted_tickets`.
     - `transition_gate_runs`, `transition_gate_failures` (local-vs-full-suite regression rate!), `transition_gate_elapsed_ms`.
     - `review_rounds_total`, `review_fix_tickets_total`, `review_stalled_trips`.
     - `summary_runs`, `summary_pass_at_1`, `summary_accepted`, `summary_rejections_by_category: BTreeMap<String, usize>`.
   - **Tool & Loop Friction Summary (`LoopFrictionSummary`):**
     - Summed across `TicketExecutionMetrics.telemetry`: `script_calls`, `script_failures`, `script_timeouts`, `bash_calls`, `bash_failures`, `bash_timeouts`, `bash_spills`, `control_tool_rejections`, `tool_parse_rejections`, `tier1_compactions`, `tier2_summaries`, `overflow_compactions`, `truncation_recoveries`, `repeated_turn_warnings`, `repeated_turn_aborts`.
3. **Deterministic Markdown Renderer (`impl CorpusReport { pub fn to_markdown(&self) -> String }`):**
   - Renders concise Markdown sections (`# ejaj Execution Metrics`, `## Mode & Cost Summary`, `## Provider Role & Cache Efficiency`, `## Ticket Queue & Hybrid Escalation`, `## Quality Gates & Convergence`, `## Tool & Loop Health`), ending with a single `\n`.

### 2. Offline `--metrics [dir]` CLI Flag (`src/main.rs`, `tests/config_cli.rs`)

1. Parse CLI arguments in a pure helper `parse_cli_args(args: &[String]) -> Result<Option<Option<PathBuf>>, String>` in `src/main.rs`:
   - `[]` -> `Ok(None)` (normal interactive startup).
   - `["--metrics"]` -> `Ok(Some(None))` (aggregate `<project_root>/.ejaj`).
   - `["--metrics", dir]` if `!dir.starts_with('-')` -> `Ok(Some(Some(PathBuf::from(dir))))`.
   - Anything else -> `Err("Usage: ejaj [--metrics [ejaj-dir]]".to_string())`.
2. At the top of `fn main()` (`src/main.rs:340`), before `load_dotenv()` and `configured()`:
   - Collect `let args: Vec<String> = env::args().skip(1).collect();`.
   - When `parse_cli_args(&args)` returns `Err(usage)`: log `[ERROR] {usage}` via `status::shared().stderr_log(...)` and `exit(1)`.
   - When `Ok(Some(optional_dir))`: resolve `let target = optional_dir.unwrap_or_else(|| PathBuf::from(project_root()).join(".ejaj"));`, build `let report = ejaj::metrics::aggregate_ejaj_dir(&target);`, print `report.to_markdown().trim_end_matches('\n')` via `status::shared().stdout_log(...)`, and `return;`.

### 3. Test Coverage

Add caller-intent (`when_<action>_given_<context>_then_<outcome>`) tests:
- `src/metrics.rs`:
  - Empty/missing `.ejaj` directory produces a valid zero-run `CorpusReport` and Markdown output.
  - Multi-session fixture directory (one `hybrid` completed run with an escalated retry + one `mid` run blocked at triage + one malformed `metrics.json`) increments `skipped_files == 1` and accurately aggregates mode completion/cost, cache hit rate, Attempt-1 vs. Attempt-2 rescue rate, `hybrid` escalation win rate, transition-gate regression count, and summary `pass@1` / rejection categories.
- `src/main.rs` & `tests/config_cli.rs`:
  - Running `ejaj --metrics <dir>` with a cleared environment (no `LLM_SMOLL_*` keys set!) exits `0` and prints `# ejaj Execution Metrics` to stdout.
  - Running `ejaj --unknown-flag` exits `1` with one `[ERROR]` usage line on stderr.

---

## Affected Files

- `src/metrics.rs`
- `src/main.rs`
- `tests/config_cli.rs`
- `TODO.md`

## Verification

```bash
cargo test --lib metrics && cargo test --bin ejaj && cargo test --test config_cli && cargo test --test test_titles && cargo clippy --all-targets -- -D warnings
```
