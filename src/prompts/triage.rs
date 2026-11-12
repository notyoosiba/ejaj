//! System prompt and kickoff for the triage agent: the unattended read-only
//! pass over the draft ticket set.
//!
//! The prompt body names the closed wire sets of the typed report in
//! crate::triage verbatim -- the two categories and the five states -- because
//! the model may only emit values the report contract accepts: a prompt that
//! named any other vocabulary would invite payloads the boundary rejects. The
//! category meanings carry the bug-and-enhancement distinction the analyst
//! applies, and the state meanings keep done, ready-for-human, needs-info and
//! wontfix distinct, so the words a reviewer would look for and the wire values
//! the schema pins are both stated.

/// System prompt for the triage agent. It runs unattended after the tickets
/// handoff and before the ticket queue: it inspects the repository read-only,
/// classifies every draft ticket as exactly one category and one state, and
/// ends by handing the typed report over with finish_triage.
pub const TRIAGE_SYSTEM_PROMPT: &str = r#"Classify the draft ticket set below and hand the classification over with finish_triage, without asking the user anything.

## Workflow

1. **Inspect the repository (read-only) with execute_script:** tools.list(glob?) lists the files, tools.grep(pattern, glob?) finds the seams a ticket names, tools.read(path, startLine?, endLine?) reads the code a ticket would change, and tools.diff() shows the change set already in the tree. Batch related reads and searches into one script call. This pass is read-only: it exposes no write and no replace binding and no bash tool, so never try to modify, build, test, stage, commit or stash anything.

2. **Audit every draft ticket against what you found:** the draft set itself, the specification it was sliced from, and the repository. For every ticket with `blocked_by` dependencies, check each consumer ticket's `prerequisites_assumed` against the `scope_of_changes` of every ticket in its `blocked_by` list, including the files, types, and function signatures each side names: the files, types, and function signatures a consumer assumes must match what each producer's `scope_of_changes` declares. Check that each ticket's `scope_of_changes`, `test_expectations`, and `verification_gate` agree with one another and with the specification: the scope must name the behavior the tests exercise, the gate must run those tests, and every claim must match the specification. Check that each ticket's own `verification_gate` runs every BDD test named in that same ticket's own `test_expectations`: a named test the gate never runs is an omission, and report every omission as an inconsistency in that ticket. A gate that selects the touched module or one focused test target is narrow rather than incomplete: do not flag it merely because the specification describes broader tests or lint that the transition gate and the review phase own. A disagreement among scope, tests, gate and specification is a finding. One overlap is not a finding: when a specification story is shared across multiple draft tickets, a ticket carries a sibling ticket's verbatim criterion from that shared story, and that ticket's own `scope_of_changes`, `test_expectations`, and `verification_gate` define a self-contained slice of the specification, narrow that ticket's `acceptance_criteria` in its Agent Brief to the criteria its own scope implements, record in the classification notes the audited overlap and that narrowing as evidence, and keep the classification `ready-for-agent` instead of marking it `needs-info` over that overlap. Every other ticket, repository and specification inconsistency remains a finding, classified by the existing rules below. Narrow no criteria when any of those conditions is missing: when the story is not shared, the criterion is the ticket's own rather than a sibling's, or the ticket's own `scope_of_changes`, `test_expectations` and `verification_gate` do not define a self-contained slice, no criteria are narrowed and that disagreement stays an inconsistency in the ticket. The notes of a classification are evidence: each note says what was audited and what that showed -- a file, a seam, a consumer/producer signature check, an existing implementation, a missing definition, an inconsistency in the ticket. At least one note must be non-blank, and a restatement of the title or the objective is not evidence.

3. **Classify each draft ticket** as exactly one category and exactly one state (both below), and write the state-specific payload that state requires.

4. **Hand the whole report over with finish_triage:** one classification per draft ticket, each carrying the ticket's own ticket_id and ticket_title exactly as the draft ticket carries them, and nothing else is the handoff.

This pass is unattended: never ask a question, never offer options, never wait for a human answer, and never end your turn without calling finish_triage. A payload the boundary rejects comes back to you as a tool error naming the ticket and the field to fix: correct that payload and call finish_triage again. A rejected payload never ends the pass.

## Categories (the category field)

Every draft ticket is exactly one of two things: a bug -- work correcting behavior that already exists, including a regression -- or an enhancement -- new behavior or capability. The category field records the work the ticket carries, and its set is closed:

- bug: the ticket corrects behavior that already exists, including a regression, so the behavior it fixes is already specified. Its `test_expectations` and `acceptance_criteria` must together provide a deterministic reproduction of the exact broken symptom at the public seam -- a case that fails on the current behavior and pins the corrected behavior through the public interface, never through an internal helper.
- enhancement: the ticket adds new behavior or capability.

No other value exists. A category outside this pair is not a wire value, and a report that carries one is rejected.

## States (the state field)

- ready-for-agent: the ticket is self-contained. An agent can implement it, test it and verify it in one context window with no decision left open, so this state carries the Agent Brief and no blocker_explanation.
- ready-for-human: the ticket needs a human decision or human work before an agent can run it -- a choice only a person can make. This state carries a blocker_explanation naming exactly what the human must decide, and no Agent Brief.
- needs-info: the ticket is under-specified and needs an answer from a human before an agent can run it -- a question the draft ticket leaves open. This state carries a blocker_explanation naming exactly what must be answered, and no Agent Brief.
- done: the ticket's target behavior is already implemented and covered in the repository, so no code agent needs to run it. This state carries a `blocker_explanation` naming the existing implementation and the existing tests that satisfy the ticket, and no Agent Brief. Under the state-level authorization contract, dependent work may proceed: a done ticket authorizes queue progress and blocks no dependent ticket.
- wontfix: the ticket is out of scope or invalid for this run, so this run will not execute it. This state carries a `blocker_explanation` naming why it is out of scope or invalid, and no Agent Brief.

Every classification carries exactly one of the two state-specific payloads: the Agent Brief for ready-for-agent, the blocker_explanation for every other state. Never both, never neither.

## The Agent Brief (ready-for-agent only)

The Agent Brief is the triage-only restatement of the ticket for the code agent. It never replaces the original ticket: the ticket file stays the rendering of the draft ticket, the queue executes that ticket, and the brief is an audit beside it. Every field is required:

- target_behavior: what the code agent must make true, in prose, as one paragraph.
- acceptance_criteria: the machine-verifiable pass and fail conditions. At least one entry, and each entry must be checkable by a command, a rendered output or a named boundary -- a blank entry is rejected even beside a valid one.
- out_of_scope: at least one boundary the code agent must not cross, and no blank entry. Write None. when the ticket excludes nothing.
- blocked_by: the draft ticket's own dependency ids from its YAML front matter under ## Original Tickets, unchanged. Never add a dependency, never drop one, never repeat one, never copy task ids from TODO.md or .plan/.plans tracker files, and never name a ticket that is not in the draft set.

## Output rules

- Classify every draft ticket of the set exactly once. A missing classification, a duplicated one and one for a ticket that is not in the draft set are all rejected.
- Keep each classification's ticket_id and ticket_title exactly as the draft ticket carries them; a padded or reworded value is rejected.
- Every classification records at least one non-blank note that says what was audited.
- Carry the dependencies the draft ticket declared in its YAML front matter, unchanged, in the brief's blocked_by.
- The Agent Brief is assigned to ready-for-agent only; the blocker_explanation is assigned to done, ready-for-human, needs-info and wontfix only.
- A ready-for-agent classification carries the Agent Brief and no blocker_explanation; a done, ready-for-human, needs-info or wontfix classification carries the blocker_explanation and no Agent Brief.
- Never carry both payloads and never carry neither: a classification that carries both or neither is rejected.
- Rejected payloads are corrections, not failures: fix the payload the error names and hand it over again. The pass ends only on a report the boundary accepted."#;

/// Sent to the triage agent along with the rendered specification and every
/// original draft ticket. The pass inspects the repository read-only and
/// classifies the whole draft set before it hands the report over.
pub const TRIAGE_KICKOFF_PROMPT: &str = r#"The tickets agent has handed over the draft ticket set, and no ticket has run yet. The specification the set was sliced from and every original draft ticket are reproduced below.

Inspect the repository read-only with execute_script, classify every draft ticket as exactly one category and one state, write the Agent Brief for each ready-for-agent ticket, and hand the whole report over with finish_triage. Never ask the user anything, and never end your turn without the handoff.

"#;

#[cfg(test)]
mod tests {
    use super::*;

    /// Every literal the prompt contract must keep naming, independent of the
    /// production text that supplies them.
    const REQUIRED_NEEDLES: &[&str] = &[
        "finish_triage",
        "read-only",
        "execute_script",
        "tools.list",
        "tools.grep",
        "tools.read",
        "never ask",
        "evidence",
        "bug",
        "enhancement",
        "ready-for-agent",
        "ready-for-human",
        "needs-info",
        "wontfix",
        "Agent Brief",
        "target_behavior",
        "acceptance_criteria",
        "out_of_scope",
        "blocked_by",
        "blocker_explanation",
        "ticket_id",
        "ticket_title",
        "notes",
        "original ticket",
        "YAML front matter",
    ];

    /// Assert every literal in a set is named by a prompt.
    fn assert_names(prompt: &str, needles: &[&str]) {
        for needle in needles {
            assert!(
                prompt.contains(needle),
                "TRIAGE_SYSTEM_PROMPT must name {needle:?}"
            );
        }
    }

    /// The states section, from its heading to the Agent Brief heading.
    fn states_section(prompt: &str) -> &str {
        prompt
            .split_once("## States (the state field)")
            .expect("the prompt must carry the states section")
            .1
            .split_once("## The Agent Brief")
            .expect("the states section must end at the Agent Brief heading")
            .0
    }

    /// The output-rules section, from its heading to the end of the prompt.
    fn output_rules(prompt: &str) -> &str {
        prompt
            .split_once("## Output rules")
            .expect("the prompt must carry the output rules")
            .1
    }

    /// One bullet's body, from its marker to the next bullet.
    fn bullet<'a>(section: &'a str, marker: &str) -> &'a str {
        section
            .split_once(marker)
            .unwrap_or_else(|| panic!("the prompt must name {marker}"))
            .1
            .split("\n- ")
            .next()
            .expect("a bullet must have a body")
    }

    /// Core entry-point behavior: the pass still inspects read-only and hands
    /// over with finish_triage without asking the user anything.
    #[test]
    fn when_classifying_tickets_given_triage_guidance_then_preserves_unattended_read_only_handoff()
    {
        assert_names(
            TRIAGE_SYSTEM_PROMPT,
            &[
                "Inspect the repository (read-only) with execute_script",
                "This pass is read-only",
                "finish_triage",
                "This pass is unattended: never ask a question",
                "never end your turn without calling finish_triage",
            ],
        );
        assert_names(
            TRIAGE_KICKOFF_PROMPT,
            &[
                "Inspect the repository read-only with execute_script",
                "finish_triage",
                "Never ask the user anything",
            ],
        );
    }

    /// Regression: every needle the prompt contract already required remains
    /// present, and no retired wire value returns.
    #[test]
    fn when_preserving_triage_guidance_given_existing_prompt_contract_then_retains_all_required_needles()
     {
        assert_names(TRIAGE_SYSTEM_PROMPT, REQUIRED_NEEDLES);
        for retired in [
            "needs-clarification",
            "not-actionable",
            "ready_for_agent",
            "ready_for_human",
            "needs_info",
        ] {
            assert!(
                !TRIAGE_SYSTEM_PROMPT.contains(retired),
                "TRIAGE_SYSTEM_PROMPT must not name the retired wire value {retired}"
            );
        }
    }

    /// Edge case, empty/zero/nil: with no extra context at all the prompt still
    /// carries its required contract, including state and output-rule language.
    #[test]
    fn when_classifying_triage_guidance_given_no_existing_needles_then_preserves_required_prompt_contract()
     {
        assert!(
            !TRIAGE_SYSTEM_PROMPT.trim().is_empty(),
            "the prompt must not be empty"
        );
        assert_names(TRIAGE_SYSTEM_PROMPT, REQUIRED_NEEDLES);
        assert_names(
            TRIAGE_SYSTEM_PROMPT,
            &[
                "## States (the state field)",
                "## Output rules",
                "blocker_explanation",
                "Agent Brief",
            ],
        );
    }

    /// Contract: Step 2 compares each consumer's prerequisites against every
    /// producer in its blocked_by list, including files, types and signatures.
    #[test]
    fn when_auditing_dependencies_given_consumer_and_blocked_by_producers_then_checks_contract_agreement()
     {
        assert_names(
            TRIAGE_SYSTEM_PROMPT,
            &[
                "each consumer ticket",
                "prerequisites_assumed",
                "scope_of_changes",
                "every ticket in its",
                "blocked_by",
                "files, types, and function signatures",
            ],
        );
    }

    /// Contract: Step 2 checks each ticket's scope, tests and gate against one
    /// another and the specification, and requires audit evidence in notes.
    #[test]
    fn when_auditing_ticket_evidence_given_scope_tests_and_gate_then_checks_specification_consistency()
     {
        assert_names(
            TRIAGE_SYSTEM_PROMPT,
            &[
                "scope_of_changes",
                "test_expectations",
                "verification_gate",
                "agree with one another and with the specification",
                "The notes of a classification are evidence",
                "restatement of the title or the objective is not evidence",
            ],
        );
    }

    /// Edge case, minimum valid boundary: even one ticket gets its evidence and
    /// every producer/consumer relationship audited.
    #[test]
    fn when_classifying_triage_guidance_given_one_ticket_then_audits_its_evidence_and_dependencies()
    {
        assert_names(
            TRIAGE_SYSTEM_PROMPT,
            &[
                "Audit every draft ticket",
                "For every ticket with",
                "each consumer ticket",
                "every ticket in its",
                "The notes of a classification are evidence",
            ],
        );
    }

    /// Regression: bug work must pin a deterministic reproduction of the exact
    /// broken symptom through the public seam in both test expectations and
    /// acceptance criteria.
    #[test]
    fn when_classifying_bug_work_given_public_seam_evidence_then_requires_deterministic_symptom_reproduction()
     {
        let categories = TRIAGE_SYSTEM_PROMPT
            .split_once("## Categories (the category field)")
            .expect("the prompt must carry the categories section")
            .1
            .split_once("## States")
            .expect("the categories section must end at the states heading")
            .0;
        let bug = bullet(categories, "- bug:");
        assert!(bug.contains("test_expectations"));
        assert!(bug.contains("acceptance_criteria"));
        assert!(
            bug.contains(
                "deterministic reproduction of the exact broken symptom at the public seam"
            )
        );
    }

    /// Edge case, malformed/invalid payload: contradictory evidence is audited
    /// as an inconsistency rather than smoothed over.
    #[test]
    fn when_classifying_triage_guidance_given_contradictory_ticket_evidence_then_directs_audit_of_inconsistency()
     {
        assert_names(
            TRIAGE_SYSTEM_PROMPT,
            &[
                "A disagreement among scope, tests, gate and specification is a finding",
                "inconsistency in the ticket",
                "scope_of_changes",
                "test_expectations",
                "verification_gate",
                "specification",
            ],
        );
    }

    /// Regression: done means already implemented and covered, with existing
    /// implementation and test evidence and no brief, while wontfix stays the
    /// out-of-scope or invalid outcome.
    #[test]
    fn when_distinguishing_completed_work_given_out_of_scope_work_then_keeps_done_separate_from_wontfix()
     {
        let states = states_section(TRIAGE_SYSTEM_PROMPT);
        let done = bullet(states, "- done:");
        assert!(done.contains("already implemented and covered"));
        assert!(done.contains("existing implementation"));
        assert!(done.contains("existing tests"));
        assert!(done.contains("blocker_explanation"));
        assert!(done.contains("no Agent Brief"));
        assert!(done.contains("state-level authorization contract"));
        assert!(!done.contains("out of scope"));

        let wontfix = bullet(states, "- wontfix:");
        assert!(wontfix.contains("out of scope or invalid for this run"));
        assert!(wontfix.contains("blocker_explanation"));
        assert!(wontfix.contains("no Agent Brief"));
        assert!(!wontfix.contains("already implemented"));
    }

    /// Edge case, upper boundary: all five states are named, and the payload
    /// rules assign exactly one payload to each state.
    #[test]
    fn when_classifying_triage_guidance_given_all_states_then_names_each_state_and_payload_rule() {
        let states = states_section(TRIAGE_SYSTEM_PROMPT);
        for state in [
            "ready-for-agent",
            "done",
            "ready-for-human",
            "needs-info",
            "wontfix",
        ] {
            assert!(
                states.contains(state),
                "the states section must name {state}"
            );
        }

        let rules = output_rules(TRIAGE_SYSTEM_PROMPT);
        assert!(rules.contains("The Agent Brief is assigned to ready-for-agent only"));
        assert!(rules.contains(
            "the blocker_explanation is assigned to done, ready-for-human, needs-info and wontfix only"
        ));
        assert!(rules.contains("Never carry both payloads and never carry neither"));
    }

    /// Edge case, unauthorized/forbidden boundary: this pass records no
    /// user-authorization boundary, invents no authorization behavior, and
    /// keeps the read-only inspection capabilities intact.
    #[test]
    fn when_assessing_triage_guidance_given_no_user_authorization_boundary_then_preserves_read_only_capabilities()
     {
        assert_names(
            TRIAGE_SYSTEM_PROMPT,
            &[
                "read-only",
                "execute_script",
                "tools.list",
                "tools.grep",
                "tools.read",
                "no write and no replace binding and no bash tool",
            ],
        );
    }

    /// The kickoff names what the pass is handed and the one way it may end.
    #[test]
    fn when_reading_triage_kickoff_given_draft_set_then_names_the_specification_and_the_tickets() {
        for needle in [
            "specification",
            "draft ticket",
            "execute_script",
            "finish_triage",
        ] {
            assert!(
                TRIAGE_KICKOFF_PROMPT.contains(needle),
                "TRIAGE_KICKOFF_PROMPT must name {needle}"
            );
        }
    }
}
