//! The triage agent: the unattended read-only pass over the draft ticket set.
//!
//! One fresh transcript per call. The pass runs after the tickets handoff and
//! before the ticket queue: it is handed the rendered specification and every
//! original draft ticket, inspects the repository read-only with
//! `execute_script`, and ends on `finish_triage` carrying the validated
//! [`TriageReport`]. Nothing from the plan interview, the spec pass or the
//! tickets pass is
//! replayed, and the pass never asks the user anything.
//!
//! The report is an audit beside the ticket files, never an execution contract:
//! the queue still runs the tickets the tickets agent wrote, and a ready
//! classification's Agent Brief is the code agent's restatement of them.
//!
//! A pass that ends without `finish_triage`, or that fails outright, hands no
//! report to its caller: [`Outcome::TurnEnd`] and [`Outcome::Failed`] both mean
//! the draft set was not triaged, so the caller must start no code session and
//! run no verification gate for it -- the pipeline reports both as its own
//! `Step::Retry`.

use crate::Config;
use crate::api::Message;
use crate::prompts;
use crate::specification::Specification;
use crate::tickets::{Ticket, TicketSet};
use crate::tools::{Phase, Source};
use crate::triage::TriageReport;

use super::FailureKind;
use super::InvocationContext;
use super::agent::{self, Stop};

/// Why one [`audit`] returned.
#[derive(Debug)]
pub enum Outcome {
    /// The triage agent called `finish_triage`: carries the validated report,
    /// one classification per draft ticket.
    Handoff(TriageReport),
    /// The model ended its turn without calling `finish_triage`. No report
    /// exists, so the caller must report the pass as unfinished.
    TurnEnd,
    /// The loop broke off on an error or the step limit; the detail is already
    /// on screen.
    Failed(FailureKind),
}

/// Run one triage pass over the draft ticket set.
///
/// One fresh transcript per call: the rendered specification, every original
/// ticket rendered by [`Ticket::to_markdown`] and the triage prompts are the whole
/// of what the pass is told, so no earlier pass leaks into its audit. The pass
/// is unattended and read-only -- it is offered `execute_script` and
/// `finish_triage` and never `bash`, and its scripts run with the read-only
/// capabilities of [`Phase::Triage`].
pub fn audit(
    config: &Config,
    scope: &str,
    specification: &Specification,
    tickets: &TicketSet,
) -> Outcome {
    audit_in(
        config,
        scope,
        specification,
        tickets,
        InvocationContext::default(),
    )
}

/// The same fresh triage pass with an explicit invocation context.
pub(crate) fn audit_in(
    config: &Config,
    scope: &str,
    specification: &Specification,
    tickets: &TicketSet,
    invocation: InvocationContext<'_>,
) -> Outcome {
    let mut messages = vec![
        Message::system(format!("{}{}", prompts::TRIAGE_SYSTEM_PROMPT, scope)),
        Message::user(format!(
            "{}{}",
            prompts::TRIAGE_KICKOFF_PROMPT,
            kickoff(specification, tickets)
        )),
    ];

    // The typed source travels into the loop, so `finish_triage` is validated
    // against this specification and this draft set: a report that drops a
    // ticket, invents one or rewrites a title comes back as a tool error the
    // agent corrects, never as a handoff.
    let source = Source::Triage {
        specification,
        tickets,
    };

    match agent::run_agent_loop_with_context(
        &mut messages,
        config,
        Phase::Triage,
        source,
        invocation,
    ) {
        Stop::Triage(report) => Outcome::Handoff(report),
        Stop::TurnEnd => Outcome::TurnEnd,
        Stop::Failed(kind) => Outcome::Failed(kind),
        // The triage phase offers exactly one control tool, so no other stop
        // can be reached.
        Stop::Summary(_)
        | Stop::Handoff(_)
        | Stop::Spec(_)
        | Stop::Tickets(_)
        | Stop::Review(_)
        | Stop::Complete => unreachable!("the triage agent offers no other control tool"),
    }
}

/// Render the upstream context of one triage pass: the specification the draft
/// set was sliced from, then every original ticket, in draft order, through the
/// same renderer the ticket files are written with.
fn kickoff(specification: &Specification, tickets: &TicketSet) -> String {
    let rendered = tickets
        .tickets
        .iter()
        .map(Ticket::to_markdown)
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "## Specification\n\n{}\n## Original Tickets\n\n{}\n",
        specification.to_markdown(),
        rendered.trim_end()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{CallError, Outgoing, RawResponse, Transport};
    use crate::specification::{
        DeferredItems, ImplementationDecisions, TestingDecisions, UserStory,
    };
    use crate::triage::{AgentBrief, TriageCategory, TriageState, TriageTicket};
    use std::sync::{Arc, Mutex};

    const MID_URL: &str = "http://mid.test/v1/chat/completions";
    const GIGA_URL: &str = "https://giga.test/v1/responses";

    /// A transport that serves canned chat-completions bodies in order and
    /// records every request body it saw. Running out of bodies fails the test:
    /// it means the pass sent a request the script did not expect.
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
                "the triage pass made an unexpected extra request"
            );
            Ok(RawResponse {
                status: 200,
                body: bodies.remove(0),
            })
        }
    }

    /// A transport that never answers: every send is a transport failure.
    struct Unreachable;

    impl Transport for Unreachable {
        fn send(&self, _outgoing: &Outgoing) -> Result<RawResponse, CallError> {
            Err(CallError::Transport(
                "the endpoint is unreachable".to_string(),
            ))
        }
    }

    fn config(transport: Arc<dyn Transport>) -> Config {
        Config::new(MID_URL, "mid-model", Some("mid-key".to_string()))
            .with_smoll("https://smoll.test/v1/messages", "smoll-model", "smoll-key")
            .with_transport(transport)
    }

    /// One chat-completions assistant message: optional text and the given
    /// (name, arguments) tool calls.
    fn reply(content: Option<&str>, calls: &[(&str, String)]) -> String {
        let tool_calls: Vec<serde_json::Value> = calls
            .iter()
            .enumerate()
            .map(|(index, (name, arguments))| {
                serde_json::json!({
                    "id": format!("call-{index}"),
                    "type": "function",
                    "function": { "name": name, "arguments": arguments },
                })
            })
            .collect();
        serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": content,
                    "tool_calls": tool_calls,
                }
            }]
        })
        .to_string()
    }

    /// The (role, content) pairs of one recorded request.
    fn messages(body: &serde_json::Value) -> Vec<(String, String)> {
        body["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|message| {
                (
                    message["role"].as_str().unwrap_or_default().to_string(),
                    message["content"].as_str().unwrap_or_default().to_string(),
                )
            })
            .collect()
    }

    /// The one user message of one recorded request, or the empty string when
    /// the request carried none.
    fn user_message(body: &serde_json::Value) -> String {
        messages(body)
            .into_iter()
            .find(|(role, _)| role == "user")
            .map(|(_, content)| content)
            .unwrap_or_default()
    }

    /// The tools one recorded request advertised, in order.
    fn tool_names(body: &serde_json::Value) -> Vec<String> {
        body["tools"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|tool| {
                tool["function"]["name"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string()
            })
            .collect()
    }

    /// The one specification the triage fixtures were sliced from.
    fn specification() -> Specification {
        Specification {
            feature_name: "Audit the draft ticket set".to_string(),
            problem_statement: "The draft ticket set has no readiness audit.".to_string(),
            solution_overview: "Classify every draft ticket as a typed report.".to_string(),
            user_stories: vec![UserStory {
                title: "Classify the draft set".to_string(),
                requirement: "Every draft ticket is classified exactly once.".to_string(),
                acceptance_criteria: vec![
                    "A draft ticket without a classification is refused.".to_string(),
                ],
            }],
            implementation_decisions: ImplementationDecisions {
                module_architecture: "A new triage module owns the report.".to_string(),
                data_models_and_contracts: "One classification per draft ticket.".to_string(),
                adr_compliance: "Mirrors the typed review-report convention.".to_string(),
            },
            testing_decisions: TestingDecisions {
                test_seams: "TriageReport::validate_against.".to_string(),
                coverage_strategy: "Unit tests over the pass and its fixtures.".to_string(),
                prior_art: "The review pass tests.".to_string(),
            },
            out_of_scope: Vec::new(),
            deferred_items: DeferredItems {
                operational_risks: String::new(),
                follow_up_work: String::new(),
            },
        }
    }

    /// One draft ticket, carrying the fields the report contract preserves.
    fn draft_ticket(id: &str, title: &str, blocked_by: &[String]) -> Ticket {
        Ticket {
            id: id.to_string(),
            blocked_by: blocked_by.to_vec(),
            blocks: Vec::new(),
            title: title.to_string(),
            objective: format!("Deliver {title} end to end."),
            acceptance_criteria: vec![format!("{title} is delivered.")],
            prerequisites_assumed: "The crate builds.".to_string(),
            out_of_scope: Vec::new(),
            scope_of_changes: vec![format!("Change what {title} needs.")],
            test_expectations: vec!["when_running_the_slice_given_setup_then_passes".to_string()],
            verification_gate: "cargo test triage".to_string(),
        }
    }

    /// The two-ticket draft set with a single dependency edge.
    fn two_ticket_draft() -> TicketSet {
        TicketSet {
            tickets: vec![
                draft_ticket("1-first", "First slice", &[]),
                draft_ticket("2-second", "Second slice", &["1-first".to_string()]),
            ],
        }
    }

    /// One ready classification carrying the smallest accepted brief.
    fn ready_classification(id: &str, title: &str, blocked_by: &[String]) -> TriageTicket {
        TriageTicket {
            ticket_id: id.to_string(),
            ticket_title: title.to_string(),
            category: TriageCategory::Enhancement,
            state: TriageState::ReadyForAgent,
            notes: vec!["The draft ticket is one self-contained slice.".to_string()],
            blocker_explanation: None,
            agent_brief: Some(AgentBrief {
                target_behavior: format!("Implement {title}."),
                acceptance_criteria: vec![format!("{id} is implemented.")],
                out_of_scope: vec!["None.".to_string()],
                blocked_by: blocked_by.to_vec(),
            }),
        }
    }

    /// One non-ready classification carrying a blocker explanation.
    fn blocked_classification(
        id: &str,
        title: &str,
        state: TriageState,
        explanation: &str,
    ) -> TriageTicket {
        TriageTicket {
            ticket_id: id.to_string(),
            ticket_title: title.to_string(),
            category: TriageCategory::Bug,
            state,
            notes: vec!["The draft ticket was audited against the repository.".to_string()],
            blocker_explanation: Some(explanation.to_string()),
            agent_brief: None,
        }
    }

    /// The arguments of one finish_triage call carrying the report.
    fn finish_triage_call(classifications: Vec<TriageTicket>) -> String {
        let report = TriageReport {
            tickets: classifications,
        };
        serde_json::json!({ "triage": report }).to_string()
    }

    /// One triage pass is handed a fresh transcript: the triage prompts, the
    /// rendered specification and every original ticket rendered by the ticket
    /// renderer, and nothing from an earlier pass. Its one request advertises
    /// only the read-only inspection tool and the handoff.
    #[test]
    fn when_running_triage_given_specification_and_draft_tickets_then_sends_complete_fresh_audit_context()
     {
        let specification = specification();
        let tickets = two_ticket_draft();
        let report = TriageReport {
            tickets: vec![
                ready_classification("1-first", "First slice", &[]),
                ready_classification("2-second", "Second slice", &["1-first".to_string()]),
            ],
        };

        let script = Script::new(vec![reply(
            None,
            &[("finish_triage", finish_triage_call(report.tickets.clone()))],
        )]);
        let config = config(script.clone());

        match audit(
            &config,
            "The project root is the working directory.",
            &specification,
            &tickets,
        ) {
            Outcome::Handoff(handed) => assert_eq!(handed, report),
            other => panic!("expected the triage handoff, got {other:?}"),
        }

        let requests = script.requests();
        assert_eq!(
            requests.len(),
            1,
            "the pass must be one request away from its handoff"
        );
        let transcript = messages(&requests[0]);
        assert_eq!(
            transcript.len(),
            2,
            "the transcript is fresh: the triage prompts and nothing else"
        );
        assert_eq!(transcript[0].0, "system");
        assert!(transcript[0].1.contains(prompts::TRIAGE_SYSTEM_PROMPT));
        assert!(
            transcript[0]
                .1
                .contains("The project root is the working directory.")
        );
        assert_eq!(transcript[1].0, "user");
        assert!(transcript[1].1.contains(prompts::TRIAGE_KICKOFF_PROMPT));

        let context = &transcript[1].1;
        assert!(
            context.contains(&specification.to_markdown()),
            "the rendered specification is missing from the audit context"
        );
        for ticket in &tickets.tickets {
            assert!(
                context.contains(&ticket.to_markdown()),
                "ticket {} is missing from the audit context",
                ticket.id
            );
        }
        assert_eq!(
            context.matches("id: ").count(),
            tickets.tickets.len(),
            "every original ticket and no invented one"
        );

        assert_eq!(
            tool_names(&requests[0]),
            ["execute_script", "finish_triage"]
        );
    }

    /// The largest exercised fixture: every original ticket is carried into the
    /// audit context, every classification survives validation, and every
    /// dependency edge survives the handoff.
    #[test]
    fn when_inspecting_repository_given_large_triage_context_then_keeps_all_original_tickets() {
        let count = 24;
        let tickets = TicketSet {
            tickets: (0..count)
                .map(|index| {
                    let blocked_by = if index == 0 {
                        Vec::new()
                    } else {
                        vec![format!("{}-slice", index)]
                    };
                    draft_ticket(
                        &format!("{}-slice", index + 1),
                        &format!("Slice {}", index + 1),
                        &blocked_by,
                    )
                })
                .collect(),
        };
        let classifications: Vec<TriageTicket> = (0..count)
            .map(|index| {
                let id = format!("{}-slice", index + 1);
                let title = format!("Slice {}", index + 1);
                if index == 0 {
                    ready_classification(&id, &title, &[])
                } else if index % 2 == 0 {
                    ready_classification(&id, &title, &[format!("{}-slice", index)])
                } else {
                    blocked_classification(
                        &id,
                        &title,
                        TriageState::NeedsInfo,
                        "The draft ticket names no test seam.",
                    )
                }
            })
            .collect();
        let report = TriageReport {
            tickets: classifications,
        };

        let script = Script::new(vec![reply(
            None,
            &[("finish_triage", finish_triage_call(report.tickets.clone()))],
        )]);
        let config = config(script.clone());

        match audit(&config, "", &specification(), &tickets) {
            Outcome::Handoff(handed) => {
                assert_eq!(handed.tickets.len(), count);
                assert_eq!(handed, report, "no classification may be truncated");
                for (draft, classification) in tickets.tickets.iter().zip(&handed.tickets) {
                    assert_eq!(classification.ticket_id, draft.id);
                    assert_eq!(classification.ticket_title, draft.title);
                    if classification.state.is_ready() {
                        assert_eq!(
                            classification
                                .agent_brief
                                .as_ref()
                                .expect("a ready classification carries a brief")
                                .blocked_by,
                            draft.blocked_by,
                            "the dependencies of {} must survive the largest fixture",
                            draft.id
                        );
                    }
                }
                let carried: usize = handed
                    .tickets
                    .iter()
                    .filter_map(|classification| classification.agent_brief.as_ref())
                    .map(|brief| brief.blocked_by.len())
                    .sum();
                assert_eq!(
                    carried,
                    count / 2 - 1,
                    "every dependency edge of a ready classification survives"
                );
            }
            other => panic!("expected the triage handoff, got {other:?}"),
        }

        let requests = script.requests();
        assert_eq!(requests.len(), 1);
        let context = user_message(&requests[0]);
        for ticket in &tickets.tickets {
            assert!(
                context.contains(&ticket.to_markdown()),
                "ticket {} was truncated out of the audit context",
                ticket.id
            );
        }
        assert_eq!(context.matches("id: ").count(), count);
    }

    /// A rejected payload never ends the pass: the rejection reaches the model
    /// as a tool result, and the corrected report in the next turn is what hands
    /// over.
    #[test]
    fn when_running_triage_given_rejected_handoff_then_corrects_it_without_ending_the_pass() {
        let specification = specification();
        let tickets = two_ticket_draft();
        let report = TriageReport {
            tickets: vec![
                ready_classification("1-first", "First slice", &[]),
                ready_classification("2-second", "Second slice", &["1-first".to_string()]),
            ],
        };
        let mut wrong_title = report.clone();
        wrong_title.tickets[1].ticket_title = "Second slice, reworded".to_string();

        let script = Script::new(vec![
            reply(
                None,
                &[("finish_triage", finish_triage_call(wrong_title.tickets))],
            ),
            reply(
                None,
                &[("finish_triage", finish_triage_call(report.tickets.clone()))],
            ),
        ]);
        let config = config(script.clone());

        match audit(&config, "", &specification, &tickets) {
            Outcome::Handoff(handed) => assert_eq!(handed, report),
            other => panic!("the corrected handoff must end the pass, got {other:?}"),
        }

        let requests = script.requests();
        assert_eq!(
            requests.len(),
            2,
            "a rejected payload must not end the pass"
        );
        let rejection = messages(&requests[1])
            .into_iter()
            .find(|(role, _)| role == "tool")
            .map(|(_, content)| content)
            .expect("the rejection reaches the model as a tool result");
        assert!(rejection.contains("2-second"), "{rejection}");
        assert!(rejection.contains("title"), "{rejection}");
    }

    /// The pass is unattended: a reply that asks the user something does not
    /// wait for an answer, so a handoff in the same turn still ends the pass.
    #[test]
    fn when_running_triage_given_a_reply_that_asks_the_user_then_hands_over_in_the_same_turn() {
        let specification = specification();
        let tickets = two_ticket_draft();
        let report = TriageReport {
            tickets: vec![
                ready_classification("1-first", "First slice", &[]),
                ready_classification("2-second", "Second slice", &["1-first".to_string()]),
            ],
        };

        let script = Script::new(vec![reply(
            Some("Should I classify the second slice as a feature?"),
            &[("finish_triage", finish_triage_call(report.tickets.clone()))],
        )]);
        let config = config(script.clone());

        match audit(&config, "", &specification, &tickets) {
            Outcome::Handoff(handed) => assert_eq!(handed, report),
            other => panic!("the unattended pass must hand over, got {other:?}"),
        }
        assert_eq!(
            script.requests().len(),
            1,
            "the pass must not wait for an answer inside the triage phase"
        );
    }

    /// A brief that carries one valid entry and one blank entry in either
    /// affected list is refused at the handoff boundary: the rejection comes
    /// back to the model as a tool error naming the ticket and the field, and
    /// the corrected report in the next turn is what ends the pass.
    #[test]
    fn when_handing_off_triage_given_mixed_blank_brief_entries_then_returns_tool_error() {
        let specification = specification();
        let tickets = two_ticket_draft();
        let valid = TriageReport {
            tickets: vec![
                ready_classification("1-first", "First slice", &[]),
                ready_classification("2-second", "Second slice", &["1-first".to_string()]),
            ],
        };

        let mut mixed_criteria = valid.clone();
        mixed_criteria.tickets[0]
            .agent_brief
            .as_mut()
            .expect("the ready helper carries a brief")
            .acceptance_criteria =
            vec!["First slice is implemented.".to_string(), "   ".to_string()];

        let mut mixed_boundaries = valid.clone();
        mixed_boundaries.tickets[0]
            .agent_brief
            .as_mut()
            .expect("the ready helper carries a brief")
            .out_of_scope = vec!["None.".to_string(), "\t".to_string()];

        let cases: [(&str, TriageReport, &str); 2] = [
            (
                "a blank acceptance criterion beside a valid one",
                mixed_criteria,
                "acceptance_criteria",
            ),
            (
                "a blank out-of-scope boundary beside a valid one",
                mixed_boundaries,
                "out_of_scope",
            ),
        ];

        for (case, mixed, field) in cases {
            let script = Script::new(vec![
                reply(
                    None,
                    &[("finish_triage", finish_triage_call(mixed.tickets.clone()))],
                ),
                reply(
                    None,
                    &[("finish_triage", finish_triage_call(valid.tickets.clone()))],
                ),
            ]);
            let config = config(script.clone());

            match audit(&config, "", &specification, &tickets) {
                Outcome::Handoff(handed) => assert_eq!(handed, valid),
                other => panic!("{case}: the corrected report must end the pass, got {other:?}"),
            }

            let requests = script.requests();
            assert_eq!(
                requests.len(),
                2,
                "{case}: a blank brief entry is a tool error, not the end of the pass"
            );
            let rejection = messages(&requests[1])
                .into_iter()
                .find(|(role, _)| role == "tool")
                .map(|(_, content)| content)
                .expect("the rejection reaches the model as a tool result");
            assert!(
                rejection.contains("1-first"),
                "{case}: must name the ticket: {rejection}"
            );
            assert!(
                rejection.contains(field),
                "{case}: must name the failing field: {rejection}"
            );
        }
    }

    /// A pass that ends without the handoff, or that fails outright, produces no
    /// report at all: the caller has nothing to accept as a triage result.
    #[test]
    fn when_running_triage_given_no_handoff_or_failure_then_reports_no_report() {
        let specification = specification();
        let tickets = two_ticket_draft();

        let script = Script::new(vec![reply(Some("I will classify these later."), &[])]);
        let agent = config(script.clone());
        match audit(&agent, "", &specification, &tickets) {
            Outcome::TurnEnd => {}
            other => panic!("a turn without the handoff must produce no report, got {other:?}"),
        }
        assert_eq!(script.requests().len(), 1);

        let unreachable = config(Arc::new(Unreachable));
        match audit(&unreachable, "", &specification, &tickets) {
            Outcome::Failed(kind) => assert_eq!(kind.label(), "transport error"),
            other => panic!("a failed pass must produce no report, got {other:?}"),
        }
    }

    /// The triage pass designs rather than runs, so hybrid mode serves it from
    /// the giga provider while the mid provider keeps the code. Summary is not
    /// mode-routed: it always uses the dedicated Smoll provider.
    #[test]
    fn when_routing_triage_given_hybrid_mode_then_uses_the_giga_provider() {
        let agent = config(Arc::new(Unreachable))
            .with_giga(GIGA_URL, "giga-model", Some("giga-key".to_string()))
            .with_mode(crate::llm::Mode::Hybrid);

        assert_eq!(agent.provider(Phase::Triage).name(), "responses");
        assert_eq!(agent.provider_role(Phase::Triage), "giga");
        assert_eq!(agent.provider(Phase::Code).name(), "chat-completions");
        assert_eq!(agent.provider_role(Phase::Code), "mid");
        assert_eq!(agent.provider(Phase::Summary).name(), "anthropic-messages");
        assert_eq!(agent.provider_role(Phase::Summary), "smoll");
    }
}
