//! System prompt and kickoff for the review agent: the post-queue review pass.

/// System prompt for the review agent. It runs unattended after a resolved
/// ticket-queue round, auditing the repository's tests and implementation
/// against the technical specification, the executed tickets, and the settled
/// test-design blueprint, then hands over its report with `finish_review`.
pub const REVIEW_SYSTEM_PROMPT: &str = "\
Audit the completed ticket queue against the technical specification and test design blueprint without prompting the user.

## Workflow

1. **Inspect the repository (read-only) with `execute_script`:** use `tools.diff()` to read the \
change set, `tools.read(path, startLine?, endLine?)` to inspect the modified and added production \
and test files, and `tools.list(glob?)` / `tools.grep(pattern, glob?)` to check existing seams. \
Batch related reads and searches into one script call. Never write, build or test.
2. **Complete the Pre-Flight Audit Scorecard (`scorecard`):** every field is required and non-blank:
   - `keeper_coverage`: verify coverage across the 4 Keeper Categories. Audit executable source-code behavior only; never emit findings or fix tickets that request unit tests for documentation, `README.md`, `TODO.md`, changelogs, `.plan/`/`.plans/` tracker files, or other human-facing prose:
     1. Core Entry-Point Behaviors (real caller journeys via CLI, API, or public interface).
     2. Edge-Case Tables (a consolidated parameterized table when boundary cases share the same \
input/assertion shape, OR the explicit per-dimension boundary tests named in the executed \
tickets' `test_expectations` across the 5 boundary dimensions; never reject a suite solely for \
using the explicit per-dimension `when_<action>_given_<context>_then_<outcome>` tests its tickets \
specified instead of a single table).
     3. Contract & Invariant Checks (schemas, error envelopes, consumer invariants).
     4. Regression Tests (applicable when the spec targets a bug fix).
   - `sediment_and_coupling`: confirm zero internal mocks (internal classes, services, and local \
persistence layers are never mocked; test doubles are restricted strictly to non-deterministic \
external boundaries), no tautologies, and no private state introspection.
   - `inferred_assumptions`: explicit log of inferred assumptions (boundaries, status codes, \
default schemas) verified against the implementation.
   - `boundary_matrix`: evaluate the Mandatory 5-Dimension Boundary Matrix across the test suite. Evaluate the boundary matrix for executable source-code behavior only; never emit findings or fix tickets that request unit tests for documentation, `README.md`, `TODO.md`, changelogs, `.plan/`/`.plans/` tracker files, or other human-facing prose:
     1. Empty / Zero / Nil
     2. Minimum valid boundary
     3. Maximum limit / Upper boundary
     4. Malformed / Invalid payload
     5. Boundary unauthorized / Forbidden access
   - `code_and_scope`: verify that every ticket's acceptance criteria, scope of changes, out-of-scope \
boundaries, and verification gate were honoured without specification drift.
3. **Verify caller-intent test naming:** every authored test title must use domain-level BDD \
language in `when_<action>_given_<context>_then_<outcome>` lowercase `snake_case`; technical \
internal function or class names in test titles are strictly forbidden.
4. **Decide the verdict and hand over with `finish_review`:**
   - **When no defects remain:** pass `findings: []` and `fixes: []`. The pipeline will write \
`review.md` and proceed to the change summary.
   - **When defects remain:** list the highest-priority defects in `findings` and emit the ordered \
`fixes` tickets that resolve them. When a finding from any prior review round is still unresolved, \
or when a later fix ticket breaks behavior that an earlier finding covered, reuse that earlier \
finding's exact `id` in `findings` (while giving the new fix ticket a fresh `r<round>-<n>-<slug>` \
ticket `id`); assign a new `finding.id` only to a defect not reported in any prior review round. \
Every finding must be covered in substance by at least one fix ticket, and every fix ticket must \
satisfy all ticket rules:
     - `id`: a new unique ticket id (`[A-Za-z0-9._-]+`, e.g. `r1-1-<slug>`) that does not collide \
with any ticket id already executed in this run.
     - `blocked_by` / `blocks`: may only reference other fix tickets emitted in this same `fixes` list.
     - `title`, `objective`, `prerequisites_assumed`, `out_of_scope`, `scope_of_changes`, \
`acceptance_criteria` (at least one non-blank criterion), `test_expectations`, and `verification_gate` (exact command exiting 0). A documentation-only fix ticket that changes no executable source code and modifies only documentation or markdown may pass `test_expectations: []`, which renders as `None.`. Every fix ticket that changes executable source code must carry at least one non-blank `test_expectations` entry, even when the fix adds no new test case, and every entry must name a test in lowercase `snake_case` `when_<action>_given_<context>_then_<outcome>` form; uppercase, whitespace, punctuation, or otherwise malformed identifiers are rejected; never request tests for documentation, `README.md`, `TODO.md`, changelogs, `.plan/`/`.plans/` tracker files, or other human-facing prose.

Always end by calling `finish_review`; never end your turn with a plain reply.";

/// Sent to the review agent along with the specification, the executed tickets,
/// the queue report, and any prior review rounds from this run.
pub const REVIEW_KICKOFF_PROMPT: &str = "\
The ticket queue resolved every ticket in the current round. The specification, the executed \
tickets, the queue report, and any prior review rounds are reproduced below. Inspect the repository \
read-only with `execute_script` (`tools.diff()` and `tools.read()`) and hand over your audit with \
`finish_review`.

";

#[cfg(test)]
mod tests {
    use crate::prompts::{
        PLAN_SYSTEM_PROMPT, REVIEW_SYSTEM_PROMPT, SPEC_SYSTEM_PROMPT, TICKETS_SYSTEM_PROMPT,
    };

    fn assert_contains_all(prompt_name: &str, prompt: &str, needles: &[&str]) {
        for needle in needles {
            assert!(
                prompt.contains(needle),
                "{prompt_name} must contain `{needle}`"
            );
        }
    }

    /// The caller-intent test identifier form the review contract requires for
    /// every fix expectation: lowercase `snake_case` with exactly one action,
    /// context, and outcome segment. Uppercase, whitespace, punctuation, and
    /// structurally malformed identifiers are rejected.
    fn names_bdd_test(entry: &str) -> bool {
        if entry.is_empty() || entry != entry.trim() {
            return false;
        }
        if !entry
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
        {
            return false;
        }
        let Some(rest) = entry.strip_prefix("when_") else {
            return false;
        };
        let Some((action, rest)) = rest.split_once("_given_") else {
            return false;
        };
        let Some((context, outcome)) = rest.split_once("_then_") else {
            return false;
        };
        let segments = [action, context, outcome];
        segments.iter().all(|segment| !segment.is_empty())
            && segments
                .iter()
                .all(|segment| !segment.contains("_given_") && !segment.contains("_then_"))
    }

    const PLAN_HEADINGS: &[&str] = &[
        "Interview the user to align on core architectural",
        "# <the goal in one line>",
        "## 1. Decisions",
        "## 2. Defaults Chosen",
        "## 3. Constraints",
        "## 4. Open Questions",
    ];

    const SPEC_HEADINGS: &[&str] = &[
        "Convert the upstream plan into a technical specification without prompting the user.",
        "## 1. Problem Statement",
        "## 2. Solution Overview",
        "## 3. User Stories & Acceptance Criteria",
        "## 4. Implementation Decisions",
        "### Module Architecture",
        "### Data Models & Contracts",
        "### ADR Compliance",
        "## 5. Testing Decisions & Seams",
        "### Test Seams",
        "### Coverage Strategy",
        "### Prior Art",
        "## 6. Out of Scope",
        "## 7. Deferred Items & Operational Risks",
    ];

    const TICKET_TEMPLATE: &[&str] = &[
        "Turn the specification below into an ordered set of tickets without prompting the user.",
        "## Test Expectations",
        "## Verification Gate",
        "`test_expectations`",
        "`verification_gate`",
    ];

    const REVIEW_FIELDS: &[&str] = &[
        "keeper_coverage",
        "sediment_and_coupling",
        "inferred_assumptions",
        "boundary_matrix",
        "code_and_scope",
        "finish_review",
    ];

    /// Every phase prompt must hand over through its own terminal tool. Losing
    /// a marker would leave the agent without an exit and stall the pipeline.
    const PHASE_HANDOFF_MARKERS: &[(&str, &str, &str)] = &[
        ("PLAN_SYSTEM_PROMPT", PLAN_SYSTEM_PROMPT, "finish_plan"),
        ("SPEC_SYSTEM_PROMPT", SPEC_SYSTEM_PROMPT, "finish_spec"),
        (
            "TICKETS_SYSTEM_PROMPT",
            TICKETS_SYSTEM_PROMPT,
            "finish_tickets",
        ),
        (
            "REVIEW_SYSTEM_PROMPT",
            REVIEW_SYSTEM_PROMPT,
            "finish_review",
        ),
    ];

    #[test]
    fn when_checking_phase_handoffs_given_prompt_contracts_then_required_markers_remain_present() {
        for (prompt_name, prompt, marker) in PHASE_HANDOFF_MARKERS {
            assert_contains_all(prompt_name, prompt, &[marker]);
        }
    }

    #[test]
    fn when_reading_phase_prompts_given_source_only_rules_then_all_four_constants_define_scope() {
        assert_contains_all(
            "PLAN_SYSTEM_PROMPT",
            PLAN_SYSTEM_PROMPT,
            &[
                "Automated tests apply only to executable source code",
                "never to documentation, `README.md`, `TODO.md`, changelogs, or tracker directories (`.plan/`, including this repository's `.plans/` spelling), or other human-facing prose",
            ],
        );
        assert_contains_all(
            "SPEC_SYSTEM_PROMPT",
            SPEC_SYSTEM_PROMPT,
            &[
                "Scope testing decisions to executable source-code behavior only",
                "Testing decisions cover only executable source-code behavior",
                "record those file updates in `### Module Architecture` but exclude them from `## 5. Testing Decisions & Seams`",
            ],
        );
        assert_contains_all(
            "TICKETS_SYSTEM_PROMPT",
            TICKETS_SYSTEM_PROMPT,
            &[
                "test cases the ticket adds or updates for executable source-code behavior",
                "test expectations are required only for executable source-code behavior",
            ],
        );
        assert_contains_all(
            "REVIEW_SYSTEM_PROMPT",
            REVIEW_SYSTEM_PROMPT,
            &[
                "Audit executable source-code behavior only",
                "Evaluate the boundary matrix for executable source-code behavior only",
            ],
        );
    }

    #[test]
    fn when_planning_documentation_given_obsolete_markdown_assertions_then_prompt_requires_removal()
    {
        assert_contains_all(
            "PLAN_SYSTEM_PROMPT",
            PLAN_SYSTEM_PROMPT,
            &[
                "Remove obsolete tests that assert markdown or tracker text or checklist status rather than refreshing them",
            ],
        );
    }

    #[test]
    fn when_specifying_documentation_only_work_given_test_sections_then_prompt_requires_none() {
        assert_contains_all(
            "SPEC_SYSTEM_PROMPT",
            SPEC_SYSTEM_PROMPT,
            &[
                "never to documentation, `README.md`, `TODO.md`, `.plan/`/`.plans/` tracker files, changelogs, or other human-facing prose",
                "When documentation or tracker updates accompany source changes, record those file updates in `### Module Architecture` but exclude them from `## 5. Testing Decisions & Seams`",
                "A documentation-only specification that changes only documentation or human-facing content writes exactly `None.` under each of `### Test Seams`, `### Coverage Strategy`, and `### Prior Art`",
            ],
        );
    }

    #[test]
    fn when_ticketing_documentation_only_work_given_no_source_changes_then_empty_expectations_are_permitted()
     {
        assert_contains_all(
            "TICKETS_SYSTEM_PROMPT",
            TICKETS_SYSTEM_PROMPT,
            &[
                "Fold `README.md`, `TODO.md`, documentation, and `.plan/`/`.plans/` tracker updates into the executable-behavior ticket whose deliverable needs them; never create standalone documentation or tracker tickets for those accompanying updates.",
                "A documentation-only or markdown-only ticket that modifies only documentation or markdown files and no executable source code may pass `test_expectations: []`, which renders as `None.`",
            ],
        );
    }

    /// Documentation-only review fixes change no executable source code, so the
    /// contract permits an empty expectation list.
    #[test]
    fn when_reviewing_fixes_given_documentation_only_changes_then_empty_expectations_are_permitted()
    {
        assert_contains_all(
            "REVIEW_SYSTEM_PROMPT",
            REVIEW_SYSTEM_PROMPT,
            &[
                "A documentation-only fix ticket that changes no executable source code and modifies only documentation or markdown may pass `test_expectations: []`, which renders as `None.`",
            ],
        );
    }

    /// A fix that changes executable source code always needs a test
    /// expectation, even when it happens to add no new test case.
    #[test]
    fn when_reviewing_fixes_given_executable_source_changes_then_expectations_are_required() {
        assert_contains_all(
            "REVIEW_SYSTEM_PROMPT",
            REVIEW_SYSTEM_PROMPT,
            &[
                "Every fix ticket that changes executable source code must carry at least one non-blank `test_expectations` entry, even when the fix adds no new test case",
            ],
        );
        assert!(
            !REVIEW_SYSTEM_PROMPT.contains("a fix ticket that adds no new test cases may pass"),
            "REVIEW_SYSTEM_PROMPT must not permit empty expectations for executable-source fixes"
        );
    }

    /// Edge-case table across the empty, whitespace, uppercase, punctuation,
    /// and structurally malformed dimensions: only the lowercase `snake_case`
    /// BDD form is accepted, and the minimum valid identifier passes without
    /// relying on the length of a hard-coded expectation array.
    #[test]
    fn when_reviewing_fixes_given_malformed_expectation_identifiers_then_lowercase_bdd_form_is_required()
     {
        assert_contains_all(
            "REVIEW_SYSTEM_PROMPT",
            REVIEW_SYSTEM_PROMPT,
            &[
                "every entry must name a test in lowercase `snake_case` `when_<action>_given_<context>_then_<outcome>` form",
                "uppercase, whitespace, punctuation, or otherwise malformed identifiers are rejected",
            ],
        );

        let valid = "when_fixing_executable_source_given_one_behaviour_change_then_names_its_test";
        assert!(
            names_bdd_test(valid),
            "the minimum valid lowercase BDD identifier must be accepted"
        );

        let malformed = [
            "",
            "   ",
            "when fixing executable source given one behaviour change then names its test",
            "When_fixing_executable_source_given_one_behaviour_change_then_names_its_test",
            "when_fixing_executable_Source_given_one_behaviour_change_then_names_its_test",
            "when_fixing_executable_source-given_one_behaviour_change_then_names_its_test",
            "when_fixing_executable_source_given_one_behaviour_change_then_names_its_test.",
            "when_fixing_executable_source_given_one_behaviour_change_then_names_its_test ",
            " when_fixing_executable_source_given_one_behaviour_change_then_names_its_test",
            "fixing_executable_source_given_one_behaviour_change_then_names_its_test",
            "when_fixing_executable_source_one_behaviour_change_then_names_its_test",
            "when_fixing_executable_source_given_one_behaviour_change_names_its_test",
            "when__given_one_behaviour_change_then_names_its_test",
            "when_fixing_executable_source_given__then_names_its_test",
            "when_fixing_executable_source_given_one_behaviour_change_then_",
            "when_fixing_executable_source_given_one_behaviour_change_then_names_its_test_then_extra",
        ];
        for entry in malformed {
            assert!(
                !names_bdd_test(entry),
                "malformed expectation `{entry}` must be rejected"
            );
        }
    }

    /// Core entry-point behavior: the review pass confines its coverage and
    /// boundary audit to executable source code and forbids test requests for
    /// human-facing content.
    #[test]
    fn when_reviewing_prompts_given_source_only_test_scope_then_human_facing_test_requests_are_forbidden()
     {
        assert_contains_all(
            "REVIEW_SYSTEM_PROMPT",
            REVIEW_SYSTEM_PROMPT,
            &[
                "Audit executable source-code behavior only",
                "Evaluate the boundary matrix for executable source-code behavior only",
                "never emit findings or fix tickets that request unit tests for documentation, `README.md`, `TODO.md`, changelogs, `.plan/`/`.plans/` tracker files, or other human-facing prose",
                "never request tests for documentation, `README.md`, `TODO.md`, changelogs, `.plan/`/`.plans/` tracker files, or other human-facing prose",
            ],
        );
    }

    /// Contract and invariant check: fix expectations for source changes need a
    /// non-blank entry in the required BDD identifier form.
    #[test]
    fn when_reviewing_prompt_contracts_given_fix_expectation_rules_then_source_changes_require_nonblank_bdd_expectations()
     {
        assert_contains_all(
            "REVIEW_SYSTEM_PROMPT",
            REVIEW_SYSTEM_PROMPT,
            &[
                "Every fix ticket that changes executable source code must carry at least one non-blank `test_expectations` entry",
                "every entry must name a test in lowercase `snake_case` `when_<action>_given_<context>_then_<outcome>` form",
            ],
        );
        assert!(
            !REVIEW_SYSTEM_PROMPT.contains("a fix ticket that adds no new test cases may pass"),
            "REVIEW_SYSTEM_PROMPT must not restore the broad empty-expectation exception"
        );
    }

    /// Regression guard: findings and fix tickets never request tests for
    /// human-facing prose, and documentation-only fixes alone may omit them.
    #[test]
    fn when_reviewing_prompts_given_human_facing_content_then_findings_and_fixes_do_not_request_tests()
     {
        assert_contains_all(
            "REVIEW_SYSTEM_PROMPT",
            REVIEW_SYSTEM_PROMPT,
            &[
                "never emit findings or fix tickets that request unit tests for documentation, `README.md`, `TODO.md`, changelogs, `.plan/`/`.plans/` tracker files, or other human-facing prose",
                "never request tests for documentation, `README.md`, `TODO.md`, changelogs, `.plan/`/`.plans/` tracker files, or other human-facing prose",
            ],
        );
        assert!(
            !REVIEW_SYSTEM_PROMPT.contains(
                "a fix ticket that adds no new test cases may pass `test_expectations: []`"
            ),
            "REVIEW_SYSTEM_PROMPT must not restore the broad empty-expectation exception"
        );
    }

    #[test]
    fn when_checking_prompt_contracts_given_substantive_test_expectations_then_bdd_naming_and_templates_remain_required()
     {
        assert_contains_all("PLAN_SYSTEM_PROMPT", PLAN_SYSTEM_PROMPT, PLAN_HEADINGS);
        assert_contains_all("SPEC_SYSTEM_PROMPT", SPEC_SYSTEM_PROMPT, SPEC_HEADINGS);
        assert_contains_all(
            "TICKETS_SYSTEM_PROMPT",
            TICKETS_SYSTEM_PROMPT,
            TICKET_TEMPLATE,
        );
        assert_contains_all("REVIEW_SYSTEM_PROMPT", REVIEW_SYSTEM_PROMPT, REVIEW_FIELDS);
        assert_contains_all(
            "TICKETS_SYSTEM_PROMPT",
            TICKETS_SYSTEM_PROMPT,
            &[
                "lowercase `snake_case` `when_<action>_given_<context>_then_<outcome>`",
                "every substantive entry must name its test",
            ],
        );
        assert_contains_all(
            "REVIEW_SYSTEM_PROMPT",
            REVIEW_SYSTEM_PROMPT,
            &[
                "when_<action>_given_<context>_then_<outcome>",
                "reuse that earlier finding's exact `id` in `findings`",
            ],
        );
    }

    #[test]
    fn when_auditing_prompt_contracts_given_human_facing_files_then_tests_are_not_requested() {
        assert_contains_all(
            "PLAN_SYSTEM_PROMPT",
            PLAN_SYSTEM_PROMPT,
            &["Automated tests apply only to executable source code"],
        );
        assert_contains_all(
            "SPEC_SYSTEM_PROMPT",
            SPEC_SYSTEM_PROMPT,
            &[
                "never to documentation, `README.md`, `TODO.md`, `.plan/`/`.plans/` tracker files, changelogs, or other human-facing prose",
            ],
        );
        assert_contains_all(
            "TICKETS_SYSTEM_PROMPT",
            TICKETS_SYSTEM_PROMPT,
            &[
                "never for documentation, `README.md`, `TODO.md`, `.plan/`/`.plans/` tracker files, or other human-facing prose",
            ],
        );
        assert_contains_all(
            "REVIEW_SYSTEM_PROMPT",
            REVIEW_SYSTEM_PROMPT,
            &[
                "never emit findings or fix tickets that request unit tests for documentation, `README.md`, `TODO.md`, changelogs, `.plan/`/`.plans/` tracker files, or other human-facing prose",
            ],
        );
    }

    #[test]
    fn when_ticketing_empty_expectations_given_documentation_only_work_then_none_is_permitted() {
        assert_contains_all(
            "TICKETS_SYSTEM_PROMPT",
            TICKETS_SYSTEM_PROMPT,
            &[
                "modifies only documentation or markdown files and no executable source code may pass `test_expectations: []`, which renders as `None.`",
            ],
        );
        assert_contains_all(
            "REVIEW_SYSTEM_PROMPT",
            REVIEW_SYSTEM_PROMPT,
            &[
                "A documentation-only fix ticket that changes no executable source code and modifies only documentation or markdown may pass `test_expectations: []`",
            ],
        );
    }

    #[test]
    fn when_including_one_source_behavior_expectation_given_a_ticket_then_bdd_naming_remains_required()
     {
        assert_contains_all(
            "TICKETS_SYSTEM_PROMPT",
            TICKETS_SYSTEM_PROMPT,
            &[
                "Every ticket that changes executable source code must carry at least one non-blank entry in `test_expectations`",
                "every substantive entry must name its test in lowercase `snake_case` `when_<action>_given_<context>_then_<outcome>` form",
            ],
        );
        assert_contains_all(
            "REVIEW_SYSTEM_PROMPT",
            REVIEW_SYSTEM_PROMPT,
            &[
                "every entry must name a test in lowercase `snake_case` `when_<action>_given_<context>_then_<outcome>` form",
            ],
        );
    }

    #[test]
    fn when_assessing_prompt_boundaries_given_no_numeric_payload_or_access_rules_then_these_dimensions_are_not_applicable()
     {
        for (name, prompt) in [
            ("PLAN_SYSTEM_PROMPT", PLAN_SYSTEM_PROMPT),
            ("SPEC_SYSTEM_PROMPT", SPEC_SYSTEM_PROMPT),
            ("TICKETS_SYSTEM_PROMPT", TICKETS_SYSTEM_PROMPT),
            ("REVIEW_SYSTEM_PROMPT", REVIEW_SYSTEM_PROMPT),
        ] {
            assert!(
                prompt.contains("executable source-code")
                    || prompt.contains("executable source code"),
                "{name} must scope automated testing to executable source-code behavior"
            );
            for forbidden in [
                "HTTP 401",
                "HTTP 403",
                "payload validation rule",
                "access-control rule",
                "numeric limit",
                "maximum number of test expectations",
            ] {
                assert!(
                    !prompt.contains(forbidden),
                    "{name} must not introduce numeric, payload-validation, or access-control constraints: `{forbidden}`"
                );
            }
        }
    }

    #[test]
    fn when_inspecting_review_prompt_given_blueprint_rules_then_contains_required_disciplines() {
        for needle in [
            "Audit the completed ticket queue against the technical specification and test design blueprint",
            "keeper_coverage",
            "sediment_and_coupling",
            "inferred_assumptions",
            "boundary_matrix",
            "code_and_scope",
            "when_<action>_given_<context>_then_<outcome>",
            "finish_review",
        ] {
            assert!(
                REVIEW_SYSTEM_PROMPT.contains(needle),
                "REVIEW_SYSTEM_PROMPT must contain `{needle}`"
            );
        }
    }

    #[test]
    fn when_inspecting_review_prompt_given_edge_case_table_and_finding_reuse_rules_then_documents_both_contracts()
     {
        for needle in [
            "explicit per-dimension boundary tests named in the executed tickets' `test_expectations`",
            "never reject a suite solely for using the explicit per-dimension",
            "reuse that earlier finding's exact `id` in `findings`",
        ] {
            assert!(
                REVIEW_SYSTEM_PROMPT.contains(needle),
                "REVIEW_SYSTEM_PROMPT must contain `{needle}`"
            );
        }
    }
}
