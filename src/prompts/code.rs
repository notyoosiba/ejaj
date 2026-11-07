//! System prompt and kickoff for the code agent: the per-ticket runner.
//!
//! The artifact the code agent receives is exactly one rendered ticket -- the
//! same string the pipeline wrote to disk -- prefixed by
//! [`CODE_TICKET_KICKOFF_PROMPT`]. The kickoff states the one-ticket, unattended
//! contract and the single implement-and-run-the-gate instruction; the ticket's
//! own `Acceptance Criteria` section carries the criteria, unchanged.

use crate::specification::NONE;
use crate::tickets::{GATE_HEADING, SCOPE_HEADING, TEST_HEADING};

/// The canonical rendered test-expectations section for a ticket that has no
/// applicable source-code test. Only this exact section omits the discipline.
fn no_expectations_section() -> String {
    format!("{TEST_HEADING}\n\n{NONE}")
}

/// A Markdown fence delimiter: the length of its backtick run and whether
/// anything other than whitespace follows that run.
///
/// Markdown allows up to three leading spaces before the delimiter. A run of at
/// least three backticks opens a fence, but only a delimiter with nothing but
/// whitespace after it can close one. A triple-backtick-prefixed command line
/// with trailing text is therefore not a closing fence.
fn fence_delimiter(line: &str) -> Option<(usize, bool)> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let ticks = rest.len() - rest.trim_start_matches('`').len();
    if ticks < 3 {
        return None;
    }
    Some((ticks, rest[ticks..].trim().is_empty()))
}

/// Locate the first (`last == false`) or last (`last == true`) line-anchored
/// occurrence of `heading` outside a fenced code block.
///
/// Rendered section headings always occupy a full line outside a fence, while
/// the verification-gate command is itself fenced. Heading-like text inside
/// that command is therefore never a section boundary. A closing fence must use
/// at least as many backticks as its opener and carry nothing but whitespace
/// after them, so a non-closing triple-backtick-prefixed command line keeps the
/// fence open.
fn locate_section_heading(text: &str, heading: &str, last: bool) -> Option<usize> {
    let mut fence_opener: Option<usize> = None;
    let mut found = None;
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        match fence_opener {
            Some(opener) => {
                if let Some((ticks, closes)) = fence_delimiter(line)
                    && closes
                    && ticks >= opener
                {
                    fence_opener = None;
                }
            }
            None => {
                if let Some((ticks, _)) = fence_delimiter(line) {
                    fence_opener = Some(ticks);
                } else if line.trim_end_matches(['\r', '\n']) == heading {
                    if !last {
                        return Some(offset);
                    }
                    found = Some(offset);
                }
            }
        }
        offset += line.len();
    }
    found
}

fn find_section_heading(text: &str, heading: &str) -> Option<usize> {
    locate_section_heading(text, heading, false)
}

fn rfind_section_heading(text: &str, heading: &str) -> Option<usize> {
    locate_section_heading(text, heading, true)
}

/// Whether the rendered ticket's actual test-expectations section is exactly
/// the canonical no-expectations section.
///
/// The renderer places the test-expectations section immediately after the
/// scope-of-changes section and immediately before the verification-gate
/// section, so the section is bounded by those two headings. Selecting the
/// first `## Test Expectations` heading after `## Scope of Changes` ensures
/// that canonical marker text embedded inside a substantive test expectation is
/// treated as part of that expectation's body rather than as the section root.
fn has_no_expectations_section(ticket: &str) -> bool {
    let before_gate = match rfind_section_heading(ticket, GATE_HEADING) {
        Some(gate_start) => &ticket[..gate_start],
        None => ticket,
    }
    .trim_end_matches('\n');
    let after_scope = match rfind_section_heading(before_gate, SCOPE_HEADING) {
        Some(scope_start) => &before_gate[scope_start..],
        None => before_gate,
    };
    let Some(section_start) = find_section_heading(after_scope, TEST_HEADING) else {
        return false;
    };
    after_scope[section_start..] == no_expectations_section()
}

/// System prompt for the code agent (`execute_script` and `bash` tool loop).
pub const CODE_SYSTEM_PROMPT: &str = "\
You are an autonomous software engineer.
You have two command tools: `execute_script` and `bash`.

Use `execute_script` for all file listing, searching, reading, diffing, and editing. Use `bash` \
only for `cargo`, `git`, and the ticket's verification gate.

`execute_script` runs a JavaScript program in an ephemeral sandbox with these bindings:

```typescript
declare namespace tools {
  /** Read a UTF-8 file (or 1-based inclusive line range), relative to the project root. */
  function read(path: string, startLine?: number, endLine?: number): string;

  /** Overwrite or create a UTF-8 file, relative to the project root. */
  function write(path: string, content: string): void;

  /** Replace one exact occurrence. Throws on zero or on more than one. */
  function replace(path: string, old_text: string, new_text: string): void;

  /** List tracked and untracked-not-ignored files (git ls-files). */
  function list(glob?: string): string[];

  /** Search tracked and untracked-not-ignored files (git grep). */
  function grep(
    pattern: string,
    glob?: string,
  ): { path: string; line: number; text: string }[];

  /** Return the repository-root unified diff (staged, unstaged, and non-ignored untracked files, excluding .ejaj/). */
  function diff(): string;
}
```

Script rules:
- Write plain JavaScript; do not use type annotations.
- Batch related reads, searches, and multi-file edits into a single `execute_script` call rather than making one call per file or operation.
- Return a string (returned as plain text), a JSON-serializable value, or log with `console.log`.
- Paths are relative to the project root.
- The script runs without network access.

Output limits and spill logs:
- Every tool result is capped at 16 KiB (16,384 bytes). Bash output at or below 16 KiB \
  (16,384 bytes) is returned unchanged, and an empty result stays empty. `execute_script` output \
  above that limit is truncated on a UTF-8 character boundary; the marker reports the exact \
  omitted bytes, so filter results in JavaScript when a returned value may be large. \
  `execute_script` output is never spilled.
- Bash output above 16 KiB is saved complete before a bounded digest is returned. The complete raw \
  output -- stdout followed by stderr, including bytes that are not valid UTF-8 -- is written to \
  the `tools-output/` directory inside the active timestamped session directory, \
  `.ejaj/<timestamp>/tools-output/`. The primary file name is \
  `step-<one-based-step>-call-<one-based-Bash-call-index>.log`; on a collision, names add `-1`, \
  `-2`, and so on before `.log`. The path the digest returns is authoritative and relative to \
  the project root, so read that exact path. The log begins with a `Command:` section holding the \
  originating command exactly as it was run, followed by an `Output:` section holding the complete \
  raw stdout followed by raw stderr; the 16 KiB threshold applies only to that captured output. \
  Inspect the complete log with `tools.read` through `execute_script`, using a line range when it \
  is large. If the complete output could not be saved, the digest reports the failure and names no \
  path.

Rules:
1. `apply_patch`, diff tools, and specialized editors DO NOT exist.
2. Never inspect or list files inside `target/` or `.git/`.
3. To list or find files, ALWAYS use `tools.list(glob?)` in `execute_script`.
4. To search text across the codebase, ALWAYS use `tools.grep(pattern, glob?)` in `execute_script`.
5. To read, diff, or edit files, ALWAYS use `tools.read()`, `tools.diff()`, `tools.write()`, or `tools.replace()` in `execute_script`. Prefer `tools.diff()` over `git diff` or `git status` in `bash` when inspecting your changes. Do not use `cat`, `sed`, `awk`, or shell heredocs in `bash` to read or write files.
6. Apply a coherent set of edits across all affected files before running `cargo check` or tests, rather than compiling after every single file edit. Fix any errors or warnings before completing.
7. Chain related `cargo` commands with `&&` in a single `bash` call instead of running them in separate turns. Tool output is automatically capped at 16 KiB by the harness: never pipe `cargo` commands (`check`, `test`, `clippy`, `fmt`) through `head`, `tail`, `grep`, or `sed`, which hides diagnostics and forces re-runs. If formatting is needed, run `cargo fmt` directly rather than `cargo fmt --check` followed by manual edits.
8. Run the ticket's verification gate command and confirm it exits 0. As soon as the ticket's scope and test expectations are implemented and the verification gate exits 0 (warning-free), call `task_complete` immediately on the next turn -- do not run extra unrequested audit passes (`git log`, repo-wide greps, reading `AGENTS.md`, etc.).
9. Never end your turn without calling `task_complete`. You are unattended: there is no user to ask, and a turn that ends without this call fails the ticket.";

/// Sent to the code agent along with the one ticket it is handed, which is
/// appended below. That ticket is the session's entire task: the conversation
/// starts fresh, nothing from any earlier ticket is replayed, and the
/// orchestrator runs the ticket's verification gate itself once the agent stops
/// -- whether it declared the ticket done or abandoned its turn.
///
/// The intro carries exactly one implement-and-run-the-gate instruction; the
/// ticket appended below is the artifact's only ticket and carries the
/// acceptance criteria.
pub const CODE_TICKET_KICKOFF_PROMPT: &str = "\
The ticket reproduced below is this session's entire task. The conversation starts fresh: nothing \
from any earlier ticket or pass is replayed. Everything the ticket lists as a prerequisite is \
assumed to exist already, and anything the ticket marks out of scope must not be touched. You \
are unattended, so there is no user to ask: a turn that ends without `task_complete` fails the \
ticket. Implement the ticket, then run its verification gate command and make sure it exits 0 \
before you call `task_complete`.

# Ticket

";

/// Red-first TDD and test-design discipline appended to rendered ticket
/// kickoffs in the ticket queue.
pub const CODE_TDD_DISCIPLINE: &str = "\
Follow a strict Red-First TDD loop for this ticket:\n\
1. Work in one focused vertical slice at a time at the module's public seam rather than writing \
every test before implementation: for each slice, author the relevant outside-in expectation from \
`## Test Expectations` using exact `when_<action>_given_<context>_then_<outcome>` test titles \
(never put internal function or class names in test titles) and exercise behavior through the \
public seam rather than requiring tests of internal helpers.\n\
2. Keep internal helpers private: do not widen `pub` visibility on internal helpers solely for \
tests. When behavior is consolidated behind a deeper seam, replace tests coupled to the removed \
internal behavior with seam-level tests rather than retaining both sets.\n\
3. Enforce strict edge-only mocking: use real internal collaborators and local persistence; never \
mock internal classes, services, or local persistence layers, never inspect private state or other \
implementation internals; restrict test doubles strictly to non-deterministic external boundaries, \
and observe behavior and state through the module's public interface.\n\
4. Test assertions must use independent, known literal expected values, not expected values \
recomputed using the implementation under test.\n\
5. Cover the ticket's applicable keeper categories (Core Entry-Point Behaviors, Edge-Case Tables \
across Empty/Zero/Nil, Minimum valid, Maximum limit, Malformed/Invalid, and Unauthorized/Forbidden \
boundaries, Contract & Invariant Checks, and Regression Tests).\n\
6. While iterating on each slice, run a focused test target with `bash`; this focused run is \
supplemental to, and never a replacement for, the ticket-supplied `verification_gate`.\n\
7. When new production behavior is missing, confirm the focused test fails naturally (Red), \
implement only the minimal production change, then rerun the focused test to confirm it passes \
(Green).\n\
8. When a newly added regression or boundary test already passes against existing production \
behavior, that passing result is sufficient evidence to proceed with the production \
implementation: do not require or manufacture an artificial red failure, and never break, \
sabotage, stash, or back up production files to force one.\n\
9. After implementing the production code, run the ticket's `verification_gate` with `bash` until \
it exits successfully.\n\
10. Complete all source and test changes before tracker edits. Make any needed `TODO.md` and \
`.plan/*.md` tracker edits last, immediately before running the verification gate. If the gate \
fails, revert those tracker edits first, preserve the production and test changes, and then \
proceed with fixes or another gate run; make tracker edits for the later attempt only after those \
changes are complete and immediately before that gate.\n\n";

/// Build the user kickoff message for `ticket`. Rendered queue tickets (which
/// begin with YAML front matter `---\n`) include [`CODE_TDD_DISCIPLINE`] unless
/// their test-expectations section is exactly the canonical
/// `## Test Expectations\n\nNone.` text; bare wire-test prompts keep the
/// minimal [`CODE_TICKET_KICKOFF_PROMPT`] prefix.
pub fn ticket_kickoff(ticket: &str) -> String {
    let rendered = ticket.starts_with("---\n");
    if rendered && !has_no_expectations_section(ticket) {
        format!("{CODE_TDD_DISCIPLINE}{CODE_TICKET_KICKOFF_PROMPT}{ticket}")
    } else {
        format!("{CODE_TICKET_KICKOFF_PROMPT}{ticket}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The output-limits section on its own, so spill wording is asserted
    /// without picking up the general tool rules below it.
    fn output_section() -> &'static str {
        CODE_SYSTEM_PROMPT
            .split_once("Output limits and spill logs:")
            .expect("the output-limits section must exist")
            .1
            .split_once("\nRules:")
            .expect("the output-limits section must end before the rules")
            .0
    }

    /// Rule 7 on its own, so the cap wording can be checked without matching
    /// unrelated 64 KiB references elsewhere in the prompt.
    fn rule_seven() -> &'static str {
        CODE_SYSTEM_PROMPT
            .split_once("\n7. ")
            .expect("rule seven must exist")
            .1
            .split_once("\n8. ")
            .expect("rule seven must end before rule eight")
            .0
    }

    /// Literal opening of the Red-First discipline. Deliberately independent of
    /// the production constant so a kickoff assertion cannot pass by
    /// recomputing the implementation's own formula.
    const DISCIPLINE_LITERAL_PREFIX: &str = "Follow a strict Red-First TDD loop for this ticket:\n";

    /// Literal sentence that opens the shared kickoff prompt.
    const KICKOFF_PROMPT_LITERAL_PREFIX: &str =
        "The ticket reproduced below is this session's entire task.";

    /// Literal boundary the kickoff prompt leaves immediately before the
    /// appended rendered ticket.
    const TICKET_LITERAL_BOUNDARY: &str = "\n\n# Ticket\n\n";

    /// Assert the disciplined kickoff with independent literal expectations and
    /// exact preservation of the supplied rendered ticket.
    fn assert_disciplined_kickoff_preserves_ticket(kickoff: &str, ticket: &str) {
        let prefix = kickoff.strip_suffix(ticket).unwrap_or_else(|| {
            panic!("the supplied rendered ticket must be appended byte for byte: {kickoff:?}")
        });
        assert!(
            prefix.starts_with(DISCIPLINE_LITERAL_PREFIX),
            "the kickoff must open with the literal discipline: {prefix:?}"
        );
        assert!(
            prefix.contains(KICKOFF_PROMPT_LITERAL_PREFIX),
            "the kickoff must carry the literal shared prompt: {prefix:?}"
        );
        assert!(
            prefix.ends_with(TICKET_LITERAL_BOUNDARY),
            "the kickoff prompt must end at the literal ticket boundary: {prefix:?}"
        );
    }

    /// Assert the minimal kickoff with independent literal expectations and
    /// exact preservation of the supplied rendered ticket.
    fn assert_minimal_kickoff_preserves_ticket(kickoff: &str, ticket: &str) {
        let prefix = kickoff.strip_suffix(ticket).unwrap_or_else(|| {
            panic!("the supplied ticket must be appended byte for byte: {kickoff:?}")
        });
        assert!(
            !prefix.contains(DISCIPLINE_LITERAL_PREFIX),
            "the minimal kickoff must omit the discipline: {prefix:?}"
        );
        assert!(
            prefix.starts_with(KICKOFF_PROMPT_LITERAL_PREFIX),
            "the minimal kickoff must open with the literal shared prompt: {prefix:?}"
        );
        assert!(
            prefix.ends_with(TICKET_LITERAL_BOUNDARY),
            "the kickoff prompt must end at the literal ticket boundary: {prefix:?}"
        );
    }

    /// Core entry-point behavior: Rule 7 names the same 16 KiB cap the
    /// output-limits section and the harness enforce.
    #[test]
    fn when_code_agent_reads_prompt_given_rule_seven_then_documents_sixteen_kib_cap() {
        let rule = rule_seven();
        assert!(
            rule.contains("Tool output is automatically capped at 16 KiB by the harness"),
            "rule seven must state the 16 KiB harness cap: {rule:?}"
        );
        assert!(
            rule.contains("automatically capped at 16 KiB"),
            "rule seven must document the corrected cap: {rule:?}"
        );
    }

    /// Empty/zero boundary: an empty result stays unchanged at the same 16 KiB
    /// threshold; correcting Rule 7 must not alter the delivery behavior.
    #[test]
    fn when_code_agent_reads_prompt_given_empty_output_then_preserves_existing_limit_behavior() {
        let output = output_section();
        for needle in [
            "at or below 16 KiB (16,384 bytes) is returned unchanged",
            "empty result stays empty",
        ] {
            assert!(
                output.contains(needle),
                "the output-limits section must keep `{needle}`: {output:?}"
            );
        }
    }

    /// Minimum valid boundary: the output-limits section keeps the exact
    /// 16 KiB (16,384 bytes) wording while Rule 7 names the same 16 KiB cap.
    #[test]
    fn when_code_agent_reads_prompt_given_minimum_valid_limit_then_documents_sixteen_kib() {
        let output = output_section();
        assert!(
            output.contains("16 KiB (16,384 bytes)"),
            "the output-limits section must keep the exact threshold: {output:?}"
        );
        assert!(
            rule_seven().contains("automatically capped at 16 KiB"),
            "rule seven must document the same 16 KiB cap: {:?}",
            rule_seven()
        );
    }

    /// Upper boundary: Rule 7 must not carry the obsolete 64 KiB cap that
    /// contradicted the 16 KiB output limit.
    #[test]
    fn when_code_agent_reads_prompt_given_upper_limit_then_rejects_obsolete_rule_seven_cap() {
        let rule = rule_seven();
        assert!(
            !rule.contains("64 KiB"),
            "rule seven must not retain the obsolete 64 KiB cap: {rule:?}"
        );
        assert!(
            rule.contains("16 KiB"),
            "rule seven must keep the corrected cap: {rule:?}"
        );
    }

    /// Regression: the obsolete Rule 7 wording must not return.
    #[test]
    fn when_prompt_limit_regresses_given_rule_seven_then_rejects_sixty_four_kib() {
        assert!(
            !rule_seven().contains("64 KiB"),
            "rule seven must never state a 64 KiB cap: {:?}",
            rule_seven()
        );
    }

    #[test]
    fn code_system_prompt_documents_diff_batching_and_verification_discipline() {
        for needle in [
            "function diff(): string;",
            "tools.diff()",
            "Batch related reads, searches, and multi-file edits into a single `execute_script` call",
            "Apply a coherent set of edits across all affected files before running `cargo check`",
            "Chain related `cargo` commands with `&&` in a single `bash` call",
            "automatically capped at 16 KiB",
            "never pipe `cargo` commands",
            "call `task_complete` immediately on the next turn",
        ] {
            assert!(
                CODE_SYSTEM_PROMPT.contains(needle),
                "CODE_SYSTEM_PROMPT must contain `{needle}`"
            );
        }
    }

    /// Core entry-point behavior: the prompt tells the agent where the active
    /// session keeps complete Bash logs, so it can find them after a spill.
    #[test]
    fn when_code_agent_reads_prompt_given_oversized_bash_output_then_finds_session_log_instructions()
     {
        let output = output_section();
        for needle in [
            "active timestamped session",
            "`.ejaj/<timestamp>/tools-output/`",
            "`tools-output/` directory",
            "`Command:`",
            "`Output:`",
            "originating command",
            "names no path",
        ] {
            assert!(
                output.contains(needle),
                "the spill section must describe `{needle}`: {output:?}"
            );
        }
    }

    /// Edge-case table: a session directory can already hold a log with the
    /// primary step-and-call name, so the prompt documents the numeric suffix.
    #[test]
    fn when_code_agent_reads_prompt_given_collision_then_finds_suffix_convention() {
        let output = output_section();
        for needle in [
            "primary file name",
            "`step-<one-based-step>-call-<one-based-Bash-call-index>.log`",
            "collision",
            "`-1`, `-2`, and so on before `.log`",
        ] {
            assert!(
                output.contains(needle),
                "the spill section must explain the collision convention `{needle}`: {output:?}"
            );
        }
    }

    /// Contract: the digest's path is not a suggestion to reconstruct. It is
    /// already project-root-relative and the log is read through script tools.
    #[test]
    fn when_code_agent_reads_prompt_given_digest_path_then_treats_it_as_authoritative() {
        let output = output_section();
        for needle in [
            "path the digest returns is authoritative",
            "relative to the project root",
            "`tools.read` through `execute_script`",
            "line range when it is large",
        ] {
            assert!(
                output.contains(needle),
                "the spill section must describe `{needle}`: {output:?}"
            );
        }
    }

    /// Regression: only Bash spills. Script output is truncated in place and
    /// the verification gate is never claimed to spill.
    #[test]
    fn when_code_agent_reads_prompt_given_script_and_gate_output_then_only_bash_is_described_as_spilled()
     {
        let output = output_section();
        assert!(
            output.contains("`execute_script` output above that limit is truncated"),
            "script output above the cap must be described as truncated: {output:?}"
        );
        assert!(
            output.contains("`execute_script` output is never spilled"),
            "script output must be described as never spilled: {output:?}"
        );
        assert!(
            !output.to_lowercase().contains("verification"),
            "the spill section must not claim verification-gate output is spilled: {output:?}"
        );
        assert!(
            !output.to_lowercase().contains("gate"),
            "the spill section must not claim gate output is spilled: {output:?}"
        );
    }

    /// Empty/zero boundary: output at or below the cap, including an empty
    /// result, is delivered unchanged rather than spilled.
    #[test]
    fn when_code_agent_reads_prompt_given_empty_bash_output_then_keeps_threshold_description() {
        let output = output_section();
        for needle in [
            "at or below 16 KiB (16,384 bytes) is returned unchanged",
            "empty result stays empty",
        ] {
            assert!(
                output.contains(needle),
                "the spill section must keep the threshold distinction `{needle}`: {output:?}"
            );
        }
    }

    /// Minimum valid boundary: the delivery cap keeps its exact wording.
    #[test]
    fn when_code_agent_reads_prompt_given_limit_output_then_keeps_sixteen_kib_boundary() {
        let output = output_section();
        for needle in ["16 KiB", "16,384 bytes", "threshold"] {
            assert!(
                output.contains(needle),
                "the spill section must keep `{needle}`: {output:?}"
            );
        }
    }

    /// Maximum/upper boundary: the returned digest is bounded, and the model
    /// is told how to filter or read around that bound.
    #[test]
    fn when_code_agent_reads_prompt_given_oversized_output_then_documents_bounded_digest() {
        let output = output_section();
        for needle in [
            "bounded digest",
            "exact omitted bytes",
            "filter results in JavaScript",
            "truncated on a UTF-8 character boundary",
        ] {
            assert!(
                output.contains(needle),
                "the spill section must explain `{needle}`: {output:?}"
            );
        }
    }

    /// Malformed payload: invalid UTF-8 does not stop the complete raw log
    /// from being stored, so the agent can still inspect the original bytes.
    #[test]
    fn when_code_agent_reads_prompt_given_invalid_utf8_output_then_documents_complete_raw_log() {
        let output = output_section();
        for needle in ["complete raw output", "not valid UTF-8", "stdout", "stderr"] {
            assert!(
                output.contains(needle),
                "the spill section must keep the complete raw log wording `{needle}`: {output:?}"
            );
        }
    }

    /// Forbidden boundary: reading a spill goes through script tools, and the
    /// prompt grants Bash no additional purpose beyond cargo, git and the gate.
    #[test]
    fn when_code_agent_reads_prompt_given_read_only_phase_then_does_not_grant_bash() {
        assert!(
            CODE_SYSTEM_PROMPT.contains(
                "Use `bash` only for `cargo`, `git`, and the ticket's verification gate."
            ),
            "the tool-permission line must stay unchanged"
        );
        let output = output_section();
        assert!(
            output.contains("`tools.read` through `execute_script`"),
            "spill logs must be read through script tools: {output:?}"
        );
        for forbidden in [
            "use `bash` to read",
            "read the log with `bash`",
            "inspect it with `bash`",
        ] {
            assert!(
                !output.to_lowercase().contains(forbidden),
                "the spill section must not grant Bash for `{forbidden}`: {output:?}"
            );
        }
    }

    /// A rendered ticket built through the real renderer, so the canonical
    /// section text is exercised end to end instead of being hard-coded.
    fn rendered_ticket(test_expectations: Vec<&str>) -> String {
        rendered_ticket_with_objective(
            "Repair the cross-suite regression that the transition gate reported.",
            test_expectations,
        )
    }

    /// A rendered ticket whose verification gate is supplied by the caller, so
    /// heading-like text can be embedded in the fenced command.
    fn rendered_ticket_with_gate(test_expectations: Vec<&str>, verification_gate: &str) -> String {
        use crate::tickets::Ticket;
        Ticket {
            id: "2-repair".to_string(),
            blocked_by: Vec::new(),
            blocks: Vec::new(),
            title: "Repair the suite".to_string(),
            objective: "Repair the cross-suite regression that the transition gate reported."
                .to_string(),
            acceptance_criteria: vec!["The suite exits 0.".to_string()],
            prerequisites_assumed: "The queue round completed.".to_string(),
            out_of_scope: vec!["Unrelated features.".to_string()],
            scope_of_changes: vec!["Resolve the regression.".to_string()],
            test_expectations: test_expectations.into_iter().map(str::to_string).collect(),
            verification_gate: verification_gate.to_string(),
        }
        .to_markdown()
    }

    /// A rendered ticket whose objective is supplied by the caller, so marker
    /// text can be embedded in a field other than the test-expectations section.
    fn rendered_ticket_with_objective(objective: &str, test_expectations: Vec<&str>) -> String {
        use crate::tickets::Ticket;
        Ticket {
            id: "2-repair".to_string(),
            blocked_by: Vec::new(),
            blocks: Vec::new(),
            title: "Repair the suite".to_string(),
            objective: objective.to_string(),
            acceptance_criteria: vec!["The suite exits 0.".to_string()],
            prerequisites_assumed: "The queue round completed.".to_string(),
            out_of_scope: vec!["Unrelated features.".to_string()],
            scope_of_changes: vec!["Resolve the regression.".to_string()],
            test_expectations: test_expectations.into_iter().map(str::to_string).collect(),
            verification_gate: "cargo test".to_string(),
        }
        .to_markdown()
    }

    /// Assert every independent literal requirement appears in the kickoff.
    fn assert_contains_all(kickoff: &str, needles: &[&str]) {
        for needle in needles {
            assert!(
                kickoff.contains(needle),
                "the discipline must contain `{needle}`: {kickoff}"
            );
        }
    }

    /// Core entry-point behavior: an eligible rendered ticket receives the
    /// complete discipline through the existing kickoff seam and stays the
    /// exact suffix.
    #[test]
    fn when_rendering_a_ticket_given_applicable_expectations_then_delivers_full_discipline_and_exact_ticket_suffix()
     {
        let rendered = rendered_ticket(vec![
            "when_running_the_suite_given_repair_then_all_tests_pass",
        ]);
        let kickoff = ticket_kickoff(&rendered);

        assert!(
            kickoff.starts_with(DISCIPLINE_LITERAL_PREFIX),
            "the eligible ticket must open with the literal discipline: {kickoff:?}"
        );
        assert_contains_all(
            &kickoff,
            &[
                "one focused vertical slice at a time",
                "at the module's public seam",
                "author the relevant outside-in expectation",
                "rather than writing every test before implementation",
                "when_<action>_given_<context>_then_<outcome>",
                "never put internal function or class names in test titles",
                "exercise behavior through the public seam",
                "run a focused test target with `bash`",
                "supplemental to, and never a replacement for, the ticket-supplied `verification_gate`",
                "confirm the focused test fails naturally",
                "implement only the minimal production change",
                "rerun the focused test to confirm it passes",
                "After implementing the production code, run the ticket's `verification_gate` with `bash` until it exits successfully",
            ],
        );
        assert_disciplined_kickoff_preserves_ticket(&kickoff, &rendered);
    }

    /// Core entry-point behavior: bare prompts and canonical
    /// no-expectations tickets keep only the minimal kickoff prefix.
    #[test]
    fn when_building_a_kickoff_given_bare_or_canonical_empty_ticket_then_keeps_minimal_prefix() {
        let bare = "Call the bash tool once.";
        let bare_kickoff = ticket_kickoff(bare);
        assert_minimal_kickoff_preserves_ticket(&bare_kickoff, bare);

        let empty = rendered_ticket(Vec::new());
        assert!(
            empty.contains("## Test Expectations\n\nNone."),
            "the renderer must emit the canonical marker: {empty}"
        );
        let empty_kickoff = ticket_kickoff(&empty);
        assert_minimal_kickoff_preserves_ticket(&empty_kickoff, &empty);
    }

    /// Core entry-point behavior: an already-passing boundary or regression
    /// expectation is sufficient evidence to implement production behavior,
    /// with no manufactured red failure and no production-file sabotage.
    #[test]
    fn when_rendering_a_ticket_given_an_already_green_boundary_expectation_then_allows_implementation_without_artificial_red()
     {
        let rendered = rendered_ticket(vec![
            "when_reading_a_boundary_input_given_empty_value_then_returns_the_default",
        ]);
        let kickoff = ticket_kickoff(&rendered);

        assert_contains_all(
            &kickoff,
            &[
                "already passes against existing production behavior",
                "sufficient evidence to proceed with the production implementation",
                "do not require or manufacture an artificial red failure",
                "never break, sabotage, stash, or back up production files",
                "run the ticket's `verification_gate` with `bash` until it exits successfully",
            ],
        );
        assert!(
            !kickoff.contains("observe the expected red failure"),
            "the discipline must not require an observed red failure: {kickoff}"
        );
        assert_disciplined_kickoff_preserves_ticket(&kickoff, &rendered);
    }

    /// Edge-case table: the empty/zero/nil and all-blank boundaries omit the
    /// discipline.
    #[test]
    fn when_building_a_kickoff_given_empty_or_blank_expectations_then_omits_discipline() {
        for rendered in [
            rendered_ticket(Vec::new()),
            rendered_ticket(vec!["", "   ", "\t \n"]),
        ] {
            assert!(
                rendered.contains("## Test Expectations\n\nNone."),
                "the renderer must emit the canonical marker: {rendered}"
            );
            let kickoff = ticket_kickoff(&rendered);
            assert!(
                !kickoff.contains(DISCIPLINE_LITERAL_PREFIX),
                "the zero/blank boundary must omit the discipline: {kickoff}"
            );
            assert_minimal_kickoff_preserves_ticket(&kickoff, &rendered);
        }
    }

    /// Minimum valid boundary: one expectation is enough to keep the full
    /// discipline and the exact ticket suffix.
    #[test]
    fn when_building_a_kickoff_given_one_valid_expectation_then_preserves_discipline_and_ticket_suffix()
     {
        let rendered = rendered_ticket(vec!["when_a_given_b_then_c"]);
        let kickoff = ticket_kickoff(&rendered);
        assert!(
            kickoff.starts_with(DISCIPLINE_LITERAL_PREFIX),
            "one valid expectation is the minimum eligible ticket: {kickoff:?}"
        );
        assert_disciplined_kickoff_preserves_ticket(&kickoff, &rendered);
    }

    /// Maximum/upper boundary: a long expectation is neither truncated nor
    /// capped; the discipline and the full ticket survive.
    #[test]
    fn when_building_a_kickoff_given_a_long_expectation_then_preserves_the_untruncated_ticket() {
        let long = format!(
            "when_{}_given_a_full_suite_regression_then_reports",
            "long".repeat(2_000)
        );
        let rendered = rendered_ticket(vec![&long]);
        let kickoff = ticket_kickoff(&rendered);
        assert!(
            kickoff.contains(&long),
            "the long expectation must not be truncated"
        );
        assert_disciplined_kickoff_preserves_ticket(&kickoff, &rendered);
    }

    /// Malformed payload: only the exact canonical no-expectations section
    /// signal omits the discipline; marker-like content in other fields or in a
    /// fenced verification command keeps the existing selection behavior.
    #[test]
    fn when_building_a_kickoff_given_noncanonical_markers_or_fenced_gate_content_then_keeps_existing_selection_behavior()
     {
        let canonical = rendered_ticket(Vec::new());
        for section in [
            "## Test Expectations\nNone.",
            "## Test Expectations\n\n**None.**",
            "## Test Expectations\n\n- None.",
            "## Test Expectations\n \nNone.",
            "## Test Expectations\n\nnone.",
        ] {
            let rendered = canonical.replace("## Test Expectations\n\nNone.", section);
            let kickoff = ticket_kickoff(&rendered);
            assert!(
                kickoff.starts_with(DISCIPLINE_LITERAL_PREFIX),
                "only the exact canonical section may omit discipline: {section:?}"
            );
            assert_disciplined_kickoff_preserves_ticket(&kickoff, &rendered);
        }

        let marker_in_objective = rendered_ticket_with_objective(
            "Repair the regression.\n\n## Test Expectations\n\nNone.\n\nKeep the suite green.",
            vec!["when_running_the_suite_given_repair_then_all_tests_pass"],
        );
        let kickoff = ticket_kickoff(&marker_in_objective);
        assert!(kickoff.starts_with(DISCIPLINE_LITERAL_PREFIX));
        assert_disciplined_kickoff_preserves_ticket(&kickoff, &marker_in_objective);

        let marker_in_expectation = rendered_ticket(vec![
            "when_running_the_suite_given_repair_then_all_tests_pass\n\n## Test Expectations\n\nNone.",
        ]);
        let kickoff = ticket_kickoff(&marker_in_expectation);
        assert!(kickoff.starts_with(DISCIPLINE_LITERAL_PREFIX));
        assert_disciplined_kickoff_preserves_ticket(&kickoff, &marker_in_expectation);

        let fenced_gate = "cargo test --lib prompts::code\n\n\
                    ## Test Expectations\n\nNone.\n\n\
                    ## Verification Gate\n\n\
                    cargo test --lib prompts::code";
        let fenced_marker = rendered_ticket_with_gate(
            vec!["when_running_the_suite_given_repair_then_all_tests_pass"],
            fenced_gate,
        );
        let kickoff = ticket_kickoff(&fenced_marker);
        assert!(kickoff.starts_with(DISCIPLINE_LITERAL_PREFIX));
        assert_disciplined_kickoff_preserves_ticket(&kickoff, &fenced_marker);

        let nonclosing_gate = "cargo test --lib prompts::code\n\
                    ```not-a-closing-fence\n\n\
                    ## Test Expectations\n\nNone.\n\n\
                    ## Verification Gate\n\n\
                    cargo test --lib prompts::code";
        let nonclosing_marker = rendered_ticket_with_gate(
            vec!["when_running_the_suite_given_repair_then_all_tests_pass"],
            nonclosing_gate,
        );
        assert!(nonclosing_marker.contains("```not-a-closing-fence"));
        let kickoff = ticket_kickoff(&nonclosing_marker);
        assert!(kickoff.starts_with(DISCIPLINE_LITERAL_PREFIX));
        assert_disciplined_kickoff_preserves_ticket(&kickoff, &nonclosing_marker);

        let canonical_fenced_gate =
            "cargo test\n\n## Test Expectations\n\nNone.\n\n## Verification Gate\n\ncargo test";
        let canonical_with_fenced_marker =
            rendered_ticket_with_gate(Vec::new(), canonical_fenced_gate);
        let kickoff = ticket_kickoff(&canonical_with_fenced_marker);
        assert!(!kickoff.contains(DISCIPLINE_LITERAL_PREFIX));
        assert_minimal_kickoff_preserves_ticket(&kickoff, &canonical_with_fenced_marker);
    }

    /// Unauthorized/forbidden analog: an already-green test never licenses
    /// artificial-red sabotage or test-only visibility widening.
    #[test]
    fn when_reading_a_discipline_given_an_already_green_test_then_forbids_sabotage_and_visibility_widening()
     {
        let rendered = rendered_ticket(vec![
            "when_reading_a_boundary_input_given_empty_value_then_returns_the_default",
        ]);
        let kickoff = ticket_kickoff(&rendered);
        assert_contains_all(
            &kickoff,
            &[
                "Keep internal helpers private",
                "do not widen `pub` visibility on internal helpers solely for tests",
                "do not require or manufacture an artificial red failure",
                "never break, sabotage, stash, or back up production files",
                "sufficient evidence to proceed",
            ],
        );
        assert_disciplined_kickoff_preserves_ticket(&kickoff, &rendered);
    }

    /// Contract & invariant: the discipline requires public seams, independent
    /// oracles, real internal collaborators, edge-only test doubles, and the
    /// applicable keeper categories.
    #[test]
    fn when_reading_a_discipline_given_a_rendered_kickoff_then_requires_public_seams_independent_oracles_real_collaborators_and_keeper_categories()
     {
        let rendered = rendered_ticket(vec!["when_a_given_b_then_c"]);
        let kickoff = ticket_kickoff(&rendered);
        assert_contains_all(
            &kickoff,
            &[
                "Work in one focused vertical slice at a time at the module's public seam",
                "when_<action>_given_<context>_then_<outcome>",
                "never put internal function or class names in test titles",
                "Keep internal helpers private",
                "do not widen `pub` visibility on internal helpers solely for tests",
                "replace tests coupled to the removed internal behavior with seam-level tests rather than retaining both sets",
                "use real internal collaborators and local persistence",
                "never mock internal classes, services, or local persistence layers",
                "never inspect private state",
                "restrict test doubles strictly to non-deterministic external boundaries",
                "observe behavior and state through the module's public interface",
                "independent, known literal expected values",
                "not expected values recomputed using the implementation under test",
                "Core Entry-Point Behaviors",
                "Edge-Case Tables across Empty/Zero/Nil, Minimum valid, Maximum limit, Malformed/Invalid, and Unauthorized/Forbidden boundaries",
                "Contract & Invariant Checks",
                "Regression Tests",
            ],
        );
        assert_disciplined_kickoff_preserves_ticket(&kickoff, &rendered);
    }

    /// Contract & invariant: tracker edits are last before the gate, and a
    /// failed gate reverts those tracker edits first while preserving source
    /// and test changes.
    #[test]
    fn when_reading_a_discipline_given_a_failed_gate_then_requires_tracker_rollback_before_fixes() {
        let rendered = rendered_ticket(vec!["when_a_given_b_then_c"]);
        let kickoff = ticket_kickoff(&rendered);
        assert_contains_all(
            &kickoff,
            &[
                "Complete all source and test changes before tracker edits",
                "`TODO.md` and `.plan/*.md` tracker edits last, immediately before running the verification gate",
                "revert those tracker edits first",
                "preserve the production and test changes",
                "make tracker edits for the later attempt only after those changes are complete and immediately before that gate",
                "run the ticket's `verification_gate` with `bash` until it exits successfully",
            ],
        );
        assert_disciplined_kickoff_preserves_ticket(&kickoff, &rendered);
    }
}
