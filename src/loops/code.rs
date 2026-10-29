//! The code agent.
//!
//! A ticket is the whole task for one code-agent call, so every ticket starts
//! from a fresh transcript: the module seeds one system message and one user
//! message for that ticket, runs the loop, and throws the transcript away. Two
//! consecutive ticket runs share no messages, and nothing from an earlier
//! ticket is replayed.

use crate::Config;
use crate::api::Message;
use crate::prompts;
use crate::tools::{Phase, Source};

use super::FailureKind;
use super::InvocationContext;
use super::agent::{self, Stop};

/// Why one `run_ticket` returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The code agent called `task_complete`: the ticket's work is done, and
    /// the caller still has to run the verification gate before it is resolved.
    Complete,
    /// The model ended its turn without calling `task_complete` (it may be
    /// waiting on a decision).
    TurnEnd,
    /// The loop broke off on an error or the step limit; the detail is already
    /// on screen.
    Failed(FailureKind),
}

/// One completed code-agent attempt: its stop outcome and the last assistant
/// narration it left, when it narrated at all.
///
/// The queue's retry path reads this to seed the next attempt's handover. The
/// ordinary [`Session::run_ticket`] callers keep the unchanged [`Outcome`].
#[derive(Debug, Clone)]
pub struct Attempt {
    /// How the attempt stopped.
    pub outcome: Outcome,
    /// The last printable assistant narration, if any reply carried one.
    pub narration: Option<String>,
}

impl Attempt {
    /// An attempt with no narration, for callers that only care how it stopped.
    pub fn new(outcome: Outcome) -> Attempt {
        Attempt {
            outcome,
            narration: None,
        }
    }
}

impl From<Outcome> for Attempt {
    fn from(outcome: Outcome) -> Attempt {
        Attempt::new(outcome)
    }
}

/// One code agent call, from the ticket it was handed to the stop.
pub struct Session {
    config: Config,
    scope: String,
}

impl Session {
    /// Prepare a code agent. It holds no transcript: the ticket passed to
    /// [`Session::run_ticket`] is the whole task, and the transcript is built
    /// from it and dropped again.
    pub fn new(config: &Config, scope: &str) -> Session {
        Session {
            config: config.clone(),
            scope: scope.to_string(),
        }
    }

    /// Hand one rendered ticket to a fresh code agent and run the tool loop to a stop.
    ///
    /// The ticket is the session's entire task: the local transcript is seeded
    /// with the code system prompt and the ticket kickoff, and nothing from any
    /// earlier ticket or pass is replayed. The queue passes the same string the
    /// ticket's file holds, so the artifact and the payload cannot drift.
    pub fn run_ticket(&self, ticket: &str) -> Outcome {
        self.run_ticket_attempt(ticket, None, None).outcome
    }

    /// The same ticket run, forwarding an optional ephemeral queue-contribution
    /// accumulator into the agent loop.
    ///
    /// Ordinary callers keep [`Session::run_ticket`]'s exact behavior and
    /// signature; the queue's timed round path uses this to account for the
    /// model attempts and executed commands it already measures.
    #[allow(dead_code)]
    pub(crate) fn run_ticket_with_contributions(
        &self,
        ticket: &str,
        contributions: Option<&mut crate::timing::QueueContributions>,
    ) -> Outcome {
        self.run_ticket_attempt(ticket, None, contributions).outcome
    }

    /// Run one fresh attempt for `ticket`, optionally seeded with a separate
    /// retry handover user message.
    ///
    /// The transcript is always rebuilt from the same ticket kickoff: the
    /// handover is one further user message after it, never a replacement for
    /// the ticket artifact and never the first attempt's transcript. Ordinary
    /// callers pass `None` and see exactly the payload they always did.
    pub(crate) fn run_ticket_attempt(
        &self,
        ticket: &str,
        handover: Option<&str>,
        contributions: Option<&mut crate::timing::QueueContributions>,
    ) -> Attempt {
        self.run_ticket_attempt_in(
            ticket,
            handover,
            contributions,
            InvocationContext::default(),
        )
    }

    /// Run one fresh attempt with the invocation's root and session directory
    /// forwarded to every Bash dispatch, including retries and later queue
    /// rounds.
    pub(crate) fn run_ticket_attempt_in(
        &self,
        ticket: &str,
        handover: Option<&str>,
        contributions: Option<&mut crate::timing::QueueContributions>,
        invocation: InvocationContext<'_>,
    ) -> Attempt {
        let mut messages = vec![
            Message::system(format!("{}{}", prompts::CODE_SYSTEM_PROMPT, self.scope)),
            Message::user(prompts::ticket_kickoff(ticket)),
        ];
        if let Some(handover) = handover {
            messages.push(Message::user(handover));
        }

        let run = agent::run_agent_loop_attempt_in(
            &mut messages,
            &self.config,
            Phase::Code,
            Source::None,
            invocation,
            contributions,
        );
        let outcome = match run.stop {
            Stop::Complete => Outcome::Complete,
            Stop::TurnEnd => Outcome::TurnEnd,
            Stop::Failed(kind) => Outcome::Failed(kind),
            // The code agent offers no handoff: only the plan, spec and tickets
            // agents end on a control tool that carries a payload, and the
            // summary phase is a text pass the code phase never runs.
            Stop::Summary(_) => unreachable!("the code agent offers no summary pass"),
            Stop::Handoff(_) => unreachable!("the code agent offers no plan handoff"),
            Stop::Spec(_) => unreachable!("the code agent offers no specification handoff"),
            Stop::Tickets(_) => unreachable!("the code agent offers no ticket handoff"),
            Stop::Review(_) => unreachable!("the code agent offers no review handoff"),
            Stop::Triage(_) => unreachable!("the code agent offers no triage handoff"),
        };
        Attempt {
            outcome,
            narration: run.narration,
        }
    }
}
