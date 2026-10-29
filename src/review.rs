//! The review report as a typed document.
//!
//! A review pass runs once after every resolved ticket-queue round. The review
//! agent audits the repository's tests and implementation against the technical
//! specification, the executed tickets, and the test-design blueprint, then
//! hands over a [`ReviewReport`] via `finish_review`. When [`ReviewReport::fixes`]
//! is empty the review passes and the pipeline proceeds to the change summary;
//! when [`ReviewReport::fixes`] is non-empty the pipeline writes the fix tickets
//! and runs another queue round (up to the pipeline's review-round bound).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use crate::specification::NONE;
use crate::tickets::{Ticket, TicketSet, is_id_char};

/// The document title of the rendered `review.md` artifact.
pub const REVIEW_TITLE: &str = "# Review report";
/// The subsection heading for the pre-flight audit scorecard.
pub const SCORECARD_HEADING: &str = "### Pre-Flight Audit Scorecard";
/// The subsection heading for the findings list.
pub const FINDINGS_HEADING: &str = "### Findings";
/// The subsection heading for the fix tickets list.
pub const FIX_TICKETS_HEADING: &str = "### Fix Tickets";

/// Pre-flight audit scorecard for one review round, covering the four keeper
/// categories, the sediment and coupling audit, inferred assumptions, the
/// five-dimension boundary matrix, and code/scope alignment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ReviewScorecard {
    /// Verification of the 4 keeper categories: Core Entry-Point Behaviors,
    /// Edge-Case Tables, Contract & Invariant Checks, and Regression Tests.
    pub keeper_coverage: String,
    /// Sediment and coupling check confirming zero internal mocks, no
    /// tautologies, and no private state introspection.
    pub sediment_and_coupling: String,
    /// Explicit log of inferred assumptions (boundaries, status codes, default
    /// schemas).
    pub inferred_assumptions: String,
    /// Evaluation of the mandatory 5-dimension boundary matrix: Empty/Zero/Nil,
    /// Minimum valid boundary, Maximum limit/Upper boundary, Malformed/Invalid
    /// payload, and Boundary unauthorized/Forbidden access.
    pub boundary_matrix: String,
    /// Audit of the implementation against ticket scope, out-of-scope
    /// boundaries, verification gate realism, and the specification.
    pub code_and_scope: String,
}

/// One defect uncovered during a review pass that requires a fix ticket.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ReviewFinding {
    /// Unique identifier for this finding within the report (`[A-Za-z0-9._-]+`).
    pub id: String,
    /// Audit category (e.g., `keeper-coverage`, `boundary-matrix`,
    /// `edge-only-mocking`, `test-naming`, `code-scope`).
    pub category: String,
    /// The file, module, test, or ticket id where the defect was observed.
    pub target: String,
    /// Concrete explanation of the defect and the required fix.
    pub detail: String,
}

/// One review pass's typed handoff: the scorecard, the defects found, and the
/// fix tickets that resolve them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ReviewReport {
    /// Pre-flight audit scorecard for this review round.
    pub scorecard: ReviewScorecard,
    /// Defects uncovered in this round; must be empty when `fixes` is empty and
    /// non-empty when `fixes` is non-empty.
    pub findings: Vec<ReviewFinding>,
    /// Ordered fix tickets to run in the next queue round; empty when the
    /// review passes.
    pub fixes: Vec<Ticket>,
}

impl ReviewReport {
    /// Whether this review round passed without needing any fix tickets.
    pub fn passed(&self) -> bool {
        self.fixes.is_empty()
    }

    /// Render this single review report as a markdown document.
    pub fn to_markdown(&self) -> String {
        render_reviews(std::slice::from_ref(self))
    }

    /// Validate the structural rules of a review report before it is accepted
    /// as a handoff.
    ///
    /// Every field of `scorecard` must be non-blank. When `fixes` is empty the
    /// review passed, so `findings` must also be empty. When `fixes` is
    /// non-empty, `findings` must be non-empty, every finding must carry a
    /// unique valid id and non-blank `category`, `target` and `detail`, every
    /// fix ticket must carry at least one non-blank acceptance criterion, and
    /// the fix tickets must form a valid [`TicketSet`].
    pub fn validate(&self) -> Result<(), String> {
        for (name, value) in [
            ("keeper_coverage", &self.scorecard.keeper_coverage),
            (
                "sediment_and_coupling",
                &self.scorecard.sediment_and_coupling,
            ),
            ("inferred_assumptions", &self.scorecard.inferred_assumptions),
            ("boundary_matrix", &self.scorecard.boundary_matrix),
            ("code_and_scope", &self.scorecard.code_and_scope),
        ] {
            if value.trim().is_empty() {
                return Err(format!(
                    "[ERROR] the review scorecard has blank `{name}`; every scorecard field \
                     must state the audit result for this round"
                ));
            }
        }

        if self.fixes.is_empty() {
            if !self.findings.is_empty() {
                return Err(
                    "[ERROR] the review report lists findings without fix tickets; when `fixes` \
                     is empty `findings` must also be empty, or hand over at least one fix \
                     ticket in `fixes` to resolve the findings"
                        .to_string(),
                );
            }
            return Ok(());
        }

        if self.findings.is_empty() {
            return Err(
                "[ERROR] the review report hands over fix tickets without any `findings`; list \
                 every defect the fix tickets resolve in `findings`, or pass an empty `fixes` \
                 list when the review passes"
                    .to_string(),
            );
        }

        let mut seen_ids: HashSet<&str> = HashSet::new();
        for finding in &self.findings {
            let id = finding.id.trim();
            if id.is_empty() {
                return Err(
                    "[ERROR] a review finding has an empty `id`; every finding needs a unique \
                     non-empty id"
                        .to_string(),
                );
            }
            if let Some(bad) = id.chars().find(|c| !is_id_char(*c)) {
                return Err(format!(
                    "[ERROR] the review finding id `{id}` contains `{bad}`, which is not \
                     allowed; ids may only contain letters, digits and `.`, `_`, `-`"
                ));
            }
            if !seen_ids.insert(id) {
                return Err(format!(
                    "[ERROR] the review finding id `{id}` is duplicated; every finding needs a \
                     unique id"
                ));
            }
            for (field, value) in [
                ("category", &finding.category),
                ("target", &finding.target),
                ("detail", &finding.detail),
            ] {
                if value.trim().is_empty() {
                    return Err(format!(
                        "[ERROR] review finding `{id}` has blank `{field}`; every finding must \
                         name its `category`, `target` and `detail`"
                    ));
                }
            }
        }

        let fix_set = TicketSet {
            tickets: self.fixes.clone(),
        };
        fix_set.validate()?;

        for ticket in &self.fixes {
            let id = ticket.id.trim();
            if ticket
                .acceptance_criteria
                .iter()
                .all(|criterion| criterion.trim().is_empty())
            {
                return Err(format!(
                    "[ERROR] fix ticket `{id}` has blank `acceptance_criteria`; every fix ticket \
                     must state at least one concrete pass/fail criterion"
                ));
            }
        }

        Ok(())
    }
}

/// Render all completed review rounds of a pipeline run into the canonical
/// `review.md` document. Ends with exactly one newline.
pub fn render_reviews(reviews: &[ReviewReport]) -> String {
    let mut blocks = vec![REVIEW_TITLE.to_string()];

    for (index, review) in reviews.iter().enumerate() {
        let round = index + 1;
        let verdict = if review.fixes.is_empty() {
            "**Verdict:** Passed (no fix tickets)".to_string()
        } else {
            let finding_noun = if review.findings.len() == 1 {
                "finding"
            } else {
                "findings"
            };
            let ticket_noun = if review.fixes.len() == 1 {
                "fix ticket"
            } else {
                "fix tickets"
            };
            format!(
                "**Verdict:** Needs fixes ({} {finding_noun}, {} {ticket_noun})",
                review.findings.len(),
                review.fixes.len()
            )
        };

        let scorecard = format!(
            "{SCORECARD_HEADING}\n\n\
             - **Keeper coverage:** {}\n\
             - **Sediment & coupling:** {}\n\
             - **Inferred assumptions:** {}\n\
             - **Boundary matrix:** {}\n\
             - **Code & scope:** {}",
            review.scorecard.keeper_coverage.trim(),
            review.scorecard.sediment_and_coupling.trim(),
            review.scorecard.inferred_assumptions.trim(),
            review.scorecard.boundary_matrix.trim(),
            review.scorecard.code_and_scope.trim(),
        );

        let findings_body = if review.findings.is_empty() {
            NONE.to_string()
        } else {
            review
                .findings
                .iter()
                .map(|finding| {
                    format!(
                        "- **{}** ({} — {}): {}",
                        finding.id.trim(),
                        finding.category.trim(),
                        finding.target.trim(),
                        finding.detail.trim()
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        let findings = format!("{FINDINGS_HEADING}\n\n{findings_body}");

        let fixes_body = if review.fixes.is_empty() {
            NONE.to_string()
        } else {
            review
                .fixes
                .iter()
                .map(|ticket| format!("- `{}`: {}", ticket.id.trim(), ticket.title.trim()))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let fixes = format!("{FIX_TICKETS_HEADING}\n\n{fixes_body}");

        blocks.push(format!(
            "## Round {round}\n\n{verdict}\n\n{scorecard}\n\n{findings}\n\n{fixes}"
        ));
    }

    let mut document = blocks.join("\n\n");
    document.push('\n');
    document
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_scorecard() -> ReviewScorecard {
        ReviewScorecard {
            keeper_coverage: "All 4 keeper categories are present.".to_string(),
            sediment_and_coupling: "Zero internal mocks; no private state.".to_string(),
            inferred_assumptions: "Default exit code is 0.".to_string(),
            boundary_matrix: "All 5 boundary dimensions covered.".to_string(),
            code_and_scope: "All tickets stayed within scope.".to_string(),
        }
    }

    fn valid_fix_ticket() -> Ticket {
        Ticket {
            id: "r1-1-add-boundary-test".to_string(),
            blocked_by: Vec::new(),
            blocks: Vec::new(),
            title: "Add the missing boundary test".to_string(),
            objective: "Cover the empty-input boundary on the parser entry point.".to_string(),
            acceptance_criteria: vec!["Empty input returns an explicit parse error.".to_string()],
            prerequisites_assumed: "The parser module exists.".to_string(),
            out_of_scope: vec![" CLI changes.".to_string()],
            scope_of_changes: vec!["Add the empty boundary test in `src/parser.rs`.".to_string()],
            test_expectations: vec![
                "when_parsing_input_given_empty_slice_then_returns_error".to_string(),
            ],
            verification_gate: "cargo test parser".to_string(),
        }
    }

    fn valid_finding() -> ReviewFinding {
        ReviewFinding {
            id: "f1-empty-boundary".to_string(),
            category: "boundary-matrix".to_string(),
            target: "src/parser.rs".to_string(),
            detail: "The empty-slice boundary dimension is not tested.".to_string(),
        }
    }

    #[test]
    fn when_validating_review_given_clean_pass_then_succeeds() {
        let report = ReviewReport {
            scorecard: valid_scorecard(),
            findings: Vec::new(),
            fixes: Vec::new(),
        };
        assert_eq!(report.validate(), Ok(()));
        assert!(report.passed());
    }

    #[test]
    fn when_validating_review_given_blank_scorecard_field_then_returns_error() {
        let mut report = ReviewReport {
            scorecard: valid_scorecard(),
            findings: Vec::new(),
            fixes: Vec::new(),
        };
        report.scorecard.boundary_matrix = "   ".to_string();
        let error = report.validate().expect_err("must reject blank scorecard");
        assert!(error.contains("boundary_matrix"), "{error}");
    }

    #[test]
    fn when_validating_review_given_findings_without_fixes_then_returns_error() {
        let report = ReviewReport {
            scorecard: valid_scorecard(),
            findings: vec![valid_finding()],
            fixes: Vec::new(),
        };
        let error = report
            .validate()
            .expect_err("findings without fixes must be rejected");
        assert!(error.contains("without fix tickets"), "{error}");
    }

    #[test]
    fn when_validating_review_given_fixes_without_findings_then_returns_error() {
        let report = ReviewReport {
            scorecard: valid_scorecard(),
            findings: Vec::new(),
            fixes: vec![valid_fix_ticket()],
        };
        let error = report
            .validate()
            .expect_err("fixes without findings must be rejected");
        assert!(error.contains("without any `findings`"), "{error}");
    }

    #[test]
    fn when_validating_review_given_finding_and_matching_fix_then_succeeds() {
        let report = ReviewReport {
            scorecard: valid_scorecard(),
            findings: vec![valid_finding()],
            fixes: vec![valid_fix_ticket()],
        };
        assert_eq!(report.validate(), Ok(()));
        assert!(!report.passed());
    }

    #[test]
    fn when_rendering_reviews_given_multiple_rounds_then_emits_ordered_sections() {
        let round_one = ReviewReport {
            scorecard: valid_scorecard(),
            findings: vec![valid_finding()],
            fixes: vec![valid_fix_ticket()],
        };
        let round_two = ReviewReport {
            scorecard: valid_scorecard(),
            findings: Vec::new(),
            fixes: Vec::new(),
        };

        let markdown = render_reviews(&[round_one, round_two]);
        assert!(markdown.starts_with("# Review report\n\n## Round 1\n"));
        assert!(markdown.contains("**Verdict:** Needs fixes (1 finding, 1 fix ticket)"));
        assert!(markdown.contains("- `r1-1-add-boundary-test`: Add the missing boundary test"));
        assert!(markdown.contains("## Round 2\n\n**Verdict:** Passed (no fix tickets)"));
        assert!(markdown.ends_with('\n') && !markdown.ends_with("\n\n"));
    }
}
