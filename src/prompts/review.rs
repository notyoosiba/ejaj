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
   - `keeper_coverage`: verify coverage across the 4 Keeper Categories:
     1. Core Entry-Point Behaviors (real caller journeys via CLI, API, or public interface).
     2. Edge-Case Tables (consolidated parameterized table).
     3. Contract & Invariant Checks (schemas, error envelopes, consumer invariants).
     4. Regression Tests (applicable when the spec targets a bug fix).
   - `sediment_and_coupling`: confirm zero internal mocks (internal classes, services, and local \
persistence layers are never mocked; test doubles are restricted strictly to non-deterministic \
external boundaries), no tautologies, and no private state introspection.
   - `inferred_assumptions`: explicit log of inferred assumptions (boundaries, status codes, \
default schemas) verified against the implementation.
   - `boundary_matrix`: evaluate the Mandatory 5-Dimension Boundary Matrix across the test suite:
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
`fixes` tickets that resolve them. Every finding must be covered in substance by at least one fix \
ticket, and every fix ticket must satisfy all ticket rules:
     - `id`: a new unique ticket id (`[A-Za-z0-9._-]+`, e.g. `r1-1-<slug>`) that does not collide \
with any ticket id already executed in this run.
     - `blocked_by` / `blocks`: may only reference other fix tickets emitted in this same `fixes` list.
     - `title`, `objective`, `prerequisites_assumed`, `out_of_scope`, `scope_of_changes`, \
`acceptance_criteria` (at least one non-blank criterion), `test_expectations` (at least one \
non-blank entry, and every entry must name a `when_<action>_given_<context>_then_<outcome>` test \
identifier), and `verification_gate` (exact command exiting 0).

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
    use super::*;

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
}
