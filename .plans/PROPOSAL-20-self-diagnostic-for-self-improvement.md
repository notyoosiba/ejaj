# Proposal 20: Interactive Read-Only `ejaj --diagnose [path]` CLI Mode (`src/tools.rs`, `src/config.rs`, `src/script.rs`, `src/metrics.rs`, `src/prompts/diagnose.rs`, `src/loops/diagnose.rs`, `src/main.rs`)

## Problem

When `ejaj` stops early, hits triage/gate/review retries, or burns excessive steps and tokens, all output records sit under `.ejaj/<timestamp>/`:

- Structured run and loop telemetry: `metrics.json` (`RunMetrics` schema v2) and `trajectories/<phase>[-<N>].traj.json` (`TrajectorySnapshot` with `ticket_id`, `attempt`, `LoopTelemetry`, and `messages`).
- Phase handoff records: `plan.md`, `spec.md`, `ticket-triage.md`, `queue-report.md`, `review.md`, `change-summary.md`, `tickets/*.md`, and `tickets/queue-status.json`.
- Spilled command logs: `tools-output/step-<N>-call-<M>.log`.

`ejaj --metrics [ejaj-dir]` provides an offline statistical scorecard across runs, and `AGENTS.md` establishes that normal agent runs must **never** read state from `.ejaj/`. However, `ejaj` still lacks an LLM-backed `ejaj --diagnose [path]` command to inspect `.ejaj/` output records alongside `ejaj`'s live source tree, explain root causes on stdout, and answer interactive follow-up questions:

1. **No isolated diagnostic phase or scoped `.ejaj/` read capability (`src/tools.rs`, `src/script.rs`):**
   - `host_list` and `host_grep` (`src/script.rs:936-1061`) unconditionally exclude `.ejaj/`, which is required for all 7 pipeline phases (`Plan`, `Spec`, `Tickets`, `Triage`, `Code`, `Review`, `Summary`), leaving no opt-in read-only capability strictly scoped to `--diagnose`.
   - Reusing `Phase::Plan` directly would advertise `finish_plan`, whereas `--diagnose` must be strictly read-only (`execute_script` only, zero file mutations, stdout Markdown output only).
2. **Raw `.traj.json` and `tools-output/*.log` files exceed the 16 KiB script output cap (`src/script.rs:29`):**
   - Individual trajectories (170–300 KB) and spill logs (up to hundreds of KB) exceed `OUTPUT_LIMIT_BYTES = 16_384`. Without a pre-computed corpus + session telemetry digest built from `metrics.json` and `LoopTelemetry` (plus JavaScript-side JSON filtering guidance), the model cannot immediately spot which sessions, tickets, or tool calls failed.
3. **No `ejaj --diagnose [path]` CLI action or `diagnose>> ` read-only REPL (`src/main.rs`, `src/loops/`):**
   - Starting `ejaj` with no arguments enters `Pipeline::ask`, which enforces a clean working tree (`git::preflight`). After a broken run leaves uncommitted changes, operators need `ejaj --diagnose [path]` to run over a dirty working tree (`git::preflight_repository`), print an initial Markdown diagnosis to stdout, and stay open at `diagnose>> ` for follow-up questions until `EOF` (`Ctrl-D`).

---

## Guardrails for Existing `ejaj` Tests & Architecture

- **Keep `.ejaj/` completely invisible to all 7 pipeline phases (`AGENTS.md`, `src/script.rs:2604-2607, 3062-3136`):**
  - `Capabilities::for_phase` for `Plan`, `Spec`, `Tickets`, `Triage`, `Code`, `Review`, and `Summary`, as well as `Capabilities::read_only` and `Capabilities::read_write`, must keep `allow_ejaj_records: false`.
  - Keep all three existing `.ejaj` exclusion tests in `src/script.rs` (`when_grepping_files_given_empty_pattern_null_or_omitted_glob_and_path_filters_then_preserves_existing_contracts`, `when_searching_files_given_explicit_ejaj_glob_then_omits_artifacts`, and `when_listing_files_given_explicit_ejaj_glob_then_omits_artifacts`) **unchanged and passing**.
  - Do **not** edit `PLAN_SYSTEM_PROMPT` (`src/prompts/plan.rs`) or `Pipeline` (`src/pipeline.rs`); keep diagnostic prompts and session state in dedicated `src/prompts/diagnose.rs` and `src/loops/diagnose.rs` modules.
- **Strictly read-only — zero file writes anywhere on disk:**
  - `Phase::Diagnose` exposes **only** `execute_script` with read-only bindings (`Object.keys(tools).sort() == ["diff","grep","list","read"]`)—no `finish_plan`, no control tools, no `bash`, no `write`, and no `replace`.
  - Pass `session_dir: None` in `InvocationContext` so `save_trajectory` writes nothing to `.ejaj/`; all diagnostic output is Markdown rendered to stdout.
- **Keep default/non-`.ejaj` globs and `tools.diff()` excluding `.ejaj/` even in `Phase::Diagnose`:**
  - Even when `allow_ejaj_records` is `true`, `tools.list()` with no glob or an ordinary glob (`"*.rs"`, `"*.md"`), `tools.grep(pattern)` with no glob or an ordinary glob, and `tools.diff()` must continue to exclude `.ejaj/`. Only explicit `.ejaj` globs (`".ejaj"` or `".ejaj/..."`) inspect `.ejaj/`.
- **Update `CLI_USAGE` in lockstep across unit and CLI tests (`src/main.rs:342, 737`, `tests/config_cli.rs:890`):**
  - Update `CLI_USAGE` to `"Usage: ejaj [--metrics [ejaj-dir] | --diagnose [path]]"` and update the exact assertions in `src/main.rs` and `tests/config_cli.rs` while keeping zero-argument interactive startup and offline zero-key `--metrics` behavior completely unchanged.
- **Enforce Clippy complexity (`<= 15`) and caller-intent test titles (`tests/guidance.rs`, `tests/test_titles.rs`):**
  - Every function must stay at or below cognitive complexity 15.
  - Every new `#[test]` must follow `when_<action>_given_<context>_then_<outcome>` and avoid all `INTERNAL_IDENTIFIERS` (`provider_role`, `summarize`, `thinking_label`, `group_digits`, `call_time`, `measured`, `new`, `with_giga`, etc.).

---

## Proposed Design

### 1. `Phase::Diagnose` Routing & Read-Only Tool Surface (`src/tools.rs`, `src/config.rs`, `src/loops/agent.rs`)

1. **`Phase::Diagnose` (`src/tools.rs`):**
   - Add `Diagnose` to `pub enum Phase` with `Phase::Diagnose.label() => "diagnose"`.
   - In `Tool::definitions(Phase::Diagnose)` (`src/tools.rs:332`), advertise `vec![execute_script]` only (no control tool and no `bash`). `Tool::parse` rejects all control tools and `bash` in `Phase::Diagnose`.
2. **Provider routing (`src/config.rs`):**
   - Route `Phase::Diagnose` identically to `Phase::Plan` in `Config::provider`, `Config::provider_role`, `Config::provider_model`, and phase rate lookups: `"mid"` in `Mode::Mid`, and `"giga"` in `Mode::Giga` and `Mode::Hybrid`.
3. **Agent loop turn completion (`src/loops/agent.rs`):**
   - `run_agent_loop_inner` already prints assistant narration via `printable_reply_narration` for every non-`Summary` phase and returns `(Stop::TurnEnd, step)` when `tool_calls.is_empty()`. With `InvocationContext { root: Some(root), session_dir: None, .. }`, `save_trajectory` is a no-op.

### 2. Scoped Explicit-`.ejaj` Discovery and Search via Git (`src/script.rs`, `src/tools.rs`)

1. **`allow_ejaj_records` on `Capabilities` (`src/script.rs:48-87`, `src/tools.rs:704, 729`):**
   - Add `pub allow_ejaj_records: bool` to `Capabilities`: `true` **only** in `Capabilities::for_phase(Phase::Diagnose)`, and `false` in all other phases and in `Capabilities::read_only` / `Capabilities::read_write` (with a builder `pub fn with_ejaj_records(mut self, allow: bool) -> Self` for unit tests).
   - Include `Phase::Diagnose` in `design_phases_expose_only_read_only_bindings` (`src/script.rs:2116`) to verify `Object.keys(tools).sort()` stays `["diff","grep","list","read"]`.
2. **Explicit `.ejaj` glob detection (`is_explicit_ejaj_glob`):**
   - After `validate_glob(pattern)?` rejects parent (`..`) and root (`/`) components, strip leading `./` segments and return `true` iff `normalized == ".ejaj"` or `normalized.starts_with(".ejaj/")`.
3. **Git-backed `.ejaj` listing & search (zero new crate dependencies):**
   - Keep `pub fn host_list(root: &Path, glob: Option<&str>)` unchanged (`allow_ejaj_records = false`); dispatch `tools.list` and `tools.grep` in `install_tools` with `caps.allow_ejaj_records`.
   - When `allow_ejaj_records && glob.is_some_and(is_explicit_ejaj_glob)`:
     - If `!root.join(".ejaj").exists()`, return `Ok(Vec::new())` immediately.
     - **Listing:** run `git ls-files --cached --others -- <pattern>` rooted at `root` (omitting `--exclude-standard` and `RUN_ARTIFACT_PATHSPEC` so `.ejaj/.gitignore` does not hide `.ejaj/` files), filtering out `.ejaj/.gitignore`.
     - **Grepping:** run `git grep -n -z --no-index -I --no-color -e <pattern> -- <glob_pattern>` rooted at `root` (preserving Git's exact regex/literal matching, binary file skipping, `GREP_MATCH_LIMIT = 1_000`, and `cap_grep_line(..., GREP_LINE_MAX_BYTES)`), filtering out `.ejaj/.gitignore`.

### 3. Pre-Computed Corpus & Session Diagnostic Digest (`src/metrics.rs`)

Add `pub enum DiagnoseTarget` and `pub fn render_diagnostic_digest(target: &Path, root: &Path) -> Result<String, String>` in `src/metrics.rs`:

1. **Target resolution (`ejaj --diagnose [path]`):**
   - **No `<path>` supplied:** target `<root>/.ejaj` in **corpus mode**. If `<root>/.ejaj` does not exist or has zero session directories, render a valid empty-corpus digest noting `0 sessions found` rather than failing.
   - **Explicit `<path>` supplied:** `<path>` must exist and be a readable directory (otherwise return `Err(reason)`). Classify `<path>` as **single-session mode** when it contains any session marker (`metrics.json`, `plan.md`, `spec.md`, `ticket-triage.md`, `queue-report.md`, `review.md`, `change-summary.md`, `trajectories`, `tickets`, or `tools-output`); otherwise classify it as **corpus mode**.
2. **Bounded Markdown Digest (`render_diagnostic_digest`):**
   - **Corpus mode:**
     - Include `render_corpus_report(target)` (mode/cost summary, queue & hybrid escalation funnel, quality-gate convergence, provider cache/token efficiency).
     - Append a **Recent & Incomplete Sessions** table (up to the 15 newest child sessions plus any non-complete sessions) built from `metrics.json` (`RunMetrics`) and `trajectories/*.traj.json` (`LoopTelemetry`: `steps_taken`, `execute_script_failures`, `bash_failures`, `bash_output_spills`, `rejected_control_tool_calls`, `truncation_recoveries`, `repeated_tool_warnings`, `tier1_compactions`, `tier2_summaries`), tolerating older sessions without `metrics.json`.
     - Include a detailed per-session breakdown (below) for the newest session (and newest incomplete session if different).
   - **Single-session breakdown:**
     - **Artifacts & terminal state:** present handoff files, `RunMetrics` phase outcomes/retries/timings, and ticket statuses.
     - **Trajectory table:** `file`, `phase`, `role`, `model`, `ticket_id`, `attempt`, `steps_taken`, `exit_status`, and `LoopTelemetry` error/rejection/compaction counters.
     - **Unique error & rejection samples:** deduplicated 1-line samples of rejected control/tool calls, `[ERROR: execute_script failed]` lines, and failed `bash` headers scanned from trajectory `messages`.
     - **Spill log inventory:** each file in `tools-output/` with byte size and `Command:` header line.

### 4. Diagnostic Prompt, Session Loop & `diagnose>> ` CLI (`src/prompts/diagnose.rs`, `src/loops/diagnose.rs`, `src/main.rs`)

1. **`src/prompts/diagnose.rs` (exported from `src/prompts/mod.rs`):**
   - Define `DIAGNOSE_SYSTEM_PROMPT` and kickoff builder:
     - Frame `ejaj --diagnose` as a strictly **read-only** forensic and self-improvement session that outputs Markdown to stdout and never creates, edits, or deletes files.
     - Instruct the model to read `AGENTS.md` and `README.md` when present, use `tools.diff()` to check any uncommitted working-tree state, and use explicit `.ejaj` globs (`tools.list(".ejaj/**")`, `tools.grep(pattern, ".ejaj/**")`, `tools.read(".ejaj/<session>/...", startLine?, endLine?)`) while filtering parsed `.traj.json` objects inside JavaScript so return values stay within the 16 KiB script cap.
     - On Turn 1, produce a clear, structured Markdown report covering: (a) executive summary of run/corpus health, (b) concrete failure modes, retries, or token/step bottlenecks with `.ejaj/` evidence, (c) root-cause mapping to exact `ejaj` source modules/prompts/validators (`src/prompts/*.rs`, `src/tools.rs`, `src/tickets.rs`, `src/triage.rs`, `src/script.rs`, `src/loops/*.rs`, `src/pipeline.rs`, `src/orchestrator.rs`), and (d) prioritized fix recommendations. On subsequent user turns, answer follow-up questions concisely using read-only inspection.
2. **`src/loops/diagnose.rs` (exported from `src/loops/mod.rs`):**
   - Add `diagnose::Session` holding `config: Config`, `root: PathBuf`, and `messages: Vec<Message>` seeded with `Message::system(format!("{DIAGNOSE_SYSTEM_PROMPT}{scope}"))`.
   - `pub fn ask(&mut self, input: &str) -> Outcome` (`TurnEnd` or `Failed(FailureKind)`): clears prior `reasoning_content`, appends `Message::user(input)`, and runs `agent::run_agent_loop_with_context` under `Phase::Diagnose`, `Source::None`, and `InvocationContext { root: Some(&self.root), session_dir: None, ticket_id: None, attempt: None }`.
3. **CLI `--diagnose [path]` & `diagnose>> ` REPL (`src/main.rs`, `tests/config_cli.rs`):**
   - Add `const DIAGNOSE_FLAG: &str = "--diagnose";` and `CliAction::Diagnose(Option<PathBuf>)` in `parse_cli_args` (`["--diagnose"]` -> `Ok(CliAction::Diagnose(None))`, `["--diagnose", path]` if `!is_flag(path)` -> `Ok(CliAction::Diagnose(Some(PathBuf::from(path))))`).
   - Update `CLI_USAGE` to `"Usage: ejaj [--metrics [ejaj-dir] | --diagnose [path]]"`.
   - In `run_diagnose(optional_path: Option<PathBuf>)`:
     1. Resolve any explicit relative `path` against the initial working directory before `enter_project_root()`; if an explicit `path` cannot be read as a directory, log `[ERROR] Could not read the diagnostic target at {}: {}` to stderr and `exit(1)` before provider calls.
     2. Run `load_dotenv()`, `configured()`, and `let root = enter_project_root();`.
     3. Run `git::preflight_repository(&root)` (allowing dirty working trees so `--diagnose` works immediately after an incomplete/broken run; exit `1` with `[ERROR] Git preflight failed: ...` if not a valid Git repository).
     4. Build the initial kickoff message from `render_diagnostic_digest(&target, &root)?` and run the initial diagnostic turn (`session.ask(&kickoff)`). If the initial turn fails (`Outcome::Failed(kind)`), log `[ERROR] Diagnostic pass ended early ({})` to stderr and `exit(1)`.
     5. Enter the interactive `diagnose>> ` rustyline loop (`rl.readline("diagnose>> ")`): send each non-blank line to `session.ask(input)` (logging a `[WARN]` and continuing to `diagnose>> ` on transient `Failed`), continue on `Interrupted` (`Ctrl-C`), and exit `0` cleanly on `Eof` (`Ctrl-D` or redirected `< /dev/null`).

---

## Affected Files

- `src/tools.rs`
- `src/config.rs`
- `src/script.rs`
- `src/metrics.rs`
- `src/prompts/mod.rs`
- `src/prompts/diagnose.rs`
- `src/loops/mod.rs`
- `src/loops/diagnose.rs`
- `src/main.rs`
- `tests/config_cli.rs`
- `TODO.md`

## Verification

```bash
cargo test --lib tools && cargo test --lib config && cargo test --lib script && cargo test --lib metrics && cargo test --lib loops && cargo test --bin ejaj && cargo test --test config_cli && cargo test --test test_titles && cargo clippy --all-targets -- -D warnings
```
