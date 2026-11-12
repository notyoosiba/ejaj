# T25: High-Signal Failure Extraction in `transition_failure_excerpt` (`src/tools.rs`, `src/pipeline.rs`)

## Problem

After a ticket queue round resolves all narrow ticket gates, `Pipeline::transition_gate` (`src/pipeline.rs:672-694`) runs the repository-wide test suite (`cargo test` or `npm test`) via `crate::tools::run_command_in(&self.root, command)`. When that full-suite gate fails, `handle_transition_gate_failure` (`src/pipeline.rs:698-736`) logs the failure reason and queues a synthetic repair ticket (`g<round>-1-fix-full-test-suite`) whose `prerequisites_assumed` embeds `transition_failure_excerpt(output)` (`src/pipeline.rs:1345-1357`).

Currently, `transition_failure_excerpt` simply takes the **last 1,200 Unicode scalar values** of `output`:

```rust
fn transition_failure_excerpt(output: &str) -> String {
    let trimmed = output.trim();
    if trimmed.is_empty() {
        return "command exited non-zero with no output".to_string();
    }
    let chars: Vec<char> = trimmed.chars().collect();
    if chars.len() <= 1_200 {
        trimmed.to_string()
    } else {
        let tail: String = chars[chars.len() - 1_200..].iter().collect();
        format!("...{tail}")
    }
}
```

This fails on oversized test-suite runs for two interacting reasons:

1. **Passing-test `stderr` logs crowd out `stdout` failure blocks (`src/tools.rs:812-815`):**
   - `render_verification_outcome` concatenates `stdout` followed by `stderr`.
   - In `ejaj`, passing integration tests emit ~35 KB of `[INFO]`, `[WARN]`, and `[ERROR]` lines directly to `stderr`. The final 1,200 characters of `cargo test` output contain only passing-test `stderr` logs, while the actual `cargo test` failure summary on `stdout` (`test ... FAILED`, `---- <test> stdout ----`, `panicked at`, `failures:`, `test result: FAILED`) sits in the middle of the combined stream and is completely omitted from the repair ticket.
2. **Leading `[ERROR: ...]` wrapper header in `run_command_in` (`src/tools.rs:819-831, 1618`):**
   - `render_verification_outcome` always prepends a synthetic single-line header (`[ERROR: exit code <code> for \`<command>\`]` or `[ERROR: command timed out after <secs>s: \`<command>\`]`).
   - Because `is_primary_diagnostic_line` matches `lowered.starts_with("[error:")`, passing the full `trimmed` output directly to `diagnostic_text` would (a) annex up to 24 non-blank startup banner lines immediately following line 0, and (b) always return a non-empty string on any `run_command_in` failure, turning the `diagnostics.is_empty()` raw-tail fallback into dead code when a command fails with plain non-diagnostic output.

---

## Guardrails for Existing `ejaj` Tests

- **Preserve `diagnostic_text` and `form_bash_digest` behavior (`src/tools.rs:1520-1673, 2359-2720`):**
  - Keep all priority classification, 25-line block continuation bounds, 512-byte per-line truncation, and source-block deduplication in `src/tools.rs` byte-for-byte unchanged.
- **Preserve all existing `transition_fix_ticket` and `transition_gate` contracts (`src/pipeline.rs:2148-2241`, `tests/pipeline.rs:14329-14382`):**
  - Keep the empty/whitespace-only fallback string `"command exited non-zero with no output"` (`src/pipeline.rs:1348`).
  - Keep short outputs (`trimmed.chars().count() <= 1_200`) returned verbatim as `trimmed.to_string()`.
  - Keep `TRANSITION_EXCERPT_CHARS: usize = 1_200` as the retained Unicode scalar-value cap with an additive `"..."` marker when shortened (`"{head}..."` when extracted diagnostics exceed 1,200 scalar values; `"...{tail}"` when falling back to the raw tail), so total length is at most `1,203` Unicode scalar values and never splits a UTF-8 code point.
  - Keep `transition_fix_ticket`'s fields, `test_expectations: Vec::new()`, and markdown rendering unchanged.
- **Preserve caller-intent test-title rules (`tests/test_titles.rs`):**
  - Name every new test `when_<action>_given_<context>_then_<outcome>` without internal helper identifiers.
- **Scope tracker updates strictly to `TODO.md`:**
  - Mark the `T25` row `[x]` in `TODO.md` right before running the verification gate; do not edit `.plans/T25-transition-gate-diagnostic-excerpt.md`.

---

## Proposed Design

### 1. Expose `diagnostic_text` to `crate::pipeline` (`src/tools.rs`)

Change `fn diagnostic_text(rendered: &str) -> String` in `src/tools.rs:1540` to `pub(crate) fn diagnostic_text(rendered: &str) -> String` without altering its implementation.

### 2. Split Optional Verification Header & Prioritize Extracted Diagnostics (`src/pipeline.rs`)

Update `transition_failure_excerpt(output: &str) -> String` (`src/pipeline.rs:1345-1357`) with `const TRANSITION_EXCERPT_CHARS: usize = 1_200;`:

1. **Empty / whitespace-only input:**
   - Let `trimmed = output.trim();`. If `trimmed.is_empty()`, return `"command exited non-zero with no output".to_string()`.
2. **Short input (`<= 1_200` Unicode scalar values):**
   - Collect `let chars: Vec<char> = trimmed.chars().collect();`. If `chars.len() <= TRANSITION_EXCERPT_CHARS`, return `trimmed.to_string()` unchanged.
3. **Separate optional `run_command_in` wrapper header (`split_verification_header`):**
   - Check the first line of `trimmed`: if `first_line.starts_with("[ERROR: ") && first_line.ends_with(']')`, split into `(Some(first_line), rest_after_newline)`; otherwise `(None, trimmed)`.
   - Extract `let diagnostics = crate::tools::diagnostic_text(body);` on `body` alone so the synthetic wrapper header never annexes startup banner lines or masks an empty diagnostic extraction.
4. **Format bounded excerpt:**
   - **When `!diagnostics.is_empty()`:**
     - Re-attach the wrapper header when present:
       ```rust
       let combined = match header {
           Some(line) => format!("{line}\n{diagnostics}"),
           None => diagnostics,
       };
       ```
     - Collect `combined`'s Unicode scalar values: if `<= TRANSITION_EXCERPT_CHARS`, return `combined`; otherwise take the first `TRANSITION_EXCERPT_CHARS` scalar values (`head`) and return `format!("{head}...")`.
   - **When `diagnostics.is_empty()`:**
     - Fall back to the raw tail of `trimmed`: take the final `TRANSITION_EXCERPT_CHARS` scalar values from `chars` (`tail`) and return `format!("...{tail}")`.

### 3. Unit Test Coverage (`src/pipeline.rs`)

Add consolidated caller-intent unit tests in `src/pipeline.rs` covering:

- **Empty, short, and exact-boundary inputs:** empty and whitespace-only strings return `"command exited non-zero with no output"`; outputs of `<= 1,200` Unicode scalar values (including multi-byte UTF-8) return `trimmed` verbatim without `"..."`.
- **Oversized `cargo test` failure buried before noisy `stderr`:** an oversized `run_command_in`-shaped output (`[ERROR: exit code 101 for \`cargo test\`]\n<startup lines>\n...`) containing primary failure blocks (`test ... FAILED`, `---- <test> stdout ----`, `panicked at`, `error[E0308]`, `failures:`, `test result: FAILED`) followed by `> 1,200` characters of `[INFO]` / `[WARN]` / secondary `[ERROR]` lines retains the `[ERROR: exit code ...]` header once without annexing non-diagnostic startup lines, orders primary blocks ahead of secondary `[ERROR]` logs, and bounds oversized extracted diagnostics to `1,200` scalar values plus `"..."` (`1,203` chars total) without splitting multi-byte UTF-8 characters.
- **Oversized plain failure without diagnostic keywords:** an oversized `run_command_in`-shaped output (`[ERROR: exit code 1 for \`npm test\`]\n...`) whose body contains no primary or secondary diagnostic lines falls back to `...{tail}` of the final `1,200` scalar values.

---

## Affected Files

- `src/tools.rs`
- `src/pipeline.rs`
- `TODO.md`

## Verification

```bash
cargo test --lib tools && cargo test --lib pipeline && cargo test --test pipeline && cargo test --test test_titles && cargo clippy --all-targets -- -D warnings
```
