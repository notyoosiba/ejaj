//! The review agent: the post-queue review pass.
//!
//! Runs unattended in a fresh transcript after each resolved ticket-queue
//! round. Receives the specification, the tickets executed so far, the
//! cumulative queue report, and any prior review rounds from this run; inspects
//! the repository read-only with `execute_script`; and ends on `finish_review`
//! carrying a [`ReviewReport`].

use crate::Config;
use crate::api::Message;
use crate::prompts;
use crate::review::{ReviewReport, render_reviews};
use crate::specification::Specification;
use crate::tickets::Ticket;
use crate::tools::{Phase, Source};

use super::FailureKind;
use super::InvocationContext;
use super::agent::{self, Stop};

/// Why one [`audit`] returned.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum Outcome {
    /// The review agent called `finish_review`: carries the validated review
    /// report for this round.
    Handoff(ReviewReport),
    /// The model ended its turn without calling `finish_review`.
    TurnEnd,
    /// The loop broke off on an error or the step limit; the detail is already
    /// on screen.
    Failed(FailureKind),
}

/// The upstream context one review round reads: the caller's scope, the
/// specification, the tickets executed so far, the cumulative queue report, any
/// prior review rounds, and the identifiers a review may reference.
pub(crate) struct ReviewInputs<'a> {
    pub(crate) scope: &'a str,
    pub(crate) specification: &'a Specification,
    pub(crate) tickets: &'a [Ticket],
    pub(crate) report: &'a str,
    pub(crate) prior_reviews: &'a [ReviewReport],
    pub(crate) existing_ticket_ids: &'a [String],
}

/// Run one review round over the repository and the run's artifacts.
pub fn audit(
    config: &Config,
    scope: &str,
    specification: &Specification,
    tickets: &[Ticket],
    report: &str,
    prior_reviews: &[ReviewReport],
    existing_ticket_ids: &[String],
) -> Outcome {
    audit_in(
        config,
        ReviewInputs {
            scope,
            specification,
            tickets,
            report,
            prior_reviews,
            existing_ticket_ids,
        },
        InvocationContext::default(),
    )
}

/// The same fresh review round with an explicit invocation context.
pub(crate) fn audit_in(
    config: &Config,
    inputs: ReviewInputs<'_>,
    invocation: InvocationContext<'_>,
) -> Outcome {
    let mut messages = vec![
        Message::system(format!("{}{}", prompts::REVIEW_SYSTEM_PROMPT, inputs.scope)),
        Message::user(format!(
            "{}{}",
            prompts::REVIEW_KICKOFF_PROMPT,
            kickoff(
                inputs.specification,
                inputs.tickets,
                inputs.report,
                inputs.prior_reviews
            )
        )),
    ];

    match agent::run_agent_loop_with_context(
        &mut messages,
        config,
        Phase::Review,
        Source::Review(inputs.existing_ticket_ids),
        invocation,
    ) {
        Stop::Review(report) => Outcome::Handoff(report),
        Stop::Triage(_) => unreachable!("the review agent offers no triage handoff"),
        Stop::TurnEnd => Outcome::TurnEnd,
        Stop::Failed(kind) => Outcome::Failed(kind),
        Stop::Summary(_) => unreachable!("the review agent offers no summary pass"),
        Stop::Handoff(_) => unreachable!("the review agent offers no plan handoff"),
        Stop::Spec(_) => unreachable!("the review agent offers no specification handoff"),
        Stop::Tickets(_) => unreachable!("the review agent offers no ticket handoff"),
        Stop::Complete => unreachable!("the review agent offers no completion"),
    }
}

/// Render the upstream context for a review round: the specification, every
/// ticket executed so far, the cumulative queue report, and any prior review
/// rounds.
fn kickoff(
    specification: &Specification,
    tickets: &[Ticket],
    report: &str,
    prior_reviews: &[ReviewReport],
) -> String {
    let rendered_tickets = tickets
        .iter()
        .map(Ticket::to_markdown)
        .collect::<Vec<_>>()
        .join("\n");
    let prior = if prior_reviews.is_empty() {
        "(none — this is review round 1)\n".to_string()
    } else {
        render_reviews(prior_reviews)
    };

    format!(
        "## Specification\n\n{}\n\
         ## Executed Tickets\n\n{}\n\
         ## Queue Report\n\n{}\n\
         ## Prior Review Rounds\n\n{}",
        specification.to_markdown(),
        rendered_tickets,
        report.trim(),
        prior
    )
}
