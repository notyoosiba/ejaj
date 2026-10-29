//! The plan agent: the design interview.
//!
//! Owns the plan agent's transcript and system prompt for the life of the
//! process. The interview continues across every REPL line, and ends only when
//! the plan agent calls `finish_plan`, which hands its plan to the spec agent.

use crate::Config;
use crate::api::Message;
use crate::plan_document::Plan;
use crate::prompts;
use crate::tools::{Phase, Source};

use super::FailureKind;
use super::InvocationContext;
use super::agent::{self, Stop};

/// The marker that separates the agreed plan from one concise downstream retry
/// outcome in the assistant handover. It is public so the reason the transcript
/// changed is visible at the boundary that owns it.
pub const DOWNSTREAM_RETRY_MARKER: &str = "[DOWNSTREAM RETRY]";

/// The largest number of bytes of a downstream outcome kept in the handover.
/// An error message or blocker explanation can be arbitrarily long; the plan
/// that has to be resumed must stay the dominant part of the next request.
pub const MAX_RETRY_DETAIL_BYTES: usize = 600;

/// Why one `ask` returned.
pub enum Outcome {
    /// The model ended its turn without calling `finish_plan`. It may be done,
    /// or it may be waiting for the user -- the API cannot tell the two apart.
    TurnEnd,
    /// The loop broke off on an error or the step limit; the detail is already
    /// on screen.
    Failed(FailureKind),
    /// The plan agent called `finish_plan`: carries the plan object it wrote in
    /// that call, and nothing else from the interview.
    Handoff(Plan),
}

/// The plan agent, from the first REPL line to the last handoff.
pub struct Session {
    config: Config,
    /// The pinned system prompt, kept separately so every handover can restore
    /// it byte-for-byte without depending on what the interview left behind.
    system: Message,
    messages: Vec<Message>,
}

impl Session {
    /// Seed the plan agent's transcript with its system prompt, followed by the
    /// scope notice.
    pub fn new(config: &Config, scope: &str) -> Session {
        let system = Message::system(format!("{}{}", prompts::PLAN_SYSTEM_PROMPT, scope));
        Session {
            config: config.clone(),
            system: system.clone(),
            messages: vec![system],
        }
    }

    /// Replace the whole interview with the unchanged system prompt and one
    /// assistant message carrying the typed plan's canonical rendering.
    ///
    /// This is called the moment a validated plan handoff is accepted, so no
    /// exploratory turn or handoff tool call can be replayed into the next
    /// plan request.
    pub fn accept_handoff(&mut self, plan: &Plan) {
        self.replace_handover(plan, None);
    }

    /// Update the same assistant handover with the agreed plan and one concise
    /// named downstream phase outcome, so the next plan request resumes from the
    /// plan plus the result that sent the run back.
    pub fn record_downstream_retry(&mut self, plan: &Plan, phase: &str, detail: &str) {
        self.replace_handover(plan, Some((phase, detail)));
    }

    /// Append one user line to the transcript and run the tool loop to a stop.
    ///
    /// Every earlier message's reasoning is cleared before the new user message
    /// is appended: plan reasoning belongs to the turn that produced it, never
    /// to the next user-message boundary.
    pub fn ask(&mut self, input: &str) -> Outcome {
        self.ask_in(input, InvocationContext::default())
    }

    /// The same interview turn with the invocation's root and session directory
    /// forwarded to every tool dispatch.
    pub(crate) fn ask_in(&mut self, input: &str, invocation: InvocationContext<'_>) -> Outcome {
        self.clear_reasoning();
        self.messages.push(Message::user(input));
        match agent::run_agent_loop_with_context(
            &mut self.messages,
            &self.config,
            Phase::Plan,
            Source::None,
            invocation,
        ) {
            Stop::Handoff(plan) => Outcome::Handoff(plan),
            Stop::TurnEnd => Outcome::TurnEnd,
            Stop::Failed(kind) => Outcome::Failed(kind),
            // The plan agent offers none of the others: only the spec and
            // tickets agents hand off, only the code agent completes, and the
            // summary pass is the summary agent's alone.
            Stop::Summary(_) => unreachable!("the plan agent offers no summary pass"),
            Stop::Spec(_) => unreachable!("the plan agent offers no specification handoff"),
            Stop::Tickets(_) => unreachable!("the plan agent offers no ticket handoff"),
            Stop::Review(_) => unreachable!("the plan agent offers no review handoff"),
            Stop::Triage(_) => unreachable!("the plan agent offers no triage handoff"),
            Stop::Complete => unreachable!("the plan agent offers no completion"),
        }
    }

    /// One transcript shape for every handover: the pinned system prompt, then
    /// one assistant message. With a retry outcome the assistant carries the
    /// canonical plan followed by that concise named result; without one it
    /// carries exactly the canonical plan.
    fn replace_handover(&mut self, plan: &Plan, retry: Option<(&str, &str)>) {
        let mut content = plan.to_markdown();
        if let Some((phase, detail)) = retry {
            content.push('\n');
            content.push_str(DOWNSTREAM_RETRY_MARKER);
            content.push('\n');
            content.push_str("phase: ");
            content.push_str(phase);
            content.push('\n');
            content.push_str("outcome: ");
            content.push_str(&concise_retry_detail(detail));
            content.push('\n');
        }

        self.messages.clear();
        self.messages.push(self.system.clone());
        self.messages.push(Message::assistant(content));
    }

    /// Clear every earlier message's reasoning at the next user boundary.
    fn clear_reasoning(&mut self) {
        for message in &mut self.messages {
            message.reasoning_content = None;
        }
    }
}

/// The marker naming the middle omitted from a long downstream outcome.
const RETRY_OMISSION_MARKER: &str = "...[omitted]...";

/// The most source bytes kept before the omission marker.
const RETRY_PREFIX_BYTES: usize = 293;

/// The most source bytes kept after the omission marker.
const RETRY_SUFFIX_BYTES: usize = 292;

/// Flatten and cap one downstream outcome for the resumed plan handover.
///
/// The bound is on UTF-8 bytes and neither cut splits a character. A detail
/// that already fits is kept as its single-line, whitespace-collapsed form; a
/// longer one keeps both the beginning -- where the phase's named result and
/// the first blocker live -- and the ending -- where the last diagnostic lives
/// -- around a marker naming the omitted middle.
fn concise_retry_detail(detail: &str) -> String {
    let one_line = detail.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.len() <= MAX_RETRY_DETAIL_BYTES {
        return one_line;
    }

    let prefix_end = floor_char_boundary(&one_line, RETRY_PREFIX_BYTES);
    let suffix_start =
        ceil_char_boundary(&one_line, one_line.len().saturating_sub(RETRY_SUFFIX_BYTES));
    format!(
        "{}{RETRY_OMISSION_MARKER}{}",
        &one_line[..prefix_end],
        &one_line[suffix_start..]
    )
}

/// The largest character boundary at or below `index`.
fn floor_char_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// The smallest character boundary at or above `index`.
fn ceil_char_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{CallError, Outgoing, RawResponse, Role, Transport};
    use std::sync::{Arc, Mutex};

    /// A transport that serves canned chat-completions replies in order and
    /// records every request body it saw.
    struct Script {
        bodies: Mutex<Vec<String>>,
        requests: Mutex<Vec<serde_json::Value>>,
    }

    impl Script {
        fn new(bodies: Vec<String>) -> Arc<Script> {
            Arc::new(Script {
                bodies: Mutex::new(bodies),
                requests: Mutex::new(Vec::new()),
            })
        }

        fn requests(&self) -> Vec<serde_json::Value> {
            self.requests.lock().unwrap().clone()
        }
    }

    impl Transport for Script {
        fn send(&self, outgoing: &Outgoing) -> Result<RawResponse, CallError> {
            self.requests.lock().unwrap().push(outgoing.body.clone());
            let mut bodies = self.bodies.lock().unwrap();
            assert!(
                !bodies.is_empty(),
                "the plan pass made an unexpected extra request"
            );
            Ok(RawResponse {
                status: 200,
                body: bodies.remove(0),
            })
        }
    }

    fn answer(text: &str) -> String {
        serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": text,
                    "tool_calls": [],
                }
            }]
        })
        .to_string()
    }

    fn config(script: Arc<Script>) -> Config {
        Config::new(
            "http://script.test/v1/chat/completions",
            "test-model",
            Some("test-key".to_string()),
        )
        .with_transport(script)
    }

    /// Regression: resuming the plan with a new user input clears only the
    /// earlier reasoning, leaving every other message field untouched.
    #[test]
    fn when_resuming_plan_given_prior_reasoning_then_clears_reasoning_without_changing_other_message_fields()
     {
        let script = Script::new(vec![answer("the plan continues")]);
        let config = config(script.clone());
        let mut session = Session::new(&config, "# scope notice");
        let mut extra = serde_json::Map::new();
        extra.insert("provider_field".to_string(), serde_json::json!("kept"));
        let earlier = Message {
            role: Role::Assistant,
            content: Some("earlier plan turn".to_string()),
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: Some("private plan reasoning".to_string()),
            extra: extra.clone(),
        };
        session.messages.push(earlier.clone());

        assert!(matches!(session.ask("refine the plan"), Outcome::TurnEnd));
        let requests = script.requests();
        assert_eq!(requests.len(), 1);
        let sent: Vec<Message> = serde_json::from_value(requests[0]["messages"].clone())
            .expect("the recorded plan transcript decodes");
        let resumed = sent
            .iter()
            .find(|message| {
                message.role == Role::Assistant
                    && message.content.as_deref() == Some("earlier plan turn")
            })
            .expect("the earlier assistant turn is replayed");
        assert_eq!(resumed.reasoning_content, None);
        assert_eq!(resumed.content, earlier.content);
        assert_eq!(resumed.role, earlier.role);
        assert_eq!(resumed.tool_calls.is_none(), earlier.tool_calls.is_none());
        assert_eq!(resumed.tool_call_id, earlier.tool_call_id);
        assert_eq!(resumed.extra, extra);
        assert_eq!(
            sent.last().and_then(|message| message.content.as_deref()),
            Some("refine the plan"),
            "the new plan user input is appended after the cleared transcript"
        );
    }
}
