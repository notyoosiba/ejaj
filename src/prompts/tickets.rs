//! System prompt and kickoff for the tickets agent: the tickets pass.

/// System prompt for the tickets agent. It runs once, unattended, between the
/// spec agent's `finish_spec` and the orchestrator's queue: the specification
/// object is its only input and the ordered ticket set its only output.
pub const TICKETS_SYSTEM_PROMPT: &str = "\
Turn the specification below into an ordered set of tickets without prompting the user.

## Workflow

1. **Inspect the codebase (read-only) with `execute_script`:** match existing module boundaries,
   types, schemas and test harnesses before deciding anything. Use `tools.list(glob?)`,
   `tools.grep(pattern, glob?)` and `tools.read(path, startLine?, endLine?)` (1-based inclusive line
   numbers). Prefer one script that gathers multiple facts over many separate tool calls. Never
   write, build or test.
2. **Slice the specification into vertical slices:** each ticket is a narrow but complete path
   through every layer it touches -- a tracer bullet that is independently verifiable on its own.
   Never slice by layer or by module: a ticket that changes only one module, only one file or only
   one layer is not a ticket, because the behaviour is not verifiable until some later ticket
   lands. Scope each ticket to at most one primary user journey or entry point and size it for one
   fresh context window. Prefer a focused set of test expectations -- typically 3–8 BDD cases in one test file -- instead of bundling 15 or more expectations across multiple suites.
   Treat the test count and single-file preference as typical sizing guidance, not an absolute prohibition: more than one test file is acceptable when one focused journey or a shared verification gate requires it.
   Batch small same-shape edits into one ticket only when they share one verification gate.
   Order the tickets in implementation order
   and name the dependency edges between them: a ticket that builds on another names that id in its
   `blocked_by`. Fold `README.md`, `TODO.md`, documentation, and `.plan/`/`.plans/` tracker updates into the executable-behavior ticket whose deliverable needs them; never create standalone documentation or tracker tickets for those accompanying updates. Fold setup, configuration, scaffolding, documentation, `README.md`, `TODO.md`, and `.plan/`/`.plans/` tracker updates into the behavioral ticket that needs them rather than creating standalone accompanying-work tickets.
3. **Sequence a wide mechanical change as expand, migrate, contract:** one mechanical edit with a
   wide effect -- a rename, a signature change or a shape change that breaks many call sites at
   once -- is not a tracer bullet. Add the new form beside the old form first (the expand), then
   migrate the call sites in batches that each keep the gate green, then remove the old form once
   no caller remains (the contract). Each migration batch is blocked by the expand, and the
   contract is blocked by every batch, so the dependency edges express the sequence exactly.
4. **Look for prefactoring first:** make the change easy, then make the easy change. A prefactor
   ticket comes before the tickets that depend on it and its id appears in their `blocked_by`. If
   no prefactor is needed, do not invent one.
5. **Hand over:** call `finish_tickets` with the whole set. A pass that ends without calling the
   tool hands over nothing, so always end by calling the tool and never write the set as a plain
   reply.

Scope every ticket behaviourally: state the observable change the ticket makes, and name the target
files and modules in `prerequisites_assumed` and `scope_of_changes` so the code agent can go
straight to them.

When a ticket consumes a contract from a ticket named in its `blocked_by`, `prerequisites_assumed` must state the exact Rust module paths, type names, and complete function signatures, including every parameter type and every return type.
When a ticket produces a contract for a ticket named in its `blocks`, `scope_of_changes` must state the exact public type names and complete function signatures, including every parameter type and every return type.
Always distinguish the consumed prerequisite contract from the produced scope contract. Do not repeat expand-migrate-contract sequencing here: Step 3 remains its sole source.
Do not invent an interface when the ticket has no applicable `blocked_by` dependency or no downstream `blocks` consumer; write `None.` instead.

`finish_tickets` takes one required argument, `tickets`: the whole set as a JSON object. Every
ticket is an object with exactly these fields:
- `id`: the numbered ticket identifier, of the form `<digits>-<slugs>`, unique in the set and made
  only of letters, digits and `.`, `_`, `-`.
- `blocked_by`: the ids of tickets that must be resolved before this one.
- `blocks`: the ids of tickets this one unblocks; documentation only, it never affects scheduling.
- `title`: an imperative action title.
- `objective`: the outcome in prose.
- `acceptance_criteria`: the acceptance criteria this ticket makes true, copied from the
  specification story it implements without paraphrasing, shortening or rewording them and without
  a leading checkbox marker.
- `prerequisites_assumed`: what the ticket takes for granted about the tree, including the existing
  files and modules it builds on, and, when the ticket consumes a contract, the exact consumed
  interface described below.
- `out_of_scope`: what this ticket must not touch.
- `scope_of_changes`: the behaviour the ticket makes work and the target files/modules it touches,
  and, when the ticket produces a contract, the exact produced interface described below.
- `test_expectations`: the outside-in test cases the ticket adds or updates for executable source-code behavior, structured by the 4
  keeper categories (Core Entry-Point Behaviors, Edge-Case Tables, Contract & Invariant Checks,
  Regression Tests) and the 5-dimension boundary matrix (Empty/Zero/Nil, Minimum valid boundary,
  Maximum limit/Upper boundary, Malformed/Invalid payload, Boundary unauthorized/Forbidden access),
  with strict edge-only mocking (never mock internal classes, services or local persistence).
  Consolidate the applicable boundary cases from that matrix into table-driven `Edge-Case Tables` test functions
  rather than emitting separate duplicate test functions for the same behavior, and omit a dimension or keeper
  category that does not apply: never require an irrelevant dimension or keeper category to be tested. Keep each
  ticket to a compact set of distinct test functions -- typically 3–6 `when_<action>_given_<context>_then_<outcome>` tests --
  and treat that as typical sizing guidance rather than a hard minimum or a hard upper bound,
  splitting into an additional ticket when the feature genuinely needs more distinct test functions.
  Write test expectations only for executable source-code behavior -- never for documentation, `README.md`, `TODO.md`, `.plan/`/`.plans/` tracker files, or other human-facing prose. Every ticket that changes executable source code must carry at least one non-blank entry in `test_expectations`, and every substantive entry must name its test in lowercase `snake_case` `when_<action>_given_<context>_then_<outcome>` form (without
  internal function or class names in the test title). A documentation-only or markdown-only ticket that modifies only documentation or markdown files and no executable source code may pass `test_expectations: []`, which renders as `None.`; test expectations are required only for executable source-code behavior.
- `verification_gate`: the exact terminal command that must exit 0.

The `verification_gate` must demonstrate this ticket's new behaviour and would fail before that
behaviour exists. Use a narrow test selection (`cargo test <module>::tests`) or an appropriate
content check (`cargo test && git grep -q <token> -- <path>`). Do not use the bare command
`cargo test` when a narrower command proves the behaviour.

Every `blocked_by` and `blocks` entry must name an id that exists in the set, and the set must be
acyclic. The orchestrator runs each ticket's verification gate itself, so the gate must be a real
command that exits 0 -- a ticket is resolved only once its gate does.

The harness renders each ticket from its fields with a fixed template, so the headings are not
yours to write. A field with no entries is written as an empty list, and a bullet-list section that
genuinely does not apply is the single line `None.`. The document looks like this:

---
id: \"<id>\"
blocked_by: []
blocks: []
---

# <id>: <title>

## Objective

<the outcome in prose>

## Acceptance Criteria

- <a criterion copied from the specification story this ticket implements>

## Context & Boundaries

**Prerequisites assumed:** <what the ticket takes for granted about the tree>

**Out of scope for this ticket:**

- <a thing this ticket must not touch>

## Scope of Changes

- <a change this ticket makes>

## Test Expectations

- <a test case naming its `when_<action>_given_<context>_then_<outcome>` test identifier>

## Verification Gate

<the exact terminal command, which must exit with code 0>
";

/// Sent to the tickets agent along with the specification object the spec agent
/// wrote in `finish_spec`, rendered to markdown and appended below. The
/// specification is the whole of what the tickets agent is told: none of the
/// plan or the interview behind it is replayed.
pub const TICKETS_KICKOFF_PROMPT: &str = "\
The specification is reproduced below. Turn it into the ordered ticket set that implements it and \
hand it over with `finish_tickets`. Inspect the codebase read-only as needed; do not implement \
anything.

# Technical specification

";

#[cfg(test)]
mod tests {
    use super::TICKETS_SYSTEM_PROMPT;

    fn assert_prompt_contains(needles: &[&str]) {
        for needle in needles {
            assert!(
                TICKETS_SYSTEM_PROMPT.contains(needle),
                "TICKETS_SYSTEM_PROMPT must contain `{needle}`"
            );
        }
    }

    /// Core entry-point behavior at the minimum-valid boundary: one primary
    /// journey and a focused, typically single-file expectation set.
    #[test]
    fn when_slicing_tickets_given_one_primary_journey_then_prompt_requires_a_focused_ticket() {
        assert_prompt_contains(&[
            "Scope each ticket to at most one primary user journey or entry point",
            "typically 3–8 BDD cases in one test file",
            "instead of bundling 15 or more expectations across multiple suites",
        ]);
    }

    /// Maximum-limit boundary: large suites stay discouraged while the sizing
    /// rules remain advisory, and same-shape edits batch only behind one gate.
    #[test]
    fn when_sizing_tickets_given_many_suites_or_one_shared_gate_then_prompt_keeps_sizing_advisory()
    {
        assert_prompt_contains(&[
            "Treat the test count and single-file preference as typical sizing guidance, not an absolute prohibition",
            "more than one test file is acceptable when one focused journey or a shared verification gate requires it",
            "Batch small same-shape edits into one ticket only when they share one verification gate",
        ]);
    }

    /// Edge-case table: the applicable Empty/Zero/Nil, Minimum valid,
    /// Maximum limit/Upper boundary, Malformed/Invalid payload, and
    /// Unauthorized/Forbidden cases consolidate into table-driven
    /// `Edge-Case Tables` functions instead of duplicate per-dimension
    /// functions, irrelevant dimensions stay optional, and the
    /// executable-source non-blank rule survives.
    #[test]
    fn when_consolidating_boundary_expectations_given_applicable_cases_then_groups_them_as_edge_case_tables()
     {
        assert_prompt_contains(&[
            "Consolidate the applicable boundary cases from that matrix into table-driven `Edge-Case Tables` test functions",
            "rather than emitting separate duplicate test functions for the same behavior",
            "never require an irrelevant dimension or keeper category to be tested",
            "Empty/Zero/Nil",
            "Minimum valid boundary",
            "Maximum limit/Upper boundary",
            "Malformed/Invalid payload",
            "Boundary unauthorized/Forbidden access",
            "Every ticket that changes executable source code must carry at least one non-blank entry in `test_expectations`",
        ]);
    }

    /// Contract and invariant check: typically 3–6 distinct BDD functions is
    /// advisory sizing, neither a hard minimum nor a hard upper bound, and a
    /// genuinely larger feature earns another ticket.
    #[test]
    fn when_sizing_ticket_expectations_given_distinct_functions_then_keeps_guidance_advisory_and_allows_a_split()
     {
        assert_prompt_contains(&[
            "typically 3–6 `when_<action>_given_<context>_then_<outcome>` tests",
            "typical sizing guidance rather than a hard minimum or a hard upper bound",
            "splitting into an additional ticket when the feature genuinely needs more distinct test functions",
        ]);
        assert!(
            !TICKETS_SYSTEM_PROMPT.contains("maximum number of test expectations"),
            "the sizing rule must stay advisory, not a hard cap"
        );
    }

    /// Empty/zero boundary: without blockers or consumers the prompt requires
    /// no invented interface, so both contracts stay conditional.
    #[test]
    fn when_describing_dependencies_given_no_blockers_or_consumers_then_prompt_requires_no_invented_interface()
     {
        assert_prompt_contains(&[
            "When a ticket consumes a contract from a ticket named in its `blocked_by`",
            "When a ticket produces a contract for a ticket named in its `blocks`",
            "Do not invent an interface when the ticket has no applicable `blocked_by` dependency or no downstream `blocks` consumer",
        ]);
    }

    /// Contract and invariant check: consumed prerequisites and produced scope
    /// contracts both demand exact names and complete signatures.
    #[test]
    fn when_describing_dependencies_given_blocker_and_downstream_tickets_then_prompt_requires_exact_signatures()
     {
        assert_prompt_contains(&[
            "`prerequisites_assumed` must state the exact Rust module paths, type names, and complete function signatures",
            "including every parameter type and every return type",
            "`scope_of_changes` must state the exact public type names and complete function signatures",
            "distinguish the consumed prerequisite contract from the produced scope contract",
        ]);
    }

    /// Regression: the handoff schema, ordered workflow, source-only test
    /// policy, prefactoring rule, and Step 3 sequencing remain intact.
    #[test]
    fn when_preserving_ticket_guidance_given_prompt_contracts_then_schema_workflow_and_test_policy_remain_unchanged()
     {
        assert_prompt_contains(&[
            "Turn the specification below into an ordered set of tickets without prompting the user.",
            "## Workflow",
            "1. **Inspect the codebase (read-only) with `execute_script`:**",
            "5. **Hand over:** call `finish_tickets` with the whole set.",
            "with exactly these fields:",
            "test expectations are required only for executable source-code behavior",
            "Every ticket that changes executable source code must carry at least one non-blank entry in `test_expectations`",
            "lowercase `snake_case` `when_<action>_given_<context>_then_<outcome>` form",
            "A documentation-only or markdown-only ticket that modifies only documentation or markdown files and no executable source code may pass `test_expectations: []`, which renders as `None.`",
            "Use a narrow test selection (`cargo test <module>::tests`)",
            "**Look for prefactoring first:** make the change easy, then make the easy change.",
            "**Sequence a wide mechanical change as expand, migrate, contract:**",
        ]);

        const FIELDS: [&str; 11] = [
            "id",
            "blocked_by",
            "blocks",
            "title",
            "objective",
            "acceptance_criteria",
            "prerequisites_assumed",
            "out_of_scope",
            "scope_of_changes",
            "test_expectations",
            "verification_gate",
        ];
        let fields_start = TICKETS_SYSTEM_PROMPT
            .find("with exactly these fields:")
            .expect("the handoff schema must stay introduced");
        let fields_end = TICKETS_SYSTEM_PROMPT
            .find("The `verification_gate` must demonstrate")
            .expect("the verification-gate guidance must follow the schema");
        let field_block = &TICKETS_SYSTEM_PROMPT[fields_start..fields_end];
        let defined: Vec<&str> = field_block
            .lines()
            .filter_map(|line| line.strip_prefix("- `"))
            .filter_map(|rest| rest.split_once("`: ").map(|(name, _)| name))
            .collect();
        assert_eq!(
            defined, FIELDS,
            "the handoff schema must keep exactly its eleven existing fields in order"
        );
        assert_eq!(
            TICKETS_SYSTEM_PROMPT
                .matches("expand, migrate, contract")
                .count(),
            1,
            "Step 3 must remain the sole source of expand-migrate-contract sequencing"
        );
    }
}
