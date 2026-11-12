# Proposal 20: Self-Diagnostic Mode for `.ejaj/` Session Analysis and Self-Improvement (`src/script.rs`, `src/prompts/plan.rs`, `src/main.rs`, `src/pipeline.rs`)

## Problem

When `ejaj` breaks, stops early, or completes a run only after excessive tool steps, repeated tool errors, rejected handoffs, or multi-round review fixes, all forensic data is already on disk under `.ejaj/<timestamp>/`:

- Phase artifacts: `plan.md`, `spec.md`, `ticket-triage.md`, `queue-report.md`, `review.md`, `change-summary.md`, `tickets/*.md`, and `tickets/queue-status.json`.
- Conversation trajectories: `trajectories/<phase>[-<N>].traj.json` (`phase`, `role`, `model`, `steps_taken`, `exit_status`, and `messages`).
- Spilled command logs: `tools-output/step-<N>-call-<M>.log`.

Today, the user must switch to an external coding agent to analyze `.ejaj/<timestamp>/` and write self-improvement proposals (`.plans/PROPOSAL-*.md` and `TODO.md`) or code fixes for `ejaj` itself because of four barriers:

1. **`.ejaj/` is completely hidden from `tools.list` and `tools.grep` (`src/script.rs:936-1061`):**
   - `host_list` and `host_grep` always append `RUN_ARTIFACT_PATHSPEC = ":(exclude).ejaj"` and filter out `.filter(|line| !is_run_artifact(line))`.
   - In addition, `ignore_run_artifacts` (`src/tools.rs:1426-1435`) writes `*\n` to `.ejaj/.gitignore`, so `git ls-files --exclude-standard` and `git grep --untracked` ignore `.ejaj/` even without the negative pathspec.
   - Because `Phase::Plan` (the `giga` design phase) only has `execute_script` (`tools.list`, `tools.grep`, `tools.read`, `tools.diff`) and no `bash`, the `giga` model cannot list `.ejaj/<timestamp>/` session directories, discover which `trajectories/*.traj.json` or `tools-output/*.log` files exist, or search across `.ejaj/`.
2. **Raw `.traj.json` and `tools-output/*.log` files exceed the 16 KiB script output cap (`src/loops/agent.rs:1025-1061`, `src/script.rs:29`):**
   - A single trajectory file (e.g., `code.traj.json` at 170–297 KB) or spill log (`step-98-call-1.log` at 909 KB) is 10–55x larger than `OUTPUT_LIMIT_BYTES = 16_384`. Without a pre-computed session telemetry digest or JavaScript-side JSON filtering guidance, calling `tools.read` on a trajectory truncates away steps, tool errors, and exit status.
3. **No `/diagnose` entry point or dirty-tree read-only exemption for self-diagnosis (`src/main.rs:297-338`, `src/pipeline.rs:183-203`):**
   - `Pipeline::ask` runs `self.preflight_root()` before calling `self.plan_agent.ask_in(...)`. If a failed run left uncommitted changes in the working tree and the user restarts `ejaj` to diagnose what happened, `git::preflight` refuses to start the interview at all.
   - There is no `/diagnose [session]` command that pre-extracts structured telemetry from `.ejaj/<timestamp>/` (steps taken, rejected handoffs, `[ERROR: execute_script failed]` counts, gate failures, review fix rounds, and spill log metadata) and hands it to the `giga` model.
4. **No `.ejaj/` self-diagnostic workflow in `PLAN_SYSTEM_PROMPT` (`src/prompts/plan.rs`):**
   - `PLAN_SYSTEM_PROMPT` instructs the Plan agent how to interview for a feature or fast-track a `.plans/` task, but does not instruct it how to audit `.ejaj/<timestamp>/` trajectories and artifacts, trace agent friction back to `ejaj`'s own source modules (`src/prompts/*.rs`, `src/tools.rs`, `src/tickets.rs`, `src/triage.rs`, `src/script.rs`, `src/loops/*.rs`), and turn those findings into either `.plans/PROPOSAL-<N>-<slug>.md` + `TODO.md` backlog items or a direct `finish_plan` handoff.

---

## Guardrails for Existing `ejaj` Tests

- **Preserve `Object.keys(tools)` across all phases (`src/script.rs:2111-2150, 2860-2899`):**
  - `design_phases_expose_only_read_only_bindings` (`src/script.rs:2128`) and `when_inspecting_repository_given_triage_phase_then_rejects_write_and_replace` (`src/script.rs:2869`) assert `Object.keys(tools).sort()` is `["diff","grep","list","read"]` for read-only phases and `["diff","grep","list","read","replace","write"]` for `Phase::Code`. Do not add new top-level keys to the `tools` object in `install_tools`.
- **Keep `.ejaj/` excluded from default `tools.list()`, default/non-`.ejaj` `tools.grep(...)`, and `tools.diff()` (`src/script.rs:1317-1424, 2559-2603, 2909-2950, 3142-3195`):**
  - `tools.list()` with no glob (or with an ordinary glob like `"*.rs"` or `"*.md"`), `tools.grep(pattern)` with no glob (or an ordinary glob), and `tools.diff()` must continue to exclude all tracked and untracked `.ejaj/` paths.
  - Update the three existing explicit-`.ejaj`-glob unit tests in `src/script.rs` (`when_grepping_files_given_empty_pattern_null_or_omitted_glob_and_path_filters_then_preserves_existing_contracts` at `src/script.rs:2604-2607`, `when_searching_files_given_explicit_ejaj_glob_then_omits_artifacts` at `src/script.rs:3062-3102`, and `when_listing_files_given_explicit_ejaj_glob_then_omits_artifacts` at `src/script.rs:3108-3136`) to reflect the new opt-in contract: explicit `.ejaj` globs return `.ejaj/` files/matches, while omitted and ordinary globs still omit `.ejaj/`.
- **Preserve all `PLAN_SYSTEM_PROMPT` contract needles and forbidden-phrase checks (`tests/prompt_contract.rs`, `src/prompts/review.rs`):**
  - Keep `OPENING_MARKER`, `PLAN_TEMPLATE_HEADINGS` in exact order, and every required substring in `tests/prompt_contract.rs:62-238` and `src/prompts/review.rs:210-256, 421-546`.
  - Never introduce any forbidden substring checked in `tests/prompt_contract.rs:204, 227-232` or `src/prompts/review.rs:532-540` (`"1 to 4 critical questions"`, `"authorization header"`, `"access control"`, `"permission check"`, `"authenticate"`, `"HTTP 401"`, `"HTTP 403"`, `"payload validation rule"`, `"access-control rule"`, `"numeric limit"`, `"maximum number of test expectations"`).

---

## Proposed Design

### 1. Opt-In `.ejaj/` Discovery and Search in `tools.list` and `tools.grep` (`src/script.rs`)

Allow `host_list` and `host_grep` to inspect `.ejaj/` **only when the caller supplies an explicit `.ejaj` glob**, while keeping `.ejaj/` excluded from all default/ordinary globs and from `host_diff`:

1. **Detect explicit `.ejaj` globs (`is_explicit_ejaj_glob`):**
   - After `validate_glob(pattern)?` rejects absolute paths and `..` parent traversal, normalize leading `./` segments and classify `pattern` as an explicit `.ejaj` glob when `normalized == ".ejaj"` or `normalized.starts_with(".ejaj/")`.
2. **Direct filesystem walk for explicit `.ejaj` globs in `host_list` and `host_grep`:**
   - Because `.ejaj/.gitignore` contains `*\n`, `git ls-files --exclude-standard` and `git grep --untracked` ignore `.ejaj/` files.
   - When `is_explicit_ejaj_glob(pattern)` is true:
     - Walk `root.join(".ejaj")` recursively in sorted lexical order (skipping `.ejaj/.gitignore` and any symlink that escapes `canonical_root` via `resolve_confined_path`).
     - Match each root-relative path (e.g., `.ejaj/20261110-182134/trajectories/code.traj.json`) against the caller's glob pattern (`".ejaj"` or `".ejaj/**"` matches all files under `.ejaj/`; `".ejaj/<session>/*"` or `".ejaj/**/*.traj.json"` or `".ejaj/**/*.md"` narrows accordingly).
     - In `host_list`, return the matching root-relative paths.
     - In `host_grep`, search UTF-8 lines of each matching file using the same regex/literal matching semantics as `git grep -I -e <pattern>`, capping results at `GREP_MATCH_LIMIT` (`1,000`) and each matched line at `GREP_LINE_MAX_BYTES` (`2,000`) via `cap_grep_line`.

### 2. Structured `.ejaj/` Session Telemetry Digest & `/diagnose` Entry Point (`src/main.rs`, `src/pipeline.rs`)

Add a self-diagnostic entry point at `plan>>` (`/diagnose` or `/diagnose <session-id>`) that extracts a compact telemetry digest from `.ejaj/` and hands it to the Plan (`giga`) agent:

1. **Session selection:**
   - `/diagnose <session-id>` targets `.ejaj/<session-id>`.
   - Bare `/diagnose` selects the newest `.ejaj/<timestamp>` session directory (plus a brief index of the 5 most recent `.ejaj/<timestamp>` sessions and their completion/failure states).
2. **Compact Telemetry Digest (`build_session_diagnostic_digest`):**
   - For the target `.ejaj/<timestamp>` session, build a bounded markdown digest containing:
     - **Artifacts present**: which of `plan.md`, `spec.md`, `ticket-triage.md`, `queue-report.md`, `review.md`, `change-summary.md`, and `tickets/queue-status.json` exist, plus the ticket statuses from `queue-status.json`.
     - **Trajectory summary table** (parsed from `trajectories/*.traj.json`): for each trajectory file, report `file`, `phase`, `role`, `model`, `steps_taken`, `exit_status`, number of `[ERROR: execute_script failed]` tool results, number of failed `bash` calls (`[ERROR: exit code ...]`), number of rejected control-tool handoffs (`finish_plan`, `finish_spec`, `finish_tickets`, `finish_triage`, `finish_review`) or summary format corrections, and the first line of each unique rejection/error.
     - **Spill log inventory**: each file under `tools-output/` with its byte size and `Command:` header line.
     - **Triage / Queue / Review blockers**: any non-ready classifications from `ticket-triage.md`, failed tickets from `queue-report.md`, or findings/fix tickets from `review.md`.
3. **Read-only diagnostic interview on dirty working trees (`Pipeline::diagnose`):**
   - Allow `/diagnose` to run `git::preflight_repository(&self.root)` instead of requiring a clean working tree up front, so the user can run `/diagnose` immediately after a broken session to analyze `.ejaj/<timestamp>/` and `tools.diff()`.

### 3. Self-Diagnostic and Self-Improvement Guidance in `PLAN_SYSTEM_PROMPT` (`src/prompts/plan.rs`)

Extend `PLAN_SYSTEM_PROMPT` with instructions for `.ejaj/` self-diagnostic requests:

- Instruct the Plan agent that when a request asks to diagnose, audit, or improve `ejaj` from `.ejaj/` session runs (or supplies a `[SESSION DIAGNOSTIC DIGEST]`):
  - Use `execute_script` with explicit `.ejaj` globs (`tools.list(".ejaj/**")`, `tools.grep(pattern, ".ejaj/**")`, and `tools.read(".ejaj/<session>/...")`) to inspect the session's artifacts, spill logs, and `trajectories/*.traj.json` (filtering parsed trajectory JSON in JavaScript so the returned value stays within the script output cap).
  - Trace every observed failure mode or inefficiency (early stops, high `steps_taken`, repeated `execute_script` or `bash` errors, rejected handoffs, triage blocks, or multi-round review fixes) to the exact `ejaj` source module (`src/prompts/*.rs`, `src/tools.rs`, `src/tickets.rs`, `src/triage.rs`, `src/script.rs`, `src/loops/*.rs`, `src/pipeline.rs`, `src/orchestrator.rs`) and its existing test seams.
  - Present a concise root-cause findings summary before `Q1` and align with the user on whether to hand off a plan that writes `.plans/PROPOSAL-<N>-<slug>.md` and `TODO.md` task entries or a plan that implements the `ejaj` source-code fix directly.

---

## Affected Files

- `src/script.rs`
- `src/prompts/plan.rs`
- `src/pipeline.rs`
- `src/main.rs`
- `tests/prompt_contract.rs`
- `TODO.md`

## Verification

```bash
cargo test --lib script && cargo test --lib prompts::plan && cargo test --lib pipeline && cargo test --test prompt_contract && cargo test --test test_titles && cargo clippy --all-targets -- -D warnings
```
