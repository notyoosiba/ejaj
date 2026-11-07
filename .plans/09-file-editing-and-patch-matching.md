# 09: CRLF-Safe File Replacement, Multi-Match Line Diagnostics, and Search Line Caps (`src/script.rs`)

This document specifies focused, deterministic upgrades to `host_replace` (`tools.replace(path, old_text, new_text)`) and `host_grep` (`tools.grep`) in `src/script.rs`:
1. **CRLF (`\r\n`) normalization, UTF-8 BOM preservation, and line-trimmed trailing whitespace fallback** in `host_replace` without sacrificing the strict unique-match (`matches == 1`) safety contract.
2. **No-op edit rejection (`old_text == new_text`)** so silent no-op writes are reported immediately.
3. **1-based match line numbers when `matches > 1`** (`matched {matches} occurrences; expected exactly 1 (at lines ...)`) so the model can disambiguate without an extra `tools.grep` turn.
4. **Per-line byte length cap in `tools.grep` (`GREP_LINE_MAX_BYTES = 2_000`):** truncate individual matched lines on a UTF-8 character boundary so minified assets or single-line JSON fixtures cannot exhaust `execute_script`'s 16 KiB output limit.

## Problem Statement

Currently in `src/script.rs:682-708`:
```rust
pub fn host_replace(root: &Path, path: &str, old_text: &str, new_text: &str) -> Result<(), String> {
    if old_text.is_empty() {
        return Err(format!(
            "tools.replace(`{path}`): `old_text` must not be empty"
        ));
    }
    let resolved = resolve_confined_path(root, path)?;
    let content =
        fs::read_to_string(&resolved).map_err(|e| format!("tools.replace(`{path}`): {e}"))?;

    let matches = content.matches(old_text).count();
    if matches == 0 {
        return Err(format!(
            "tools.replace(`{path}`): `old_text` matched 0 occurrences; expected exactly 1"
        ));
    }
    if matches > 1 {
        return Err(format!(
            "tools.replace(`{path}`): `old_text` matched {matches} occurrences; expected exactly 1"
        ));
    }

    let updated = content.replacen(old_text, new_text, 1);
    fs::write(&resolved, updated).map_err(|e| format!("tools.replace(`{path}`): {e}"))
}
```

Three common edge cases cause avoidable tool errors or silent no-ops during `Phase::Code`:
1. **CRLF (`\r\n`) line endings:** `tools.read(path, startLine, endLine)` (`src/script.rs:654`) uses `.lines().join("\n")`, which strips `\r\n` and shows the model LF-delimited text. When a file on disk contains `\r\n`, `content.matches(old_text)` returns `0` even when the text is otherwise identical.
2. **Trailing horizontal whitespace on context lines:** Models frequently strip trailing spaces/tabs at the ends of unchanged context lines, causing `matched 0 occurrences`.
3. **Silent no-op edits (`old_text == new_text`):** Passing identical `old_text` and `new_text` succeeds without alerting the model that nothing changed.

---

## Guardrails for Existing `ejaj` Tests

- **Preserve exact error prefixes for existing unit tests:**
  - Empty `old_text`: `tools.replace(\`{path}\`): \`old_text\` must not be empty`
  - Zero matches: `tools.replace(\`{path}\`): \`old_text\` matched 0 occurrences; expected exactly 1`
  - Multiple matches: `tools.replace(\`{path}\`): \`old_text\` matched {matches} occurrences; expected exactly 1`
- **Enforce uniqueness at whichever tier matches:**
  - Tier 1 (exact match on LF-normalized view) and Tier 2 (line-by-line trailing horizontal whitespace trimmed match) run strictly as a fallback cascade: Tier 2 runs only when Tier 1 finds `0` matches.
  - Whichever tier matches must yield **exactly 1** non-overlapping match; if it yields `> 1` matches, `host_replace` fails with `matched {matches} occurrences; expected exactly 1 (at lines ...)`.
- **Preserve original line endings and UTF-8 BOM:**
  - Unless `old_text` and `new_text` differ *only* in `\r`, match against the LF-normalized view (`\r\n` -> `\n`), format `new_text` using the matched region's (or file's dominant) line ending (`\r\n` vs `\n`), and preserve any leading UTF-8 BOM (`\u{FEFF}`).

---

## Implementation Code for `src/script.rs`

```rust
const UTF8_BOM: char = '\u{FEFF}';

/// Find non-overlapping exact byte ranges of `needle` in `haystack`.
fn find_exact_ranges(haystack: &str, needle: &str) -> Vec<(usize, usize)> {
    if needle.is_empty() {
        return Vec::new();
    }
    haystack
        .match_indices(needle)
        .map(|(start, matched)| (start, start + matched.len()))
        .collect()
}

/// Find non-overlapping byte ranges in LF-normalized `view` where every line in
/// `search` matches after stripping trailing horizontal whitespace (`' '`, `'\t'`).
fn find_line_trimmed_ranges(view: &str, search: &str) -> Vec<(usize, usize)> {
    let trailing_newline = search.ends_with('\n');
    let mut expected: Vec<&str> = search.split('\n').collect();
    if trailing_newline {
        expected.pop();
    }
    if expected.is_empty() {
        return Vec::new();
    }

    struct LineSpan<'a> {
        start: usize,
        end_with_newline: usize,
        end_without_newline: usize,
        has_newline: bool,
        text: &'a str,
    }

    let mut lines = Vec::new();
    let mut offset = 0;
    for segment in view.split_inclusive('\n') {
        let has_newline = segment.ends_with('\n');
        let text = if has_newline {
            &segment[..segment.len() - 1]
        } else {
            segment
        };
        let start = offset;
        let end_with_newline = offset + segment.len();
        let end_without_newline = start + text.len();
        lines.push(LineSpan {
            start,
            end_with_newline,
            end_without_newline,
            has_newline,
            text,
        });
        offset = end_with_newline;
    }

    if lines.len() < expected.len() {
        return Vec::new();
    }

    let trimmed_expected: Vec<&str> = expected
        .iter()
        .map(|line| line.trim_end_matches([' ', '\t']))
        .collect();

    let mut matches: Vec<(usize, usize)> = Vec::new();
    for idx in 0..=(lines.len() - expected.len()) {
        let window = &lines[idx..idx + expected.len()];
        let all_match = window
            .iter()
            .zip(&trimmed_expected)
            .all(|(actual, exp)| actual.text.trim_end_matches([' ', '\t']) == *exp);
        if !all_match {
            continue;
        }
        let last = &window[window.len() - 1];
        if trailing_newline && !last.has_newline {
            continue;
        }
        let start = window[0].start;
        let end = if trailing_newline {
            last.end_with_newline
        } else {
            last.end_without_newline
        };
        if matches
            .last()
            .is_some_and(|&(_, prev_end)| prev_end > start)
        {
            continue;
        }
        matches.push((start, end));
    }
    matches
}

/// Dominant line ending (`"\r\n"` or `"\n"`) in `text`, or `fallback` on a tie.
fn dominant_line_ending<'a>(text: &str, fallback: &'a str) -> &'a str {
    let crlf_count = text.matches("\r\n").count();
    let total_lf = text.matches('\n').count();
    let lf_count = total_lf.saturating_sub(crlf_count);
    if crlf_count == lf_count {
        fallback
    } else if crlf_count > lf_count {
        "\r\n"
    } else {
        "\n"
    }
}

/// Map a byte offset in the LF-normalized `view` back to the original `source`
/// byte offset using the sorted positions where `\r\n` had `\r` removed.
fn view_offset_to_source(crlf_view_offsets: &[usize], view_offset: usize) -> usize {
    let cr_before = crlf_view_offsets.partition_point(|&pos| pos < view_offset);
    view_offset + cr_before
}

/// Compute the 1-based line number of `byte_offset` in `text`.
fn one_based_line_number(text: &str, byte_offset: usize) -> usize {
    1 + text[..byte_offset.min(text.len())]
        .bytes()
        .filter(|&b| b == b'\n')
        .count()
}

/// Format the error when `old_text` matched more than once, naming up to the
/// first 5 1-based line numbers where matches start.
fn format_multi_match_error(path: &str, text: &str, ranges: &[(usize, usize)]) -> String {
    let matches = ranges.len();
    let line_list: Vec<String> = ranges
        .iter()
        .take(5)
        .map(|&(start, _)| one_based_line_number(text, start).to_string())
        .collect();
    let suffix = if matches > 5 {
        format!(" (first 5 at lines {}; add surrounding context lines)", line_list.join(", "))
    } else {
        format!(" (at lines {}; add surrounding context lines)", line_list.join(", "))
    };
    format!("tools.replace(`{path}`): `old_text` matched {matches} occurrences; expected exactly 1{suffix}")
}

/// Maximum UTF-8 bytes retained for a single matched line in `tools.grep` (2,000 bytes).
pub const GREP_LINE_MAX_BYTES: usize = 2_000;

/// Truncate a single `git grep` matched line at `limit` bytes on a UTF-8
/// character boundary so minified files or single-line JSON fixtures cannot
/// exhaust the 16 KiB `execute_script` output budget.
fn cap_grep_line(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let end = floor_char_boundary(text, limit);
    let omitted = text.len() - end;
    format!(
        "{}... [line truncated to {limit} bytes; +{omitted} bytes]",
        &text[..end]
    )
}
```

---

## Verification

```bash
cargo test --lib script && cargo clippy --all-targets -- -D warnings
```

Add unit tests in `src/script.rs`:
- `when_replacing_text_given_crlf_file_and_lf_old_text_then_matches_and_preserves_crlf_endings`
- `when_replacing_text_given_trailing_whitespace_difference_then_matches_via_line_trimmed_fallback`
- `when_replacing_text_given_identical_old_and_new_text_then_returns_identical_error`
- `when_replacing_text_given_utf8_bom_then_preserves_bom_on_write`
- `when_replacing_text_given_multiple_occurrences_then_reports_one_based_line_numbers`
- `when_grepping_files_given_line_over_two_thousand_bytes_then_truncates_matched_line_on_utf8_boundary`
