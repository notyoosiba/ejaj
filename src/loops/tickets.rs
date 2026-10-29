//! The tickets agent: the tickets pass.
//!
//! Holds no long-lived session. Every specification runs one fresh pass in a
//! fresh transcript, so nothing from an earlier pass is carried over; the
//! transcript is thrown away once the ticket set has been handed to the
//! orchestrator. This is the one place on this side of the boundary where the
//! specification object is rendered to markdown.

use crate::Config;
use crate::api::Message;
use crate::prompts;
use crate::specification::Specification;
use crate::tickets::TicketSet;
use crate::tools::{Phase, Source};

use super::FailureKind;
use super::InvocationContext;
use super::agent::{self, Stop};

/// Why one `to_tickets` returned.
pub enum Outcome {
    /// The tickets agent called `finish_tickets`: carries the ordered set it
    /// handed over in that call, and nothing else from the pass.
    Handoff(TicketSet),
    /// The model ended its turn without calling `finish_tickets`. It may be
    /// done, or it may be waiting for the user -- the API cannot tell the two
    /// apart.
    TurnEnd,
    /// The loop broke off on an error or the step limit; the detail is already
    /// on screen.
    Failed(FailureKind),
}

/// Turn the specification object into an ordered ticket set and hand it over.
///
/// One fresh transcript per call: nothing from an earlier pass is carried over.
/// The specification is rendered to markdown here and nowhere else on the
/// tickets side of the boundary, so the tickets agent receives exactly the
/// document the spec agent's object describes.
pub fn to_tickets(config: &Config, scope: &str, specification: &Specification) -> Outcome {
    to_tickets_in(config, scope, specification, InvocationContext::default())
}

/// The same fresh tickets pass with an explicit invocation context.
pub(crate) fn to_tickets_in(
    config: &Config,
    scope: &str,
    specification: &Specification,
    invocation: InvocationContext<'_>,
) -> Outcome {
    let mut messages = vec![
        Message::system(format!("{}{}", prompts::TICKETS_SYSTEM_PROMPT, scope)),
        Message::user(format!(
            "{}{}",
            prompts::TICKETS_KICKOFF_PROMPT,
            specification.to_markdown()
        )),
    ];

    match agent::run_agent_loop_with_context(
        &mut messages,
        config,
        Phase::Tickets,
        Source::Specification(specification),
        invocation,
    ) {
        Stop::Tickets(tickets) => Outcome::Handoff(tickets),
        Stop::TurnEnd => Outcome::TurnEnd,
        Stop::Failed(kind) => Outcome::Failed(kind),
        // The tickets agent offers none of the others: only the plan and spec
        // agents hand off a document, only the code agent completes, and the
        // summary pass is the summary agent's alone.
        Stop::Summary(_) => unreachable!("the tickets agent offers no summary pass"),
        Stop::Handoff(_) => unreachable!("the tickets agent offers no plan handoff"),
        Stop::Spec(_) => unreachable!("the tickets agent offers no specification handoff"),
        Stop::Review(_) => unreachable!("the tickets agent offers no review handoff"),
        Stop::Triage(_) => unreachable!("the tickets agent offers no triage handoff"),
        Stop::Complete => unreachable!("the tickets agent offers no completion"),
    }
}
