//! System prompt and kickoff for the code agent: the per-ticket runner.
//!
//! The artifact the code agent receives is exactly one rendered ticket -- the
//! same string the pipeline wrote to disk -- prefixed by
//! [`CODE_TICKET_KICKOFF_PROMPT`]. The kickoff states the one-ticket, unattended
//! contract and the single implement-and-run-the-gate instruction; the ticket's
//! own `Acceptance Criteria` section carries the criteria, unchanged.

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
1. Author the outside-in tests listed in `## Test Expectations` first, using exact \
`when_<action>_given_<context>_then_<outcome>` test titles (never put internal function or \
class names in test titles).\n\
2. Enforce strict edge-only mocking: never mock internal classes, services, or local persistence \
layers, and never introspect private state; restrict test doubles strictly to non-deterministic \
external boundaries.\n\
3. Cover the ticket's applicable keeper categories (Core Entry-Point Behaviors, Edge-Case Tables \
across Empty/Zero/Nil, Minimum valid, Maximum limit, Malformed/Invalid, and Unauthorized/Forbidden \
boundaries, Contract & Invariant Checks, and Regression Tests).\n\
4. Run the test command with `bash` to observe the expected red failure before implementing \
production changes, then implement the production code and run the ticket's `verification_gate` \
until it exits 0.\n\n";

/// Build the user kickoff message for `ticket`. Rendered queue tickets (which
/// begin with YAML front matter `---\n`) include [`CODE_TDD_DISCIPLINE`]; bare
/// wire-test prompts keep the minimal [`CODE_TICKET_KICKOFF_PROMPT`] prefix.
pub fn ticket_kickoff(ticket: &str) -> String {
    if ticket.starts_with("---\n") {
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

    #[test]
    fn when_building_ticket_kickoff_given_rendered_ticket_then_includes_tdd_discipline() {
        let rendered = "---\nid: \"1-test\"\n---\n# 1-test: Title\n";
        let kickoff = ticket_kickoff(rendered);
        assert!(kickoff.contains("when_<action>_given_<context>_then_<outcome>"));
        assert!(kickoff.contains("Red-First TDD"));
        assert!(kickoff.ends_with(rendered));

        let bare = "Call the bash tool once.";
        let bare_kickoff = ticket_kickoff(bare);
        assert_eq!(bare_kickoff, format!("{CODE_TICKET_KICKOFF_PROMPT}{bare}"));
    }
}
