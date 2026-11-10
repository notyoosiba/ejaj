//! Public contract for the plan and specification agents' prompts.
//!
//! Each agent reaches the model only through its exported `ejaj::prompts`
//! constant, so these contracts observe those public seams instead of any
//! private helper. The assertions pin the repository-first interview rules, the
//! question frontier, the handoff rules, the specification architecture and
//! testing policy, and the existing templates with known literal text.

use ejaj::prompts::{
    PLAN_SYSTEM_PROMPT, SPEC_KICKOFF_PROMPT, SPEC_SYSTEM_PROMPT, TICKETS_SYSTEM_PROMPT,
    TRIAGE_SYSTEM_PROMPT,
};

/// The opening sentence every existing plan-prompt consumer already relies on.
const OPENING_MARKER: &str =
    "Interview the user to align on core architectural and product direction.";

/// The plan-template headings the harness parses back into a plan, in order.
const PLAN_TEMPLATE_HEADINGS: &[&str] = &[
    "# <the goal in one line>",
    "## 1. Decisions",
    "## 2. Defaults Chosen",
    "## 3. Constraints",
    "## 4. Open Questions",
];

/// Assert every known literal instruction is present in the exported prompt.
fn assert_plan_prompt_contains(needles: &[&str]) {
    for needle in needles {
        assert!(
            PLAN_SYSTEM_PROMPT.contains(needle),
            "PLAN_SYSTEM_PROMPT must contain `{needle}`"
        );
    }
}

/// Assert every required plan-template heading appears exactly once and in the
/// order the plan harness requires.
fn assert_template_headings_in_order() {
    let mut previous_end = 0usize;
    for heading in PLAN_TEMPLATE_HEADINGS {
        let occurrences = PLAN_SYSTEM_PROMPT.matches(heading).count();
        assert_eq!(
            occurrences, 1,
            "PLAN_SYSTEM_PROMPT must render `{heading}` exactly once"
        );
        let start = PLAN_SYSTEM_PROMPT
            .find(heading)
            .unwrap_or_else(|| panic!("PLAN_SYSTEM_PROMPT must contain `{heading}`"));
        assert!(
            start >= previous_end,
            "PLAN_SYSTEM_PROMPT must order `{heading}` after the heading before it"
        );
        previous_end = start + heading.len();
    }
}

/// Core entry-point behavior: a code-related request starts with read-only
/// repository facts, the guidance files, the links they carry, the behaviour
/// that exists today, and a short summary before the first question.
#[test]
fn when_starting_given_code_related_request_then_plan_prompt_requires_repository_facts_before_questions()
 {
    assert_plan_prompt_contains(&[
        "For a code-related request",
        "complete read-only repository reconnaissance before you ask your first question",
        "read `AGENTS.md` and `README.md` when present",
        "follow the relevant repository links they contain",
        "check how the affected behaviour works today before proposing a design",
        "brief (1 to 3 sentence) pre-question summary",
        "restate the user's goal",
        "the relevant current behaviour you found in the repository",
        "any discovered constraint or simpler path",
    ]);
}

/// Edge-case table across the prerequisite frontier: a blocking question is
/// never silently dropped, one currently answerable critical question is
/// allowed, the round stays within the 1-3 bound, and a dependent question is
/// deferred until its prerequisite is settled.
#[test]
fn when_asking_given_unsettled_prerequisite_then_plan_prompt_defers_dependent_questions() {
    assert_plan_prompt_contains(&[
        "The **frontier** contains only unsettled decisions that block immediate work and that the user can answer now",
        "never leave a blocking question unasked merely because a question does not seem needed",
        "ask the one critical question when exactly one currently answerable blocking decision remains",
        "Keep each round short (1 to 3 critical questions maximum)",
        "Never ask a dependent question in the same round as the unsettled prerequisite it depends on",
        "defer that question until the prerequisite is settled",
    ]);
}

/// Contract and invariant check: a settled choice is written by its meaning,
/// automated testing is limited to executable source code, tests are never
/// requested for prose content, obsolete markdown or tracker assertions are
/// removed instead of refreshed, and the template headings stay ordered.
#[test]
fn when_handing_off_given_settled_plan_choices_then_prompt_requires_substantive_choices_and_source_only_tests()
 {
    assert_plan_prompt_contains(&[
        "State each settled choice directly by its meaning",
        "never refer to an interview option letter",
        "Automated tests apply only to executable source code",
        "Never request, plan, or record a test for documentation, markdown, or tracker text",
        "Remove obsolete tests that assert markdown or tracker text or checklist status rather than refreshing them",
    ]);
    assert_template_headings_in_order();
}

/// Contract and invariant check: before offering design options the plan
/// prompt checks whether the requested behaviour or a close equivalent already
/// exists, and when it does, the first round cites the existing module and its
/// path and asks whether to reuse, extend or replace it.
#[test]
fn when_proposing_given_existing_behavior_then_plan_prompt_requires_reuse_extend_or_replace() {
    assert_plan_prompt_contains(&[
        "Before you offer design options",
        "check whether the requested behaviour or a close equivalent already exists in the repository",
        "cite the existing module and its path",
        "ask whether to reuse it, extend it or replace it",
    ]);
}

/// Core entry-point behavior and malformed-input boundary: a request that
/// references a repository task, tracker, or proposal document is fast-tracked
/// only when reconnaissance confirms it against the live code. The documented
/// target module, modification approach, and constraints count as settled, and
/// the agent hands off with `finish_plan` in Turn 1 without asking `Q1`. A
/// missing or uninspectable fact, a contradiction, an unstated blocking
/// prerequisite, or task-selection ambiguity stays unsettled and becomes a
/// question instead of a handoff that depends on it.
#[test]
fn when_starting_given_preplanned_task_then_plan_prompt_treats_documented_choices_as_settled_and_hands_off()
 {
    assert_plan_prompt_contains(&[
        "For a code-related request",
        "complete that reconnaissance for every code-related request, including one that references no task document",
        "such as `TODO.md`, `.plan/`, or `.plans/`",
        "inspect that document and the target modules it names during reconnaissance",
        "treat the concrete design choices, target paths, and integration mode it specifies as decisions the user has already settled",
        "unless they contradict the live codebase",
        "never ask the user to re-confirm an integration choice that the request or the referenced plan already specifies and the codebase supports",
        "When the request or a referenced task, tracker, or proposal file already specifies the target module, how to extend or modify it, and the constraints to honour",
        "read-only reconnaissance finds no contradiction with the repository",
        "call `finish_plan` in that first turn without asking `Q1`",
        "a missing or uninspectable referenced fact",
        "a concrete mismatch with the live code",
        "an unstated blocking prerequisite",
        "uncertainty about which task is intended",
        "is not settled",
        "Ask the user about that blocker and wait",
        "ask whether to reuse it, extend it or replace it whenever the request or the referenced document has not already settled that choice",
        "The **frontier** contains only unsettled decisions that block immediate work and that the user can answer now",
        "A genuinely unresolved blocking decision is never silently treated as settled",
    ]);
}

/// Regression: the existing opening marker, focused-round mechanics, terminal
/// handoff, and ordered template headings remain intact.
#[test]
fn when_preserving_given_existing_plan_prompt_contract_then_opening_marker_and_template_remain() {
    assert!(
        PLAN_SYSTEM_PROMPT.starts_with(OPENING_MARKER),
        "PLAN_SYSTEM_PROMPT must keep its opening marker"
    );
    assert_plan_prompt_contains(&[
        "Work in focused **rounds**",
        "Wait for the user's response before proceeding.",
        "call the `finish_plan` tool immediately",
    ]);
    assert_template_headings_in_order();
}

/// Empty/zero boundary: with no blocking decision left, the prompt permits
/// completion without asking a question, and the exported constant is not
/// empty.
#[test]
fn when_completing_given_no_blocking_decision_then_plan_prompt_allows_no_question() {
    assert!(
        !PLAN_SYSTEM_PROMPT.is_empty(),
        "the exported plan prompt must not be empty"
    );
    assert_plan_prompt_contains(&[
        "The moment no genuinely blocking decision remains, call the `finish_plan` tool immediately",
        "do not summarize the plan first",
    ]);
}

/// Minimum valid boundary: with exactly one currently answerable blocker, one
/// critical question is a valid round.
#[test]
fn when_asking_given_one_answerable_blocker_then_plan_prompt_allows_one_critical_question() {
    assert_plan_prompt_contains(&[
        "ask the one critical question when exactly one currently answerable blocking decision remains",
        "1 to 3 critical questions maximum",
    ]);
}

/// Maximum limit boundary: a round never exceeds three critical questions.
#[test]
fn when_asking_given_maximum_round_then_plan_prompt_caps_questions_at_three() {
    assert_plan_prompt_contains(&["Keep each round short (1 to 3 critical questions maximum)"]);
    assert!(
        !PLAN_SYSTEM_PROMPT.contains("1 to 4 critical questions"),
        "PLAN_SYSTEM_PROMPT must not widen the round beyond three questions"
    );
}

/// Malformed/invalid payload boundary: the plan prompt is compile-time static
/// text, so there is no payload to validate; the existing marker and template
/// are still verified.
#[test]
fn when_preserving_given_static_plan_prompt_then_marker_and_template_are_verified() {
    assert!(
        PLAN_SYSTEM_PROMPT.starts_with(OPENING_MARKER),
        "the static plan prompt must keep its opening marker"
    );
    assert_template_headings_in_order();
}

/// Unauthorized/forbidden boundary: the plan prompt is static text that
/// implements no authorization boundary, so this contract only proves that no
/// access-control instruction is asserted as behavior.
#[test]
fn when_reviewing_given_prompt_without_access_control_then_no_authorization_behavior_is_asserted() {
    let prompt = PLAN_SYSTEM_PROMPT.to_ascii_lowercase();
    for forbidden in [
        "authorization header",
        "access control",
        "permission check",
        "authenticate",
    ] {
        assert!(
            !prompt.contains(forbidden),
            "the plan prompt must not define an authorization boundary: `{forbidden}`"
        );
    }
}

/// The opening sentence the specification pass already relies on.
const SPEC_OPENING_MARKER: &str =
    "Convert the upstream plan into a technical specification without prompting the user.";

/// The kickoff sentence the specification pass already relies on.
const SPEC_KICKOFF_MARKER: &str =
    "The user has confirmed the design; the plan is reproduced below.";

/// The specification-template headings the harness parses back into a
/// specification, in order. Section 7 and its subsections stay last.
const SPEC_TEMPLATE_HEADINGS: &[&str] = &[
    "# <the feature name in one line>",
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
    "### Operational Risks",
    "### Follow-Up Work",
];

/// Assert every known literal instruction is present in the exported
/// specification prompt.
fn assert_spec_prompt_contains(needles: &[&str]) {
    for needle in needles {
        assert!(
            SPEC_SYSTEM_PROMPT.contains(needle),
            "SPEC_SYSTEM_PROMPT must contain `{needle}`"
        );
    }
}

/// Assert the template block carries every required heading exactly once and
/// in the order the specification harness requires.
fn assert_spec_template_headings_in_order() {
    let start = SPEC_SYSTEM_PROMPT
        .find("The document must match this template exactly")
        .expect("SPEC_SYSTEM_PROMPT must introduce the template");
    let template = &SPEC_SYSTEM_PROMPT[start..];
    let end = template
        .find("Rules for the document:")
        .expect("SPEC_SYSTEM_PROMPT must end the template before its rules");
    let template = &template[..end];
    let mut previous_end = 0usize;
    for heading in SPEC_TEMPLATE_HEADINGS {
        let occurrences = template.matches(heading).count();
        assert_eq!(
            occurrences, 1,
            "SPEC_SYSTEM_PROMPT template must render `{heading}` exactly once"
        );
        let position = template
            .find(heading)
            .unwrap_or_else(|| panic!("SPEC_SYSTEM_PROMPT must contain `{heading}`"));
        assert!(
            position >= previous_end,
            "SPEC_SYSTEM_PROMPT must order `{heading}` after the heading before it"
        );
        previous_end = position + heading.len();
    }
}

/// Core entry-point behavior: a specification starts from repository guidance
/// and its links, and prefers deep modules with small public interfaces over
/// shallow wrappers.
#[test]
fn when_specifying_given_repository_context_then_prompt_requires_guidance_and_deep_modules() {
    assert_spec_prompt_contains(&[
        "Read `AGENTS.md` and `README.md` when present",
        "follow the relevant repository links they contain before deciding the design",
        "prefer deep modules with small public interfaces over shallow wrappers",
        "keep internal implementation details private",
        "avoid shallow pass-through wrappers",
        "Never write, build, test, prompt the user, or pause for an intermediate confirmation",
    ]);
}

/// Edge-case table across local collaborators: external I/O gets a testable
/// boundary, in-process and locally substitutable collaborators are exercised
/// directly, and doubles stay at non-deterministic external boundaries.
#[test]
fn when_testing_given_local_collaborators_then_prompt_requires_direct_exercise_and_external_only_doubles()
 {
    assert_spec_prompt_contains(&[
        "Require testable boundaries around external I/O",
        "inject external boundary dependencies",
        "Exercise in-process and locally substitutable collaborators directly with real collaborators or local stand-ins",
        "Use test doubles only at non-deterministic external boundaries",
        "network or RPC, the system clock, randomness, and the OS",
    ]);
}

/// Contract and invariant check: cross-module dependencies carry exact
/// signatures, internal collaborators and local persistence are never mocked,
/// testing covers executable source code only, and a documentation-only
/// specification carries exactly `None.` in all three testing subsections.
#[test]
fn when_defining_given_cross_module_dependencies_then_prompt_requires_exact_signatures() {
    assert_spec_prompt_contains(&[
        "Wherever producer and consumer modules depend on one another, including a single dependency",
        "state the exact function and type signatures",
        "including every parameter type and every return type",
        "never mock internal classes, services, or local persistence layers",
        "Scope testing decisions to executable source-code behavior only",
        "Testing decisions cover only executable source-code behavior",
        "A documentation-only specification that changes only documentation or human-facing content writes exactly `None.` under each of `### Test Seams`, `### Coverage Strategy`, and `### Prior Art`",
    ]);
}

/// Core entry-point behavior: a specification structures its stories as
/// independently verifiable vertical slices, and each acceptance criterion
/// stays inside one cohesive slice or module boundary instead of combining
/// requirements that are gated separately.
#[test]
fn when_specifying_given_independently_verifiable_stories_then_criteria_follow_vertical_slices() {
    assert_spec_prompt_contains(&[
        "Structure the user stories as independently verifiable vertical slices",
        "each story delivers one end-to-end slice of user-facing behaviour",
        "implemented and verified on its own",
        "never make a story's verification depend on a later story",
        "Keep each acceptance criterion scoped to one cohesive slice or module boundary",
        "rather than combining separately gated requirements into one criterion",
    ]);
}

/// Contract and invariant check: when documentation or tracker updates
/// accompany source changes, the specification names their exact target file
/// paths in `### Module Architecture`, and documentation, `README.md`, `TODO.md`
/// and `.plan/` or `.plans/` tracker files are examples rather than substitute
/// paths.
#[test]
fn when_specifying_given_accompanying_documentation_updates_then_module_architecture_names_exact_paths()
 {
    assert_spec_prompt_contains(&[
        "When documentation or tracker changes accompany the source changes",
        "name the exact target file paths in `### Module Architecture`",
        "documentation, `README.md`, `TODO.md`, and `.plan/` or `.plans/` tracker files",
        "are examples of the kinds of files to name, never substitutes for the exact path",
    ]);
}

/// Regression: the existing system opening marker, kickoff marker, workflow
/// handoff, template rules, and every required heading -- including section 7
/// and its subsections -- remain intact and ordered.
#[test]
fn when_preserving_given_specification_prompt_contract_then_kickoff_and_template_remain() {
    assert!(
        SPEC_SYSTEM_PROMPT.starts_with(SPEC_OPENING_MARKER),
        "SPEC_SYSTEM_PROMPT must keep its opening marker"
    );
    assert!(
        SPEC_KICKOFF_PROMPT.starts_with(SPEC_KICKOFF_MARKER),
        "SPEC_KICKOFF_PROMPT must keep its kickoff marker"
    );
    assert_spec_prompt_contains(&[
        "call `finish_spec` with the complete specification as a markdown document",
        "The document must match this template exactly",
        "Rules for the document:",
        "Use only the `##` and `###` headings above, once each and in this order",
        "Sections 1 to 6 and the `###` subsections under sections 4 and 5 are required",
        "Section 7 and its two `###` subsections are optional",
    ]);
    assert_spec_template_headings_in_order();
}

/// Empty/zero boundary: a documentation-only specification writes exactly
/// `None.` under each testing subsection, and the exported constants are not
/// empty.
#[test]
fn when_reviewing_given_documentation_only_specification_then_prompt_requires_none_for_testing_sections()
 {
    assert!(
        !SPEC_SYSTEM_PROMPT.is_empty(),
        "the exported specification prompt must not be empty"
    );
    assert!(
        !SPEC_KICKOFF_PROMPT.is_empty(),
        "the exported specification kickoff must not be empty"
    );
    assert_spec_prompt_contains(&[
        "A documentation-only specification that changes only documentation or human-facing content writes exactly `None.` under each of `### Test Seams`, `### Coverage Strategy`, and `### Prior Art`",
    ]);
}

/// Minimum valid boundary: a single cross-module dependency already requires
/// complete signatures with every parameter type and return type.
#[test]
fn when_defining_given_one_cross_module_dependency_then_prompt_requires_complete_signatures() {
    assert_spec_prompt_contains(&[
        "including a single dependency",
        "including every parameter type and every return type",
    ]);
}

/// Maximum limit boundary: the specification prompt declares no numeric limit
/// or upper boundary on the work it describes; the seam guidance stays a
/// preference rather than a hard cap.
#[test]
fn when_reviewing_given_unbounded_specification_prompt_then_no_numeric_limit_is_asserted() {
    let prompt = SPEC_SYSTEM_PROMPT.to_ascii_lowercase();
    for forbidden in ["at most ", "no more than ", "maximum of ", "upper limit"] {
        assert!(
            !prompt.contains(forbidden),
            "SPEC_SYSTEM_PROMPT must not declare a numeric limit: `{forbidden}`"
        );
    }
    assert_spec_prompt_contains(&["the ideal number is one"]);
}

/// Malformed/invalid payload boundary: the specification prompt is compile-time
/// static text, so there is no payload to validate; the kickoff marker and
/// ordered template are still verified.
#[test]
fn when_preserving_given_static_specification_prompt_then_kickoff_marker_is_verified() {
    assert!(
        SPEC_KICKOFF_PROMPT.starts_with(SPEC_KICKOFF_MARKER),
        "the static specification kickoff must keep its opening marker"
    );
    for needle in [
        "Turn it into one complete",
        "technical specification and hand it over with `finish_spec`",
        "Inspect the codebase read-only as needed; do not implement anything.",
    ] {
        assert!(
            SPEC_KICKOFF_PROMPT.contains(needle),
            "SPEC_KICKOFF_PROMPT must contain `{needle}`"
        );
    }
    assert_spec_template_headings_in_order();
}

/// Unauthorized/forbidden boundary: the specification prompts are static text
/// that implement no authorization boundary, so this contract only proves that
/// no access-control instruction is asserted as behavior.
#[test]
fn when_reviewing_given_specification_prompt_without_access_control_then_no_authorization_behavior_is_asserted()
 {
    for prompt in [SPEC_SYSTEM_PROMPT, SPEC_KICKOFF_PROMPT] {
        let prompt = prompt.to_ascii_lowercase();
        for forbidden in [
            "authorization header",
            "access control",
            "permission check",
            "authenticate",
        ] {
            assert!(
                !prompt.contains(forbidden),
                "the specification prompts must not define an authorization boundary: `{forbidden}`"
            );
        }
    }
}

/// The exact tracker-folding instruction the ticket-authoring pass already
/// relies on; the authorization wording must qualify it without rewriting it.
const TICKETS_TRACKER_FOLDING_NEEDLE: &str = "Fold `README.md`, `TODO.md`, documentation, and `.plan/`/`.plans/` tracker updates into the executable-behavior ticket whose deliverable needs them; never create standalone documentation or tracker tickets for those accompanying updates.";

/// Return Workflow Step 2 of the exported ticket prompt, the step that slices
/// the specification and places accompanying work.
fn tickets_step_two() -> &'static str {
    let start = TICKETS_SYSTEM_PROMPT
        .find("2. **Slice the specification into vertical slices:**")
        .expect("TICKETS_SYSTEM_PROMPT must keep its specification-slicing step");
    let end = TICKETS_SYSTEM_PROMPT
        .find("3. **Sequence a wide mechanical change as expand, migrate, contract:**")
        .expect("TICKETS_SYSTEM_PROMPT must keep its sequencing step");
    &TICKETS_SYSTEM_PROMPT[start..end]
}

/// Core entry-point behavior at the unauthorized/forbidden boundary:
/// accompanying documentation and tracker work is folded only when the
/// specification itself calls for it, while omitted work is never invented.
#[test]
fn when_authorizing_accompanying_work_given_specification_omits_updates_then_ticket_prompt_prohibits_invention()
 {
    let step_two = tickets_step_two();
    for needle in [
        TICKETS_TRACKER_FOLDING_NEEDLE,
        "Only fold documentation, `README.md`, `TODO.md`, or `.plan/`/`.plans/` tracker updates into a ticket when the specification itself calls for updating them",
        "do not invent tracker or documentation tasks that the specification omits",
        "never create standalone documentation or tracker tickets",
    ] {
        assert!(
            step_two.contains(needle),
            "Step 2 of TICKETS_SYSTEM_PROMPT must contain `{needle}`"
        );
    }
}

/// The eleven ticket fields the handoff schema must keep, in their required
/// order. The schema is the contract the harness renders every ticket from.
const TICKETS_FIELDS_IN_ORDER: [&str; 11] = [
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

/// Core entry-point behavior: one specification story implemented by several
/// tickets hands each ticket only the verbatim criteria its own changes make
/// true, so the ticket set still covers the whole story while no ticket claims
/// out-of-scope behaviour or repeats a criterion.
#[test]
fn when_assigning_criteria_given_shared_story_spans_tickets_then_each_ticket_carries_only_its_verbatim_scope()
 {
    for needle in [
        "copied verbatim from the specification story it implements",
        "without paraphrasing, shortening, rewording, or a leading checkbox marker",
        "When a specification story spans multiple tickets, copy into each ticket only the verbatim criteria from that story",
        "that this ticket's own `scope_of_changes` and `verification_gate` make true",
        "the union across the ticket set covers every criterion of the story",
        "Never copy a criterion for behaviour listed in this ticket's `out_of_scope`",
        "never carry duplicate or altered criteria from an earlier attempt",
    ] {
        assert!(
            TICKETS_SYSTEM_PROMPT.contains(needle),
            "TICKETS_SYSTEM_PROMPT must contain `{needle}`"
        );
    }
}

/// Contract and invariant check: adding the shared-story criteria rule leaves
/// the eleven handoff fields in their existing order and keeps
/// `expand, migrate, contract` stated by its single sequencing step.
#[test]
fn when_preserving_prompt_contracts_given_shared_story_guidance_then_fields_and_sequence_remain_stable()
 {
    let schema_start = TICKETS_SYSTEM_PROMPT
        .find("with exactly these fields:")
        .expect("TICKETS_SYSTEM_PROMPT must keep its handoff schema");
    let schema_end = TICKETS_SYSTEM_PROMPT
        .find("The `verification_gate` must demonstrate")
        .expect("TICKETS_SYSTEM_PROMPT must keep the verification-gate guidance after it");
    let fields: Vec<&str> = TICKETS_SYSTEM_PROMPT[schema_start..schema_end]
        .lines()
        .filter_map(|line| line.strip_prefix("- `"))
        .filter_map(|rest| rest.split_once("`: ").map(|(name, _)| name))
        .collect();
    assert_eq!(
        fields, TICKETS_FIELDS_IN_ORDER,
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

/// The four exact Step 2 strings every existing triage-prompt consumer already
/// relies on; narrowing the Agent Brief criteria must not rewrite them.
const TRIAGE_LEGACY_STEP_TWO_NEEDLES: &[&str] = &[
    "A disagreement among scope, tests, gate and specification is a finding",
    "inconsistency in the ticket",
    "The notes of a classification are evidence",
    "restatement of the title or the objective is not evidence",
];

/// Return Workflow Step 2 of the exported triage prompt, the step that audits
/// every draft ticket against the draft set, the specification and the code.
fn triage_step_two() -> &'static str {
    let start = TRIAGE_SYSTEM_PROMPT
        .find("2. **Audit every draft ticket against what you found:**")
        .expect("TRIAGE_SYSTEM_PROMPT must keep its ticket-audit step");
    let end = TRIAGE_SYSTEM_PROMPT
        .find("3. **Classify each draft ticket**")
        .expect("TRIAGE_SYSTEM_PROMPT must keep its classification step");
    &TRIAGE_SYSTEM_PROMPT[start..end]
}

/// Assert Step 2 of the exported triage prompt carries every known literal.
fn assert_triage_step_two_contains(needles: &[&str]) {
    let step_two = triage_step_two();
    for needle in needles {
        assert!(
            step_two.contains(needle),
            "Step 2 of TRIAGE_SYSTEM_PROMPT must contain `{needle}`"
        );
    }
}

/// Core entry-point behavior: a ticket that only carries a sibling ticket's
/// verbatim criterion from a shared specification story stays ready for an
/// agent, with its Agent Brief criteria narrowed to its own self-contained
/// slice and the audited overlap recorded as evidence.
#[test]
fn when_triaging_given_shared_story_sibling_criterion_then_narrows_ready_agent_brief() {
    assert_triage_step_two_contains(&[
        "a specification story is shared across multiple draft tickets",
        "a ticket carries a sibling ticket's verbatim criterion from that shared story",
        "that ticket's own `scope_of_changes`, `test_expectations`, and `verification_gate` define a self-contained slice of the specification",
        "narrow that ticket's `acceptance_criteria` in its Agent Brief to the criteria its own scope implements",
        "record in the classification notes the audited overlap and that narrowing as evidence",
        "keep the classification `ready-for-agent`",
        "instead of marking it `needs-info` over that overlap",
    ]);
    for legacy in TRIAGE_LEGACY_STEP_TWO_NEEDLES {
        assert!(
            triage_step_two().contains(legacy),
            "Step 2 of TRIAGE_SYSTEM_PROMPT must keep the legacy wording `{legacy}`"
        );
    }
}

/// Edge-case table across the narrowing boundary: the ticket-local slice
/// conditions are checked one by one, a qualifying multi-ticket story narrows
/// only its own criteria, and every unrelated inconsistency stays a finding.
#[test]
fn when_triaging_given_shared_story_boundary_cases_then_narrows_only_self_contained_slices() {
    let step_two = triage_step_two();
    let cases: &[(&str, &[&str])] = &[
        (
            "shared-story premise absent",
            &["when the story is not shared", "no criteria are narrowed"],
        ),
        (
            "sibling verbatim criterion absent",
            &["the criterion is the ticket's own rather than a sibling's"],
        ),
        (
            "ticket-local scope, tests or gate do not agree",
            &[
                "the ticket's own `scope_of_changes`, `test_expectations` and `verification_gate` do not define a self-contained slice",
                "no criteria are narrowed",
            ],
        ),
        (
            "qualifying story shared across several tickets and criteria",
            &[
                "shared across multiple draft tickets",
                "a sibling ticket's verbatim criterion from that shared story",
                "narrow that ticket's `acceptance_criteria` in its Agent Brief",
                "keep the classification `ready-for-agent`",
            ],
        ),
        (
            "unrelated inconsistency",
            &[
                "A disagreement among scope, tests, gate and specification is a finding",
                "Every other ticket, repository and specification inconsistency remains a finding",
                "classified by the existing rules below",
            ],
        ),
    ];
    for (case, needles) in cases {
        for needle in *needles {
            assert!(
                step_two.contains(needle),
                "Step 2 of TRIAGE_SYSTEM_PROMPT must keep `{needle}` for the {case} case"
            );
        }
    }
    assert_eq!(
        step_two
            .matches("narrow that ticket's `acceptance_criteria`")
            .count(),
        1,
        "only the qualifying shared-story overlap may direct a narrowing of Agent Brief criteria"
    );
    for unconditional in [
        "Always narrow",
        "always narrow",
        "narrow every ticket's `acceptance_criteria`",
    ] {
        assert!(
            !step_two.contains(unconditional),
            "the narrowing directive must stay conditional, never `{unconditional}`"
        );
    }
}
