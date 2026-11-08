# Proposal 17: High-Signal Test and Compiler Failure Extraction in `form_bash_digest` (`src/tools.rs`)

## Problem

When an oversized `bash` command (such as `cargo test --test pipeline`, which produces ~838 KB / 17,300 lines of output) fails and spills to `.ejaj/<timestamp>/tools-output/step-<N>-call-<M>.log`, `form_bash_digest` (`src/tools.rs:1462-1561`) fails to include the actual `cargo test` failure summary (`failures:`, `---- <test> stdout ----`, `thread '...' panicked at ...`, and `test ... FAILED`) in the 16 KiB model digest for four reasons:

1. **Passing tests emit `[ERROR]` and `[WARN] ... failed` directly to `stdout`/`stderr`:**
   - `status::shared()` (`src/status.rs`) writes directly to `std::io::stdout()` and `std::io::stderr()`, bypassing Rust `libtest`'s per-test output capture.
   - As a result, 230 passing integration tests that exercise error-handling paths emit hundreds of `[ERROR] 400 ...`, `[ERROR] Malformed response ...`, and `[WARN] attempt 1/3 failed: ...` lines into the raw output stream.
   - `is_diagnostic_line` (`src/tools.rs:1551-1561`) matches `lowered.contains("[error")` and `lowered.contains("failed")` (including `[WARN] ... failed` lines) with the exact same priority as real test/compiler failures.
2. **Unbounded non-blank block expansion absorbs interleaved parallel test output (`src/tools.rs:1536`):**
   - When `diagnostic_text` matches a line like `test when_foo ... FAILED` (at line 5,995 of `step-98-call-1.log`), `while index < lines.len() && !lines[index].trim().is_empty()` consumes every subsequent non-blank line.
   - Because parallel test threads print `#thinking ...`, `[INFO] ...`, and `test ... ... ok` without blank lines between them, a single `test ... FAILED` line pulls in thousands of bytes of unrelated passing-test output.
3. **Uncapped single-line panic payloads exhaust `BASH_DIGEST_DIAGNOSTIC_BYTES` (8 KiB):**
   - In `tests/pipeline.rs:10954`, `panic!("the pipeline made an unexpected request: {}", outgoing.body)` prints a 13,400-byte JSON request body on a single line.
   - Because `diagnostic_text` does not cap individual line lengths, a single 13.4 KB panic line consumes the entire 8 KiB `BASH_DIGEST_DIAGNOSTIC_BYTES` budget and truncates away all subsequent `---- <test> stdout ----` blocks, `panicked at` locations, and the final `failures:` list.
4. **Front-only truncation (`truncate_utf8_prefix`) drops end-of-run `failures:` summaries (`src/tools.rs:1483`):**
   - Cargo prints the `failures:` summary at the end of `stdout` (around line 16,827 of 17,309), while `combine_output` appends 35 KB of passing-test `stderr` after `stdout`.
   - `tail` (last 2 KiB of `stdout + stderr`) only sees the end of `stderr`, and `truncate_utf8_prefix(&diagnostics, diagnostic_budget)` only keeps the first 8 KiB of `diagnostics`, so the `failures:` summary in the middle of the combined stream is completely omitted from the digest.

---

## Guardrails for Existing `ejaj` Tests

- **Preserve all existing `bash` spill and digest tests in `src/tools.rs`:**
  - `when_bash_output_given_oversized_session_command_then_saves_under_session_and_reports_root_relative_path` (`src/tools.rs:2232`) and `when_bash_output_given_failure_or_save_error_then_preserves_status_without_false_path` (`src/tools.rs:2484`) emit `[ERROR] demo failure` and `[ERROR] deliberate failure` and assert `error.contains("[ERROR] deliberate failure")`, `FAIL-HEAD`, `FAIL-TAIL`, and `error.len() <= crate::script::OUTPUT_LIMIT_BYTES`.
  - Keep `[ERROR` recognized as a secondary diagnostic when diagnostic budget remains, so `[ERROR] deliberate failure` continues to be extracted.
- **Preserve the 16 KiB delivery cap (`TOOL_OUTPUT_LIMIT_BYTES`) and step-warning reserve (`STEP_WARNING_RESERVE_BYTES`):**
  - The returned digest (plus any `[WARN: step ...]` suffix) must strictly remain `<= crate::script::OUTPUT_LIMIT_BYTES` (`16,384` bytes) and on a valid UTF-8 character boundary.
- **Scope tracker updates strictly to `TODO.md`:**
  - Update `TODO.md` only; do not edit `.plans/PROPOSAL-17-high-signal-bash-failure-digest.md` during implementation.

---

## Proposed Design

### 1. Distinguish Primary Test/Compiler Failure Lines from Secondary Log Lines (`src/tools.rs`)

Replace flat `is_diagnostic_line` matching in `src/tools.rs:1523-1561` with two priority tiers and exclude `[WARN]` log lines:

1. **Primary (high-signal) failure starters (`is_primary_diagnostic_line`):**
   - Cargo test failure lines: `trimmed.starts_with("test ") && trimmed.ends_with(" ... FAILED")`, `trimmed == "failures:"`, `trimmed.starts_with("test result: FAILED")`, or `trimmed.starts_with("---- ") && trimmed.ends_with(" stdout ----")`.
   - Panic and compiler/tool failure lines: `lowered.contains("panicked at")`, `lowered.contains("panic:")`, `lowered.starts_with("error[")`, `lowered.starts_with("error:")`, or `lowered.starts_with("[error:")`.
2. **Secondary diagnostic starters (`is_secondary_diagnostic_line`):**
   - Exclude harness/test warning lines (`trimmed.starts_with("[WARN")`).
   - Match remaining lines containing `"[error"`, `"error:"`, or `"failed"` (case-insensitively).

### 2. Bound Diagnostic Block Continuation and Per-Line Length (`src/tools.rs`)

When collecting a diagnostic block starting at `start`:
1. **Stop block continuation at routine harness/test progress lines (`is_non_diagnostic_progress_line`):**
   - A `test ... ... FAILED` line is a single-line record (do not annex subsequent lines).
   - For multi-line blocks (`---- <test> stdout ----`, `failures:`, `panicked at`, `error[`, etc.), stop the block on an empty line, after at most `DIAGNOSTIC_BLOCK_MAX_LINES = 25` lines, or when the next line is a routine harness/test progress line (`trimmed.starts_with("#thinking ")`, `trimmed.starts_with("[INFO] ")`, `trimmed.starts_with("[WARN] ")`, `trimmed.starts_with("=== ")`, or `trimmed.starts_with("test ") && trimmed.ends_with(" ... ok")`).
2. **Cap individual lines inside a diagnostic block (`DIAGNOSTIC_LINE_MAX_BYTES = 512`):**
   - Truncate each line in a diagnostic block to at most `512` UTF-8 bytes (`truncate_utf8_prefix(line, DIAGNOSTIC_LINE_MAX_BYTES)`) with a `...` suffix when truncated, so a 13 KB single-line JSON panic dump cannot consume the entire 8 KiB diagnostic budget.

### 3. Order Primary Failures Before Secondary Logs in `diagnostic_text` (`src/tools.rs`)

- Collect de-duplicated primary blocks and secondary blocks separately in original line order.
- Assemble `diagnostic_text` with all **primary failure blocks first**, followed by **secondary diagnostic blocks**, so Cargo's `test ... FAILED`, `---- <test> stdout ----`, `panicked at`, `failures:`, and `error:` blocks are always preserved at the front of `[diagnostics]` even when passing tests emit hundreds of `[ERROR]` lines.

---

## Affected Files

- `src/tools.rs`
- `TODO.md`

## Verification

```bash
cargo test --lib tools && cargo test --test test_titles && cargo clippy --all-targets -- -D warnings
```
