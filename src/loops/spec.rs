//! The spec agent: the specification pass.
//!
//! Holds no long-lived session. Every handoff runs one fresh pass in a fresh
//! transcript, so nothing from an earlier pass is carried over; the transcript
//! is thrown away once the specification has been handed to the tickets agent.

use crate::Config;
use crate::api::Message;
use crate::plan_document::Plan;
use crate::prompts;
use crate::specification::Specification;
use crate::tools::{Phase, Source};

use super::FailureKind;
use super::InvocationContext;
use super::agent::{self, Stop};

/// Why one `to_spec` returned.
// The specification payload is large, but it is moved straight to the caller:
// boxing it would only change the type the spec fixes here.
#[allow(clippy::large_enum_variant)]
pub enum Outcome {
    /// The spec agent called `finish_spec`: carries the specification object it
    /// wrote in that call, and nothing else from the pass.
    Handoff(Specification),
    /// The model ended its turn without calling `finish_spec`. It may be done,
    /// or it may be waiting for the user -- the API cannot tell the two apart.
    TurnEnd,
    /// The loop broke off on an error or the step limit; the detail is already
    /// on screen.
    Failed(FailureKind),
}

/// Turn the plan object into a technical specification and hand it over. One
/// fresh transcript per call: nothing from an earlier pass is carried over. The
/// plan is rendered to markdown here and nowhere else on this side of the
/// boundary, so the spec agent receives exactly the document the plan agent's
/// object describes.
pub fn to_spec(config: &Config, scope: &str, plan: &Plan) -> Outcome {
    to_spec_in(config, scope, plan, InvocationContext::default())
}

/// The same fresh specification pass with an explicit invocation context.
pub(crate) fn to_spec_in(
    config: &Config,
    scope: &str,
    plan: &Plan,
    invocation: InvocationContext<'_>,
) -> Outcome {
    let mut messages = vec![
        Message::system(format!("{}{}", prompts::SPEC_SYSTEM_PROMPT, scope)),
        Message::user(format!(
            "{}{}",
            prompts::SPEC_KICKOFF_PROMPT,
            plan.to_markdown()
        )),
    ];

    match agent::run_agent_loop_with_context(
        &mut messages,
        config,
        Phase::Spec,
        Source::Plan(plan),
        invocation,
    ) {
        Stop::Spec(spec) => Outcome::Handoff(spec),
        Stop::TurnEnd => Outcome::TurnEnd,
        Stop::Failed(kind) => Outcome::Failed(kind),
        // The spec agent offers none of the others: only the plan and tickets
        // agents hand off, only the code agent completes, and the summary pass
        // is the summary agent's alone.
        Stop::Summary(_) => unreachable!("the spec agent offers no summary pass"),
        Stop::Handoff(_) => unreachable!("the spec agent offers no plan handoff"),
        Stop::Tickets(_) => unreachable!("the spec agent offers no ticket handoff"),
        Stop::Review(_) => unreachable!("the spec agent offers no review handoff"),
        Stop::Triage(_) => unreachable!("the spec agent offers no triage handoff"),
        Stop::Complete => unreachable!("the spec agent offers no completion"),
    }
}
