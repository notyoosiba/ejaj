//! The agentic tool loop the four agents run on.
//!
//! One copy of the loop keeps assistant-message replay, the status line, and
//! the `tool_calls` bookkeeping of `drop_calls_from` correct in a single place;
//! the agents differ only in which control tool ends them.

use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use crate::Config;
use crate::api::{CallError, Message, Role, ToolCall};
use crate::plan_document::Plan;
use crate::retry;
use crate::review::ReviewReport;
use crate::specification::Specification;
use crate::status;
use crate::tickets::TicketSet;
use crate::tools::{
    MAX_STEPS, Phase, STEP_WARN_REMAINING, Source, TOOL_OUTPUT_LIMIT_BYTES, Tool, ToolContext,
};
use crate::triage::TriageReport;

/// Upper bound on the bytes of one tool result kept in the transcript, aligned
/// with [`crate::script::OUTPUT_LIMIT_BYTES`] (16 KiB). A chatty command can
/// produce far more output than the model needs, and the full result would then
/// dominate the context.
const TOOL_RESULT_BYTES: usize = TOOL_OUTPUT_LIMIT_BYTES;
/// The provider-prepared transcript size above which the loop runs Tier 1
/// compaction before the ordinary request: six newest tool-call steps are kept
/// whole, older tool results become one-line stubs and older reasoning is
/// cleared.
const TIER_ONE_CONTEXT_BYTES: usize = 400 * 1024;

/// The provider-prepared transcript size above which the loop makes exactly one
/// tool-free handover-summary request and replaces the compacted middle with
/// its answer. At or below this, no summarization request is made.
const TIER_TWO_CONTEXT_BYTES: usize = 200 * 1024;

/// Hard ceiling on one encoded request body. The guard measures the whole
/// `Outgoing` body, tool schemas included, because that is what the endpoint
/// rejects; `Prepared::context_bytes` deliberately excludes the schemas. The
/// boundary is inclusive: exactly this many bytes is allowed.
const CONTEXT_GUARD_BYTES: usize = 4 * 1024 * 1024;

/// How many of the newest assistant tool-call steps Tier 1 keeps whole.
const RETAINED_TOOL_STEPS: usize = 6;

/// The marker every accepted handover summary must begin with.
const HANDOVER_SUMMARY_MARKER: &str = "[HANDOVER SUMMARY]";

/// The four labeled fields an accepted handover summary must carry. The
/// summary instruction asks for exactly these labels, and the validator refuses
/// a reply that drops any of them, so a partial handover never replaces the
/// compactable middle.
const HANDOVER_SUMMARY_LABELS: [&str; 4] = [
    "Completed/work context:",
    "Decisions:",
    "Unresolved work/blockers:",
    "Relevant findings:",
];

/// The instruction appended to the compacted history for the one tool-free
/// handover-summary request. The request advertises no tools, so the model can
/// answer with text only.
const HANDOVER_SUMMARY_INSTRUCTION: &str = "[HANDOVER SUMMARY REQUEST] The transcript above has been compacted. Write one concise handover summary so the same agent can continue the work without re-reading it. If the transcript already contains an earlier `[HANDOVER SUMMARY]`, merge its still-relevant context into a single consolidated summary rather than copying it verbatim. Begin the reply with `[HANDOVER SUMMARY]` and use these labeled lines: `Completed/work context:`, `Decisions:`, `Unresolved work/blockers:`, `Relevant findings:`. Do not call tools; reply with text only.";

/// How many consecutive truncation recoveries one invocation may request
/// before a still-truncated tool-free reply is accepted as the turn's end.
const MAX_TRUNCATION_RECOVERIES: usize = 2;

/// The concise instruction appended after a truncated tool-free reply. It asks
/// the model to continue from the partial reply in the same transcript and to
/// complete either its intended prose or the tool call it meant to make.
const TRUNCATION_RECOVERY_INSTRUCTION: &str = "The assistant reply above was cut off. Continue from that partial reply and complete the intended response or tool call. Do not repeat completed content.";

/// How many consecutive assistant turns may issue the same ordered tool calls
/// before the loop appends a redirect notice to the newest turn's last result.
const REPEATED_TOOL_CALL_WARN_THRESHOLD: usize = 3;

/// The first consecutive identical tool-call turn the loop stops before any of
/// its calls are parsed or dispatched.
const REPEATED_TOOL_CALL_ABORT_THRESHOLD: usize = 5;

/// The most bytes of the ordered canonical-arguments preview the redirect
/// notice carries. The cut never splits a UTF-8 code point.
const REPEATED_TOOL_CALL_PREVIEW_BYTES: usize = 500;

/// The stderr warning logged when a repeating turn is redirected.
const REPEATED_TOOL_CALL_WARNING: &str =
    "[WARN] repeated tool calls; asking the model to choose a different action";

/// The stderr warning logged when an over-repeating turn is stopped before
/// dispatch.
const REPEATED_TOOL_CALL_ABORT_WARNING: &str =
    "[WARN] repeated tool calls; stopping before the step limit";

/// The fixed leading text of the redirect notice appended to a repeating
/// turn's last delivered tool result.
const REPEATED_TOOL_CALL_REDIRECT: &str = "[WARN: repeated tool calls] Your last turns issued the same tool calls. Choose a different action instead of repeating them. Canonical arguments preview: ";

/// Why the tool loop stopped. Every exit carries its own meaning, so the caller
/// can report what actually happened instead of assuming success.
// The plan, specification and ticket-set payloads are large, but they are moved
// straight to the caller: boxing them would only change the types the spec fixes
// here.
#[allow(clippy::large_enum_variant)]
pub enum Stop {
    /// The model ended its turn without requesting a tool. It may be done, or
    /// it may be waiting for the user -- the API cannot tell the two apart, so
    /// the caller must not claim either.
    TurnEnd,
    /// The summary agent ended its turn: carries the final assistant text --
    /// `Some` when the turn had content, `None` when it had none at all, which
    /// is the difference between an absent answer and an empty one. Only
    /// [`Phase::Summary`] stops this way; every other phase would have ended on
    /// a tool call, so its own turn end keeps its own meaning.
    Summary(Option<String>),
    /// The loop broke off on an error or the step limit. The detail has already
    /// been printed by the loop; this carries the label for the summary line.
    Failed(FailureKind),
    /// The plan agent called `finish_plan`: the design is settled and the spec
    /// agent should take over. Carries the plan object the plan agent wrote in
    /// that call -- only that object, none of the interview around it.
    Handoff(Plan),
    /// The spec agent called `finish_spec`: the plan has become a technical
    /// specification for the tickets agent. Carries only the specification
    /// object the spec agent wrote in that call; markdown is rendered from it
    /// later.
    Spec(Specification),
    /// The tickets agent called `finish_tickets`: the specification has become
    /// an ordered ticket set. Carries only the set the tickets agent handed
    /// over in that call.
    Tickets(TicketSet),
    /// The review agent called `finish_review`: carries the review report for
    /// this round.
    Review(ReviewReport),
    /// The triage agent called `finish_triage`: carries the validated report
    /// that classifies every draft ticket of the set it was handed.
    Triage(TriageReport),
    /// The code agent called `task_complete`: the only end state that counts as
    /// a finished ticket.
    Complete,
}

/// Subdirectory under the active session directory where conversation
/// trajectories (`<phase>.traj.json`) are persisted.
const TRAJECTORY_DIRECTORY: &str = "trajectories";

/// Serializable snapshot of one agent-loop invocation's final transcript and
/// exit metadata.
#[derive(serde::Serialize)]
struct TrajectorySnapshot<'a> {
    phase: &'static str,
    role: &'static str,
    model: &'a str,
    steps_taken: usize,
    exit_status: &'static str,
    messages: &'a [Message],
}

impl Stop {
    fn exit_status_label(&self) -> &'static str {
        match self {
            Stop::TurnEnd => "turn_end",
            Stop::Summary(_) => "summary",
            Stop::Failed(kind) => kind.label(),
            Stop::Handoff(_) => "handoff",
            Stop::Spec(_) => "spec",
            Stop::Tickets(_) => "tickets",
            Stop::Review(_) => "review",
            Stop::Triage(_) => "triage",
            Stop::Complete => "complete",
        }
    }
}

/// What went wrong, for the caller's one-line summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    Api,
    Transport,
    Malformed,
    StepLimit,
    ContextGuard,
}

impl FailureKind {
    pub fn label(self) -> &'static str {
        match self {
            FailureKind::Api => "API error",
            FailureKind::Transport => "transport error",
            FailureKind::Malformed => "malformed response",
            FailureKind::StepLimit => "step limit reached",
            FailureKind::ContextGuard => "context guard reached",
        }
    }
}

// The transcript's spacing, framing and flush policy lives in the shared output
// coordinator ([`status::shared`]): the model's narration is written as
// [`status::OutputKind::Narration`] and a permanent call line as
// [`status::OutputKind::ToolCall`]. Because that memory belongs to the process
// rather than to this invocation, the spacing spans a whole run: a call line
// here is still spaced against narration an earlier invocation printed, and an
// invocation that writes nothing neither creates nor resets the remembered
// kind. The two payload formats -- [`status::agent_text`] for narration and
// [`status::call_line`] for a call -- are unchanged; only their destination
// and the separator in front of them moved out of the loop.

/// Run the agentic tool loop against `messages` until the model stops
/// requesting tools. In [`Phase::Plan`] the plan agent ends only on
/// `finish_plan` ([`Stop::Handoff`]); in [`Phase::Spec`] the spec agent ends
/// only on `finish_spec`, which carries the specification object
/// ([`Stop::Spec`]); in [`Phase::Tickets`] the tickets agent ends only on
/// `finish_tickets`, which carries the ticket set ([`Stop::Tickets`]); in
/// [`Phase::Triage`] the triage agent ends only on `finish_triage`, which
/// carries the validated triage report ([`Stop::Triage`]); in
/// [`Phase::Code`] the code agent ends only on `task_complete`
/// such, and the transcript stays in the caller's hands. [`Phase::Summary`] is
/// the one phase with no control tool: it ends on the model's own final turn,
/// whose text -- absent when the turn carried none -- comes back in
/// [`Stop::Summary`] for the caller to validate.
///
/// `source` is the typed upstream object of the pass, threaded straight into
/// [`Tool::parse`]: the spec pass carries the confirmed plan, the tickets
/// pass carries the specification and the triage pass carries the specification
/// together with the draft ticket set, so a handoff that dropped upstream
/// content is rejected as a tool result and the loop takes another iteration.
/// The plan and code passes carry [`Source::None`]. A transient failure (a transport
/// error, or 429/5xx) is retried a bounded number of times before it is
/// reported, with `std::thread::sleep` for the backoff.
#[allow(dead_code)]
pub(crate) fn run_agent_loop(
    messages: &mut Vec<Message>,
    config: &Config,
    phase: Phase,
    source: Source<'_>,
) -> Stop {
    run_agent_loop_in(
        messages,
        config,
        phase,
        source,
        InvocationContext::default(),
        None,
    )
}

/// The filesystem identity of one agent-loop invocation.
///
/// The context-free wrappers above keep their exact behavior: no root and no
/// session directory, so Bash resolves its root from the process working
/// directory and an oversized result has nowhere to spill. The pipeline passes
/// the root it is anchored to and the session directory of the current ask, so
/// every dispatch in that invocation -- plan, fresh downstream sessions, ticket
/// attempts, retries and later queue rounds -- writes under the same session.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct InvocationContext<'a> {
    /// The project root the invocation is anchored to, when a caller knows it.
    pub root: Option<&'a Path>,
    /// The session directory of this invocation, when one was supplied.
    pub session_dir: Option<&'a Path>,
}

impl<'a> InvocationContext<'a> {
    /// A context for one invocation rooted at `root` with artifacts and spills
    /// under `session_dir`.
    pub(crate) fn new(root: &'a Path, session_dir: &'a Path) -> InvocationContext<'a> {
        InvocationContext {
            root: Some(root),
            session_dir: Some(session_dir),
        }
    }
}

/// The same loop with an explicit invocation context, for callers that know the
/// project root and the session directory every dispatch belongs to.
pub(crate) fn run_agent_loop_with_context(
    messages: &mut Vec<Message>,
    config: &Config,
    phase: Phase,
    source: Source<'_>,
    invocation: InvocationContext<'_>,
) -> Stop {
    run_agent_loop_in(messages, config, phase, source, invocation, None)
}

/// The same loop with an optional ephemeral queue-contribution accumulator.
///
/// Ordinary agent turns pass `None`: every attempt and executed command is
/// still measured and displayed exactly as before, but nothing is added up. The
/// queue's timed ticket path passes a borrow of its per-round accumulator, so
/// the same already-measured durations feed the later queue accounting without
/// a second clock.
#[allow(dead_code)]
pub(crate) fn run_agent_loop_with_contributions(
    messages: &mut Vec<Message>,
    config: &Config,
    phase: Phase,
    source: Source<'_>,
    contributions: Option<&mut crate::timing::QueueContributions>,
) -> Stop {
    run_agent_loop_in(
        messages,
        config,
        phase,
        source,
        InvocationContext::default(),
        contributions,
    )
}

/// The loop body shared by every context-aware caller.
fn run_agent_loop_in(
    messages: &mut Vec<Message>,
    config: &Config,
    phase: Phase,
    source: Source<'_>,
    invocation: InvocationContext<'_>,
    contributions: Option<&mut crate::timing::QueueContributions>,
) -> Stop {
    let mut ignored_narration = None;
    run_agent_loop_capturing(
        messages,
        config,
        phase,
        source,
        invocation,
        contributions,
        &mut ignored_narration,
    )
}

/// One finished agent loop plus the last assistant narration it printed.
///
/// The queue's retry handover needs the narration the first attempt left the
/// user with, but every ordinary caller keeps the unchanged Stop result.
pub(crate) struct AgentAttempt {
    /// Why the loop returned.
    pub stop: Stop,
    /// The last printable assistant narration, if any turn carried one.
    pub narration: Option<String>,
    /// A bounded summary of the latest delivered tool-call/result pairs, when
    /// the attempt delivered any.
    pub tool_activity: Option<String>,
}

/// Run one loop and keep the last assistant narration it saw.
///
/// The narration is the same text the ordinary path prints; a missing or
/// whitespace-only reply never replaces an earlier narration, and a loop that
/// never narrated comes back with None.
#[allow(dead_code)]
pub(crate) fn run_agent_loop_attempt(
    messages: &mut Vec<Message>,
    config: &Config,
    phase: Phase,
    source: Source<'_>,
    contributions: Option<&mut crate::timing::QueueContributions>,
) -> AgentAttempt {
    run_agent_loop_attempt_in(
        messages,
        config,
        phase,
        source,
        InvocationContext::default(),
        contributions,
    )
}

/// The attempt-preserving loop with an explicit invocation context.
pub(crate) fn run_agent_loop_attempt_in(
    messages: &mut Vec<Message>,
    config: &Config,
    phase: Phase,
    source: Source<'_>,
    invocation: InvocationContext<'_>,
    contributions: Option<&mut crate::timing::QueueContributions>,
) -> AgentAttempt {
    let mut narration = None;
    let stop = run_agent_loop_capturing(
        messages,
        config,
        phase,
        source,
        invocation,
        contributions,
        &mut narration,
    );
    let tool_activity = summarize_tool_activity(messages);
    AgentAttempt {
        stop,
        narration,
        tool_activity,
    }
}

/// The provider and pricing identity one model turn reports on every attempt.
///
/// Config resolves role, model and input rate from the same mode-and-phase
/// split that picked the provider, so none can disagree with where the request
/// actually goes. They are resolved once per turn, so every retry reports the
/// same metadata and prices its label with the same selected rate.
struct TurnReport<'a> {
    role: &'static str,
    model: &'a str,
    input_price: Option<f64>,
    phase_label: &'static str,
}

/// Send one prepared request and decode its reply, retrying transient failures
/// within the same model turn.
///
/// One attempt is exactly the send and the provider's decode. Its label is set
/// before the blocking operation, the attempt is measured on its own, and the
/// completed attempt's report is emitted -- and the label cleared -- before the
/// result is inspected, so a retry warning can never precede its attempt's
/// report. The measured interval ends before the retry decision, so the warning
/// and backoff that follow are never part of the attempt. MAX_STEPS still
/// counts model turns: the attempt budget lives entirely inside this call.
// The failure stays the loop's existing Stop so the outer path reports it once.
#[allow(clippy::result_large_err)]
fn send_turn_with_retry(
    config: &Config,
    phase: Phase,
    provider: &dyn crate::llm::LlmProvider,
    prepared: &crate::llm::Prepared,
    report: &TurnReport<'_>,
    status: &mut status::Status,
    mut contributions: Option<&mut crate::timing::QueueContributions>,
) -> Result<Message, CallError> {
    let mut attempt = 1;
    loop {
        status.set(&status::thinking_label(
            phase,
            report.role,
            prepared.context_bytes,
            report.input_price,
        ));
        // One attempt is exactly the send and the provider's decode. The
        // measured interval ends before the retry decision, so the warning and
        // backoff that follow are never part of the attempt.
        let (result, elapsed) = crate::timing::measured(|| match config.send(&prepared.outgoing) {
            Ok(raw) => provider.decode(raw),
            Err(error) => Err(error),
        });
        if let Some(contributions) = contributions.as_deref_mut() {
            contributions.record_model_attempt(elapsed);
        }
        // The compact report follows the completed attempt: the label is erased
        // first, then one metadata line carries this attempt's own measured
        // duration and the prepared request's context bytes. It lands before
        // the result is inspected, so a retry warning that follows can never
        // precede its attempt's report.
        status.clear();
        let _ = status::shared().thinking_log(
            report.role,
            report.model,
            report.phase_label,
            prepared.context_bytes,
            elapsed,
        );
        match result {
            Ok(message) => return Ok(message),
            Err(error) => match retry::should_retry(&error, attempt) {
                Some(delay) => {
                    status.clear();
                    // A retry warning is a stderr diagnostic: one
                    // status::OutputKind::Log message, framed by the
                    // coordinator, on the same stream it always used.
                    let _ = status::shared().stderr_log(&format!(
                        "[WARN] attempt {attempt}/{} failed: {error}; retrying.",
                        retry::MAX_ATTEMPTS
                    ));
                    std::thread::sleep(delay);
                    attempt += 1;
                }
                None => return Err(error),
            },
        }
    }
}

/// What executing one assistant call group did to the loop.
// The Stop payloads are large handoff documents, exactly as they are in Stop.
#[allow(clippy::large_enum_variant)]
enum CallOutcome {
    /// Every ordinary call ran; the loop takes another iteration.
    Continue,
    /// A control call was dropped but the reply is waiting on the user: stop
    /// processing later calls and let the caller end the turn without a handoff.
    Suspend,
    /// A control call ended the loop with this stop.
    Stop(Stop),
}

/// One parsed call that either handed control off or produced a run result.
#[allow(clippy::large_enum_variant)]
enum ParsedCall {
    Handoff(CallOutcome),
    Run {
        display: String,
        result: Result<String, String>,
        elapsed: Option<Duration>,
    },
}

/// The per-turn state one assistant call group shares.
struct CallRunner<'a, 'b> {
    phase: Phase,
    source: Source<'a>,
    step: usize,
    invocation: InvocationContext<'a>,
    status: &'b mut status::Status,
    contributions: Option<&'b mut crate::timing::QueueContributions>,
    bash_calls: usize,
}

impl CallRunner<'_, '_> {
    /// Run every call in order, returning as soon as a control call ends the
    /// turn. A rejected call still delivers its error result and continues.
    fn run(
        &mut self,
        messages: &mut Vec<Message>,
        assistant_index: usize,
        tool_calls: &[ToolCall],
        waiting: bool,
    ) -> CallOutcome {
        for call in tool_calls {
            match self.call(messages, assistant_index, call, waiting) {
                CallOutcome::Continue => {}
                outcome => return outcome,
            }
        }
        CallOutcome::Continue
    }

    /// Parse and dispatch one call. A parse failure is not fatal: it becomes the
    /// tool result, so the model can fix its arguments on the next step. The
    /// display and its optional execution duration are owned because the call
    /// line is drawn only after the tool has run.
    fn call(
        &mut self,
        messages: &mut Vec<Message>,
        assistant_index: usize,
        call: &ToolCall,
        waiting: bool,
    ) -> CallOutcome {
        let (display, result, elapsed) = match Tool::parse(
            &call.function.name,
            &call.function.arguments,
            self.phase,
            self.source,
        ) {
            Ok(tool) => match self.parsed(messages, assistant_index, call, tool, waiting) {
                ParsedCall::Handoff(outcome) => return outcome,
                ParsedCall::Run {
                    display,
                    result,
                    elapsed,
                } => (display, result, elapsed),
            },
            Err(error) => self.reject(call, error),
        };
        self.deliver(messages, call, display, result, elapsed);
        CallOutcome::Continue
    }

    /// Dispatch a successfully parsed call: a control tool hands off exactly as
    /// it always did, anything else runs as a command.
    fn parsed(
        &mut self,
        messages: &mut [Message],
        assistant_index: usize,
        call: &ToolCall,
        tool: Tool,
        waiting: bool,
    ) -> ParsedCall {
        match tool {
            Tool::FinishPlan(plan) if self.phase == Phase::Plan => {
                let outcome = self.handoff(
                    messages,
                    assistant_index,
                    call,
                    waiting,
                    Stop::Handoff(plan),
                );
                ParsedCall::Handoff(outcome)
            }
            Tool::FinishSpec(spec) if self.phase == Phase::Spec => {
                let outcome =
                    self.handoff(messages, assistant_index, call, waiting, Stop::Spec(spec));
                ParsedCall::Handoff(outcome)
            }
            Tool::FinishTickets(tickets) if self.phase == Phase::Tickets => {
                let outcome = self.handoff(
                    messages,
                    assistant_index,
                    call,
                    waiting,
                    Stop::Tickets(tickets),
                );
                ParsedCall::Handoff(outcome)
            }
            Tool::FinishReview(review) if self.phase == Phase::Review => {
                let outcome = self.handoff(
                    messages,
                    assistant_index,
                    call,
                    waiting,
                    Stop::Review(review),
                );
                ParsedCall::Handoff(outcome)
            }
            Tool::FinishTriage(report) if self.phase == Phase::Triage => {
                let outcome = self.handoff(
                    messages,
                    assistant_index,
                    call,
                    waiting,
                    Stop::Triage(report),
                );
                ParsedCall::Handoff(outcome)
            }
            Tool::TaskComplete(_) if self.phase == Phase::Code => {
                self.status.clear();
                ParsedCall::Handoff(CallOutcome::Stop(Stop::Complete))
            }
            tool => self.run_command(call, tool),
        }
    }

    /// Clear the status line, drop the handoff call and every call after it
    /// from the assistant group -- no tool result answers them -- and decide
    /// whether the turn hands off or waits for the user.
    fn handoff(
        &mut self,
        messages: &mut [Message],
        assistant_index: usize,
        call: &ToolCall,
        waiting: bool,
        stop: Stop,
    ) -> CallOutcome {
        self.status.clear();
        drop_calls_from(messages, assistant_index, &call.id);
        if waiting {
            CallOutcome::Suspend
        } else {
            CallOutcome::Stop(stop)
        }
    }

    /// Run one ordinary tool, measuring only bash and execute_script.
    fn run_command(&mut self, call: &ToolCall, tool: Tool) -> ParsedCall {
        let running_target = format_tool_target(self.step, &tool);
        let display = match &tool {
            Tool::ExecuteScript(args) => format!("js: {}", args.code),
            _ => tool.target().to_string(),
        };
        self.status.set(&status::running_label(&running_target));
        // A model-issued command is measured around the tool run itself: the
        // duration is carried to the permanent call echo as soon as the run
        // returns, for a success and for an error alike, and the result the
        // model receives is untouched. Nothing around the call is timed -- not
        // the request, the parse or the step -- and nothing that never reaches
        // Tool::run is timed either, so a rejected call and a control-tool
        // handoff carry no duration.
        let context = if matches!(&tool, Tool::Bash(_)) {
            self.bash_calls += 1;
            ToolContext {
                step: self.step,
                call_index: self.bash_calls,
            }
        } else {
            ToolContext {
                step: self.step,
                call_index: 1,
            }
        };
        let session_dir = self.invocation.session_dir;
        let run_tool = || match self.invocation.root {
            Some(root) => tool.run_with_context_in(root, self.phase, context, session_dir),
            None => tool.run_with_context(self.phase, context),
        };
        let (result, elapsed) = match &tool {
            Tool::Bash(_) | Tool::ExecuteScript(_) => {
                let (result, elapsed) = crate::timing::measured(run_tool);
                (result, Some(elapsed))
            }
            _ => (run_tool(), None),
        };
        if let (Some(contributions), Some(elapsed)) = (self.contributions.as_deref_mut(), elapsed) {
            contributions.record_executed_tool(elapsed);
        }
        if let Err(error) = &result {
            let _ = status::shared().stderr_log(&format!(
                "[WARN] {} failed: {}",
                call.function.name,
                first_error_line(error)
            ));
        }
        ParsedCall::Run {
            display,
            result,
            elapsed,
        }
    }

    /// Report a parse failure as the existing non-fatal rejected call: warn on
    /// stderr, show the function name, and hand the model the full error as the
    /// tool result with no execution duration.
    fn reject(
        &mut self,
        call: &ToolCall,
        error: String,
    ) -> (String, Result<String, String>, Option<Duration>) {
        let running_target = format!("[{}/{}] {}", self.step, MAX_STEPS, call.function.name);
        self.status.set(&status::running_label(&running_target));
        // A rejected call is visible: the retry is otherwise invisible on
        // screen. Only the first line is printed -- the model still gets the
        // full message as the tool result -- and the leading [ERROR] tag is
        // stripped so this reads as a warning, not a second error. It goes to
        // stderr through the coordinator, as a log.
        let reason = first_error_line(&error);
        let _ = status::shared().stderr_log(&format!(
            "[WARN] {} rejected: {}",
            call.function.name, reason
        ));
        // The function name is the display of a call that never ran, and it
        // carries no duration.
        (call.function.name.clone(), Err(error), None)
    }

    /// Draw the permanent call line and append the capped, step-aware result
    /// the model receives.
    fn deliver(
        &mut self,
        messages: &mut Vec<Message>,
        call: &ToolCall,
        display: String,
        result: Result<String, String>,
        elapsed: Option<Duration>,
    ) {
        self.status.clear();
        // The call itself is the only thing the user sees. The output --
        // however much of it a chatty command produces -- goes back to the
        // model alone, so the screen stays a transcript of what was run. An
        // executed command's duration rides on this same line; a rejected call
        // has none. Two calls in a row stay adjacent; the coordinator adds the
        // blank line only when the previous message was narration.
        let line = status::call_line(self.step, MAX_STEPS, &display, elapsed);
        let _ = status::shared().tool_call(&line);

        let text = result.unwrap_or_else(|error| error);
        let capped = with_step_warning(cap_tool_result(text), self.step);
        messages.push(Message::tool(&call.id, capped));
    }
}

/// The loop body shared by every caller. The caller supplies the slot that
/// receives the last printable assistant narration.
fn run_agent_loop_capturing(
    messages: &mut Vec<Message>,
    config: &Config,
    phase: Phase,
    source: Source<'_>,
    invocation: InvocationContext<'_>,
    contributions: Option<&mut crate::timing::QueueContributions>,
    last_narration: &mut Option<String>,
) -> Stop {
    let (stop, steps_taken) = run_agent_loop_inner(
        messages,
        config,
        phase,
        source,
        invocation,
        contributions,
        last_narration,
    );
    if let Err(error) = save_trajectory(invocation, config, phase, steps_taken, &stop, messages) {
        let _ =
            status::shared().stderr_log(&format!("[WARN] could not persist trajectory: {error}"));
    }
    stop
}

fn run_agent_loop_inner(
    messages: &mut Vec<Message>,
    config: &Config,
    phase: Phase,
    source: Source<'_>,
    invocation: InvocationContext<'_>,
    mut contributions: Option<&mut crate::timing::QueueContributions>,
    last_narration: &mut Option<String>,
) -> (Stop, usize) {
    // Drawn before every blocking operation below and erased before every
    // write; Drop covers the error returns and the final break. The size shown
    // comes from the request about to be sent.
    let mut status = status::Status::new();
    // How many recovery prompts this invocation has appended since the last
    // reply that carried a tool call. It is invocation-local: a new call to the
    // loop starts a new conversation state, and any tool-call reply proves the
    // model resumed, so the count begins again from zero.
    let mut consecutive_recoveries = 0usize;
    // The signature and consecutive count of the newest tool-call turn. It is
    // invocation-local, and a tool-free turn clears it.
    let mut repeated_tool_turns: Option<(Vec<(String, String)>, usize)> = None;
    for step_idx in 0..MAX_STEPS {
        let step = step_idx + 1;
        let provider = config.provider(phase);
        // Every ordinary attempt starts from the watermark-driven preparation:
        // Tier 1 compaction above 409,600 provider-prepared bytes, one tool-free
        // handover summary above 204,800 after Tier 1, and the hard encoded-body
        // ceiling before any send. The check recurs every turn, so a transcript
        // that grows past a watermark later is handled again rather than only
        // once.
        let mut prepared = match prepare_for_send(messages, config, phase, provider.as_ref()) {
            Ok(prepared) => prepared,
            Err(detail) => return (context_guard_stop(&detail), step_idx),
        };
        // The role, model, input rate and canonical phase label named on every
        // report and status line for this turn. Config resolves all of them
        // from the same mode-and-phase split that picked provider, so none can
        // disagree with where the request actually goes. They are resolved once
        // per turn, so every attempt in the retry helper below reports the same
        // metadata, carries the same prepared request's context byte count and
        // prices the label with the same selected rate. The transient label
        // above each send carries phase itself, the value this turn's provider
        // and prepared request were selected with, so the prefix names the
        // request actually going out on every attempt.
        let report = TurnReport {
            role: config.provider_role(phase),
            model: config.provider_model(phase),
            input_price: config.input_price(phase),
            phase_label: phase.label(),
        };
        // One turn is a send plus a decode. A transient failure is retried with
        // a visible warning -- otherwise a retry is invisible on screen -- and
        // the attempt budget stays inside this iteration, so MAX_STEPS still
        // counts model turns rather than HTTP calls.
        let assistant = match send_turn_with_retry(
            config,
            phase,
            provider.as_ref(),
            &prepared,
            &report,
            &mut status,
            contributions.as_deref_mut(),
        ) {
            Ok(assistant) => assistant,
            Err(error) if is_context_overflow_error(&error) => {
                match force_compact_on_overflow(messages, config, phase, provider.as_ref(), &error)
                {
                    Ok(Some(compacted_prepared)) => {
                        let _ = status::shared().stderr_log(&format!(
                            "[WARN] turn failed with context overflow ({error}); compacted history and retrying once."
                        ));
                        prepared = compacted_prepared;
                        match send_turn_with_retry(
                            config,
                            phase,
                            provider.as_ref(),
                            &prepared,
                            &report,
                            &mut status,
                            contributions.as_deref_mut(),
                        ) {
                            Ok(assistant) => assistant,
                            Err(retry_error) => {
                                return (report_call_error(&mut status, retry_error), step);
                            }
                        }
                    }
                    Ok(None) => return (report_call_error(&mut status, error), step),
                    Err(detail) => return (context_guard_stop(&detail), step),
                }
            }
            Err(error) => return (report_call_error(&mut status, error), step),
        };

        // Whether this reply is the model speaking to the user or, in the
        // summary phase, the pass's own answer. The distinction is the tool
        // calls: an intermediate turn still calls tools and narrates, a
        // tool-free summary turn is the answer and stays off this path until
        // the caller has validated it.
        let has_tool_calls = assistant
            .tool_calls
            .as_ref()
            .is_some_and(|calls| !calls.is_empty());
        // Print the model's narration if present. Agent text is the model
        // speaking: status::agent_text picks the rendering for the stdout this
        // run actually has -- the Tokyo Night Markdown styling on a terminal,
        // the redirected prefix before every non-empty line otherwise. A tool
        // call's permanent echo is drawn only after the tool has run, and the
        // tool's output itself never reaches the screen. A reasoning-only reply
        // has no content to print, so the label simply stays up. The block is
        // handed to the shared coordinator as narration: the coordinator
        // supplies the blank line that separates it from a preceding call line,
        // and remembers the kind for whatever prints next.
        //
        // A summary-phase final answer is the one withheld reply: it is not
        // user-visible until the summary validator has accepted it, and the
        // accepted canonical text is published once by the pipeline's own
        // summary-publication path instead.
        if let Some(narration) =
            printable_reply_narration(phase, has_tool_calls, assistant.content.as_deref())
        {
            *last_narration = Some(narration.to_string());
            status.clear();
            let rendered = status::agent_text(narration);
            let _ = status::shared().narration(&rendered);
        }
        // A reply may be addressing the user -- asking a question, or asking
        // for confirmation -- rather than ending. The plan interview must then
        // wait for the answer, and a finish_plan in that same reply must not be
        // honoured: it would answer the question on the user's behalf. Only the
        // plan phase is a conversation; the spec, tickets and code phases are
        // unattended.
        let waiting = phase == Phase::Plan && asks_the_user(assistant.content.as_deref());

        // Replay the assistant message (reasoning_content and tool_calls
        // included) before answering its tool calls, as the API requires.
        let tool_calls = assistant.tool_calls.clone().unwrap_or_default();
        // Count this turn's ordered calls before anything is parsed or
        // dispatched, so a rejected call counts exactly like an executed one. A
        // tool-free turn clears the sequence instead of extending it.
        let (redirect_repeated_turn, abort_repeated_turn) =
            repeated_tool_turn_action(&mut repeated_tool_turns, &tool_calls);
        if abort_repeated_turn {
            // The response is already received, but none of its calls may run:
            // no result is appended and no further request is sent.
            let _ = status::shared().stderr_log(REPEATED_TOOL_CALL_ABORT_WARNING);
            return (Stop::Failed(FailureKind::StepLimit), step);
        }
        messages.push(assistant);
        // Where that message landed. Its tool results are appended after it, so
        // a control-tool exit below cannot simply look at the last element.
        let assistant_index = messages.len() - 1;

        if tool_calls.is_empty() {
            // A tool-free reply that the provider stopped at its output limit
            // is not a finished turn: append one concise instruction after the
            // partial assistant message and continue in the same transcript.
            // At most two consecutive prompts are allowed; after that the
            // still-truncated reply is accepted as the phase's own end. At the
            // final available step the appended prompt is never sent, so the
            // existing step limit still governs.
            if messages[assistant_index].truncated
                && consecutive_recoveries < MAX_TRUNCATION_RECOVERIES
            {
                messages.push(Message::user(TRUNCATION_RECOVERY_INSTRUCTION));
                consecutive_recoveries += 1;
                continue;
            }
            // The summary phase has no control tool to end it, so the model's
            // own last turn is the answer: its text -- absent when the turn
            // carried none -- is handed to the caller exactly as it was
            // written, and normalized and validated there.
            if phase == Phase::Summary {
                let answer = messages[assistant_index].content.clone();
                return (Stop::Summary(answer), step);
            }
            return (Stop::TurnEnd, step);
        }

        // Any tool-call reply -- truncated or not -- shows the model resumed
        // its work, so the consecutive recovery count starts over.
        consecutive_recoveries = 0;

        let mut runner = CallRunner {
            phase,
            source,
            step,
            invocation,
            status: &mut status,
            contributions: contributions.as_deref_mut(),
            bash_calls: 0,
        };
        let outcome = runner.run(messages, assistant_index, &tool_calls, waiting);
        if redirect_repeated_turn {
            // The calls have now delivered their results; point the model at the
            // repetition. A turn that delivered no result still gets the warning,
            // it simply has nothing to annotate.
            let _ = status::shared().stderr_log(REPEATED_TOOL_CALL_WARNING);
            annotate_last_tool_result(messages, assistant_index, &tool_turn_signature(&tool_calls));
        }
        if let CallOutcome::Stop(stop) = outcome {
            return (stop, step);
        }

        if waiting {
            // The reply asked the user something, so the turn belongs to the
            // user: return to the plan prompt and let the answer come back as
            // the next user message.
            return (Stop::TurnEnd, step);
        }
    }

    let _ = status::shared().stderr_log("[WARN] step limit reached; the task may be incomplete");
    (Stop::Failed(FailureKind::StepLimit), MAX_STEPS)
}

/// Persist the conversation trajectory to `<session-dir>/trajectories/<phase>.traj.json`
/// when an `.ejaj/` session directory is supplied.
fn save_trajectory(
    invocation: InvocationContext<'_>,
    config: &Config,
    phase: Phase,
    steps_taken: usize,
    stop: &Stop,
    messages: &[Message],
) -> Result<(), String> {
    if phase == Phase::Plan && !matches!(stop, Stop::Handoff(_)) {
        return Ok(());
    }
    let Some(session_dir) = invocation.session_dir else {
        return Ok(());
    };
    let base = match invocation.root {
        Some(root) if !session_dir.is_absolute() => root.join(session_dir),
        _ => session_dir.to_path_buf(),
    };
    if crate::tools::ejaj_ancestor(&base).is_none() {
        return Ok(());
    }
    let directory = base.join(TRAJECTORY_DIRECTORY);
    std::fs::create_dir_all(&directory)
        .map_err(|e| format!("could not create the trajectories directory: {e}"))?;
    crate::tools::ignore_run_artifacts(&directory)
        .map_err(|e| format!("could not prepare the self-ignoring .ejaj directory: {e}"))?;

    let snapshot = TrajectorySnapshot {
        phase: phase.label(),
        role: config.provider_role(phase),
        model: config.provider_model(phase),
        steps_taken,
        exit_status: stop.exit_status_label(),
        messages,
    };
    let encoded = serde_json::to_vec_pretty(&snapshot)
        .map_err(|e| format!("could not serialize trajectory JSON: {e}"))?;

    let phase_label = phase.label();
    for suffix in 0..10_000usize {
        let file_name = if suffix == 0 {
            format!("{phase_label}.traj.json")
        } else {
            format!("{phase_label}-{suffix}.traj.json")
        };
        let path = directory.join(&file_name);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                if let Err(e) = std::io::Write::write_all(&mut file, &encoded) {
                    let _ = std::fs::remove_file(&path);
                    return Err(format!("could not write trajectory file: {e}"));
                }
                return Ok(());
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("could not create trajectory file: {e}")),
        }
    }
    Err("could not create a unique trajectory file after 10000 attempts".to_string())
}

/// Erase the status line, print the failure exactly as it was before the
/// provider seam landed, and name the label the caller will summarise with.
/// Both the send failure and the decode failure go through here, so every
/// `CallError` maps to one stderr diagnostic -- routed through the coordinator
/// as a log, on the stream it always used -- and one [`FailureKind`].
fn report_call_error(status: &mut status::Status, error: CallError) -> Stop {
    status.clear();
    match error {
        CallError::Transport(message) => {
            let _ = status::shared().stderr_log(&format!("[ERROR] {}", message));
            Stop::Failed(FailureKind::Transport)
        }
        CallError::Api { status, message } => {
            let _ = status::shared().stderr_log(&format!("[ERROR] {} {}", status, message));
            Stop::Failed(FailureKind::Api)
        }
        CallError::Malformed(detail) => {
            let _ = status::shared().stderr_log(&format!("[ERROR] Malformed response: {}", detail));
            Stop::Failed(FailureKind::Malformed)
        }
    }
}

/// The block of one assistant reply that may reach the ordinary narration
/// output, or `None` when the reply must stay off it.
///
/// A summary-phase turn that requested no tool is the pass's answer, not
/// narration: its text is not user-visible until the summary validator has
/// accepted it, and the accepted canonical text is published exactly once
/// through the pipeline's own summary-publication path. Every other reply keeps
/// its narration eligibility -- an intermediate summary turn that still calls
/// tools (the inspection steps on the way to the answer), and a final turn in
/// any non-summary phase -- so nothing but the unvalidated summary final is
/// withheld.
///
/// Missing and whitespace-only content is skipped by
/// [`status::printable_narration`], exactly as it always was, so a withheld
/// reply and an empty one are both no message to the coordinator.
fn printable_reply_narration(
    phase: Phase,
    has_tool_calls: bool,
    content: Option<&str>,
) -> Option<&str> {
    if phase == Phase::Summary && !has_tool_calls {
        return None;
    }
    status::printable_narration(content)
}

/// Whether an assistant reply is addressing the user rather than ending.
///
/// The plan interview is a conversation, but the wire format cannot tell a
/// turn that is done from one that is waiting for an answer. The loop
/// therefore reads the reply's own text: a question mark, the prompt's
/// `### Q<n>` question heading, or a request for the user's confirmation or
/// input all mean the model is speaking to the user. The plan loop then
/// returns to `plan>>` -- and refuses to honour a `finish_plan` sent in the
/// same reply.
///
/// The marker list is deliberately short and literal. A false positive only
/// costs one extra prompt turn, while a false negative would let the
/// interview answer its own question and hand over a design the user never
/// confirmed.
fn asks_the_user(content: Option<&str>) -> bool {
    let Some(text) = content else {
        return false;
    };
    if text.contains('?') {
        return true;
    }
    let lowered = text.to_lowercase();
    const MARKERS: [&str; 11] = [
        "### q",
        "please confirm",
        "confirm this",
        "confirm the plan",
        "confirm whether",
        "let me know",
        "your confirmation",
        "your input",
        "your answer",
        "waiting for your",
        "await your",
    ];
    MARKERS.iter().any(|marker| lowered.contains(marker))
}

/// Format the running-label target for `tool` at 1-based `step`:
/// `[step/100] <cmd>` for `bash` and `[step/100] js: <code>` for
/// `execute_script`. The permanent call line is built separately by
/// [`status::call_line`], which places the step counter and the inline
/// duration itself.
fn format_tool_target(step: usize, tool: &Tool) -> String {
    match tool {
        Tool::ExecuteScript(args) => format!("[{step}/{MAX_STEPS}] js: {}", args.code),
        _ => format!("[{step}/{MAX_STEPS}] {}", tool.target()),
    }
}

/// How many of the newest delivered tool-call/result pairs the attempt trail
/// keeps, in their original chronological order.
const TOOL_ACTIVITY_PAIRS: usize = 3;

/// The largest raw argument string parsed as JSON for a call preview.
const TOOL_ACTIVITY_JSON_BYTES: usize = 16_384;

/// The most raw argument bytes inspected for the fallback preview.
const TOOL_ACTIVITY_FALLBACK_BYTES: usize = 512;

/// The most bytes one complete call preview may carry, ellipsis included.
const TOOL_ACTIVITY_PREVIEW_BYTES: usize = 160;

/// The most bytes one error status may carry, ellipsis and spill footer
/// included.
const TOOL_ACTIVITY_STATUS_BYTES: usize = 240;

/// The most bytes the complete activity trail may carry, ellipsis included.
const TOOL_ACTIVITY_TRAIL_BYTES: usize = 1_500;

/// The prefix of the line a spilled Bash digest appends after its output.
const SPILL_FOOTER_PREFIX: &str = "[full output saved to";

/// The prefix of the loop's step-budget warning line.
const STEP_WARNING_PREFIX: &str = "[WARN: step ";

/// The prefix of the loop's repeated-call warning line.
const REPEATED_WARNING_PREFIX: &str = "[WARN: repeated tool calls]";

/// Collapse consecutive whitespace so one preview or status stays on one line.
fn collapse_activity_text(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Bound one activity text to a UTF-8-safe prefix, marking every cut with an
/// ellipsis that stays inside the byte limit.
fn bound_activity_text(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    const ELLIPSIS: &str = "…";
    if limit < ELLIPSIS.len() {
        return String::new();
    }
    let end = floor_char_boundary(text, limit - ELLIPSIS.len());
    format!("{}{ELLIPSIS}", &text[..end])
}

/// Render one assistant tool call for the activity trail. The known Bash and
/// execute-script argument fields are unwrapped only when the raw arguments are
/// small enough to parse; every other shape falls back to a bounded raw prefix.
fn format_tool_activity_call(call: &ToolCall) -> String {
    let name = call.function.name.as_str();
    let arguments = call.function.arguments.as_str();
    let field = match name {
        "bash" => Some("command"),
        "execute_script" => Some("code"),
        _ => None,
    };
    let parsed = if arguments.len() <= TOOL_ACTIVITY_JSON_BYTES {
        serde_json::from_str::<serde_json::Value>(arguments)
            .ok()
            .and_then(|value| field.and_then(|field| value.get(field).cloned()))
            .and_then(|value| value.as_str().map(str::to_string))
    } else {
        None
    };
    let preview = match parsed {
        Some(text) => format!("{name}({})", collapse_activity_text(&text)),
        None => {
            let end =
                floor_char_boundary(arguments, arguments.len().min(TOOL_ACTIVITY_FALLBACK_BYTES));
            let fallback = collapse_activity_text(&arguments[..end]);
            if fallback.is_empty() || fallback == "{}" {
                format!("{name}()")
            } else {
                format!("{name}({fallback})")
            }
        }
    };
    bound_activity_text(&preview, TOOL_ACTIVITY_PREVIEW_BYTES)
}

/// The first non-empty trimmed line of a result, when there is one.
fn first_non_empty_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).find(|line| !line.is_empty())
}

/// Whether a line is one of the loop's injected trailing warnings.
fn is_loop_warning(line: &str) -> bool {
    line.starts_with(STEP_WARNING_PREFIX) || line.starts_with(REPEATED_WARNING_PREFIX)
}

/// Remove only contiguous loop-injected warning lines from the end of a result.
fn without_trailing_warnings(text: &str) -> String {
    let mut lines: Vec<&str> = text.lines().collect();
    while lines
        .last()
        .is_some_and(|line| is_loop_warning(line.trim()))
    {
        lines.pop();
    }
    lines.join("\n")
}

/// Find the spill footer line an oversized Bash digest appended, if any.
fn spill_footer(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|line| line.starts_with(SPILL_FOOTER_PREFIX))
        .map(str::to_string)
}

/// Remove the spill footer from the text used for status classification.
fn without_spill_footer(text: &str) -> String {
    text.lines()
        .filter(|line| !line.trim().starts_with(SPILL_FOOTER_PREFIX))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The first non-empty diagnostic line after a command-exit or timeout error's
/// first line, preferring the digest's explicit diagnostics marker.
fn first_diagnostic_after(text: &str, first: &str) -> Option<String> {
    let lines: Vec<&str> = text.lines().map(str::trim).collect();
    if let Some(marker) = lines.iter().position(|line| *line == "[diagnostics]") {
        return lines[marker + 1..]
            .iter()
            .find(|line| !line.is_empty())
            .map(|line| (*line).to_string());
    }
    let first_index = lines.iter().position(|line| *line == first)?;
    lines[first_index + 1..]
        .iter()
        .find(|line| !line.is_empty() && **line != "[diagnostics]")
        .map(|line| (*line).to_string())
}

/// Classify one error result into its bounded one-line status text.
fn error_activity_text(text: &str) -> String {
    let Some(first) = first_non_empty_line(text) else {
        return String::new();
    };
    if first == "[ERROR: execute_script failed]" {
        let detail = first_error_line(text);
        if detail.is_empty() || detail == first {
            return first.to_string();
        }
        return format!("{first}: {detail}");
    }
    if first.starts_with("[ERROR: exit code ") || first.starts_with("[ERROR: command timed out") {
        return match first_diagnostic_after(text, first) {
            Some(detail) => format!("{first} — {detail}"),
            None => first.to_string(),
        };
    }
    first_error_line(text).to_string()
}

/// Join one status text with an optional spill footer, keeping the footer as a
/// suffix and shortening diagnostic text first when a byte limit applies.
fn join_activity_status(text: &str, footer: Option<&str>, limit: Option<usize>) -> String {
    let text = collapse_activity_text(text);
    let footer = footer
        .map(collapse_activity_text)
        .filter(|footer| !footer.is_empty());
    match (footer, limit) {
        (None, Some(limit)) => bound_activity_text(&text, limit),
        (None, None) => text,
        (Some(footer), None) => {
            if text.contains(&footer) {
                text
            } else if text.is_empty() {
                footer
            } else {
                format!("{text} {footer}")
            }
        }
        (Some(footer), Some(limit)) => {
            if footer.len() >= limit {
                return bound_activity_text(&footer, limit);
            }
            let separator = if text.is_empty() { "" } else { " " };
            let budget = limit - footer.len() - separator.len();
            let head = if text.contains(&footer) && text.ends_with(&footer) {
                text[..text.len() - footer.len()].trim_end()
            } else {
                text.as_str()
            };
            let head = bound_activity_text(head, budget);
            if head.is_empty() {
                footer
            } else {
                format!("{head}{separator}{footer}")
            }
        }
    }
}

/// Summarize one delivered tool result for the activity trail.
fn format_tool_activity_status(content: Option<&str>) -> String {
    let Some(content) = content else {
        return "ok".to_string();
    };
    let footer = spill_footer(content);
    let without_warnings = without_trailing_warnings(content);
    let classification = without_spill_footer(&without_warnings);
    let trimmed = classification.trim();
    if trimmed.is_empty() {
        return join_activity_status("ok", footer.as_deref(), None);
    }
    if is_tool_result_stub(trimmed) {
        return join_activity_status(trimmed, footer.as_deref(), None);
    }
    if trimmed.starts_with("[ERROR") || trimmed.starts_with("Error:") {
        let text = error_activity_text(trimmed);
        return join_activity_status(&text, footer.as_deref(), Some(TOOL_ACTIVITY_STATUS_BYTES));
    }
    join_activity_status("ok", footer.as_deref(), None)
}

/// Summarize the last three delivered assistant tool-call/result pairs, in
/// chronological order. A call counts only when a later tool message carries
/// its exact id.
fn summarize_tool_activity(messages: &[Message]) -> Option<String> {
    let mut delivered: Vec<(&ToolCall, &Message)> = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        if message.role != Role::Assistant {
            continue;
        }
        let Some(calls) = message.tool_calls.as_deref() else {
            continue;
        };
        for call in calls {
            let result = messages[index + 1..].iter().find(|later| {
                later.role == Role::Tool && later.tool_call_id.as_deref() == Some(call.id.as_str())
            });
            if let Some(result) = result {
                delivered.push((call, result));
            }
        }
    }

    let start = delivered.len().saturating_sub(TOOL_ACTIVITY_PAIRS);
    let lines: Vec<String> = delivered[start..]
        .iter()
        .map(|(call, result)| {
            format!(
                "- {} -> {}",
                format_tool_activity_call(call),
                format_tool_activity_status(result.content.as_deref())
            )
        })
        .collect();
    if lines.is_empty() {
        return None;
    }
    let trail = lines.join("\n");
    Some(bound_activity_text(&trail, TOOL_ACTIVITY_TRAIL_BYTES))
}

/// Extract the first informative line of a tool error for stderr `[WARN]` logs.
fn first_error_line(error: &str) -> &str {
    let line = error
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && *line != "[ERROR: execute_script failed]")
        .unwrap_or_else(|| error.lines().next().unwrap_or("").trim());
    line.strip_prefix("[ERROR] ").unwrap_or(line)
}

/// Append a step-budget warning when `STEP_WARN_REMAINING` or fewer steps
/// remain, cutting the body first so the delivered result -- warning included --
/// never exceeds the cap and the whole warning survives at the tail.
fn with_step_warning(text: String, step: usize) -> String {
    let remaining = MAX_STEPS.saturating_sub(step);
    if remaining > STEP_WARN_REMAINING {
        return text;
    }

    let warning = format!(
        "[WARN: step {step}/{MAX_STEPS}; {remaining} steps remaining before the step limit]"
    );
    let budget = TOOL_RESULT_BYTES.saturating_sub(warning.len() + 1);
    let body = cap_tool_output(text, budget);
    format!("{body}\n{warning}")
}

/// Keep the head of one tool result, ending with a marker that names the bytes
/// the model did not see. A result within the cap is returned unchanged, and
/// the cut never splits a UTF-8 character. The returned value -- marker
/// included -- stays within the delivery cap.
fn cap_tool_result(text: String) -> String {
    cap_tool_output(text, TOOL_RESULT_BYTES)
}

/// Observe one assistant turn and report whether it is the redirect or abort
/// threshold of a consecutive identical tool-call sequence.
///
/// A tool-free turn is not part of a tool-call sequence, so it clears the
/// remembered signature. Every tool-call turn is counted before any call is
/// parsed or dispatched, so a rejected call counts exactly like an executed one.
/// The returned flags are (redirect, abort).
fn repeated_tool_turn_action(
    last: &mut Option<(Vec<(String, String)>, usize)>,
    tool_calls: &[ToolCall],
) -> (bool, bool) {
    if tool_calls.is_empty() {
        *last = None;
        return (false, false);
    }
    let signature = tool_turn_signature(tool_calls);
    let count = match last.take() {
        Some((previous, count)) if previous == signature => count + 1,
        _ => 1,
    };
    *last = Some((signature, count));
    (
        count == REPEATED_TOOL_CALL_WARN_THRESHOLD,
        count >= REPEATED_TOOL_CALL_ABORT_THRESHOLD,
    )
}

/// One turn's ordered tool-call signature: every call's function name and
/// canonical arguments, in emitted order. Tool-call ids and kinds are excluded.
fn tool_turn_signature(tool_calls: &[ToolCall]) -> Vec<(String, String)> {
    tool_calls
        .iter()
        .map(|call| {
            (
                call.function.name.clone(),
                canonical_arguments(&call.function.arguments),
            )
        })
        .collect()
}

/// Canonicalize one argument string. Valid JSON is re-serialized compactly with
/// every object's keys sorted recursively, preserving array order and every
/// value; a string that is not valid JSON is kept byte-for-byte.
fn canonical_arguments(arguments: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(arguments) {
        Ok(value) => {
            serde_json::to_string(&sort_json_keys(value)).unwrap_or_else(|_| arguments.to_string())
        }
        Err(_) => arguments.to_string(),
    }
}

/// Recursively sort every object's keys, keeping array order and every value.
fn sort_json_keys(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(object) => {
            let mut entries: Vec<(String, serde_json::Value)> = object.into_iter().collect();
            entries.sort_by(|left, right| left.0.cmp(&right.0));
            serde_json::Value::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| (key, sort_json_keys(value)))
                    .collect(),
            )
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.into_iter().map(sort_json_keys).collect())
        }
        other => other,
    }
}

/// The canonical argument strings of a tool-call signature, joined by ` | `
/// in call order and cut to REPEATED_TOOL_CALL_PREVIEW_BYTES at a UTF-8
/// character boundary.
fn canonical_arguments_preview(signature: &[(String, String)]) -> String {
    let mut preview = String::new();
    for (index, (_, arguments)) in signature.iter().enumerate() {
        if index > 0 {
            preview.push_str(" | ");
        }
        preview.push_str(arguments);
    }
    let end = floor_char_boundary(&preview, REPEATED_TOOL_CALL_PREVIEW_BYTES);
    preview.truncate(end);
    preview
}

/// Build the redirect notice for a repeating turn: the fixed instruction and
/// the ordered canonical arguments preview of its signature.
fn repeated_tool_call_notice(signature: &[(String, String)]) -> String {
    format!(
        "{REPEATED_TOOL_CALL_REDIRECT}{}",
        canonical_arguments_preview(signature)
    )
}

/// Append the redirect notice to one already-delivered tool result, cutting the
/// result first so the notice stays intact at the tail and the whole value
/// never exceeds TOOL_RESULT_BYTES.
fn with_repeated_tool_call_notice(result: String, notice: &str) -> String {
    let budget = TOOL_RESULT_BYTES.saturating_sub(notice.len() + 1);
    let body = cap_tool_output(result, budget);
    format!("{body}\n{notice}")
}

/// Append the redirect notice to the last tool result the given assistant turn
/// delivered. A turn that delivered none has nothing to annotate; the caller
/// logs the warning either way.
fn annotate_last_tool_result(
    messages: &mut [Message],
    assistant_index: usize,
    signature: &[(String, String)],
) {
    let notice = repeated_tool_call_notice(signature);
    let Some(result) = messages[assistant_index + 1..]
        .iter_mut()
        .rev()
        .find(|message| message.role == Role::Tool)
    else {
        return;
    };
    let delivered = result.content.take().unwrap_or_default();
    result.content = Some(with_repeated_tool_call_notice(delivered, &notice));
}

/// The shared final cap: retain a UTF-8-safe prefix of `text`, replace the
/// omission with a marker naming the exact omitted bytes, and keep the whole
/// value within `limit` bytes.
fn cap_tool_output(text: String, limit: usize) -> String {
    if text.len() <= limit {
        return text;
    }

    let mut keep = limit.min(text.len());
    loop {
        let end = floor_char_boundary(&text, keep);
        let omitted = text.len() - end;
        let marker = format!("\n[tool output truncated to {limit} bytes; {omitted} bytes omitted]");
        if end + marker.len() <= limit {
            let mut output = String::with_capacity(end + marker.len());
            output.push_str(&text[..end]);
            output.push_str(&marker);
            return output;
        }

        if end == 0 {
            return text[..floor_char_boundary(&text, limit)].to_string();
        }

        let excess = (end + marker.len()).saturating_sub(limit);
        keep = end.saturating_sub(excess.max(1));
    }
}

/// The largest index at or below `index` that is a UTF-8 character boundary.
fn floor_char_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// One compacted transcript plus the two boundaries the summary replacement
/// needs: everything before `prefix_end` is pinned byte-for-byte, everything
/// from `tail_start` on belongs to the retained six-step tail, and the span
/// between them is the compacted middle a handover summary may replace.
struct CompactedHistory {
    messages: Vec<Message>,
    prefix_end: usize,
    tail_start: usize,
}

/// Prepare one ordinary request, applying the recurring two-tier compaction and
/// the hard encoded-body ceiling.
///
/// Tool-call/result pairing is checked for every request, so an invalid history
/// is refused before a provider call whether or not compaction is due. A
/// transcript at or below Tier 1 is never mutated: the provider's prepared
/// history is returned unchanged after the body guard. Above Tier 1, the oldest
/// tool results are stubbed and old reasoning cleared, then the context is
/// measured again through the active provider. A context still above Tier 2
/// gets exactly one tool-free handover-summary request; its accepted text
/// replaces only the compacted middle. Every failure -- an unpaired history,
/// an oversized summary body, a summary error, malformed or empty summary text,
/// an invalid replacement or a replacement still above Tier 2 -- is returned as
/// the detail of a context-guard stop, so no oversized history is sent.
fn prepare_for_send(
    messages: &mut Vec<Message>,
    config: &Config,
    phase: Phase,
    provider: &dyn crate::llm::LlmProvider,
) -> Result<crate::llm::Prepared, String> {
    // Pairing is a precondition for every provider request, not just for
    // compaction: an invalid history must never leave the process.
    validate_tool_pairing(messages)?;

    let prepared = provider.prepare(messages, phase);
    if prepared.context_bytes <= TIER_ONE_CONTEXT_BYTES {
        if !fits_hard_body_guard(&prepared.outgoing.body) {
            return Err(format!(
                "the encoded request body is {} bytes, over the {CONTEXT_GUARD_BYTES}-byte hard guard",
                encoded_body_bytes(&prepared.outgoing.body)
            ));
        }
        return Ok(prepared);
    }

    let CompactedHistory {
        messages: compacted,
        prefix_end,
        tail_start,
    } = compact_history(messages)?;
    *messages = compacted;

    let mut prepared = provider.prepare(messages, phase);
    if !fits_hard_body_guard(&prepared.outgoing.body) {
        return Err(format!(
            "the encoded request body is still over the {CONTEXT_GUARD_BYTES}-byte hard guard after Tier 1 compaction"
        ));
    }

    if prepared.context_bytes > TIER_TWO_CONTEXT_BYTES {
        let summary = request_handover_summary(messages, config, phase, provider)?;
        *messages = replacement_history(messages, prefix_end, tail_start, summary)?;

        prepared = provider.prepare(messages, phase);
        if prepared.context_bytes > TIER_TWO_CONTEXT_BYTES {
            return Err(format!(
                "the handover summary left the prepared context at {} bytes, over the {TIER_TWO_CONTEXT_BYTES}-byte Tier 2 watermark",
                prepared.context_bytes
            ));
        }
        if !fits_hard_body_guard(&prepared.outgoing.body) {
            return Err(format!(
                "the encoded request body is still over the {CONTEXT_GUARD_BYTES}-byte hard guard after the handover summary"
            ));
        }
    }

    Ok(prepared)
}

/// Tier 1: keep the pinned prefix and the six newest assistant tool-call steps
/// and their results intact, clear reasoning from assistant messages before the
/// retained tail and replace every older tool result with a one-line stub that
/// names its function and target and counts the exact UTF-8 bytes omitted.
///
/// The whole history is checked for valid tool-call/result pairing first; an
/// unpaired history is refused rather than compacted into something a provider
/// would reject. The tool calls themselves and every `tool_call_id` are left
/// untouched.
fn compact_history(messages: &[Message]) -> Result<CompactedHistory, String> {
    validate_tool_pairing(messages)?;

    let prefix_end = pinned_prefix_end(messages);
    let tail_start = retained_tail_start(messages).max(prefix_end);
    let mut compacted = messages.to_vec();

    for message in compacted.iter_mut().take(tail_start) {
        match message.role {
            Role::Assistant => {
                message.reasoning_content = None;
            }
            Role::Tool => {
                let Some(content) = message.content.as_deref() else {
                    continue;
                };
                if is_tool_result_stub(content) {
                    continue;
                }
                let omitted = content.len();
                let id = message.tool_call_id.as_deref().unwrap_or("");
                let Some(call) = tool_call_by_id(messages, id) else {
                    return Err(format!(
                        "the transcript carries a tool result without its matching tool call: {id}"
                    ));
                };
                message.content = Some(tool_result_stub(call, omitted));
            }
            _ => {}
        }
    }

    Ok(CompactedHistory {
        messages: compacted,
        prefix_end,
        tail_start,
    })
}

/// The original system prompt and the first kickoff user message, pinned
/// byte-for-byte. Only those two turns are pinned: a later user turn is part of
/// the compactable middle even when no assistant turn separates them.
fn pinned_prefix_end(messages: &[Message]) -> usize {
    let mut end = 0;
    let mut system_pinned = false;
    let mut user_pinned = false;
    for (index, message) in messages.iter().enumerate() {
        match message.role {
            Role::System if !system_pinned => {
                system_pinned = true;
                end = index + 1;
            }
            Role::User if !user_pinned => {
                user_pinned = true;
                end = index + 1;
            }
            Role::System | Role::User => break,
            _ => break,
        }
    }
    end
}

/// The index where the retained six-step tail begins: the assistant message of
/// the sixth-newest tool-call step, the oldest such step when there are fewer,
/// or the end of the transcript when it holds no tool calls at all.
fn retained_tail_start(messages: &[Message]) -> usize {
    let steps: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, message)| {
            message.role == Role::Assistant
                && message
                    .tool_calls
                    .as_ref()
                    .is_some_and(|calls| !calls.is_empty())
        })
        .map(|(index, _)| index)
        .collect();

    if steps.len() > RETAINED_TOOL_STEPS {
        steps[steps.len() - RETAINED_TOOL_STEPS]
    } else {
        steps.first().copied().unwrap_or(messages.len())
    }
}

/// Validate that every assistant tool call has a unique, non-empty id,
/// every call is answered by a matching tool result, every result names a
/// real pending call, and no tool-call group is left partial or interrupted
/// by another turn.
///
/// Call ids are checked against the whole transcript, not just the group that
/// carries them: a provider cannot tell two calls with the same id apart, so a
/// reused id in a later group is as invalid as a repeat inside one group.
pub(crate) fn validate_tool_pairing(messages: &[Message]) -> Result<(), String> {
    let mut pending: Vec<String> = Vec::new();
    let mut seen_call_ids: HashSet<String> = HashSet::new();
    for message in messages {
        match message.role {
            Role::Assistant => {
                if !pending.is_empty() {
                    return Err(
                        "the transcript interrupts a tool-call group before all of its results"
                            .to_string(),
                    );
                }
                if let Some(calls) = &message.tool_calls {
                    for call in calls {
                        if call.id.is_empty() {
                            return Err(
                                "the transcript carries a tool call without an id".to_string()
                            );
                        }
                        if !seen_call_ids.insert(call.id.clone()) {
                            return Err(format!(
                                "the transcript carries a duplicate tool call id: {}",
                                call.id
                            ));
                        }
                        pending.push(call.id.clone());
                    }
                }
            }
            Role::Tool => {
                let Some(id) = message.tool_call_id.as_deref() else {
                    return Err(
                        "the transcript carries a tool result without a tool_call_id".to_string(),
                    );
                };
                let Some(position) = pending.iter().position(|pending_id| pending_id == id) else {
                    return Err(format!(
                        "the transcript carries a tool result without a matching tool call: {id}"
                    ));
                };
                pending.remove(position);
            }
            Role::System | Role::User => {
                if !pending.is_empty() {
                    return Err(
                        "the transcript leaves a tool-call group unanswered before a new turn"
                            .to_string(),
                    );
                }
            }
        }
    }
    if pending.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "the transcript leaves {} tool call(s) unanswered",
            pending.len()
        ))
    }
}

/// The tool call a result's `tool_call_id` names.
fn tool_call_by_id<'a>(messages: &'a [Message], id: &str) -> Option<&'a ToolCall> {
    messages.iter().find_map(|message| {
        message
            .tool_calls
            .as_ref()?
            .iter()
            .find(|call| call.id == id)
    })
}

/// The one-line stub Tier 1 leaves in place of an older tool result, naming the
/// tool function, its target and the exact UTF-8 bytes omitted.
fn tool_result_stub(call: &ToolCall, omitted: usize) -> String {
    format!(
        "[tool result omitted: {} target `{}`; {omitted} bytes omitted]",
        call.function.name,
        tool_result_target(call)
    )
}

/// A concise, single-line target for a tool call: the bash command or the
/// script's first characters, with line breaks flattened and the tail elided.
fn tool_result_target(call: &ToolCall) -> String {
    let argument = serde_json::from_str::<serde_json::Value>(&call.function.arguments)
        .ok()
        .and_then(|value| match call.function.name.as_str() {
            "bash" => value
                .get("command")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            "execute_script" => value
                .get("code")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            _ => None,
        })
        .unwrap_or_else(|| call.function.arguments.clone());

    let one_line = argument.replace(['\n', '\r', '\t'], " ");
    let mut end = one_line.len().min(80);
    while !one_line.is_char_boundary(end) {
        end -= 1;
    }
    if end < one_line.len() {
        format!("{}...", &one_line[..end])
    } else {
        one_line
    }
}

/// Whether a tool result already carries Tier 1's stub, so a later compaction
/// never overwrites the original byte count with the stub's own length.
fn is_tool_result_stub(text: &str) -> bool {
    text.starts_with("[tool result omitted:")
}

/// Ask the active phase's provider for one tool-free handover summary and
/// return the accepted assistant text. The request is never retried, advertises
/// no tools, and is not sent at all when its encoded body exceeds the hard
/// guard. Every failure becomes the detail of a context-guard stop.
fn request_handover_summary(
    compacted: &[Message],
    config: &Config,
    phase: Phase,
    provider: &dyn crate::llm::LlmProvider,
) -> Result<Message, String> {
    let mut summary_messages = compacted.to_vec();
    summary_messages.push(Message::user(HANDOVER_SUMMARY_INSTRUCTION));
    validate_tool_pairing(&summary_messages)?;

    let prepared = provider.prepare_tool_free(&summary_messages, phase);
    if !fits_hard_body_guard(&prepared.outgoing.body) {
        return Err(format!(
            "the handover summary request body is {} bytes, over the {CONTEXT_GUARD_BYTES}-byte hard guard; it was not sent",
            encoded_body_bytes(&prepared.outgoing.body)
        ));
    }

    let message = send_summary_with_retry(config, provider, &prepared.outgoing)?;

    if message
        .tool_calls
        .as_ref()
        .is_some_and(|calls| !calls.is_empty())
    {
        return Err(
            "the handover summary response requested a tool instead of returning text".to_string(),
        );
    }
    let Some(content) = message.content else {
        return Err("the handover summary response carried no text".to_string());
    };
    let content = content.trim();
    if content.is_empty() || !content.starts_with(HANDOVER_SUMMARY_MARKER) {
        return Err(format!(
            "the handover summary response did not begin with {HANDOVER_SUMMARY_MARKER}"
        ));
    }
    for label in HANDOVER_SUMMARY_LABELS {
        if !content.contains(label) {
            return Err(format!(
                "the handover summary response is missing the required `{label}` field"
            ));
        }
    }

    Ok(Message {
        role: Role::Assistant,
        content: Some(content.to_string()),
        tool_calls: None,
        tool_call_id: None,
        reasoning_content: None,
        extra: serde_json::Map::new(),
        truncated: false,
    })
}

/// Detect whether an API error from a provider is a context-window, payload-size,
/// or post-Tier-1 reasoning-field rejection that full compaction can recover from.
fn is_context_overflow_error(error: &CallError) -> bool {
    let CallError::Api { status, message } = error else {
        return false;
    };
    if *status == 413 {
        return true;
    }
    if !matches!(*status, 400 | 422) {
        return false;
    }
    let lowered = message.to_ascii_lowercase();
    const OVERFLOW_MARKERS: [&str; 8] = [
        "context_length_exceeded",
        "model_context_window_exceeded",
        "maximum context length",
        "prompt is too long",
        "context window",
        "too many tokens",
        "request_too_large",
        "missing reasoning_content",
    ];
    OVERFLOW_MARKERS
        .iter()
        .any(|marker| lowered.contains(marker))
}

/// Send the tool-free handover summary request with transient retry support.
fn send_summary_with_retry(
    config: &Config,
    provider: &dyn crate::llm::LlmProvider,
    outgoing: &crate::api::Outgoing,
) -> Result<Message, String> {
    let mut attempt = 1;
    loop {
        let result = match config.send(outgoing) {
            Ok(raw) => {
                let status = raw.status;
                match provider.decode(raw) {
                    Ok(message) => Ok(message),
                    Err(CallError::Transport(detail)) if (200..300).contains(&status) => {
                        return Err(format!(
                            "the handover summary response could not be decoded: transport error: {detail}"
                        ));
                    }
                    Err(error) => Err((true, error)),
                }
            }
            Err(error) => Err((false, error)),
        };
        match result {
            Ok(message) => return Ok(message),
            Err((from_decode, error)) => match retry::should_retry(&error, attempt) {
                Some(delay) => {
                    let _ = status::shared().stderr_log(&format!(
                        "[WARN] handover summary attempt {attempt}/{} failed: {error}; retrying.",
                        retry::MAX_ATTEMPTS
                    ));
                    std::thread::sleep(delay);
                    attempt += 1;
                }
                None => {
                    if from_decode {
                        return Err(format!(
                            "the handover summary response could not be decoded: {error}"
                        ));
                    }
                    return Err(format!("the handover summary request failed: {error}"));
                }
            },
        }
    }
}

/// Reactively compact `messages` once after a provider context-overflow or
/// post-Tier-1 reasoning-field error. Returns `Ok(None)` when the transcript
/// cannot be compacted any further, `Ok(Some(prepared))` when a smaller or
/// summarized transcript was prepared, or `Err(detail)` when compaction or
/// summary validation fails.
fn force_compact_on_overflow(
    messages: &mut Vec<Message>,
    config: &Config,
    phase: Phase,
    provider: &dyn crate::llm::LlmProvider,
    error: &CallError,
) -> Result<Option<crate::llm::Prepared>, String> {
    let before_bytes = provider.prepare(messages, phase).context_bytes;
    let CompactedHistory {
        messages: compacted,
        prefix_end,
        tail_start,
    } = compact_history(messages)?;
    let after_tier_one = provider.prepare(&compacted, phase);
    if !fits_hard_body_guard(&after_tier_one.outgoing.body) {
        return Err(format!(
            "the encoded request body is still over the {CONTEXT_GUARD_BYTES}-byte hard guard after Tier 1 compaction"
        ));
    }

    let needs_summary = match error {
        CallError::Api { message, .. }
            if message
                .to_ascii_lowercase()
                .contains("missing reasoning_content") =>
        {
            true
        }
        _ => {
            after_tier_one.context_bytes > TIER_TWO_CONTEXT_BYTES
                || after_tier_one.context_bytes >= before_bytes
        }
    };

    if needs_summary {
        if tail_start <= prefix_end {
            return Ok(None);
        }
        let summary = request_handover_summary(&compacted, config, phase, provider)?;
        let replaced = replacement_history(&compacted, prefix_end, tail_start, summary)?;
        let prepared = provider.prepare(&replaced, phase);
        if prepared.context_bytes > TIER_TWO_CONTEXT_BYTES {
            return Err(format!(
                "the handover summary left the prepared context at {} bytes, over the {TIER_TWO_CONTEXT_BYTES}-byte Tier 2 watermark",
                prepared.context_bytes
            ));
        }
        if !fits_hard_body_guard(&prepared.outgoing.body) {
            return Err(format!(
                "the encoded request body is still over the {CONTEXT_GUARD_BYTES}-byte hard guard after the handover summary"
            ));
        }
        *messages = replaced;
        return Ok(Some(prepared));
    }

    *messages = compacted;
    Ok(Some(after_tier_one))
}

/// Replace only the compacted middle with the accepted summary. The pinned
/// prefix and the retained six-step tail are copied unchanged, then the whole
/// replacement is checked for valid pairing before it can be sent.
fn replacement_history(
    compacted: &[Message],
    prefix_end: usize,
    tail_start: usize,
    summary: Message,
) -> Result<Vec<Message>, String> {
    let tail_start = tail_start.max(prefix_end);
    let mut replacement =
        Vec::with_capacity(prefix_end + 1 + compacted.len().saturating_sub(tail_start));
    replacement.extend_from_slice(&compacted[..prefix_end]);
    replacement.push(summary);
    replacement.extend_from_slice(&compacted[tail_start..]);
    validate_tool_pairing(&replacement)?;
    Ok(replacement)
}

/// Report a context-guard stop on the shared error stream and hand back the
/// stop the caller returns.
fn context_guard_stop(detail: &str) -> Stop {
    let _ = status::shared().stderr_log(&format!(
        "[ERROR] the context guard stopped the pass: {detail}"
    ));
    Stop::Failed(FailureKind::ContextGuard)
}

/// The encoded size of one outgoing body, the measure the context guard uses.
fn encoded_body_bytes(body: &serde_json::Value) -> usize {
    serde_json::to_string(body)
        .map(|json| json.len())
        .unwrap_or(0)
}

/// Whether an encoded body is within the hard ceiling. The boundary is
/// inclusive: exactly `CONTEXT_GUARD_BYTES` is permitted.
fn fits_hard_body_guard(body: &serde_json::Value) -> bool {
    encoded_body_bytes(body) <= CONTEXT_GUARD_BYTES
}

/// Drop the tool call with `call_id` -- and every call after it in the same
/// assistant message -- from `messages[assistant_index]`. Calls before it have
/// already been answered by tool results that stay in the transcript, so the
/// message is left with every remaining `tool_calls` entry accounted for and
/// the history stays valid for a later resume.
fn drop_calls_from(messages: &mut [Message], assistant_index: usize, call_id: &str) {
    let message = &mut messages[assistant_index];
    let now_empty = match message.tool_calls.as_mut() {
        Some(calls) => {
            match calls.iter().position(|call| call.id == call_id) {
                Some(keep) => calls.truncate(keep),
                // Should not happen (the id came from this message), but a
                // stale id must not leave the bogus call behind.
                None => calls.clear(),
            }
            calls.is_empty()
        }
        None => false,
    };
    if now_empty {
        message.tool_calls = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Auth, FunctionCall, Outgoing, RawResponse, Role, Transport};
    use crate::triage::{AgentBrief, TriageCategory, TriageState, TriageTicket};
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    /// The plan document every `finish_plan` in these tests carries. It parses,
    /// so the loop reaches its handoff decision instead of a parse error.
    const PLAN_MARKDOWN: &str = "# Typed plan handoff\n\n\
        ## 1. Decisions\n\n\
        1. **Add the document type**\n\n\
        A new `plan_document` module owns it.\n\n\
        ## 2. Defaults Chosen\n\n\
        - Keep the signature.\n\n\
        ## 3. Constraints\n\n\
        - No new dependencies.\n\n\
        ## 4. Open Questions\n\n\
        - Which flag spelling wins? The fact needed to settle it: the existing CLI conventions.\n";

    /// One scripted transport reply: a canned body to decode, or a failure the
    /// transport hands the loop exactly as a boundary would.
    enum Reply {
        Body(String),
        Error(CallError),
    }

    /// A transport that serves canned replies in order and records every
    /// request it saw -- the count and the encoded body -- so a test can assert
    /// both how many turns the loop took and what it offered the model in each
    /// one. Running out of replies fails the test: it means the loop sent a
    /// request the script did not expect.
    struct Script {
        replies: Mutex<Vec<Reply>>,
        requests: AtomicUsize,
        sent: Mutex<Vec<serde_json::Value>>,
        urls: Mutex<Vec<String>>,
        auths: Mutex<Vec<Auth>>,
    }

    impl Script {
        fn new(bodies: Vec<String>) -> Arc<Script> {
            Self::with_replies(bodies.into_iter().map(Reply::Body).collect())
        }

        /// A script whose every reply is a failure at the transport boundary:
        /// how a test drives the loop's retry and permanent-error paths without
        /// a network.
        fn with_errors(errors: Vec<CallError>) -> Arc<Script> {
            Self::with_replies(errors.into_iter().map(Reply::Error).collect())
        }

        fn with_replies(replies: Vec<Reply>) -> Arc<Script> {
            Arc::new(Script {
                replies: Mutex::new(replies),
                requests: AtomicUsize::new(0),
                sent: Mutex::new(Vec::new()),
                urls: Mutex::new(Vec::new()),
                auths: Mutex::new(Vec::new()),
            })
        }

        fn requests(&self) -> usize {
            self.requests.load(Ordering::Relaxed)
        }

        /// Every request body the loop sent, in order.
        fn sent(&self) -> Vec<serde_json::Value> {
            self.sent.lock().unwrap().clone()
        }

        /// The endpoint each request went to, in order.
        fn urls(&self) -> Vec<String> {
            self.urls.lock().unwrap().clone()
        }

        /// The authentication scheme each request carried, in order.
        fn auths(&self) -> Vec<Auth> {
            self.auths.lock().unwrap().clone()
        }
    }

    impl Transport for Script {
        fn send(&self, outgoing: &Outgoing) -> Result<RawResponse, CallError> {
            self.requests.fetch_add(1, Ordering::Relaxed);
            self.sent.lock().unwrap().push(outgoing.body.clone());
            self.urls.lock().unwrap().push(outgoing.url.clone());
            self.auths.lock().unwrap().push(outgoing.auth.clone());
            let mut replies = self.replies.lock().unwrap();
            assert!(
                !replies.is_empty(),
                "the loop made an unexpected extra request"
            );
            match replies.remove(0) {
                Reply::Body(body) => Ok(RawResponse { status: 200, body }),
                Reply::Error(error) => Err(error),
            }
        }
    }

    fn config(script: Arc<Script>) -> Config {
        Config::new(
            "http://script.test/v1/chat/completions",
            "test-model",
            Some("test-key".to_string()),
        )
        .with_smoll("https://smoll.test/v1/messages", "smoll-model", "smoll-key")
        .with_transport(script)
    }

    /// A hybrid configuration with distinct Mid, Giga and Smoll models, all
    /// routed through the one scripted transport.
    fn hybrid_config(script: Arc<Script>) -> Config {
        Config::new(
            "http://script.test/v1/chat/completions",
            "mid-model",
            Some("mid-key".to_string()),
        )
        .with_mode(crate::llm::Mode::Hybrid)
        .with_giga(
            "http://script.test/v1/responses",
            "giga-model",
            Some("giga-key".to_string()),
        )
        .with_smoll("https://smoll.test/v1/messages", "smoll-model", "smoll-key")
        .with_transport(script)
    }

    /// One isolated environment table, consulted by canonical variable name so
    /// configuration resolution runs without reading or mutating the process
    /// environment.
    fn environment(values: Vec<(&'static str, &'static str)>) -> impl Fn(&str) -> Option<String> {
        move |name| {
            values
                .iter()
                .find(|(variable, _)| *variable == name)
                .map(|(_, value)| (*value).to_string())
        }
    }

    /// A configuration resolved through the same environment-to-settings route
    /// production uses, with the given optional provider rates attached, then
    /// routed through the scripted transport. The rate values are strings so an
    /// invalid one can be exercised at the resolver boundary.
    fn priced_config(
        script: Arc<Script>,
        mid_price: Option<&'static str>,
        giga_price: Option<&'static str>,
        smoll_price: Option<&'static str>,
    ) -> Config {
        let mut values = vec![
            ("LLM_MID_URL", "http://script.test/v1/chat/completions"),
            ("LLM_MID_MODEL", "test-model"),
            ("LLM_MID_API_KEY", "test-key"),
            ("LLM_GIGA_URL", "http://script.test/v1/responses"),
            ("LLM_GIGA_MODEL", "giga-model"),
            ("LLM_GIGA_API_KEY", "giga-key"),
            ("LLM_SMOLL_MODEL", "smoll-model"),
            ("LLM_SMOLL_API_KEY", "smoll-key"),
        ];
        if let Some(rate) = mid_price {
            values.push(("LLM_MID_INPUT_PRICE_PER_MILLION_USD", rate));
        }
        if let Some(rate) = giga_price {
            values.push(("LLM_GIGA_INPUT_PRICE_PER_MILLION_USD", rate));
        }
        if let Some(rate) = smoll_price {
            values.push(("LLM_SMOLL_INPUT_PRICE_PER_MILLION_USD", rate));
        }
        let environment = environment(values);
        Config::from_environment(&environment)
            .expect("the priced test environment must resolve")
            .with_transport(script)
    }

    /// One responses-API text reply, the wire shape the giga provider decodes.
    fn responses_text(text: &str) -> String {
        serde_json::json!({
            "output": [{
                "type": "message",
                "content": [{ "type": "output_text", "text": text }],
            }]
        })
        .to_string()
    }

    /// One scripted native Anthropic Messages reply: its text block when it
    /// carries one, then one tool_use block per `(name, arguments)` call.
    fn native_reply(content: Option<&str>, calls: &[(&str, String)]) -> String {
        native_reply_with_ids(content, calls, 0)
    }

    /// Like [`native_reply`], but numbering its tool_use ids from `first_id`
    /// so a scripted multi-turn pass never reuses an id across assistant turns.
    fn native_reply_with_ids(
        content: Option<&str>,
        calls: &[(&str, String)],
        first_id: usize,
    ) -> String {
        let mut blocks = Vec::new();
        if let Some(text) = content {
            blocks.push(serde_json::json!({ "type": "text", "text": text }));
        }
        for (index, (name, arguments)) in calls.iter().enumerate() {
            let input = serde_json::from_str(arguments).unwrap_or_else(|_| serde_json::json!({}));
            blocks.push(serde_json::json!({
                "type": "tool_use",
                "id": format!("toolu_{}", first_id + index),
                "name": name,
                "input": input,
            }));
        }
        serde_json::json!({ "content": blocks }).to_string()
    }

    /// A native Anthropic text answer.
    fn native_text(text: &str) -> String {
        native_reply(Some(text), &[])
    }

    /// One chat-completions assistant message: optional `content` and the given
    /// `(name, arguments)` tool calls.
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

    /// Like [`reply`], but numbering its tool-call ids from `first_id`. Repeated
    /// turns must never reuse an id: the transcript pairing guard would reject
    /// them before the repetition logic ever saw the next request.
    fn reply_numbered(first_id: usize, content: Option<&str>, calls: &[(&str, String)]) -> String {
        let mut value: serde_json::Value =
            serde_json::from_str(&reply(content, calls)).expect("the scripted reply is JSON");
        if let Some(tool_calls) = value["choices"][0]["message"]["tool_calls"].as_array_mut() {
            for (index, call) in tool_calls.iter_mut().enumerate() {
                call["id"] = serde_json::json!(format!("call-{}", first_id + index));
            }
        }
        value.to_string()
    }

    /// One truncated chat-completions assistant message: the same content and
    /// calls as [`reply`], with the provider's `length` stop reason that marks
    /// the message truncated.
    fn truncated_reply(content: Option<&str>, calls: &[(&str, String)]) -> String {
        let mut value: serde_json::Value =
            serde_json::from_str(&reply(content, calls)).expect("the scripted reply is JSON");
        value["choices"][0]["finish_reason"] = serde_json::json!("length");
        value.to_string()
    }

    /// One truncated chat-completions reply whose message omits `tool_calls`
    /// entirely: the absent-list shape, as opposed to [`truncated_reply`]'s
    /// explicit empty list.
    fn truncated_reply_without_tool_calls(content: &str) -> String {
        serde_json::json!({
            "choices": [{
                "finish_reason": "length",
                "message": { "role": "assistant", "content": content },
            }]
        })
        .to_string()
    }

    /// One truncated native Anthropic Messages reply: the same blocks as
    /// [`native_reply`], with the `max_tokens` stop reason that marks the
    /// message truncated.
    fn native_truncated_reply(content: Option<&str>, calls: &[(&str, String)]) -> String {
        let mut value: serde_json::Value = serde_json::from_str(&native_reply(content, calls))
            .expect("the scripted reply is JSON");
        value["stop_reason"] = serde_json::json!("max_tokens");
        value.to_string()
    }

    fn finish_plan_arguments() -> String {
        serde_json::json!({ "plan": PLAN_MARKDOWN }).to_string()
    }

    fn plan_transcript() -> Vec<Message> {
        vec![Message::system("sys"), Message::user("do it")]
    }

    /// The summary pass's own transcript: a system prompt and the kickoff, with
    /// no control tool to end on.
    fn summary_transcript() -> Vec<Message> {
        vec![Message::system("sys"), Message::user("summarise the run")]
    }

    /// One assistant tool-call step and its result, as the loop replays them.
    fn compaction_step(index: usize, result: String) -> Vec<Message> {
        let call_id = format!("call-{index}");
        let arguments = serde_json::json!({ "code": format!("return {index};") }).to_string();
        vec![
            Message {
                role: Role::Assistant,
                content: Some(format!("step {index}")),
                tool_calls: Some(vec![ToolCall {
                    id: call_id.clone(),
                    kind: "function".to_string(),
                    function: FunctionCall {
                        name: "execute_script".to_string(),
                        arguments,
                    },
                }]),
                tool_call_id: None,
                reasoning_content: Some(format!("reasoning {index}")),
                extra: serde_json::Map::new(),
                truncated: false,
            },
            Message::tool(&call_id, result),
        ]
    }

    /// One assistant message that issues `ids` as a single tool-call group.
    fn call_group(ids: &[&str]) -> Message {
        Message {
            role: Role::Assistant,
            content: Some("calling".to_string()),
            tool_calls: Some(
                ids.iter()
                    .map(|id| ToolCall {
                        id: (*id).to_string(),
                        kind: "function".to_string(),
                        function: FunctionCall {
                            name: "execute_script".to_string(),
                            arguments: serde_json::json!({ "code": "return 0;" }).to_string(),
                        },
                    })
                    .collect(),
            ),
            tool_call_id: None,
            reasoning_content: Some("reasoning".to_string()),
            extra: serde_json::Map::new(),
            truncated: false,
        }
    }

    /// A transcript whose older tool results are large enough that Tier 1 can
    /// bring it back under the Tier 2 watermark without a summary request.
    fn tier_one_history(steps: usize, old_result_bytes: usize) -> Vec<Message> {
        let mut messages = vec![
            Message::system("SYSTEM PROMPT: keep this byte-for-byte."),
            Message::user("KICKOFF: start the work."),
        ];
        let older = steps.saturating_sub(RETAINED_TOOL_STEPS);
        for index in 1..=steps {
            let bytes = if index <= older { old_result_bytes } else { 32 };
            messages.extend(compaction_step(index, "r".repeat(bytes)));
        }
        messages
    }

    /// A transcript that needs Tier 1 and still exceeds the Tier 2 watermark,
    /// because its compacted middle carries an older user turn Tier 1 does not
    /// stub. It therefore drives the one handover-summary request.
    fn summary_history() -> Vec<Message> {
        let mut messages = vec![
            Message::system("SYSTEM PROMPT: keep this byte-for-byte."),
            Message::user("KICKOFF: start the work."),
        ];
        for index in 1..=2 {
            messages.extend(compaction_step(index, "o".repeat(100 * 1024)));
        }
        messages.push(Message::user("m".repeat(300 * 1024)));
        for index in 3..=8 {
            messages.extend(compaction_step(index, format!("small result {index}")));
        }
        messages
    }

    /// A transcript too old and large to leave alone, but with no older tool
    /// result to stub: every tool step belongs to the retained six-step tail.
    /// Only its middle narration can make way for the handover summary.
    fn history_without_older_tool_results() -> Vec<Message> {
        let mut messages = vec![
            Message::system("SYSTEM PROMPT: keep this byte-for-byte."),
            Message::user("KICKOFF: start the work."),
            Message {
                role: Role::Assistant,
                content: Some("working".to_string()),
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: Some("older reasoning".to_string()),
                extra: serde_json::Map::new(),
                truncated: false,
            },
            Message::user("m".repeat(500 * 1024)),
        ];
        for index in 1..=3 {
            messages.extend(compaction_step(index, format!("small result {index}")));
        }
        messages
    }

    /// The one handover text every successful summary in these tests returns.
    const HANDOVER_REPLY: &str = "[HANDOVER SUMMARY]\nCompleted/work context: compacted the middle\nDecisions: keep the retained tail\nUnresolved work/blockers: none\nRelevant findings: the ordinary request can proceed";

    /// The four labeled fields every accepted handover summary must carry.
    const REQUIRED_HANDOVER_LABELS: [&str; 4] = [
        "Completed/work context:",
        "Decisions:",
        "Unresolved work/blockers:",
        "Relevant findings:",
    ];

    /// Like [`summary_history`], but with an exactly sized compactable middle
    /// user turn. Tier 1 stubs the two older tool results, so after compaction
    /// the remaining context is the middle turn plus the small retained tail.
    fn summary_history_with_middle(middle_bytes: usize) -> Vec<Message> {
        let mut messages = vec![
            Message::system("SYSTEM PROMPT: keep this byte-for-byte."),
            Message::user("KICKOFF: start the work."),
        ];
        for index in 1..=2 {
            messages.extend(compaction_step(index, "o".repeat(150 * 1024)));
        }
        messages.push(Message::user("m".repeat(middle_bytes)));
        for index in 3..=8 {
            messages.extend(compaction_step(index, format!("small result {index}")));
        }
        messages
    }

    /// A history whose original context is above Tier 1 and whose context after
    /// Tier 1 compaction is exactly `target` bytes. The target is measured with
    /// the active provider, so the fixture lands on the real watermark rather
    /// than on an estimate.
    fn summary_history_compacted_to_context(
        config: &Config,
        target: usize,
        phase: Phase,
    ) -> Vec<Message> {
        let empty_middle = summary_history_with_middle(0);
        let compacted = compact_history(&empty_middle)
            .expect("the fixture is a paired history")
            .messages;
        let current = context_bytes(config, &compacted, phase);
        assert!(
            target > current,
            "the target {target} must exceed the compacted base {current}"
        );
        let messages = summary_history_with_middle(target - current);
        let compacted = compact_history(&messages)
            .expect("the padded fixture is still a paired history")
            .messages;
        assert_eq!(
            context_bytes(config, &compacted, phase),
            target,
            "the fixture must land exactly on the requested post-Tier-1 context"
        );
        messages
    }

    fn context_bytes(config: &Config, messages: &[Message], phase: Phase) -> usize {
        config
            .provider(phase)
            .prepare(messages, phase)
            .context_bytes
    }

    /// Append one padding user turn whose length makes the provider-prepared
    /// context exactly `target` bytes. The padding is a JSON-safe `x` run, so
    /// each added byte advances the encoded context by exactly one byte.
    fn pad_to_context(
        config: &Config,
        mut messages: Vec<Message>,
        target: usize,
        phase: Phase,
    ) -> Vec<Message> {
        messages.push(Message::user(""));
        let current = context_bytes(config, &messages, phase);
        assert!(
            target > current,
            "the base context {current} must fit below the target {target}"
        );
        let padding = target - current;
        if let Some(last) = messages.last_mut() {
            last.content = Some("x".repeat(padding));
        }
        assert_eq!(
            context_bytes(config, &messages, phase),
            target,
            "the padding must land on the exact context boundary"
        );
        messages
    }

    /// The messages array of one recorded request, decoded back into the loop's
    /// own transcript type.
    fn recorded_messages(body: &serde_json::Value, key: &str) -> Vec<Message> {
        serde_json::from_value(body[key].clone()).expect("the recorded transcript decodes")
    }

    /// The assistant tool-call messages in one transcript.
    fn assistant_tool_steps(messages: &[Message]) -> Vec<&Message> {
        messages
            .iter()
            .filter(|message| {
                message.role == Role::Assistant
                    && message
                        .tool_calls
                        .as_ref()
                        .is_some_and(|calls| !calls.is_empty())
            })
            .collect()
    }

    /// Narration eligibility: a missing `content` or a whitespace-only one has
    /// nothing to narrate and is skipped, exactly as the loop skipped it before
    /// the coordinator existed. The predicate is the coordinator's own, so the
    /// loop and the coordinator cannot disagree about what a skipped narration
    /// is -- and a skipped one is never handed over, so it writes nothing and
    /// leaves the remembered kind alone. The spacing that decision drives is
    /// covered by the coordinator's own unit tests in `status.rs`.
    #[test]
    fn narration_eligibility_matches_the_coordinator_predicate() {
        assert_eq!(status::printable_narration(None), None);
        assert_eq!(status::printable_narration(Some("")), None);
        assert_eq!(status::printable_narration(Some("   \n\t\n")), None);
        assert_eq!(
            status::printable_narration(Some("# a line")),
            Some("# a line"),
            "real narration must still print"
        );
    }

    // -----------------------------------------------------------------------
    // The summary-phase narration policy
    //
    // Only one reply is withheld from ordinary narration: a summary-phase turn
    // that requested no tool, which is the pass's answer. Its text is not
    // user-visible until the summary validator has accepted it, and the accepted
    // canonical bytes are published once through the pipeline's own
    // summary-publication path. Every other reply narrates as it always did:
    // an intermediate summary turn that still calls tools, and a final turn in
    // any non-summary phase.
    // -----------------------------------------------------------------------

    /// An intermediate summary reply that still calls tools is the pass
    /// inspecting the change set, not its answer, so it narrates as before.
    #[test]
    fn when_narrating_summary_given_intermediate_tool_call_then_keeps_narration_eligible() {
        let narration = "Inspecting the change set.";
        assert_eq!(
            printable_reply_narration(Phase::Summary, true, Some(narration)),
            Some(narration),
            "an intermediate summary tool step is ordinary narration"
        );
    }

    /// Only the summary phase is affected: a final reply in every other phase
    /// keeps its narration eligibility.
    #[test]
    fn when_narrating_reply_given_non_summary_phase_then_keeps_narration_eligible() {
        let narration = "Handing the settled design over now.";
        for phase in [
            Phase::Plan,
            Phase::Spec,
            Phase::Tickets,
            Phase::Triage,
            Phase::Code,
            Phase::Review,
        ] {
            assert_eq!(
                printable_reply_narration(phase, false, Some(narration)),
                Some(narration),
                "a final reply in {phase:?} must stay narration-eligible"
            );
        }
    }

    /// The regression: a summary-phase final answer used to reach the ordinary
    /// narration output before the validator saw it. The loop-boundary policy
    /// withholds it, while the same reply still travels back to the caller
    /// unchanged so validation sees exactly the model's text.
    #[test]
    fn when_summary_is_rejected_given_final_answer_then_withholds_unvalidated_text() {
        // A format-invalid final answer: the summary validator will refuse it,
        // but the narration policy withholds it before validation even runs.
        let rejected = "Here is the summary:\n\nfeat: add the thing\n";
        assert_eq!(
            printable_reply_narration(Phase::Summary, false, Some(rejected)),
            None,
            "an unvalidated summary final must not be narrated"
        );
        assert_eq!(
            status::printable_narration(Some(rejected)),
            Some(rejected),
            "the text is not empty; only the narration policy withholds it"
        );

        // The loop makes no extra request: the withheld turn is the pass's
        // answer, and it comes back for validation byte for byte.
        let script = Script::new(vec![native_text(rejected)]);
        let config = config(script.clone());
        let mut messages = summary_transcript();
        match run_agent_loop(&mut messages, &config, Phase::Summary, Source::None) {
            Stop::Summary(Some(text)) => assert_eq!(text, rejected),
            _ => panic!("the withheld text must still reach the caller for validation"),
        }
        assert_eq!(script.requests(), 1);
    }

    /// The minimum valid boundary: even a canonical final answer is withheld
    /// until the validator has accepted it, and the minimal single-turn pass
    /// still hands the exact text back to the caller.
    #[test]
    fn when_narrating_summary_given_one_final_answer_then_withholds_text_until_validation() {
        const VALID: &str = "feat(api): add retry backoff\n\nRetry the call twice.\n";
        assert_eq!(
            printable_reply_narration(Phase::Summary, false, Some(VALID)),
            None,
            "a valid final answer is withheld too; only the validator decides its fate"
        );

        let script = Script::new(vec![native_text(VALID)]);
        let config = config(script.clone());
        let mut messages = summary_transcript();
        match run_agent_loop(&mut messages, &config, Phase::Summary, Source::None) {
            Stop::Summary(Some(text)) => assert_eq!(text, VALID),
            _ => panic!("the single final answer must reach the caller for validation"),
        }
        assert_eq!(script.requests(), 1);
    }

    /// The upper boundary: however many intermediate tool-call turns precede
    /// the final answer, every one of them narrates and only the no-tool-call
    /// final turn is withheld. There is no invented turn limit.
    #[test]
    fn when_narrating_summary_given_tool_steps_before_final_then_keeps_intermediate_narration() {
        const INTERMEDIATE: &str = "Inspecting the change set.";
        const FINAL: &str = "test(summary): report the change\n\nOne body line.\n";

        assert_eq!(
            printable_reply_narration(Phase::Summary, true, Some(INTERMEDIATE)),
            Some(INTERMEDIATE)
        );
        assert_eq!(
            printable_reply_narration(Phase::Summary, false, Some(FINAL)),
            None
        );

        let tool_call = serde_json::json!({ "code": "return 1;" }).to_string();
        let script = Script::new(vec![
            native_reply_with_ids(
                Some(INTERMEDIATE),
                &[("execute_script", tool_call.clone())],
                0,
            ),
            native_reply_with_ids(None, &[("execute_script", tool_call)], 1),
            native_text(FINAL),
        ]);
        let config = config(script.clone());
        let mut messages = summary_transcript();
        match run_agent_loop(&mut messages, &config, Phase::Summary, Source::None) {
            Stop::Summary(Some(text)) => assert_eq!(text, FINAL),
            _ => panic!("the final tool-free turn must answer the pass"),
        }
        assert_eq!(
            script.requests(),
            3,
            "the policy must not shorten or lengthen the pass"
        );
    }

    /// The malformed payload boundary: a format-invalid answer, a `Refs:`
    /// footer and an answer naming a generated ticket id are all withheld,
    /// because the policy runs before the validator ever sees them.
    #[test]
    fn when_narrating_summary_given_invalid_final_answer_then_withholds_text() {
        for invalid in [
            "feat: Add the thing\n\nBody line.\n",
            "feat: add the thing\n\nA body line.\n\nRefs: 1-verify-command-output\n",
            "test(pipeline): summarize the run\n\nThe queue completed 1-verify-command-output.\n",
            "Here is the summary:\n\n",
        ] {
            assert_eq!(
                printable_reply_narration(Phase::Summary, false, Some(invalid)),
                None,
                "{invalid:?} must not be narrated"
            );
        }
    }

    /// Zero/absent content: a missing or whitespace-only final turn has nothing
    /// to narrate, and the loop still hands it back as the absent answer the
    /// validator reports as empty.
    #[test]
    fn when_narrating_summary_given_empty_final_text_then_emits_no_narration() {
        assert_eq!(printable_reply_narration(Phase::Summary, false, None), None);
        assert_eq!(
            printable_reply_narration(Phase::Summary, false, Some("")),
            None
        );
        assert_eq!(
            printable_reply_narration(Phase::Summary, false, Some("   \n\t\n")),
            None
        );

        let attempts = crate::retry::MAX_ATTEMPTS as usize;
        let script = Script::new(vec![native_reply(None, &[]); attempts]);
        let config = config(script.clone());
        let mut messages = summary_transcript();
        match run_agent_loop(&mut messages, &config, Phase::Summary, Source::None) {
            Stop::Failed(FailureKind::Malformed) => {}
            _ => panic!("a native turn with no content blocks is an unusable response"),
        }
        assert_eq!(script.requests(), attempts);
    }

    /// Read-only boundary: this is a narration-only policy change, so the
    /// summary pass keeps exactly the one inspection tool it had -- no `bash`,
    /// no control tool and no handoff -- and `bash` is still refused there.
    #[test]
    fn when_narrating_summary_given_final_answer_then_keeps_read_only_tool_access() {
        let script = Script::new(vec![native_text(
            "feat(api): add retry backoff\n\nRetry the call twice.\n",
        )]);
        let config = config(script.clone());
        let mut messages = summary_transcript();

        let _ = run_agent_loop(&mut messages, &config, Phase::Summary, Source::None);

        let sent = script.sent();
        assert_eq!(sent.len(), 1, "one summary request");
        let advertised: Vec<String> = sent[0]["tools"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|tool| tool["name"].as_str().map(str::to_string))
            .collect();
        assert_eq!(
            advertised,
            ["execute_script"],
            "the summary phase keeps its one read-only tool"
        );

        let defined: Vec<&str> = Tool::definitions(Phase::Summary)
            .into_iter()
            .map(|definition| definition.function.name)
            .collect();
        assert_eq!(defined, ["execute_script"]);
        assert!(
            Tool::parse("bash", r#"{"command":"ls"}"#, Phase::Summary, Source::None).is_err(),
            "the narration policy must not open bash to the summary pass"
        );
    }

    /// The marker set `asks_the_user` reads: a question mark, the prompt's
    /// question heading, and a request for confirmation or input.
    #[test]
    fn a_reply_is_a_question_only_when_it_addresses_the_user() {
        for question in [
            "Which mode should the line show?",
            "### Q1 - Which mode?",
            "Please confirm this approach.",
            "Let me know which option you prefer.",
            "I will wait for your confirmation.",
        ] {
            assert!(asks_the_user(Some(question)), "missed question: {question}");
        }

        for statement in [
            "Handing the settled design over now.",
            "The plan is complete and follows the template.",
            "",
        ] {
            assert!(
                !asks_the_user(Some(statement)),
                "false positive: {statement}"
            );
        }

        assert!(!asks_the_user(None));
    }

    /// The reported bug: a reply that asks the user to confirm and calls
    /// `finish_plan` in the same message must not hand over. The turn waits for
    /// the user, and the dropped call leaves the transcript valid.
    #[test]
    fn a_finish_plan_bundled_with_a_question_waits_for_the_user() {
        let script = Script::new(vec![reply(
            Some("Understood. Please confirm this approach."),
            &[("finish_plan", finish_plan_arguments())],
        )]);
        let config = config(script.clone());
        let mut messages = plan_transcript();

        let stop = run_agent_loop(&mut messages, &config, Phase::Plan, Source::None);

        assert!(
            matches!(stop, Stop::TurnEnd),
            "the turn must wait for the user, not hand over"
        );
        assert_eq!(
            script.requests(),
            1,
            "the loop must not send another request before the user answers"
        );

        let last = messages.last().expect("the assistant reply is replayed");
        assert_eq!(last.role, Role::Assistant);
        assert!(
            last.tool_calls.is_none(),
            "the premature handoff is dropped from the transcript"
        );
        assert!(
            !messages.iter().any(|message| message.role == Role::Tool),
            "a dropped call must leave no tool result behind"
        );
    }

    /// A question carried in the same message as a tool call still yields:
    /// the tool runs, then the turn goes back to the user.
    #[test]
    fn a_question_with_a_tool_call_yields_after_the_tool_runs() {
        let script = Script::new(vec![
            reply(
                Some("### Q1 - Which option?\n\nA) One\nB) Two"),
                &[(
                    "execute_script",
                    serde_json::json!({ "code": "return 1 + 1;" }).to_string(),
                )],
            ),
            // Never served: a second request means the loop ignored the question.
            reply(None, &[("finish_plan", finish_plan_arguments())]),
        ]);
        let config = config(script.clone());
        let mut messages = plan_transcript();

        let stop = run_agent_loop(&mut messages, &config, Phase::Plan, Source::None);

        assert!(matches!(stop, Stop::TurnEnd));
        assert_eq!(
            script.requests(),
            1,
            "the loop must stop at the question, not continue"
        );
        assert!(
            messages.iter().any(|message| message.role == Role::Tool),
            "the tool call in the question message still ran"
        );
    }

    /// A reply that asks nothing hands the plan over immediately.
    #[test]
    fn a_finish_plan_without_a_question_hands_the_plan_over() {
        let script = Script::new(vec![reply(
            Some("Handing the settled design over now."),
            &[("finish_plan", finish_plan_arguments())],
        )]);
        let config = config(script.clone());
        let mut messages = plan_transcript();

        match run_agent_loop(&mut messages, &config, Phase::Plan, Source::None) {
            Stop::Handoff(plan) => assert_eq!(plan.goal, "Typed plan handoff"),
            _ => panic!("expected the plan handoff"),
        }
        assert_eq!(script.requests(), 1);
    }

    /// Only the plan interview is a conversation: a question in an unattended
    /// phase does not stop its control tool from ending the turn.
    #[test]
    fn a_question_does_not_block_an_unattended_phases_completion() {
        let script = Script::new(vec![reply(
            Some("Should more be done? The ticket is verified."),
            &[("task_complete", "{}".to_string())],
        )]);
        let config = config(script);
        let mut messages = plan_transcript();

        assert!(matches!(
            run_agent_loop(&mut messages, &config, Phase::Code, Source::None),
            Stop::Complete
        ));
    }

    // -----------------------------------------------------------------------
    // Source-aware coverage rejections stay in the loop
    // -----------------------------------------------------------------------

    use crate::plan_document::PlanDecision;
    use crate::specification::{
        DeferredItems, ImplementationDecisions, TestingDecisions, UserStory,
    };

    /// The plan the spec-loop retry test must cover.
    fn coverage_plan() -> Plan {
        Plan {
            goal: "Add a --version flag".to_string(),
            decisions: vec![PlanDecision {
                title: "Add the flag".to_string(),
                detail: "The binary prints its crate version and exits.".to_string(),
            }],
            defaults: Vec::new(),
            constraints: Vec::new(),
            open_questions: Vec::new(),
        }
    }

    /// A specification document that either covers the plan's decision or
    /// leaves it out entirely.
    fn specification_document(covers: bool) -> String {
        let (feature, story, requirement, criterion) = if covers {
            (
                "Version flag",
                "Version output",
                "Add the flag: the binary prints its crate version and exits.",
                "The version is printed.",
            )
        } else {
            (
                "Status page",
                "Status page",
                "Serve a static status page.",
                "The page is served.",
            )
        };

        Specification {
            feature_name: feature.to_string(),
            problem_statement: if covers {
                "There is no version flag.".to_string()
            } else {
                "There is no status page.".to_string()
            },
            solution_overview: if covers {
                "Print the crate version.".to_string()
            } else {
                "Serve a static status page.".to_string()
            },
            user_stories: vec![UserStory {
                title: story.to_string(),
                requirement: requirement.to_string(),
                acceptance_criteria: vec![criterion.to_string()],
            }],
            implementation_decisions: ImplementationDecisions {
                module_architecture: if covers {
                    "Reuse the parser.".to_string()
                } else {
                    "Add a route to the server.".to_string()
                },
                data_models_and_contracts: if covers {
                    "Read the version from the environment.".to_string()
                } else {
                    "Read the page template from disk.".to_string()
                },
                adr_compliance: "No new dependency.".to_string(),
            },
            testing_decisions: TestingDecisions {
                test_seams: if covers {
                    "The parser.".to_string()
                } else {
                    "The router.".to_string()
                },
                coverage_strategy: "One unit test.".to_string(),
                prior_art: "The existing tests.".to_string(),
            },
            out_of_scope: Vec::new(),
            deferred_items: DeferredItems {
                operational_risks: String::new(),
                follow_up_work: String::new(),
            },
        }
        .to_markdown()
    }

    /// One `finish_spec` call carrying a specification that covers the plan or
    /// does not.
    fn spec_call(covers: bool) -> String {
        serde_json::json!({ "spec": specification_document(covers) }).to_string()
    }

    /// The specification the tickets-loop retry test must cover.
    fn tickets_specification() -> Specification {
        Specification {
            feature_name: "Version flag".to_string(),
            problem_statement: "There is no version flag.".to_string(),
            solution_overview: "Print the crate version.".to_string(),
            user_stories: vec![UserStory {
                title: "Version output".to_string(),
                requirement: "The binary prints its crate version and exits.".to_string(),
                acceptance_criteria: vec!["Prints the crate version and exits.".to_string()],
            }],
            implementation_decisions: ImplementationDecisions {
                module_architecture: "Reuse the parser.".to_string(),
                data_models_and_contracts: "Read the version from the environment.".to_string(),
                adr_compliance: "No new dependency.".to_string(),
            },
            testing_decisions: TestingDecisions {
                test_seams: "The parser.".to_string(),
                coverage_strategy: "One unit test.".to_string(),
                prior_art: "The CLI tests.".to_string(),
            },
            out_of_scope: Vec::new(),
            deferred_items: DeferredItems {
                operational_risks: String::new(),
                follow_up_work: String::new(),
            },
        }
    }

    /// One `finish_tickets` call whose single ticket carries `criteria`.
    fn tickets_call(criteria: &[&str]) -> String {
        serde_json::json!({
            "tickets": {
                "tickets": [{
                    "id": "1-version",
                    "blocked_by": [],
                    "blocks": [],
                    "title": "Add the version flag",
                    "objective": "Print the crate version and exit.",
                    "acceptance_criteria": criteria,
                    "prerequisites_assumed": "The parser builds.",
                    "out_of_scope": [],
                    "scope_of_changes": ["Add the flag handle."],
                    "test_expectations": ["when_printing_version_given_flag_then_exits_zero"],
                    "verification_gate": "cargo check"
                }]
            }
        })
        .to_string()
    }

    /// A specification handoff rejected for uncovered decisions does not end the
    /// pass: the error is handed back as a tool result and the corrected call in
    /// the next turn is what hands over.
    #[test]
    fn a_rejected_spec_handoff_is_retried_as_a_tool_result() {
        let script = Script::new(vec![
            reply(None, &[("finish_spec", spec_call(false))]),
            reply(None, &[("finish_spec", spec_call(true))]),
        ]);
        let config = config(script.clone());
        let plan = coverage_plan();
        let mut messages = vec![Message::system("sys"), Message::user("specify")];

        match run_agent_loop(&mut messages, &config, Phase::Spec, Source::Plan(&plan)) {
            Stop::Spec(specification) => assert_eq!(specification.feature_name, "Version flag"),
            _ => panic!("expected the corrected specification handoff"),
        }
        assert_eq!(
            script.requests(),
            2,
            "the rejected call must not end the pass"
        );
        assert!(
            messages.iter().any(|message| {
                message.role == Role::Tool
                    && message
                        .content
                        .as_deref()
                        .unwrap_or_default()
                        .contains("Add the flag")
            }),
            "the rejection must reach the model as a tool result naming the uncovered decision"
        );
    }

    /// A ticket handoff rejected for a dropped criterion is retried the same
    /// way: the bad set never reaches the queue and the corrected set does.
    #[test]
    fn a_rejected_ticket_handoff_is_retried_as_a_tool_result() {
        let script = Script::new(vec![
            reply(
                None,
                &[("finish_tickets", tickets_call(&["Something else."]))],
            ),
            reply(
                None,
                &[(
                    "finish_tickets",
                    tickets_call(&["Prints the crate version and exits."]),
                )],
            ),
        ]);
        let config = config(script.clone());
        let specification = tickets_specification();
        let mut messages = vec![Message::system("sys"), Message::user("slice")];

        match run_agent_loop(
            &mut messages,
            &config,
            Phase::Tickets,
            Source::Specification(&specification),
        ) {
            Stop::Tickets(tickets) => {
                assert_eq!(tickets.tickets.len(), 1);
                assert_eq!(
                    tickets.tickets[0].acceptance_criteria,
                    ["Prints the crate version and exits."]
                );
            }
            _ => panic!("expected the corrected ticket handoff"),
        }
        assert_eq!(
            script.requests(),
            2,
            "the rejected call must not end the pass"
        );
        assert!(
            messages.iter().any(|message| {
                message.role == Role::Tool
                    && message
                        .content
                        .as_deref()
                        .unwrap_or_default()
                        .contains("Prints the crate version and exits.")
            }),
            "the rejection must reach the model as a tool result quoting the criterion"
        );
    }

    /// A `finish_tickets` call whose single ticket streams the output. It is
    /// the corrected handoff that ends the uncovered-story retry.
    fn streaming_tickets_call() -> String {
        serde_json::json!({
            "tickets": {
                "tickets": [{
                    "id": "1-stream",
                    "blocked_by": [],
                    "blocks": [],
                    "title": "Stream the output",
                    "objective": "Stream each log record to the remote collector.",
                    "acceptance_criteria": [],
                    "prerequisites_assumed": "The logger exists.",
                    "out_of_scope": [],
                    "scope_of_changes": ["Add the streamer."],
                    "test_expectations": ["when_streaming_output_given_log_record_then_sends_to_collector"],
                    "verification_gate": "cargo check"
                }]
            }
        })
        .to_string()
    }

    /// The specification whose only story carries no criteria: the tickets pass
    /// must represent it through the tickets' own content, not through an exact
    /// criterion.
    fn story_only_specification() -> Specification {
        let mut specification = tickets_specification();
        specification.user_stories = vec![UserStory {
            title: "Stream the output".to_string(),
            requirement: "Stream each log record to the remote collector.".to_string(),
            acceptance_criteria: Vec::new(),
        }];
        specification
    }

    /// A ticket handoff rejected for an uncovered story is retried the same way
    /// a dropped criterion is: the bad set never reaches the queue and the
    /// corrected set does.
    #[test]
    fn a_rejected_ticket_handoff_for_an_uncovered_story_is_retried() {
        let script = Script::new(vec![
            reply(
                None,
                &[("finish_tickets", tickets_call(&["Something else."]))],
            ),
            reply(None, &[("finish_tickets", streaming_tickets_call())]),
        ]);
        let config = config(script.clone());
        let specification = story_only_specification();
        let mut messages = vec![Message::system("sys"), Message::user("slice")];

        match run_agent_loop(
            &mut messages,
            &config,
            Phase::Tickets,
            Source::Specification(&specification),
        ) {
            Stop::Tickets(tickets) => {
                assert_eq!(tickets.tickets.len(), 1);
                assert_eq!(tickets.tickets[0].title, "Stream the output");
            }
            _ => panic!("expected the corrected ticket handoff for the uncovered story"),
        }
        assert_eq!(
            script.requests(),
            2,
            "the rejected call must not end the pass"
        );
        assert!(
            messages.iter().any(|message| {
                message.role == Role::Tool
                    && message
                        .content
                        .as_deref()
                        .unwrap_or_default()
                        .contains("Stream the output")
            }),
            "the rejection must reach the model as a tool result naming the uncovered story"
        );
    }

    /// The review report a successful review handoff carries: a clean pass
    /// whose fixed scorecard and empty findings and fixes make payload equality
    /// a complete check.
    fn passing_review_report() -> ReviewReport {
        serde_json::from_value(serde_json::json!({
            "scorecard": {
                "keeper_coverage": "All four keeper categories are covered.",
                "sediment_and_coupling": "No internal mocks and no private state.",
                "inferred_assumptions": "The empty transcript is the only new boundary.",
                "boundary_matrix": "Every boundary dimension is exercised.",
                "code_and_scope": "The review stays inside the repository."
            },
            "findings": [],
            "fixes": []
        }))
        .expect("the passing review fixture decodes")
    }

    /// The wire arguments of one successful review handoff.
    fn finish_review_call(report: &ReviewReport) -> String {
        serde_json::json!({ "review": report }).to_string()
    }

    /// The draft ticket set the triage handoff classifies: the same valid ticket
    /// a successful tickets handoff carries.
    fn triage_draft_set() -> TicketSet {
        let value: serde_json::Value =
            serde_json::from_str(&tickets_call(&["Prints the crate version and exits."]))
                .expect("the draft ticket fixture is JSON");
        serde_json::from_value(value["tickets"].clone())
            .expect("the draft ticket fixture decodes into a typed set")
    }

    /// One ready classification per draft ticket, preserving its identity,
    /// criteria and dependencies exactly as the triage boundary requires.
    fn ready_triage_report(tickets: &TicketSet) -> TriageReport {
        TriageReport {
            tickets: tickets
                .tickets
                .iter()
                .map(|ticket| TriageTicket {
                    ticket_id: ticket.id.clone(),
                    ticket_title: ticket.title.clone(),
                    category: TriageCategory::Enhancement,
                    state: TriageState::ReadyForAgent,
                    notes: vec!["The draft ticket is one self-contained slice.".to_string()],
                    blocker_explanation: None,
                    agent_brief: Some(AgentBrief {
                        target_behavior: format!("Implement {}.", ticket.title),
                        acceptance_criteria: ticket.acceptance_criteria.clone(),
                        out_of_scope: vec!["None.".to_string()],
                        blocked_by: ticket.blocked_by.clone(),
                    }),
                })
                .collect(),
        }
    }

    /// The wire arguments of one successful triage handoff.
    fn finish_triage_call(report: &TriageReport) -> String {
        serde_json::json!({ "triage": report }).to_string()
    }

    /// The reasoning every successful-handoff reply carries, so the replay
    /// assertion can see it survive the handoff.
    const HANDOFF_REASONING: &str = "I checked the payload and can hand it over.";

    /// One assistant reply that carries reasoning as well as its tool calls.
    fn reply_with_reasoning(
        content: Option<&str>,
        calls: &[(&str, String)],
        reasoning: &str,
    ) -> String {
        let mut value: serde_json::Value =
            serde_json::from_str(&reply(content, calls)).expect("the scripted reply is JSON");
        value["choices"][0]["message"]["reasoning_content"] = serde_json::json!(reasoning);
        value.to_string()
    }

    /// Assert every replayed assistant tool call has exactly one matching tool
    /// result, and every tool result answers a call in the same transcript.
    fn assert_no_unanswered_calls(messages: &[Message]) {
        let mut unanswered: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for message in messages {
            if message.role == Role::Assistant {
                for call in message.tool_calls.iter().flatten() {
                    assert!(
                        unanswered.insert(call.id.as_str()),
                        "a tool-call id is replayed twice: {}",
                        call.id
                    );
                }
            }
            if message.role == Role::Tool {
                let id = message
                    .tool_call_id
                    .as_deref()
                    .expect("a tool result must name the call it answers");
                assert!(
                    unanswered.remove(id),
                    "the tool result {id} has no unanswered call"
                );
            }
        }
        assert!(
            unanswered.is_empty(),
            "the handoff left unanswered tool-call ids: {unanswered:?}"
        );
    }

    /// Run one successful control handoff through the real loop after an
    /// ordinary call and before a second one, then assert the assistant message
    /// and its surviving result keep the transcript pairable.
    fn run_successful_handoff(
        phase: Phase,
        source: Source<'_>,
        handoff: (&str, String),
    ) -> (Stop, Vec<Message>) {
        let body = reply_with_reasoning(
            Some("Handing the result over."),
            &[
                (
                    "execute_script",
                    serde_json::json!({ "code": "return 'kept';" }).to_string(),
                ),
                (handoff.0, handoff.1),
                (
                    "execute_script",
                    serde_json::json!({ "code": "return 'dropped';" }).to_string(),
                ),
            ],
            HANDOFF_REASONING,
        );
        let script = Script::new(vec![body]);
        let config = config(script.clone());
        let mut messages = vec![Message::system("sys"), Message::user("work")];

        let stop = run_agent_loop(&mut messages, &config, phase, source);
        assert_eq!(
            script.requests(),
            1,
            "a successful handoff must end the turn without another request"
        );

        let assistant = &messages[2];
        assert_eq!(assistant.role, Role::Assistant);
        assert_eq!(
            assistant.content.as_deref(),
            Some("Handing the result over."),
            "the assistant's own text must be replayed"
        );
        assert_eq!(
            assistant.reasoning_content.as_deref(),
            Some(HANDOFF_REASONING),
            "the assistant's reasoning must survive the handoff"
        );
        let surviving: Vec<&str> = assistant
            .tool_calls
            .iter()
            .flatten()
            .map(|call| call.id.as_str())
            .collect();
        assert_eq!(
            surviving,
            ["call-0"],
            "the handoff call and every later call are dropped, leaving the answered call"
        );

        let results: Vec<(usize, &Message)> = messages
            .iter()
            .enumerate()
            .filter(|(_, message)| message.role == Role::Tool)
            .collect();
        assert_eq!(results.len(), 1, "the one ordinary call has one result");
        let (result_index, result) = results[0];
        assert!(
            result_index > 2,
            "the assistant message must be replayed before its call result"
        );
        assert_eq!(result.tool_call_id.as_deref(), Some("call-0"));
        assert_eq!(result.content.as_deref(), Some("kept"));
        assert_no_unanswered_calls(&messages);

        (stop, messages)
    }

    /// Four matching-phase control handoffs each preserve their typed payload,
    /// replay the assistant's reasoning before the kept call's result, drop the
    /// handoff call and every later call, and leave no orphaned tool result.
    #[test]
    fn when_valid_control_handoffs_finish_given_matching_agent_phases_then_preserves_payload_and_drops_calls()
     {
        let plan = coverage_plan();
        let expected_spec =
            Specification::from_markdown(&specification_document(true)).expect("valid spec");
        let (stop, _) = run_successful_handoff(
            Phase::Spec,
            Source::Plan(&plan),
            ("finish_spec", spec_call(true)),
        );
        match stop {
            Stop::Spec(specification) => assert_eq!(specification, expected_spec),
            _ => panic!("the matching spec phase must hand over the specification"),
        }

        let specification = tickets_specification();
        let expected_tickets: TicketSet = {
            let value: serde_json::Value =
                serde_json::from_str(&tickets_call(&["Prints the crate version and exits."]))
                    .expect("the ticket handoff fixture is JSON");
            serde_json::from_value(value["tickets"].clone())
                .expect("the ticket handoff fixture decodes")
        };
        let (stop, _) = run_successful_handoff(
            Phase::Tickets,
            Source::Specification(&specification),
            (
                "finish_tickets",
                tickets_call(&["Prints the crate version and exits."]),
            ),
        );
        match stop {
            Stop::Tickets(tickets) => assert_eq!(tickets, expected_tickets),
            _ => panic!("the matching tickets phase must hand over the ticket set"),
        }

        let expected_review = passing_review_report();
        let executed = vec!["1-earlier".to_string()];
        let (stop, _) = run_successful_handoff(
            Phase::Review,
            Source::Review(&executed),
            ("finish_review", finish_review_call(&expected_review)),
        );
        match stop {
            Stop::Review(review) => assert_eq!(review, expected_review),
            _ => panic!("the matching review phase must hand over the review report"),
        }

        let specification = tickets_specification();
        let draft = triage_draft_set();
        let expected_triage = ready_triage_report(&draft);
        let (stop, _) = run_successful_handoff(
            Phase::Triage,
            Source::Triage {
                specification: &specification,
                tickets: &draft,
            },
            ("finish_triage", finish_triage_call(&expected_triage)),
        );
        match stop {
            Stop::Triage(report) => assert_eq!(report, expected_triage),
            _ => panic!("the matching triage phase must hand over the triage report"),
        }
    }

    /// A tool result within the cap is handed over unchanged.
    #[test]
    fn a_small_tool_result_is_not_capped() {
        let text = "all green".to_string();
        assert_eq!(cap_tool_result(text.clone()), text);
    }

    /// A tool result over the cap keeps a UTF-8-safe head, names the exact
    /// omitted source bytes, and stays within the cap with its marker included.
    #[test]
    fn a_large_tool_result_keeps_its_head_and_names_the_omission() {
        let source = "x".repeat(TOOL_RESULT_BYTES + 100);
        let capped = cap_tool_result(source.clone());

        assert!(
            capped.len() <= TOOL_RESULT_BYTES,
            "the capped result must stay within the cap: {}",
            capped.len()
        );
        let marker_start = capped
            .find("\n[tool output truncated")
            .expect("the truncation marker is present");
        let kept = &capped[..marker_start];
        let omitted = source.len() - kept.len();
        assert!(
            capped[marker_start..].contains(&format!("{omitted} bytes omitted")),
            "unexpected marker: {capped:?}"
        );
    }

    #[test]
    fn step_warning_is_appended_from_step_ninety_onward() {
        assert_eq!(with_step_warning("ok".to_string(), 1), "ok");
        assert_eq!(with_step_warning("ok".to_string(), 89), "ok");
        assert_eq!(
            with_step_warning("ok".to_string(), 90),
            "ok\n[WARN: step 90/100; 10 steps remaining before the step limit]"
        );
        assert_eq!(
            with_step_warning("ok".to_string(), 100),
            "ok\n[WARN: step 100/100; 0 steps remaining before the step limit]"
        );
    }

    #[test]
    fn format_tool_target_distinguishes_bash_and_execute_script_with_step_counter() {
        let bash = Tool::parse(
            "bash",
            r#"{"command":"cargo test"}"#,
            Phase::Code,
            Source::None,
        )
        .unwrap();
        let script = Tool::parse(
            "execute_script",
            r#"{"code":"return tools.list();"}"#,
            Phase::Plan,
            Source::None,
        )
        .unwrap();

        assert_eq!(format_tool_target(3, &bash), "[3/100] cargo test");
        assert_eq!(
            format_tool_target(12, &script),
            "[12/100] js: return tools.list();"
        );
        assert_eq!(
            first_error_line("[ERROR: execute_script failed]\nError: boom"),
            "Error: boom"
        );
    }

    /// The cap never splits a UTF-8 character: the cut moves back to the
    /// previous boundary and the marker counts the bytes it left out.
    #[test]
    fn the_cap_does_not_split_a_character() {
        let text = format!("{}€", "x".repeat(TOOL_RESULT_BYTES - 1));
        let source_len = text.len();
        let capped = cap_tool_result(text);

        assert!(
            capped.len() <= TOOL_RESULT_BYTES,
            "the capped result must stay within the cap: {}",
            capped.len()
        );
        let marker_start = capped
            .find("\n[tool output truncated")
            .expect("the truncation marker is present");
        let kept = &capped[..marker_start];
        let omitted = source_len - kept.len();
        assert!(
            kept.chars().all(|c| c == 'x'),
            "the cut split a character: {kept:?}"
        );
        assert!(
            capped[marker_start..].contains(&format!("{omitted} bytes omitted")),
            "the omitted count must survive: {capped:?}"
        );
    }

    /// Contract and invariant: whatever a tool result carries -- a command
    /// error, truncation metadata, or the step-budget warning near the limit --
    /// the bytes appended to the transcript never exceed the delivery cap, and
    /// the warning survives the final cut.
    #[test]
    fn when_tool_results_given_warning_and_error_metadata_then_never_exceed_delivery_limit() {
        let error = format!(
            "[ERROR: exit code 1 for cargo test]\n{}",
            "e".repeat(TOOL_RESULT_BYTES + 500)
        );
        for step in [1, 89, 90, 100] {
            let delivered = with_step_warning(cap_tool_result(error.clone()), step);
            assert!(
                delivered.len() <= TOOL_RESULT_BYTES,
                "step {step} delivered {} bytes, over the {TOOL_RESULT_BYTES}-byte cap",
                delivered.len()
            );
            assert!(
                delivered.starts_with("[ERROR: exit code 1"),
                "the error metadata must stay at the head: {delivered:?}"
            );
            if step >= 90 {
                assert!(
                    delivered.contains("[WARN: step"),
                    "the step warning must survive the final cut: {delivered:?}"
                );
                assert!(
                    delivered.ends_with("before the step limit]"),
                    "the whole step warning must stay at the tail: {delivered:?}"
                );
            }
        }

        let script_metadata = "x".repeat(TOOL_RESULT_BYTES + 500);
        let delivered = with_step_warning(cap_tool_result(script_metadata), 100);
        assert!(delivered.len() <= TOOL_RESULT_BYTES);
        assert!(
            delivered.contains("bytes omitted"),
            "the truncation metadata must survive: {delivered:?}"
        );
        assert!(
            delivered.contains("[WARN: step 100/100; 0 steps remaining before the step limit]"),
            "the final warning must survive beside truncation metadata: {delivered:?}"
        );
    }

    /// Tier 1's stub names the function, its target and the exact byte count,
    /// and is recognised by its prefix so a later compaction never replaces it
    /// again and loses the original count.
    #[test]
    fn when_older_tool_results_are_compacted_then_stub_names_function_target_and_omitted_bytes() {
        use crate::api::FunctionCall;

        let call = ToolCall {
            id: "call-1".to_string(),
            kind: "function".to_string(),
            function: FunctionCall {
                name: "bash".to_string(),
                arguments: serde_json::json!({ "command": "cargo test\nmore" }).to_string(),
            },
        };
        let stub = tool_result_stub(&call, 123);

        assert!(is_tool_result_stub(&stub));
        assert!(!is_tool_result_stub("all green"));
        assert!(stub.contains("bash"), "the function is named: {stub}");
        assert!(stub.contains("cargo test"), "the target is named: {stub}");
        assert!(
            stub.contains("123 bytes omitted"),
            "the count is exact: {stub}"
        );
        assert!(!stub.contains('\n'), "the stub is one line: {stub}");
    }

    /// The guard stop carries its own failure label, so the summary line names
    /// the real cause instead of a generic error.
    #[test]
    fn the_context_guard_has_its_own_label() {
        assert_eq!(FailureKind::ContextGuard.label(), "context guard reached");
    }

    /// The guard measures the encoded body, the same bytes the transport sends.
    #[test]
    fn the_guard_measures_the_encoded_body() {
        let body = serde_json::json!({ "model": "m", "messages": [] });
        assert_eq!(
            encoded_body_bytes(&body),
            serde_json::to_string(&body).unwrap().len()
        );
    }

    /// A handoff truncates the assistant message's tool calls, so the interview can be
    /// resumed without leaving unanswered tool calls that a strict API would reject.
    #[test]
    fn drop_calls_from_leaves_transcript_api_valid_after_handoff() {
        use crate::api::{FunctionCall, ToolCall};
        let mut assistant = Message::user("Hello");
        assistant.role = Role::Assistant;
        assistant.content = None;
        assistant.tool_calls = Some(vec![ToolCall {
            id: "call_handoff_1".to_string(),
            kind: "function".to_string(),
            function: FunctionCall {
                name: "finish_plan".to_string(),
                arguments: "{}".to_string(),
            },
        }]);
        let mut messages = vec![
            Message::system("You are an assistant."),
            Message::user("Hello"),
            assistant,
        ];
        drop_calls_from(&mut messages, 2, "call_handoff_1");
        assert!(messages[2].tool_calls.is_none());
    }

    // -----------------------------------------------------------------------
    // Inline command timing
    //
    // A model-issued bash or execute_script call that reaches Tool::run is
    // measured, and nothing else is: a rejected payload, a call the phase does
    // not offer and a control-tool handoff never reach it. Each test below
    // reads that off the production transcript: the run is repeated for real in
    // a child copy of this test binary, so the lines it printed are captured at
    // the process boundary -- no mock and no private seam into the loop. The
    // duration lives on the permanent call echo, never in a separate command
    // elapsed line.
    // -----------------------------------------------------------------------

    /// The environment variable a child copy of a timing test sets on itself. A
    /// copy that sees it performs the run for real -- letting the loop's own
    /// output reach the streams the parent captured -- and returns, instead of
    /// starting a child of its own.
    const TIMING_CHILD: &str = "EJAJ_TOOL_TIMING_CHILD";

    /// The marker a child copy sees when this run is the missing-bash case: the
    /// parent has emptied PATH, so only the spawn-failure branch may run.
    const TIMING_SPAWN_ONLY: &str = "EJAJ_TOOL_TIMING_SPAWN_ONLY";

    /// Whether this process is the child copy of a timing test.
    fn is_timing_child() -> bool {
        std::env::var_os(TIMING_CHILD).is_some()
    }

    /// The filter that makes a child copy run exactly a whole caller-intent
    /// test name: the module path libtest knows the test by.
    fn timing_child_filter(name: &str) -> String {
        format!("loops::agent::tests::{name}")
    }

    /// Run the timing test name for real in a child copy of this test binary
    /// and return everything that run wrote, in the order it wrote it: the
    /// production transcript, captured at the process boundary.
    fn timing_transcript(name: &str) -> String {
        timing_transcript_with(name, &[])
    }

    /// The same capture with env set on the child, so a test can change one
    /// boundary fact the production path reads -- the search path a tool spawn
    /// looks bash up on, say -- and still read a transcript every byte of
    /// which came from the real run.
    ///
    /// Both streams are redirected onto one file, so the interleaving a terminal
    /// would have shown survives the capture -- a call echo on stdout and a
    /// failure warning on stderr keep their real order instead of being
    /// collected per stream.
    fn timing_transcript_with(name: &str, env: &[(&str, &str)]) -> String {
        let exe = std::env::current_exe().expect("the test binary path is known");
        let filter = timing_child_filter(name);
        let path = std::env::temp_dir().join(format!(
            "ejaj-timing-{}-{name}.transcript",
            std::process::id()
        ));
        let sink = std::fs::File::create(&path).expect("the transcript file is created");
        let mut command = Command::new(exe);
        command
            .args([
                "--exact",
                filter.as_str(),
                "--nocapture",
                "--test-threads=1",
            ])
            .env(TIMING_CHILD, "1")
            .stdout(std::process::Stdio::from(
                sink.try_clone().expect("the transcript file is clonable"),
            ))
            .stderr(std::process::Stdio::from(sink));
        for (name, value) in env {
            command.env(name, value);
        }
        let output = command
            .output()
            .expect("the child test process starts and is waited for");
        let transcript = std::fs::read_to_string(&path).unwrap_or_default();
        let _ = std::fs::remove_file(&path);

        assert!(
            output.status.success(),
            "the child timing run failed ({}):\n{transcript}",
            output.status
        );
        transcript
    }

    /// Run the timing test in a child copy whose PATH names one empty directory
    /// and nothing else, so the production bash spawn finds no executable at
    /// all, and return that run's transcript. The spawn-only marker keeps the
    /// child from trying the cases that need a working bash.
    ///
    /// The directory really exists and really is empty, so the spawn fails for
    /// the one reason this test is about -- there is no bash on the search
    /// path -- rather than because a path was malformed. Only this child's
    /// search path changes; the parent's environment is untouched.
    fn timing_transcript_without_bash(name: &str) -> String {
        let search_path =
            std::env::temp_dir().join(format!("ejaj-empty-path-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&search_path).expect("the empty search path is creatable");
        let search_path = search_path.to_string_lossy().into_owned();

        let transcript = timing_transcript_with(
            name,
            &[("PATH", search_path.as_str()), (TIMING_SPAWN_ONLY, "1")],
        );

        let _ = std::fs::remove_dir_all(&search_path);
        transcript
    }

    /// Run one turn whose single reply requests calls in phase, followed by the
    /// model's tool-free turn, and return the messages the loop built -- the
    /// tool results the model was handed included.
    fn timed_turn(phase: Phase, calls: &[(&str, String)]) -> Vec<Message> {
        let script = Script::new(vec![
            reply(Some("Running the ticket's commands."), calls),
            reply(None, &[]),
        ]);
        let config = config(script.clone());
        let mut messages = vec![Message::system("sys"), Message::user("work the ticket")];

        let stop = run_agent_loop(&mut messages, &config, phase, Source::None);

        assert!(
            matches!(stop, Stop::TurnEnd),
            "the model's tool-free turn must end the pass"
        );
        assert_eq!(
            script.requests(),
            2,
            "the call turn and the tool-free turn, and nothing else"
        );
        messages
    }

    /// The last tool result in messages: what the model was handed back.
    fn last_tool_result(messages: &[Message]) -> String {
        messages
            .iter()
            .rev()
            .find(|message| message.role == Role::Tool)
            .and_then(|message| message.content.clone())
            .expect("the transcript must carry the tool result")
    }

    /// The tool result delivered for one call id, if any.
    fn tool_result_for(messages: &[Message], id: &str) -> Option<String> {
        messages
            .iter()
            .find(|message| {
                message.role == Role::Tool && message.tool_call_id.as_deref() == Some(id)
            })
            .and_then(|message| message.content.clone())
    }

    /// Whether one transcript line is a permanent call echo rather than a log.
    fn is_call_line(line: &str) -> bool {
        let Some(rest) = line.strip_prefix('[') else {
            return false;
        };
        let Some((counter, rest)) = rest.split_once("] ") else {
            return false;
        };
        let Some((step, max)) = counter.split_once('/') else {
            return false;
        };
        let numeric = |value: &str| !value.is_empty() && value.chars().all(|c| c.is_ascii_digit());
        numeric(step) && numeric(max) && (rest.starts_with('(') || rest.starts_with("$ "))
    }

    /// Every permanent tool-call line in transcript, in order.
    fn call_lines(transcript: &str) -> Vec<String> {
        transcript
            .lines()
            .map(str::trim_end)
            .filter(|line| is_call_line(line))
            .map(str::to_string)
            .collect()
    }

    /// Every separate informational elapsed line transcript carries, in order.
    fn elapsed_lines(transcript: &str) -> Vec<String> {
        transcript
            .lines()
            .map(str::trim_end)
            .filter(|line| line.starts_with("[INFO] ") && line.contains(" elapsed: "))
            .map(str::to_string)
            .collect()
    }

    /// The child-run marker carrying the prepared context byte count the parent
    /// compares a captured report against.
    const EXPECTED_CONTEXT: &str = "ejaj-expected-context=";

    /// The child-run marker carrying the encoded outgoing-body size, so the
    /// parent can prove the report is not that figure.
    const EXPECTED_BODY: &str = "ejaj-expected-body=";

    /// Every compact thinking report transcript carries, in order: one per
    /// model attempt that reached the transport.
    fn thinking_lines(transcript: &str) -> Vec<String> {
        transcript
            .lines()
            .map(str::trim_end)
            .filter(|line| line.starts_with("#thinking ("))
            .map(str::to_string)
            .collect()
    }

    /// Every separate command elapsed line, which must never appear: an
    /// executed command's duration belongs on its permanent call line.
    fn command_elapsed_lines(transcript: &str) -> Vec<String> {
        elapsed_lines(transcript)
    }

    /// One compact report's fields, split at the fixed separators.
    struct ThinkingReport<'a> {
        duration: &'a str,
        provider: &'a str,
        model: &'a str,
        phase: &'a str,
        context: &'a str,
    }

    /// Split `#thinking (<duration>s) <provider> model=<model> phase=<phase>
    /// context=<bytes> B` into its fields.
    fn thinking_report_parts(line: &str) -> ThinkingReport<'_> {
        let rest = line
            .strip_prefix("#thinking (")
            .unwrap_or_else(|| panic!("not a compact thinking report: {line:?}"));
        let (duration, rest) = rest
            .split_once("s) ")
            .unwrap_or_else(|| panic!("the report has no duration: {line:?}"));
        let (provider, rest) = rest
            .split_once(" model=")
            .unwrap_or_else(|| panic!("the report has no provider: {line:?}"));
        let (model, rest) = rest
            .split_once(" phase=")
            .unwrap_or_else(|| panic!("the report has no model: {line:?}"));
        let (phase, context) = rest
            .split_once(" context=")
            .unwrap_or_else(|| panic!("the report has no phase: {line:?}"));
        let context = context
            .strip_suffix(" B")
            .unwrap_or_else(|| panic!("the report has no byte unit: {line:?}"));
        ThinkingReport {
            duration,
            provider,
            model,
            phase,
            context,
        }
    }

    /// Assert `line` is the exact compact report shape: whole seconds, a dot,
    /// exactly three floored millisecond digits and the seconds unit, one
    /// canonical provider role, a configured model, a canonical phase label and
    /// a grouped decimal byte figure.
    fn assert_thinking_report(line: &str) {
        assert!(
            !line.starts_with("[INFO]"),
            "the compact report is not an INFO line: {line:?}"
        );
        let report = thinking_report_parts(line);
        let (seconds, millis) = report
            .duration
            .split_once('.')
            .unwrap_or_else(|| panic!("the report has no fraction: {line:?}"));
        assert!(
            !seconds.is_empty() && seconds.chars().all(|c| c.is_ascii_digit()),
            "whole seconds must be digits: {line:?}"
        );
        assert_eq!(millis.len(), 3, "the fraction is three digits: {line:?}");
        assert!(
            millis.chars().all(|c| c.is_ascii_digit()),
            "the fraction must be digits: {line:?}"
        );
        assert!(
            ["mid", "giga", "smoll"].contains(&report.provider),
            "the provider is one canonical role: {line:?}"
        );
        assert!(!report.model.is_empty(), "the model is named: {line:?}");
        assert!(
            [
                "plan", "spec", "tickets", "triage", "code", "review", "summary"
            ]
            .contains(&report.phase),
            "the phase is one canonical label: {line:?}"
        );
        assert!(
            !report.context.is_empty()
                && report
                    .context
                    .chars()
                    .all(|c| c.is_ascii_digit() || c == ','),
            "the context is a grouped decimal figure: {line:?}"
        );
        assert!(
            !report.context.starts_with(',') && !report.context.ends_with(','),
            "the grouping has no edge separator: {line:?}"
        );
    }

    /// The measured seconds of one compact report, for the assertion that a
    /// retry's warning and backoff stayed outside the measured attempt.
    fn thinking_seconds(line: &str) -> f64 {
        thinking_report_parts(line)
            .duration
            .parse()
            .unwrap_or_else(|_| panic!("the report has no numeric duration: {line:?}"))
    }

    /// The context byte count a compact report carries, with its grouping
    /// removed.
    fn thinking_context_bytes(line: &str) -> usize {
        thinking_report_parts(line)
            .context
            .replace(',', "")
            .parse()
            .unwrap_or_else(|_| panic!("the report's context is not a byte count: {line:?}"))
    }

    /// Read one child-run marker value from the captured transcript.
    fn marker_value(transcript: &str, marker: &str) -> usize {
        transcript
            .lines()
            .find_map(|line| line.trim().strip_prefix(marker))
            .unwrap_or_else(|| panic!("missing {marker:?} in:\n{transcript}"))
            .parse()
            .unwrap_or_else(|_| panic!("{marker:?} is not a number in:\n{transcript}"))
    }

    /// The interleaving of thinking reports and retry warnings, as events in
    /// transcript order: each completed attempt is `attempt <n>`, and each
    /// warning that follows it is `retry warning <n>`.
    fn attempt_events(transcript: &str) -> Vec<String> {
        let mut events = Vec::new();
        let mut attempts = 0;
        for line in transcript.lines().map(str::trim_end) {
            if line.starts_with("#thinking (") {
                attempts += 1;
                events.push(format!("attempt {attempts}"));
            } else if line.starts_with("[WARN] attempt ") && line.contains("; retrying.") {
                events.push(format!("retry warning {attempts}"));
            }
        }
        events
    }

    /// Every standardized error diagnostic in transcript, in order. A detail
    /// that spans lines contributes only its first physical line here; the
    /// tests that need the rest assert on the transcript directly.
    fn error_lines(transcript: &str) -> Vec<String> {
        transcript
            .lines()
            .filter(|line| line.starts_with("[ERROR] "))
            .map(str::to_string)
            .collect()
    }

    /// One long, multiline provider-supplied detail, used by the boundary
    /// cases that prove the diagnostic never truncates or reflows it.
    fn long_multiline_detail() -> String {
        format!("{} first\nsecond line retained verbatim", "x".repeat(4_000))
    }

    /// Start a captured child run on a fresh line. The test harness prints its
    /// progress line without a terminator before the body runs, so without this
    /// the first message the loop writes would share that line and be invisible
    /// to the line-oriented assertions below.
    fn start_timing_child() {
        println!();
    }

    /// Assert one call line carries an inline duration in the exact
    /// seconds-dot-three-floored-milliseconds-s form, and return that duration.
    fn assert_inline_duration(line: &str) -> &str {
        let after_counter = line
            .split_once("] ")
            .unwrap_or_else(|| panic!("the call line has no counter: {line:?}"))
            .1;
        let after_open = after_counter
            .strip_prefix('(')
            .unwrap_or_else(|| panic!("the executed call must carry a duration: {line:?}"));
        let (duration, _) = after_open
            .split_once(") $")
            .unwrap_or_else(|| panic!("the duration must precede the dollar marker: {line:?}"));
        let (seconds, fraction) = duration
            .strip_suffix('s')
            .and_then(|value| value.split_once('.'))
            .unwrap_or_else(|| panic!("the inline duration has no fraction: {line:?}"));
        assert!(
            !seconds.is_empty() && seconds.chars().all(|c| c.is_ascii_digit()),
            "whole seconds must be digits: {line:?}"
        );
        assert_eq!(
            fraction.len(),
            3,
            "the inline fraction is exactly three digits: {line:?}"
        );
        assert!(
            fraction.chars().all(|c| c.is_ascii_digit()),
            "the inline fraction must be ASCII digits: {line:?}"
        );
        duration
    }

    /// Assert transcript carries exactly one executed call echo, on step one,
    /// with an inline duration and the expected command display -- and no
    /// separate command elapsed log.
    fn assert_one_timed_call(transcript: &str, display: &str) -> String {
        let calls = call_lines(transcript);
        assert_eq!(
            calls.len(),
            1,
            "expected exactly one call echo, found {calls:?} in:\n{transcript}"
        );
        let line = calls[0].clone();
        assert!(
            line.starts_with("[1/100] "),
            "the first step is one-based: {line:?}"
        );
        assert_inline_duration(&line);
        assert!(
            line.ends_with(&format!("$ {display}")),
            "the call line lost its display: {line:?}"
        );
        assert!(
            !line.contains('\u{1b}'),
            "a redirected call line stays plain: {line:?}"
        );
        assert!(
            command_elapsed_lines(transcript).is_empty(),
            "the duration belongs on the call line, not in a separate log:\n{transcript}"
        );
        line
    }

    /// Assert transcript carries count rejected call echoes, each exactly the
    /// expected untimed line, with no duration and no separate elapsed log.
    fn assert_untimed_calls(transcript: &str, count: usize, expected: &str) {
        let calls = call_lines(transcript);
        assert_eq!(
            calls.len(),
            count,
            "expected {count} call echoes, found {calls:?} in:\n{transcript}"
        );
        for line in &calls {
            assert_eq!(line, expected, "a rejected call is untimed:\n{transcript}");
            assert!(
                !line.contains(") $"),
                "a rejected call must not imply that it ran: {line:?}"
            );
        }
        assert!(
            command_elapsed_lines(transcript).is_empty(),
            "a rejected call reports no command elapsed line:\n{transcript}"
        );
    }

    /// Core entry-point behavior: a successful bash call that really ran shows
    /// its duration inline on the one permanent call echo, while its output
    /// stays in the model-visible tool result.
    #[test]
    fn when_bash_finishes_given_success_then_shows_elapsed_on_call_line() {
        if is_timing_child() {
            let messages = timed_turn(
                Phase::Code,
                &[(
                    "bash",
                    serde_json::json!({ "command": "printf 'ejaj-\\x6futput'" }).to_string(),
                )],
            );
            let result = last_tool_result(&messages);
            assert_eq!(
                result.trim(),
                "ejaj-output",
                "the command output must reach the model: {result:?}"
            );
            return;
        }

        let transcript =
            timing_transcript("when_bash_finishes_given_success_then_shows_elapsed_on_call_line");
        assert_one_timed_call(&transcript, "printf 'ejaj-\\x6futput'");
        assert!(
            !transcript.contains("ejaj-output"),
            "command output stays out of the visible call line:\n{transcript}"
        );
    }

    /// Core entry-point behavior: a successful execute_script call shows its
    /// js-prefixed display and inline duration, while the script result remains
    /// in the tool result.
    #[test]
    fn when_script_finishes_given_success_then_shows_elapsed_on_call_line() {
        if is_timing_child() {
            let messages = timed_turn(
                Phase::Code,
                &[(
                    "execute_script",
                    serde_json::json!({ "code": "return 40 + 2;" }).to_string(),
                )],
            );
            assert_eq!(
                last_tool_result(&messages).trim(),
                "42",
                "the script result must reach the model"
            );
            return;
        }

        let transcript =
            timing_transcript("when_script_finishes_given_success_then_shows_elapsed_on_call_line");
        assert_one_timed_call(&transcript, "js: return 40 + 2;");
        assert!(
            !transcript.lines().any(|line| line.trim() == "42"),
            "the script result stays out of the visible call line:\n{transcript}"
        );
    }

    /// Zero/empty result: a command whose output is empty still shows an inline
    /// duration, and the tool-free turn that follows adds no command timing.
    #[test]
    fn when_command_finishes_given_empty_output_then_shows_inline_duration() {
        if is_timing_child() {
            let messages = timed_turn(
                Phase::Code,
                &[("bash", serde_json::json!({ "command": "true" }).to_string())],
            );
            assert!(
                last_tool_result(&messages).trim().is_empty(),
                "the command really produced no output"
            );
            return;
        }

        let transcript = timing_transcript(
            "when_command_finishes_given_empty_output_then_shows_inline_duration",
        );
        assert_one_timed_call(&transcript, "true");
    }

    /// Contract: a success and a failure keep the existing model-visible
    /// results, the failure warning survives, no command-tool INFO elapsed line
    /// is written, and the warning precedes the permanent call echo.
    #[test]
    fn when_command_finishes_given_success_or_error_then_keeps_result_and_omits_duplicate_elapsed_log()
     {
        if is_timing_child() {
            let success = timed_turn(
                Phase::Code,
                &[(
                    "bash",
                    serde_json::json!({ "command": "echo ejaj-success" }).to_string(),
                )],
            );
            assert!(
                last_tool_result(&success).contains("ejaj-success"),
                "the success output must reach the model"
            );

            let failure = timed_turn(
                Phase::Code,
                &[(
                    "bash",
                    serde_json::json!({ "command": "exit 7" }).to_string(),
                )],
            );
            assert!(
                last_tool_result(&failure).contains("[ERROR: exit code 7"),
                "the failure result must reach the model unchanged"
            );
            return;
        }

        let transcript = timing_transcript(
            "when_command_finishes_given_success_or_error_then_keeps_result_and_omits_duplicate_elapsed_log",
        );
        let calls = call_lines(&transcript);
        assert_eq!(
            calls.len(),
            2,
            "one call echo per executed command:\n{transcript}"
        );
        for line in &calls {
            assert_inline_duration(line);
        }
        assert!(
            command_elapsed_lines(&transcript).is_empty(),
            "no separate command elapsed line:\n{transcript}"
        );
        assert!(
            !thinking_lines(&transcript).is_empty(),
            "the model attempts still report their own thinking duration:\n{transcript}"
        );

        let warning = "[WARN] bash failed: [ERROR: exit code 7";
        let warning_at = transcript
            .find(warning)
            .expect("the failure warning is present");
        let echo_at = transcript
            .find(&calls[1])
            .expect("the failure call echo is present");
        assert!(
            warning_at < echo_at,
            "the warning precedes the permanent call echo:\n{transcript}"
        );
    }

    /// Regression: a non-zero exit, a spawn failure and a script runtime error
    /// each keep their warning and model-visible result, show exactly one inline
    /// duration, and emit no separate command elapsed log.
    #[test]
    fn when_command_fails_given_nonzero_exit_spawn_error_or_script_error_then_keeps_warning_and_result()
     {
        if is_timing_child() {
            if std::env::var_os(TIMING_SPAWN_ONLY).is_some() {
                let messages = timed_turn(
                    Phase::Code,
                    &[(
                        "bash",
                        serde_json::json!({ "command": "echo ejaj-no-bash" }).to_string(),
                    )],
                );
                let result = last_tool_result(&messages);
                let mut lines = result.lines();
                assert!(
                    lines
                        .next()
                        .is_some_and(|line| line.starts_with("[ERROR: exit code -1 for ")),
                    "the failed spawn must reach the model as its error result: {result:?}"
                );
                assert!(
                    lines.next().is_some_and(|line| !line.trim().is_empty()),
                    "the spawn's own error keeps its line under the heading: {result:?}"
                );
                assert!(
                    result.contains("echo ejaj-no-bash"),
                    "the spawn result keeps the command it could not start: {result:?}"
                );
                return;
            }

            let nonzero = timed_turn(
                Phase::Code,
                &[(
                    "bash",
                    serde_json::json!({ "command": "exit 7" }).to_string(),
                )],
            );
            assert!(
                last_tool_result(&nonzero).contains("[ERROR: exit code 7"),
                "the non-zero exit result must reach the model unchanged"
            );

            let script = timed_turn(
                Phase::Code,
                &[(
                    "execute_script",
                    serde_json::json!({ "code": "throw new Error('boom');" }).to_string(),
                )],
            );
            assert!(
                last_tool_result(&script).contains("boom"),
                "the script error must reach the model"
            );
            return;
        }

        let transcript = timing_transcript(
            "when_command_fails_given_nonzero_exit_spawn_error_or_script_error_then_keeps_warning_and_result",
        );
        let calls = call_lines(&transcript);
        assert_eq!(
            calls.len(),
            2,
            "the non-zero exit and the script error each echo once:\n{transcript}"
        );
        for line in &calls {
            assert_inline_duration(line);
        }
        assert!(
            command_elapsed_lines(&transcript).is_empty(),
            "a failed command still has no separate elapsed log:\n{transcript}"
        );

        let nonzero_warning = "[WARN] bash failed: [ERROR: exit code 7";
        let script_warning = "[WARN] execute_script failed:";
        let nonzero_at = transcript
            .find(nonzero_warning)
            .expect("the non-zero warning is present");
        let nonzero_echo_at = transcript
            .find(&calls[0])
            .expect("the non-zero call echo is present");
        let script_at = transcript
            .find(script_warning)
            .expect("the script warning is present");
        let script_echo_at = transcript
            .find(&calls[1])
            .expect("the script call echo is present");
        assert!(
            nonzero_at < nonzero_echo_at
                && nonzero_echo_at < script_at
                && script_at < script_echo_at,
            "each warning precedes its permanent call echo:\n{transcript}"
        );

        let spawn = timing_transcript_without_bash(
            "when_command_fails_given_nonzero_exit_spawn_error_or_script_error_then_keeps_warning_and_result",
        );
        let spawn_calls = call_lines(&spawn);
        assert_eq!(
            spawn_calls.len(),
            1,
            "the failed spawn still echoes its call once:\n{spawn}"
        );
        assert_inline_duration(&spawn_calls[0]);
        assert!(
            spawn_calls[0].ends_with("$ echo ejaj-no-bash"),
            "the spawn failure keeps its display: {spawn_calls:?}"
        );
        assert!(
            spawn.contains("[WARN] bash failed: [ERROR: exit code -1"),
            "the existing spawn warning must survive:\n{spawn}"
        );
        let spawn_warning_at = spawn
            .find("[WARN] bash failed: [ERROR: exit code -1")
            .expect("the spawn warning is present");
        let spawn_echo_at = spawn
            .find(&spawn_calls[0])
            .expect("the spawn call echo is present");
        assert!(
            spawn_warning_at < spawn_echo_at,
            "the spawn warning precedes the call echo:\n{spawn}"
        );
        assert!(
            command_elapsed_lines(&spawn).is_empty(),
            "a failed spawn has no separate elapsed log:\n{spawn}"
        );
    }

    /// Malformed/invalid payload boundary: a call rejected before Tool::run is
    /// untimed, keeps its existing rejection result and warning, and never emits
    /// an informational duration line.
    #[test]
    fn when_invalid_call_is_rejected_given_malformed_arguments_then_has_no_duration() {
        if is_timing_child() {
            let messages = timed_turn(Phase::Code, &[("bash", "{ this is not json".to_string())]);
            let result = last_tool_result(&messages);
            assert!(
                !result.is_empty(),
                "the rejection must reach the model as a tool result"
            );
            return;
        }

        let transcript = timing_transcript(
            "when_invalid_call_is_rejected_given_malformed_arguments_then_has_no_duration",
        );
        assert_untimed_calls(&transcript, 1, "[1/100] $ bash");
        assert!(
            transcript.contains("[WARN] bash rejected:"),
            "the existing rejection warning must survive:\n{transcript}"
        );
    }

    /// Unauthorized boundary: bash in a phase that forbids it is rejected
    /// before Tool::run, so its call echo stays untimed and no execution warning
    /// or duration appears.
    #[test]
    fn when_bash_is_forbidden_given_non_code_phase_then_keeps_rejection_without_duration() {
        if is_timing_child() {
            let messages = timed_turn(
                Phase::Plan,
                &[(
                    "bash",
                    serde_json::json!({ "command": "echo forbidden" }).to_string(),
                )],
            );
            let result = last_tool_result(&messages);
            assert!(
                result.contains("only available in the code phase"),
                "the phase refusal must reach the model: {result:?}"
            );
            return;
        }

        let transcript = timing_transcript(
            "when_bash_is_forbidden_given_non_code_phase_then_keeps_rejection_without_duration",
        );
        assert_untimed_calls(&transcript, 1, "[1/100] $ bash");
        assert!(
            transcript.contains("[WARN] bash rejected:"),
            "the existing rejection warning must survive:\n{transcript}"
        );
    }

    /// Malformed and forbidden requests pair: both are rejected before
    /// Tool::run, so neither gains a duration and both keep their warnings.
    #[test]
    fn when_command_call_is_rejected_given_malformed_or_forbidden_request_then_remains_untimed() {
        if is_timing_child() {
            let malformed = timed_turn(Phase::Code, &[("bash", "{ this is not json".to_string())]);
            assert!(
                !last_tool_result(&malformed).is_empty(),
                "the malformed rejection must reach the model"
            );

            let forbidden = timed_turn(
                Phase::Plan,
                &[(
                    "bash",
                    serde_json::json!({ "command": "echo forbidden" }).to_string(),
                )],
            );
            assert!(
                last_tool_result(&forbidden).contains("only available in the code phase"),
                "the forbidden rejection must reach the model"
            );
            return;
        }

        let transcript = timing_transcript(
            "when_command_call_is_rejected_given_malformed_or_forbidden_request_then_remains_untimed",
        );
        assert_untimed_calls(&transcript, 2, "[1/100] $ bash");
        assert_eq!(
            transcript.matches("[WARN] bash rejected:").count(),
            2,
            "each rejection keeps its warning:\n{transcript}"
        );
    }

    /// Regression: a valid handoff, a completion and a tool-free turn are not
    /// commands. None gains a call echo or a command duration, and their
    /// existing control behavior is unchanged.
    #[test]
    fn when_control_call_finishes_given_handoff_or_tool_free_turn_then_emits_no_command_timing() {
        if is_timing_child() {
            let handoff = Script::new(vec![reply(
                Some("Handing the settled design over now."),
                &[("finish_plan", finish_plan_arguments())],
            )]);
            let handoff_config = config(handoff.clone());
            let mut messages = plan_transcript();
            assert!(
                matches!(
                    run_agent_loop(&mut messages, &handoff_config, Phase::Plan, Source::None),
                    Stop::Handoff(_)
                ),
                "the handoff must end the plan pass"
            );
            assert_eq!(handoff.requests(), 1);

            let completion = Script::new(vec![reply(None, &[("task_complete", "{}".to_string())])]);
            let completion_config = config(completion.clone());
            let mut messages = plan_transcript();
            assert!(
                matches!(
                    run_agent_loop(&mut messages, &completion_config, Phase::Code, Source::None),
                    Stop::Complete
                ),
                "the completion must end the code pass"
            );
            assert_eq!(completion.requests(), 1);

            let free_turn = Script::new(vec![reply(Some("Nothing to run."), &[])]);
            let free_turn_config = config(free_turn.clone());
            let mut messages = vec![Message::system("sys"), Message::user("work the ticket")];
            assert!(
                matches!(
                    run_agent_loop(&mut messages, &free_turn_config, Phase::Code, Source::None),
                    Stop::TurnEnd
                ),
                "a tool-free turn ends the pass"
            );
            assert_eq!(free_turn.requests(), 1);
            return;
        }

        let transcript = timing_transcript(
            "when_control_call_finishes_given_handoff_or_tool_free_turn_then_emits_no_command_timing",
        );
        assert!(
            call_lines(&transcript).is_empty(),
            "a handoff, completion or tool-free turn has no call echo:\n{transcript}"
        );
        assert!(
            command_elapsed_lines(&transcript).is_empty(),
            "a control or tool-free turn has no command duration:\n{transcript}"
        );
    }

    // -----------------------------------------------------------------------
    // Per-attempt thinking timing
    //
    // Every attempt that reaches the transport is one measured send plus decode,
    // and the measured interval is reported once on the thinking label after the
    // attempt finishes. The report is emitted before any retry decision, warning
    // or backoff, so a failed attempt never carries the time spent deciding to
    // retry. As with the command timings above, every assertion is read off a
    // real child run captured at the process boundary.
    // -----------------------------------------------------------------------

    /// Core entry-point behavior: one successful request is one completed
    /// attempt, so it writes exactly one compact metadata report.
    #[test]
    fn when_model_attempt_finishes_given_success_then_reports_compact_request_metadata() {
        if is_timing_child() {
            start_timing_child();
            let script = Script::new(vec![reply(Some("done"), &[])]);
            let config = config(script.clone());
            let mut messages = vec![Message::system("sys"), Message::user("work the ticket")];
            assert!(matches!(
                run_agent_loop(&mut messages, &config, Phase::Code, Source::None),
                Stop::TurnEnd
            ));
            assert_eq!(script.requests(), 1, "one completed send");
            return;
        }

        let transcript = timing_transcript(
            "when_model_attempt_finishes_given_success_then_reports_compact_request_metadata",
        );
        let lines = thinking_lines(&transcript);
        assert_eq!(
            lines.len(),
            1,
            "one successful attempt is reported once:\n{transcript}"
        );
        assert_thinking_report(&lines[0]);
        let report = thinking_report_parts(&lines[0]);
        assert_eq!(report.provider, "mid");
        assert_eq!(report.model, "test-model");
        assert_eq!(report.phase, "code");
        assert!(
            thinking_context_bytes(&lines[0]) > 0,
            "a real prepared request carries context: {transcript}"
        );
        assert!(
            !transcript.contains("[INFO] thinking elapsed:"),
            "the old model-attempt elapsed line is gone:\n{transcript}"
        );
    }

    /// Empty/zero boundary: a guard stop never reaches the transport, so there
    /// is no attempt to report. The child scripts no reply at all, so any send
    /// would panic before the parent could read the transcript.
    #[test]
    fn when_context_guard_stops_given_oversized_request_then_emits_no_thinking_report() {
        if is_timing_child() {
            start_timing_child();
            let script = Script::new(Vec::new());
            let config = config(script.clone());
            let mut messages = vec![
                Message::system("x".repeat(CONTEXT_GUARD_BYTES + 1)),
                Message::user("work the ticket"),
            ];
            assert!(matches!(
                run_agent_loop(&mut messages, &config, Phase::Code, Source::None),
                Stop::Failed(FailureKind::ContextGuard)
            ));
            assert_eq!(script.requests(), 0, "the guard stops before any send");
            return;
        }

        let transcript = timing_transcript(
            "when_context_guard_stops_given_oversized_request_then_emits_no_thinking_report",
        );
        assert!(
            thinking_lines(&transcript).is_empty(),
            "no send means no thinking report:\n{transcript}"
        );
        assert!(
            transcript.contains("context guard"),
            "the guard failure is still reported:\n{transcript}"
        );
    }

    /// Minimum valid response boundary: a decoded reply with no content and no
    /// tool calls still completed one send, so it gets one compact report.
    #[test]
    fn when_model_attempt_finishes_given_tool_free_response_then_reports_once() {
        if is_timing_child() {
            start_timing_child();
            let script = Script::new(vec![reply(None, &[])]);
            let config = config(script.clone());
            let mut messages = vec![Message::system("sys"), Message::user("work the ticket")];
            assert!(matches!(
                run_agent_loop(&mut messages, &config, Phase::Code, Source::None),
                Stop::TurnEnd
            ));
            assert_eq!(script.requests(), 1, "one completed send");
            return;
        }

        let transcript = timing_transcript(
            "when_model_attempt_finishes_given_tool_free_response_then_reports_once",
        );
        let lines = thinking_lines(&transcript);
        assert_eq!(
            lines.len(),
            1,
            "a tool-free response is still one attempt:\n{transcript}"
        );
        assert_thinking_report(&lines[0]);
    }

    /// Upper boundary: the attempt limit is unchanged at three, and each failed
    /// attempt's report precedes the warning and backoff that follow it.
    #[test]
    fn when_model_attempt_retries_given_transient_failures_then_reports_each_attempt_before_warning()
     {
        if is_timing_child() {
            start_timing_child();
            let script = Script::with_errors(vec![
                CallError::Transport("connection reset".to_string()),
                CallError::Transport("connection reset".to_string()),
                CallError::Transport("connection reset".to_string()),
            ]);
            let config = config(script.clone());
            let mut messages = vec![Message::system("sys"), Message::user("work the ticket")];
            assert!(matches!(
                run_agent_loop(&mut messages, &config, Phase::Code, Source::None),
                Stop::Failed(FailureKind::Transport)
            ));
            assert_eq!(script.requests(), 3, "the attempt limit is unchanged");
            assert_eq!(retry::MAX_ATTEMPTS, 3);
            return;
        }

        let transcript = timing_transcript(
            "when_model_attempt_retries_given_transient_failures_then_reports_each_attempt_before_warning",
        );
        let lines = thinking_lines(&transcript);
        assert_eq!(
            lines.len(),
            retry::MAX_ATTEMPTS as usize,
            "one report per attempt:\n{transcript}"
        );
        for line in &lines {
            assert_thinking_report(line);
            assert!(
                thinking_seconds(line) < 1.0,
                "the retry warning and backoff are outside the measured attempt: {line:?}"
            );
        }
        assert_eq!(
            attempt_events(&transcript),
            [
                "attempt 1",
                "retry warning 1",
                "attempt 2",
                "retry warning 2",
                "attempt 3",
            ],
            "each report lands before the retry warning that follows it:\n{transcript}"
        );
    }

    /// Retryable status table: each transient API status the policy allows --
    /// 429, 500, 502, 503 and 504 -- is retried after the failed attempt and
    /// recovered by the next reply, so the loop finishes the turn inside the
    /// unchanged three-attempt budget. The last scenario spends that whole
    /// budget on one transient status, which proves the cap holds for an API
    /// failure too: the final attempt is not retried and its failure reaches
    /// the ordinary error path exactly once.
    #[test]
    fn when_retryable_api_errors_are_recovered_given_transient_statuses_then_retries_within_the_attempt_budget()
     {
        if is_timing_child() {
            start_timing_child();
            for status in [429, 500, 502, 503, 504] {
                let script = Script::with_replies(vec![
                    Reply::Error(CallError::Api {
                        status,
                        message: "overloaded".to_string(),
                    }),
                    Reply::Body(reply(Some("done"), &[])),
                ]);
                let config = config(script.clone());
                let mut messages = vec![Message::system("sys"), Message::user("work the ticket")];
                assert!(matches!(
                    run_agent_loop(&mut messages, &config, Phase::Code, Source::None),
                    Stop::TurnEnd
                ));
                assert_eq!(
                    script.requests(),
                    2,
                    "status {status} is retried once and the retry recovered the turn"
                );
                assert_eq!(
                    retry::MAX_ATTEMPTS,
                    3,
                    "the configured retry attempt budget is unchanged"
                );
            }

            let script = Script::with_errors(vec![
                CallError::Api {
                    status: 503,
                    message: "overloaded".to_string(),
                },
                CallError::Api {
                    status: 503,
                    message: "overloaded".to_string(),
                },
                CallError::Api {
                    status: 503,
                    message: "overloaded".to_string(),
                },
            ]);
            let config = config(script.clone());
            let mut messages = vec![Message::system("sys"), Message::user("work the ticket")];
            assert!(matches!(
                run_agent_loop(&mut messages, &config, Phase::Code, Source::None),
                Stop::Failed(FailureKind::Api)
            ));
            assert_eq!(
                script.requests(),
                retry::MAX_ATTEMPTS as usize,
                "a transient status still stops at the attempt budget"
            );
            return;
        }

        let transcript = timing_transcript(
            "when_retryable_api_errors_are_recovered_given_transient_statuses_then_retries_within_the_attempt_budget",
        );
        let lines = thinking_lines(&transcript);
        assert_eq!(
            lines.len(),
            13,
            "each recovered status reports both attempts and the spent budget reports all three:\n{transcript}"
        );
        for line in &lines {
            assert_thinking_report(line);
            assert!(
                thinking_seconds(line) < 1.0,
                "the retry warning and backoff stay outside the measured attempt: {line:?}"
            );
        }
        let mut expected = Vec::new();
        for recovery in 0..5 {
            let first = recovery * 2 + 1;
            expected.push(format!("attempt {first}"));
            expected.push(format!("retry warning {first}"));
            expected.push(format!("attempt {}", first + 1));
        }
        for attempt in 11..=12 {
            expected.push(format!("attempt {attempt}"));
            expected.push(format!("retry warning {attempt}"));
        }
        expected.push("attempt 13".to_string());
        assert_eq!(
            attempt_events(&transcript),
            expected,
            "every attempt report lands before the retry warning that follows it:\n{transcript}"
        );
        for status in [429, 500, 502, 503, 504] {
            assert!(
                transcript.contains(&format!(
                    "[WARN] attempt 1/3 failed: API error {status}: overloaded; retrying."
                )),
                "status {status} retries inside the three-attempt budget:\n{transcript}"
            );
        }
        assert_eq!(
            transcript.matches("[WARN] attempt ").count(),
            7,
            "one warning per retry, and no retry past the budget:\n{transcript}"
        );
        assert_eq!(
            error_lines(&transcript),
            ["[ERROR] 503 overloaded"],
            "the spent budget reports its final API failure once:\n{transcript}"
        );
    }

    /// Malformed payload boundary: the completed request is reported once, and
    /// the failed decode keeps its permanent outcome -- no retry and no warning.
    #[test]
    fn when_model_attempt_fails_given_malformed_response_then_reports_once_and_preserves_failure() {
        if is_timing_child() {
            start_timing_child();
            let script = Script::new(vec![serde_json::json!({ "choices": [] }).to_string()]);
            let config = config(script.clone());
            let mut messages = vec![Message::system("sys"), Message::user("work the ticket")];
            assert!(matches!(
                run_agent_loop(&mut messages, &config, Phase::Code, Source::None),
                Stop::Failed(FailureKind::Malformed)
            ));
            assert_eq!(script.requests(), 1, "a malformed response is not retried");
            return;
        }

        let transcript = timing_transcript(
            "when_model_attempt_fails_given_malformed_response_then_reports_once_and_preserves_failure",
        );
        let lines = thinking_lines(&transcript);
        assert_eq!(
            lines.len(),
            1,
            "the completed request is reported once:\n{transcript}"
        );
        assert_thinking_report(&lines[0]);
        assert_eq!(
            transcript.matches("[WARN] attempt ").count(),
            0,
            "a malformed response is not retried:\n{transcript}"
        );
        assert!(
            transcript.contains("Malformed response"),
            "the malformed outcome is unchanged:\n{transcript}"
        );
    }

    /// Unauthorized and forbidden boundaries: a permanent API failure is one
    /// completed attempt. Each gets one report and is never retried.
    #[test]
    fn when_access_is_denied_given_unauthorized_or_forbidden_response_then_reports_once_without_retry()
     {
        if is_timing_child() {
            start_timing_child();
            for (status, message) in [(401, "unauthorized"), (403, "forbidden")] {
                let script = Script::with_errors(vec![CallError::Api {
                    status,
                    message: message.to_string(),
                }]);
                let config = config(script.clone());
                let mut messages = vec![Message::system("sys"), Message::user("work the ticket")];
                assert!(matches!(
                    run_agent_loop(&mut messages, &config, Phase::Code, Source::None),
                    Stop::Failed(FailureKind::Api)
                ));
                assert_eq!(
                    script.requests(),
                    1,
                    "a permanent API error is not retried (HTTP {status})"
                );
            }
            return;
        }

        let transcript = timing_transcript(
            "when_access_is_denied_given_unauthorized_or_forbidden_response_then_reports_once_without_retry",
        );
        let lines = thinking_lines(&transcript);
        assert_eq!(
            lines.len(),
            2,
            "each permanent failure reports its one completed attempt:\n{transcript}"
        );
        for line in &lines {
            assert_thinking_report(line);
            let report = thinking_report_parts(line);
            assert_eq!(report.provider, "mid");
            assert_eq!(report.model, "test-model");
            assert_eq!(report.phase, "code");
        }
        assert_eq!(
            transcript.matches("[WARN] attempt ").count(),
            0,
            "neither 401 nor 403 is retried:\n{transcript}"
        );
        assert!(
            transcript.contains("[ERROR] 401 unauthorized"),
            "the unauthorized failure is still reported:\n{transcript}"
        );
        assert!(
            transcript.contains("[ERROR] 403 forbidden"),
            "the forbidden failure is still reported:\n{transcript}"
        );
        assert!(
            !transcript.contains("[API Error"),
            "the legacy API wrapper must be absent:\n{transcript}"
        );
    }

    // -----------------------------------------------------------------------
    // Standardized CallError diagnostics
    //
    // Every failure kind is driven through the real loop against the scripted
    // transport, and the diagnostic is read from one combined process
    // transcript: stderr as the user would see it, with stdout interleaved.
    // The only double is the external transport boundary.
    // -----------------------------------------------------------------------

    /// Core entry-point behavior: a transport failure still takes the retry
    /// path, and its final diagnostic is one standardized stderr marker with
    /// the provider's detail.
    #[test]
    fn when_provider_returns_transport_failure_given_connection_reset_then_reports_standard_error_and_preserves_retry_count()
     {
        if is_timing_child() {
            start_timing_child();
            let script = Script::with_errors(vec![
                CallError::Transport("connection reset".to_string()),
                CallError::Transport("connection reset".to_string()),
                CallError::Transport("connection reset".to_string()),
            ]);
            let config = config(script.clone());
            let mut messages = vec![Message::system("sys"), Message::user("work the ticket")];
            assert!(matches!(
                run_agent_loop(&mut messages, &config, Phase::Code, Source::None),
                Stop::Failed(FailureKind::Transport)
            ));
            assert_eq!(
                script.requests(),
                retry::MAX_ATTEMPTS as usize,
                "the transport retry count is unchanged"
            );
            return;
        }

        let transcript = timing_transcript(
            "when_provider_returns_transport_failure_given_connection_reset_then_reports_standard_error_and_preserves_retry_count",
        );
        assert_eq!(
            error_lines(&transcript),
            vec!["[ERROR] connection reset".to_string()],
            "the final transport failure is one standardized diagnostic:\n{transcript}"
        );
        assert_eq!(
            transcript.matches("[WARN] attempt ").count(),
            retry::MAX_ATTEMPTS as usize - 1,
            "the transient retries still warn before the final failure:\n{transcript}"
        );
        assert!(
            !transcript.contains("[Error calling LLM:"),
            "the legacy transport wrapper must be absent:\n{transcript}"
        );
    }

    /// Core entry-point behavior: a malformed response is permanent, so it is
    /// reported once with the complete standardized diagnostic.
    #[test]
    fn when_provider_returns_invalid_response_given_malformed_json_then_reports_standard_error_without_retry()
     {
        if is_timing_child() {
            start_timing_child();
            let script =
                Script::with_errors(vec![CallError::Malformed("invalid JSON".to_string())]);
            let config = config(script.clone());
            let mut messages = vec![Message::system("sys"), Message::user("work the ticket")];
            assert!(matches!(
                run_agent_loop(&mut messages, &config, Phase::Code, Source::None),
                Stop::Failed(FailureKind::Malformed)
            ));
            assert_eq!(script.requests(), 1, "a malformed response is not retried");
            return;
        }

        let transcript = timing_transcript(
            "when_provider_returns_invalid_response_given_malformed_json_then_reports_standard_error_without_retry",
        );
        assert_eq!(
            error_lines(&transcript),
            vec!["[ERROR] Malformed response: invalid JSON".to_string()],
            "the complete malformed diagnostic is standardized:\n{transcript}"
        );
        assert_eq!(
            transcript.matches("[WARN] attempt ").count(),
            0,
            "a malformed response is not retried:\n{transcript}"
        );
        assert!(
            !transcript.contains("[API Error"),
            "the legacy API wrapper must be absent:\n{transcript}"
        );
    }

    /// Empty/zero boundary: an empty provider detail still produces the marker
    /// and its required trailing space, for every failure shape -- transport,
    /// API status and malformed response alike.
    #[test]
    fn when_provider_returns_failure_given_empty_detail_then_emits_the_error_marker_and_trailing_space()
     {
        if is_timing_child() {
            start_timing_child();
            let transport = Script::with_errors(vec![
                CallError::Transport(String::new()),
                CallError::Transport(String::new()),
                CallError::Transport(String::new()),
            ]);
            let transport_config = config(transport.clone());
            let mut transport_messages =
                vec![Message::system("sys"), Message::user("work the ticket")];
            assert!(matches!(
                run_agent_loop(
                    &mut transport_messages,
                    &transport_config,
                    Phase::Code,
                    Source::None
                ),
                Stop::Failed(FailureKind::Transport)
            ));
            assert_eq!(
                transport.requests(),
                3,
                "the transport retry count is unchanged"
            );

            let api = Script::with_errors(vec![CallError::Api {
                status: 401,
                message: String::new(),
            }]);
            let api_config = config(api.clone());
            let mut api_messages = vec![Message::system("sys"), Message::user("work the ticket")];
            assert!(matches!(
                run_agent_loop(&mut api_messages, &api_config, Phase::Code, Source::None),
                Stop::Failed(FailureKind::Api)
            ));
            assert_eq!(api.requests(), 1, "a permanent API error is not retried");

            let malformed = Script::with_errors(vec![CallError::Malformed(String::new())]);
            let malformed_config = config(malformed.clone());
            let mut malformed_messages =
                vec![Message::system("sys"), Message::user("work the ticket")];
            assert!(matches!(
                run_agent_loop(
                    &mut malformed_messages,
                    &malformed_config,
                    Phase::Code,
                    Source::None
                ),
                Stop::Failed(FailureKind::Malformed)
            ));
            assert_eq!(
                malformed.requests(),
                1,
                "an empty malformed detail is not retried"
            );
            return;
        }

        let transcript = timing_transcript(
            "when_provider_returns_failure_given_empty_detail_then_emits_the_error_marker_and_trailing_space",
        );
        let errors = error_lines(&transcript);
        assert_eq!(
            errors
                .iter()
                .filter(|line| line.as_str() == "[ERROR] ")
                .count(),
            1,
            "the empty transport detail keeps the marker and its trailing space:\n{transcript}"
        );
        assert_eq!(
            errors
                .iter()
                .filter(|line| line.as_str() == "[ERROR] 401 ")
                .count(),
            1,
            "an empty API detail keeps the status and the trailing space:\n{transcript}"
        );
        assert_eq!(
            errors
                .iter()
                .filter(|line| line.as_str() == "[ERROR] Malformed response: ")
                .count(),
            1,
            "an empty malformed detail keeps the whole marker and its trailing space:\n{transcript}"
        );
        assert_eq!(
            errors.len(),
            3,
            "no empty failure drops its event:\n{transcript}"
        );
        assert_eq!(
            transcript.matches("[WARN] attempt ").count(),
            retry::MAX_ATTEMPTS as usize - 1,
            "only the transport detail retries; the empty API and malformed details do not:\n{transcript}"
        );
    }

    /// Upper boundary: a long, multiline detail is retained verbatim, and the
    /// marker still begins its first diagnostic line.
    #[test]
    fn when_provider_returns_failure_given_long_or_multiline_detail_then_preserves_supplied_text() {
        let detail = long_multiline_detail();
        if is_timing_child() {
            start_timing_child();
            let script = Script::with_errors(vec![CallError::Api {
                status: 400,
                message: detail.clone(),
            }]);
            let config = config(script.clone());
            let mut messages = vec![Message::system("sys"), Message::user("work the ticket")];
            assert!(matches!(
                run_agent_loop(&mut messages, &config, Phase::Code, Source::None),
                Stop::Failed(FailureKind::Api)
            ));
            assert_eq!(script.requests(), 1, "a 400 is not retried");
            return;
        }

        let transcript = timing_transcript(
            "when_provider_returns_failure_given_long_or_multiline_detail_then_preserves_supplied_text",
        );
        let first_line = detail.lines().next().unwrap();
        let errors = error_lines(&transcript);
        assert_eq!(errors.len(), 1, "one failure is reported:\n{transcript}");
        assert_eq!(
            errors[0],
            format!("[ERROR] 400 {first_line}"),
            "the marker and the beginning of the detail share the first line:\n{transcript}"
        );
        assert!(
            transcript.contains(&format!("[ERROR] 400 {detail}")),
            "the supplied long, multiline text is retained verbatim:\n{transcript}"
        );
    }

    /// Malformed payload boundary: provider-supplied decode detail survives
    /// after the standardized malformed marker.
    #[test]
    fn when_provider_returns_invalid_response_given_provider_detail_then_preserves_malformed_diagnostic_detail()
     {
        const DETAIL: &str = "unexpected end of input at line 3, column 1";
        if is_timing_child() {
            start_timing_child();
            let script = Script::with_errors(vec![CallError::Malformed(DETAIL.to_string())]);
            let config = config(script.clone());
            let mut messages = vec![Message::system("sys"), Message::user("work the ticket")];
            assert!(matches!(
                run_agent_loop(&mut messages, &config, Phase::Code, Source::None),
                Stop::Failed(FailureKind::Malformed)
            ));
            assert_eq!(script.requests(), 1, "a malformed response is not retried");
            return;
        }

        let transcript = timing_transcript(
            "when_provider_returns_invalid_response_given_provider_detail_then_preserves_malformed_diagnostic_detail",
        );
        assert_eq!(
            error_lines(&transcript),
            vec![format!("[ERROR] Malformed response: {DETAIL}")],
            "the provider's malformed detail is retained:\n{transcript}"
        );
        assert!(
            transcript.contains(DETAIL),
            "the diagnostic detail is not discarded:\n{transcript}"
        );
    }

    /// Unauthorized and forbidden boundaries: both keep their status, their
    /// detail, their API failure outcome and their single attempt.
    #[test]
    fn when_provider_returns_api_failure_given_unauthorized_or_forbidden_status_then_reports_status_without_retry()
     {
        if is_timing_child() {
            start_timing_child();
            for (status, message) in [(401, "unauthorized"), (403, "forbidden")] {
                let script = Script::with_errors(vec![CallError::Api {
                    status,
                    message: message.to_string(),
                }]);
                let config = config(script.clone());
                let mut messages = vec![Message::system("sys"), Message::user("work the ticket")];
                assert!(matches!(
                    run_agent_loop(&mut messages, &config, Phase::Code, Source::None),
                    Stop::Failed(FailureKind::Api)
                ));
                assert_eq!(
                    script.requests(),
                    1,
                    "a permanent API error is not retried (HTTP {status})"
                );
            }
            return;
        }

        let transcript = timing_transcript(
            "when_provider_returns_api_failure_given_unauthorized_or_forbidden_status_then_reports_status_without_retry",
        );
        assert_eq!(
            error_lines(&transcript),
            vec![
                "[ERROR] 401 unauthorized".to_string(),
                "[ERROR] 403 forbidden".to_string(),
            ],
            "each status and detail is reported exactly once:\n{transcript}"
        );
        assert_eq!(
            transcript.matches("[WARN] attempt ").count(),
            0,
            "neither unauthorized nor forbidden is retried:\n{transcript}"
        );
        assert!(
            !transcript.contains("[API Error"),
            "the legacy API wrapper must be absent:\n{transcript}"
        );
    }

    /// Contract: every error kind begins its single diagnostic line with the
    /// exact marker and one space, and the retry decisions stay as they were.
    #[test]
    fn when_call_fails_given_each_error_kind_then_keeps_marker_and_detail_on_one_line() {
        if is_timing_child() {
            start_timing_child();
            let transport = Script::with_errors(vec![
                CallError::Transport("connection reset".to_string()),
                CallError::Transport("connection reset".to_string()),
                CallError::Transport("connection reset".to_string()),
            ]);
            let transport_config = config(transport.clone());
            let mut transport_messages =
                vec![Message::system("sys"), Message::user("work the ticket")];
            assert!(matches!(
                run_agent_loop(
                    &mut transport_messages,
                    &transport_config,
                    Phase::Code,
                    Source::None
                ),
                Stop::Failed(FailureKind::Transport)
            ));
            assert_eq!(
                transport.requests(),
                3,
                "transport keeps its three attempts"
            );

            let malformed =
                Script::with_errors(vec![CallError::Malformed("invalid JSON".to_string())]);
            let malformed_config = config(malformed.clone());
            let mut malformed_messages =
                vec![Message::system("sys"), Message::user("work the ticket")];
            assert!(matches!(
                run_agent_loop(
                    &mut malformed_messages,
                    &malformed_config,
                    Phase::Code,
                    Source::None
                ),
                Stop::Failed(FailureKind::Malformed)
            ));
            assert_eq!(malformed.requests(), 1, "malformed keeps one attempt");

            let api = Script::with_errors(vec![CallError::Api {
                status: 400,
                message: "bad request".to_string(),
            }]);
            let api_config = config(api.clone());
            let mut api_messages = vec![Message::system("sys"), Message::user("work the ticket")];
            assert!(matches!(
                run_agent_loop(&mut api_messages, &api_config, Phase::Code, Source::None),
                Stop::Failed(FailureKind::Api)
            ));
            assert_eq!(api.requests(), 1, "a permanent API error keeps one attempt");
            return;
        }

        let transcript = timing_transcript(
            "when_call_fails_given_each_error_kind_then_keeps_marker_and_detail_on_one_line",
        );
        let errors = error_lines(&transcript);
        assert_eq!(
            errors,
            vec![
                "[ERROR] connection reset".to_string(),
                "[ERROR] Malformed response: invalid JSON".to_string(),
                "[ERROR] 400 bad request".to_string(),
            ],
            "every kind uses the exact marker and its own detail:\n{transcript}"
        );
        for line in &errors {
            assert!(
                !line.starts_with("[ERROR]  "),
                "exactly one space follows the marker: {line:?}"
            );
        }
        assert!(
            !transcript.contains("[Error calling LLM:") && !transcript.contains("[API Error"),
            "no legacy wrapper survives:\n{transcript}"
        );
    }

    /// Regression: redirected diagnostics stay plain text; the marker's red
    /// belongs to the terminal renderer alone.
    #[test]
    fn when_error_is_redirected_given_standardized_diagnostics_then_contains_no_terminal_escape_sequences()
     {
        if is_timing_child() {
            start_timing_child();
            let script = Script::with_errors(vec![CallError::Api {
                status: 401,
                message: "unauthorized".to_string(),
            }]);
            let config = config(script.clone());
            let mut messages = vec![Message::system("sys"), Message::user("work the ticket")];
            assert!(matches!(
                run_agent_loop(&mut messages, &config, Phase::Code, Source::None),
                Stop::Failed(FailureKind::Api)
            ));
            assert_eq!(script.requests(), 1, "a permanent API error is not retried");
            return;
        }

        let transcript = timing_transcript(
            "when_error_is_redirected_given_standardized_diagnostics_then_contains_no_terminal_escape_sequences",
        );
        assert!(
            transcript.contains("[ERROR] 401 unauthorized"),
            "the standardized diagnostic is present:\n{transcript}"
        );
        assert!(
            !transcript.contains('\x1b'),
            "redirected output must contain no terminal escape sequences:\n{transcript}"
        );
    }

    /// Contract: each send attempt reports its own interval, the failed
    /// attempt's report lands before the retry warning, and the warning plus the
    /// backoff are outside the measured attempt -- so an attempt that waited a
    /// second still measures less than one second.
    #[test]
    fn when_model_attempt_retries_given_same_prepared_request_then_reuses_context_metadata() {
        if is_timing_child() {
            start_timing_child();
            let script = Script::with_replies(vec![
                Reply::Error(CallError::Transport("connection reset".to_string())),
                Reply::Body(reply(Some("recovered"), &[])),
            ]);
            let config = config(script.clone());
            let mut messages = vec![Message::system("sys"), Message::user("work the ticket")];
            let expected = config
                .provider(Phase::Code)
                .prepare(&messages, Phase::Code)
                .context_bytes;
            println!("{EXPECTED_CONTEXT}{expected}");
            assert!(matches!(
                run_agent_loop(&mut messages, &config, Phase::Code, Source::None),
                Stop::TurnEnd
            ));
            assert_eq!(script.requests(), 2, "the failed attempt and its retry");
            return;
        }

        let transcript = timing_transcript(
            "when_model_attempt_retries_given_same_prepared_request_then_reuses_context_metadata",
        );
        let expected = marker_value(&transcript, EXPECTED_CONTEXT);
        let lines = thinking_lines(&transcript);
        assert_eq!(lines.len(), 2, "one report per send attempt:\n{transcript}");
        for line in &lines {
            assert_thinking_report(line);
            assert_eq!(
                thinking_context_bytes(line),
                expected,
                "a retry repeats the prepared request's context value: {line:?}"
            );
            assert!(
                thinking_seconds(line) < 1.0,
                "the retry warning and backoff are outside the measured attempt: {line:?}"
            );
        }
        assert_eq!(
            attempt_events(&transcript),
            ["attempt 1", "retry warning 1", "attempt 2"],
            "the failed attempt's report precedes the retry warning:\n{transcript}"
        );
    }

    /// Contract: the reported context is exactly the selected provider's
    /// prepared context byte count -- not an estimate and not the encoded
    /// outgoing body, which also carries the model and tool schemas.
    #[test]
    fn when_model_attempt_finishes_given_prepared_request_then_reports_exact_context_bytes_on_stdout()
     {
        if is_timing_child() {
            start_timing_child();
            let script = Script::new(vec![reply(Some("done"), &[])]);
            let config = config(script.clone());
            let mut messages = vec![Message::system("sys"), Message::user("work the ticket")];
            let prepared = config.provider(Phase::Code).prepare(&messages, Phase::Code);
            println!("{EXPECTED_CONTEXT}{}", prepared.context_bytes);
            println!(
                "{EXPECTED_BODY}{}",
                encoded_body_bytes(&prepared.outgoing.body)
            );
            assert!(matches!(
                run_agent_loop(&mut messages, &config, Phase::Code, Source::None),
                Stop::TurnEnd
            ));
            assert_eq!(script.requests(), 1);
            return;
        }

        let transcript = timing_transcript(
            "when_model_attempt_finishes_given_prepared_request_then_reports_exact_context_bytes_on_stdout",
        );
        let expected = marker_value(&transcript, EXPECTED_CONTEXT);
        let body = marker_value(&transcript, EXPECTED_BODY);
        let lines = thinking_lines(&transcript);
        assert_eq!(lines.len(), 1, "one completed attempt:\n{transcript}");
        assert_thinking_report(&lines[0]);
        assert_eq!(
            thinking_context_bytes(&lines[0]),
            expected,
            "the report must use the provider's prepared context_bytes: {transcript}"
        );
        assert_ne!(
            expected, body,
            "the prepared context excludes the schemas, so it differs from the body"
        );
        assert_ne!(
            thinking_context_bytes(&lines[0]),
            body,
            "the report must not substitute the outgoing-body size: {transcript}"
        );
    }

    /// Contract: in hybrid mode each routed phase reports its selected provider,
    /// configured model and canonical phase label -- Code on Mid, the planning
    /// and audit phases on Giga, and Summary on the dedicated Smoll route.
    #[test]
    fn when_model_attempt_finishes_given_hybrid_phases_then_reports_selected_provider_model_and_phase()
     {
        if is_timing_child() {
            start_timing_child();
            for (phase, body) in [
                (Phase::Plan, responses_text("done")),
                (Phase::Spec, responses_text("done")),
                (Phase::Tickets, responses_text("done")),
                (Phase::Triage, responses_text("done")),
                (Phase::Code, reply(Some("done"), &[])),
                (Phase::Review, responses_text("done")),
                (Phase::Summary, native_text("done")),
            ] {
                let script = Script::new(vec![body]);
                let config = hybrid_config(script.clone());
                let mut messages = vec![Message::system("sys"), Message::user("work the ticket")];
                let stop = run_agent_loop(&mut messages, &config, phase, Source::None);
                assert!(
                    matches!(stop, Stop::TurnEnd | Stop::Summary(_)),
                    "phase {phase:?} must complete its tool-free turn"
                );
                assert_eq!(script.requests(), 1, "phase {phase:?} sends once");
            }
            return;
        }

        let transcript = timing_transcript(
            "when_model_attempt_finishes_given_hybrid_phases_then_reports_selected_provider_model_and_phase",
        );
        let lines = thinking_lines(&transcript);
        assert_eq!(lines.len(), 7, "one report per routed phase:\n{transcript}");
        for (phase, provider, model) in [
            ("plan", "giga", "giga-model"),
            ("spec", "giga", "giga-model"),
            ("tickets", "giga", "giga-model"),
            ("triage", "giga", "giga-model"),
            ("code", "mid", "mid-model"),
            ("review", "giga", "giga-model"),
            ("summary", "smoll", "smoll-model"),
        ] {
            let line = lines
                .iter()
                .find(|line| thinking_report_parts(line).phase == phase)
                .unwrap_or_else(|| panic!("no report for phase {phase} in:\n{transcript}"));
            let report = thinking_report_parts(line);
            assert_eq!(report.provider, provider, "phase {phase}: {line:?}");
            assert_eq!(report.model, model, "phase {phase}: {line:?}");
            assert_eq!(report.phase, phase, "canonical lowercase phase: {line:?}");
            assert_thinking_report(line);
        }
        assert!(
            lines
                .iter()
                .all(|line| thinking_report_parts(line).provider != "hybrid"),
            "hybrid is a mode, never a provider:\n{transcript}"
        );
    }

    // -----------------------------------------------------------------------
    // -----------------------------------------------------------------------
    // Priced in-flight status
    //
    // The loop resolves the serving provider's optional rate once per turn and
    // hands the same value to the pure label formatter on every attempt. These
    // tests exercise the existing configuration-routing and formatting seams;
    // the loop's handoff itself is checked by the ticket's verification gate.
    // -----------------------------------------------------------------------

    /// Core entry-point behavior: a configured rate belongs to the provider the
    /// phase actually routes to, and every attempt renders that same selected
    /// rate inside the existing parentheses.
    #[test]
    fn when_running_a_phase_given_a_configured_provider_rate_then_uses_the_selected_rate_for_each_attempt()
     {
        let script = Script::new(vec![reply(Some("done"), &[])]);
        let config = priced_config(script.clone(), Some("2.5"), Some("1.25"), Some("3.75"));
        assert_eq!(
            config.input_price(Phase::Code),
            Some(2.5),
            "Code routes to the priced Mid provider"
        );

        let expected = "code... [mid] (12,345 B ~ 3,086 tokens ~ $0.007715 input)";
        for attempt in 1..=2 {
            let label = status::thinking_label(
                Phase::Code,
                config.provider_role(Phase::Code),
                12_345,
                config.input_price(Phase::Code),
            );
            assert_eq!(
                label, expected,
                "attempt {attempt} must use the selected rate"
            );
        }

        let mut messages = vec![Message::system("sys"), Message::user("work the ticket")];
        assert!(matches!(
            run_agent_loop(&mut messages, &config, Phase::Code, Source::None),
            Stop::TurnEnd
        ));
        assert_eq!(script.requests(), 1, "the priced turn still sends once");
    }

    /// Empty/nil boundary: a provider without its own rate keeps the old label
    /// shape byte for byte, even when a sibling provider is priced.
    #[test]
    fn when_running_a_phase_given_the_selected_provider_has_no_rate_then_keeps_the_old_label_shape()
    {
        let script = Script::new(vec![reply(Some("done"), &[])]);
        let config = priced_config(script.clone(), None, Some("1.25"), Some("3.75"));
        assert_eq!(
            config.input_price(Phase::Code),
            None,
            "the selected Mid provider is unpriced"
        );

        let label = status::thinking_label(
            Phase::Code,
            config.provider_role(Phase::Code),
            12_345,
            config.input_price(Phase::Code),
        );
        assert_eq!(
            label, "code... [mid] (12,345 B ~ 3,086 tokens)",
            "an unpriced provider keeps the legacy phase, role, bytes and tokens"
        );
        assert!(
            !label.contains('$'),
            "no placeholder for a missing rate: {label}"
        );
        assert!(
            !label.contains("input"),
            "no input suffix for a missing rate: {label}"
        );

        let mut messages = vec![Message::system("sys"), Message::user("work the ticket")];
        assert!(matches!(
            run_agent_loop(&mut messages, &config, Phase::Code, Source::None),
            Stop::TurnEnd
        ));
        assert_eq!(script.requests(), 1, "the unpriced turn still sends once");
    }

    /// Minimum valid boundary: a configured zero rate is present, so the label
    /// keeps a visible zero estimate rather than disappearing like an absent
    /// rate.
    #[test]
    fn when_running_a_phase_given_a_selected_zero_rate_then_preserves_the_visible_zero_estimate() {
        let script = Script::new(vec![reply(Some("done"), &[])]);
        let config = priced_config(script.clone(), Some("0"), Some("1.25"), Some("3.75"));
        assert_eq!(config.input_price(Phase::Code), Some(0.0));

        let label = status::thinking_label(
            Phase::Code,
            config.provider_role(Phase::Code),
            12_345,
            config.input_price(Phase::Code),
        );
        assert_eq!(
            label,
            "code... [mid] (12,345 B ~ 3,086 tokens ~ $0.000000 input)"
        );

        let mut messages = vec![Message::system("sys"), Message::user("work the ticket")];
        assert!(matches!(
            run_agent_loop(&mut messages, &config, Phase::Code, Source::None),
            Stop::TurnEnd
        ));
        assert_eq!(script.requests(), 1, "the zero-rate turn still sends once");
    }

    /// Upper boundary: a priced label is still capped at the existing width, so
    /// an enormous context cannot wrap and leave a partial row behind.
    #[test]
    fn when_running_a_phase_given_a_large_context_then_keeps_the_existing_bounded_label() {
        let script = Script::new(vec![reply(Some("done"), &[])]);
        let config = priced_config(script.clone(), Some("2.5"), None, None);
        let label = status::thinking_label(
            Phase::Code,
            config.provider_role(Phase::Code),
            usize::MAX,
            config.input_price(Phase::Code),
        );

        assert_eq!(label.chars().count(), 60, "the existing cap is unchanged");
        assert!(
            label.ends_with("..."),
            "the capped label keeps its ellipsis"
        );
        assert_eq!(
            label,
            "code... [mid] (18,446,744,073,709,551,615 B ~ 4,611,686,0..."
        );
        assert_eq!(
            label,
            status::thinking_label(Phase::Code, "mid", usize::MAX, None),
            "the cap preserves the existing leading identity"
        );

        let mut messages = vec![Message::system("sys"), Message::user("work the ticket")];
        assert!(matches!(
            run_agent_loop(&mut messages, &config, Phase::Code, Source::None),
            Stop::TurnEnd
        ));
        assert_eq!(
            script.requests(),
            1,
            "the large-context turn still sends once"
        );
    }

    /// Malformed/invalid payload: an unparseable configured rate stops at
    /// configuration resolution and names its variable, so no invalid rate ever
    /// reaches the loop or the label.
    #[test]
    fn when_running_a_phase_given_an_invalid_configured_rate_then_stops_at_configuration_resolution()
     {
        let environment = environment(vec![
            ("LLM_MID_URL", "http://script.test/v1/chat/completions"),
            ("LLM_MID_MODEL", "test-model"),
            ("LLM_MID_API_KEY", "test-key"),
            ("LLM_MID_INPUT_PRICE_PER_MILLION_USD", "not-a-number"),
            ("LLM_SMOLL_MODEL", "smoll-model"),
            ("LLM_SMOLL_API_KEY", "smoll-key"),
        ]);

        match Config::from_environment(&environment) {
            Ok(_) => panic!("an invalid configured rate must stop at configuration resolution"),
            Err(message) => {
                assert!(
                    message.contains("LLM_MID_INPUT_PRICE_PER_MILLION_USD"),
                    "the diagnostic must name the invalid variable: {message}"
                );
                assert!(
                    message.contains("finite, non-negative"),
                    "the diagnostic must state the accepted range: {message}"
                );
            }
        }
    }

    /// Unauthorized/forbidden boundary: adding the price leaves the provider
    /// request, endpoint and existing credential behavior untouched.
    #[test]
    fn when_running_a_phase_given_existing_provider_credentials_then_keeps_existing_authorization_behavior()
     {
        let script = Script::new(vec![reply(Some("done"), &[])]);
        let config = priced_config(script.clone(), Some("2.5"), None, None);
        let mut messages = vec![Message::system("sys"), Message::user("work the ticket")];

        assert!(matches!(
            run_agent_loop(&mut messages, &config, Phase::Code, Source::None),
            Stop::TurnEnd
        ));
        assert_eq!(script.requests(), 1);
        assert_eq!(
            script.urls(),
            vec!["http://script.test/v1/chat/completions"]
        );
        assert_eq!(
            script.auths(),
            vec![Auth::Bearer("test-key".to_string())],
            "the configured credential still travels unchanged"
        );
        let body = script.sent()[0].to_string();
        assert_eq!(script.sent()[0]["model"], serde_json::json!("test-model"));
        assert!(
            !body.contains("input_price_per_million_usd") && !body.contains("2.500000"),
            "the local price is not request payload: {body}"
        );
    }

    // Recurring two-tier transcript compaction and the hard body guard
    //
    // The watermark, the retained tail and the hard ceiling are all observed at
    // the scripted transport boundary: the loop runs for real, the external
    // transport is the only double, and the assertions read the recorded
    // requests rather than private state.
    // -----------------------------------------------------------------------

    /// Empty/zero and the exact lower boundary: no compaction and no summary
    /// request, and the provider-prepared history reaches the transport
    /// unchanged.
    #[test]
    fn when_compacting_given_context_at_zero_or_watermark_then_preserves_prepared_history_without_summary()
     {
        let empty_script = Script::new(vec![reply(Some("done"), &[])]);
        let empty_config = config(empty_script.clone());
        let mut empty = Vec::new();
        assert!(matches!(
            run_agent_loop(&mut empty, &empty_config, Phase::Code, Source::None),
            Stop::TurnEnd
        ));
        assert_eq!(empty_script.requests(), 1, "no summary request at zero");
        assert_eq!(
            empty_script.sent()[0]["messages"],
            serde_json::json!([]),
            "an empty transcript is sent unchanged"
        );

        let script = Script::new(vec![reply(Some("done"), &[])]);
        let config = config(script.clone());
        let original = pad_to_context(
            &config,
            tier_one_history(8, 150 * 1024),
            TIER_ONE_CONTEXT_BYTES,
            Phase::Code,
        );
        let mut messages = original.clone();
        assert!(matches!(
            run_agent_loop(&mut messages, &config, Phase::Code, Source::None),
            Stop::TurnEnd
        ));
        assert_eq!(script.requests(), 1, "the watermark sends no summary");
        assert_eq!(
            script.sent()[0]["messages"],
            serde_json::to_value(&original).unwrap(),
            "the prepared history is byte-stable at the watermark"
        );
    }

    /// Minimum valid boundary: exactly 409,600 bytes of prepared context is
    /// sent unchanged and makes no summary request.
    #[test]
    fn when_compacting_given_context_at_watermark_then_sends_unchanged_history_without_summary() {
        let script = Script::new(vec![reply(Some("done"), &[])]);
        let config = config(script.clone());
        let original = pad_to_context(
            &config,
            tier_one_history(8, 150 * 1024),
            TIER_ONE_CONTEXT_BYTES,
            Phase::Code,
        );
        let mut messages = original.clone();

        assert_eq!(
            context_bytes(&config, &messages, Phase::Code),
            TIER_ONE_CONTEXT_BYTES
        );
        assert!(matches!(
            run_agent_loop(&mut messages, &config, Phase::Code, Source::None),
            Stop::TurnEnd
        ));
        assert_eq!(script.requests(), 1, "no summary request at the watermark");
        assert_eq!(
            script.sent()[0]["messages"],
            serde_json::to_value(&original).unwrap(),
            "the exact watermark is not mutated"
        );
    }

    /// Minimum valid boundary, one byte above: Tier 1 runs before the ordinary
    /// request, keeping the tail under the Tier 2 watermark so no summary is
    /// requested.
    #[test]
    fn when_compacting_given_context_one_byte_above_watermark_then_runs_tier_one() {
        let script = Script::new(vec![reply(Some("done"), &[])]);
        let config = config(script.clone());
        let original = pad_to_context(
            &config,
            tier_one_history(8, 150 * 1024),
            TIER_ONE_CONTEXT_BYTES + 1,
            Phase::Code,
        );
        let mut messages = original.clone();

        assert_eq!(
            context_bytes(&config, &messages, Phase::Code),
            TIER_ONE_CONTEXT_BYTES + 1
        );
        assert!(matches!(
            run_agent_loop(&mut messages, &config, Phase::Code, Source::None),
            Stop::TurnEnd
        ));
        assert_eq!(
            script.requests(),
            1,
            "Tier 1 must bring the transcript under Tier 2 so the ordinary request is sent directly"
        );

        let sent = recorded_messages(&script.sent()[0], "messages");
        assert_eq!(
            serde_json::to_value(&sent[..2]).unwrap(),
            serde_json::to_value(&original[..2]).unwrap(),
            "the prefix is pinned"
        );
        assert!(
            is_tool_result_stub(sent[3].content.as_deref().unwrap()),
            "the oldest tool result was compacted: {:?}",
            sent[3].content
        );
        assert!(
            is_tool_result_stub(sent[5].content.as_deref().unwrap()),
            "the second-oldest tool result was compacted: {:?}",
            sent[5].content
        );
        assert!(validate_tool_pairing(&sent).is_ok());
    }

    /// Core entry-point behavior: above the watermark the compacted history is
    /// sent before the ordinary request, with the accepted summary replacing
    /// only the middle.
    #[test]
    fn when_compacting_given_context_above_watermark_then_sends_compacted_history_before_ordinary_request()
     {
        let script = Script::new(vec![
            reply(Some(HANDOVER_REPLY), &[]),
            reply(Some("done"), &[]),
        ]);
        let config = config(script.clone());
        let original = summary_history();
        let mut messages = original.clone();

        assert!(
            context_bytes(&config, &messages, Phase::Code) > TIER_ONE_CONTEXT_BYTES,
            "the fixture must cross the Tier 1 watermark"
        );
        assert!(matches!(
            run_agent_loop(&mut messages, &config, Phase::Code, Source::None),
            Stop::TurnEnd
        ));
        assert_eq!(
            script.requests(),
            2,
            "one summary then one ordinary request"
        );

        let sent = script.sent();
        assert_eq!(
            sent[0]["tools"],
            serde_json::json!([]),
            "the summary request advertises no tools"
        );
        assert!(
            sent[0]["messages"]
                .as_array()
                .is_some_and(|items| items.len() > original.len()),
            "the summary request carries the compacted history plus its instruction"
        );

        let ordinary = recorded_messages(&sent[1], "messages");
        assert!(!ordinary.is_empty(), "the ordinary request was sent");
        assert_eq!(
            serde_json::to_value(&ordinary[..2]).unwrap(),
            serde_json::to_value(&original[..2]).unwrap(),
            "the pinned prefix leads the ordinary request"
        );
        assert_eq!(
            ordinary[2].content.as_deref(),
            Some(HANDOVER_REPLY),
            "the accepted handover text replaces the middle"
        );
        assert_eq!(
            serde_json::to_value(&ordinary[3..]).unwrap(),
            serde_json::to_value(&original[7..]).unwrap(),
            "the six-step tail follows the summary unchanged"
        );
        assert!(
            !sent[1]["tools"].as_array().unwrap().is_empty(),
            "the ordinary request keeps the phase's tools"
        );
    }

    /// Contract and invariant: the pinned prefix is byte-identical and every
    /// one of the six retained tool-call steps keeps a complete call/result
    /// pair, while older results become named stubs and older reasoning clears.
    #[test]
    fn when_compacting_given_older_tool_steps_then_preserves_pinned_prefix_and_six_valid_tail_steps()
     {
        let script = Script::new(vec![reply(Some("done"), &[])]);
        let config = config(script.clone());
        let original = pad_to_context(
            &config,
            tier_one_history(8, 150 * 1024),
            TIER_ONE_CONTEXT_BYTES + 1,
            Phase::Code,
        );
        let mut messages = original.clone();

        assert!(matches!(
            run_agent_loop(&mut messages, &config, Phase::Code, Source::None),
            Stop::TurnEnd
        ));
        let sent = recorded_messages(&script.sent()[0], "messages");

        assert_eq!(
            serde_json::to_value(&sent[..2]).unwrap(),
            serde_json::to_value(&original[..2]).unwrap(),
            "the system prompt and kickoff are byte-identical"
        );
        assert_eq!(
            serde_json::to_value(&sent[6..]).unwrap(),
            serde_json::to_value(&original[6..]).unwrap(),
            "the six newest tool-call steps and their results are unchanged"
        );
        assert_eq!(
            assistant_tool_steps(&sent[6..]).len(),
            RETAINED_TOOL_STEPS,
            "the retained tail holds all six newest steps"
        );

        for index in [3_usize, 5] {
            let stub = sent[index].content.as_deref().unwrap();
            assert!(is_tool_result_stub(stub), "old result {index} is stubbed");
            assert!(
                stub.contains("execute_script"),
                "the function is named: {stub}"
            );
            assert!(stub.contains("return"), "the target is named: {stub}");
            assert!(
                stub.contains("153600 bytes omitted"),
                "the exact UTF-8 byte count is named: {stub}"
            );
            assert_eq!(
                sent[index].tool_call_id, original[index].tool_call_id,
                "the tool_call_id survives"
            );
        }
        assert_eq!(
            sent[2].tool_calls.as_ref().unwrap().len(),
            original[2].tool_calls.as_ref().unwrap().len(),
            "an older tool call is never replaced"
        );
        assert_eq!(sent[2].reasoning_content, None);
        assert_eq!(sent[4].reasoning_content, None);
        assert!(validate_tool_pairing(&sent).is_ok());
    }

    /// Empty/zero older-output set: with every result inside the retained tail,
    /// Tier 1 has nothing to stub, the summary replaces only the middle, and the
    /// tail stays valid.
    #[test]
    fn when_compacting_given_history_without_older_tool_results_then_preserves_valid_tail() {
        let script = Script::new(vec![
            reply(Some(HANDOVER_REPLY), &[]),
            reply(Some("done"), &[]),
        ]);
        let config = config(script.clone());
        let original = history_without_older_tool_results();
        let mut messages = original.clone();

        assert!(matches!(
            run_agent_loop(&mut messages, &config, Phase::Code, Source::None),
            Stop::TurnEnd
        ));
        assert_eq!(script.requests(), 2, "the middle still needs a summary");

        let ordinary = recorded_messages(&script.sent()[1], "messages");
        assert_eq!(
            serde_json::to_value(&ordinary[..2]).unwrap(),
            serde_json::to_value(&original[..2]).unwrap(),
            "the prefix is pinned"
        );
        assert_eq!(
            ordinary[2].content.as_deref(),
            Some(HANDOVER_REPLY),
            "the summary replaces the middle narration"
        );
        assert_eq!(
            serde_json::to_value(&ordinary[3..]).unwrap(),
            serde_json::to_value(&original[4..]).unwrap(),
            "the valid tool-call tail is preserved"
        );
        assert!(
            !ordinary
                .iter()
                .filter(|message| message.role == Role::Tool)
                .any(|message| is_tool_result_stub(message.content.as_deref().unwrap_or(""))),
            "there were no older tool results to stub"
        );
        assert!(validate_tool_pairing(&ordinary).is_ok());
    }

    /// Core entry-point behavior: an accepted handover summary with the full
    /// labeled contract replaces only the compactable middle, leaving the
    /// pinned prefix and retained tail byte-for-byte intact.
    #[test]
    fn when_summarizing_given_valid_contract_then_replaces_only_compactable_middle() {
        let script = Script::new(vec![
            reply(Some(HANDOVER_REPLY), &[]),
            reply(Some("done"), &[]),
        ]);
        let config = config(script.clone());
        let original = summary_history();
        let mut messages = original.clone();

        assert!(
            context_bytes(&config, &messages, Phase::Code) > TIER_ONE_CONTEXT_BYTES,
            "the fixture must need Tier 1 before it can need a summary"
        );
        assert!(matches!(
            run_agent_loop(&mut messages, &config, Phase::Code, Source::None),
            Stop::TurnEnd
        ));
        assert_eq!(
            script.requests(),
            2,
            "one summary then one ordinary request"
        );

        let sent = script.sent();
        assert_eq!(
            sent[0]["tools"],
            serde_json::json!([]),
            "the summary request advertises no tools"
        );
        let summary_request = recorded_messages(&sent[0], "messages");
        let instruction = summary_request
            .last()
            .and_then(|message| message.content.as_deref())
            .expect("the summary request ends with its instruction");
        assert!(
            instruction.contains(HANDOVER_SUMMARY_MARKER),
            "the instruction names the required marker: {instruction}"
        );
        for label in REQUIRED_HANDOVER_LABELS {
            assert!(
                instruction.contains(label),
                "the instruction asks for the {label} field: {instruction}"
            );
        }

        let ordinary = recorded_messages(&sent[1], "messages");
        assert_eq!(
            serde_json::to_value(&ordinary[..2]).unwrap(),
            serde_json::to_value(&original[..2]).unwrap(),
            "the pinned prefix is byte-identical"
        );
        assert_eq!(
            ordinary[2].content.as_deref(),
            Some(HANDOVER_REPLY),
            "the accepted summary replaces the compactable middle"
        );
        assert_eq!(
            serde_json::to_value(&ordinary[3..]).unwrap(),
            serde_json::to_value(&original[7..]).unwrap(),
            "the retained tail is byte-identical"
        );
        assert!(
            !sent[1]["tools"].as_array().unwrap().is_empty(),
            "the ordinary request keeps its own tools"
        );
        assert_complete_tool_pairing(&ordinary);
    }

    /// Maximum/upper boundary: the synthetic Tier 2 summary request is appended
    /// without clearing reasoning from the retained tail, and it neither
    /// rewrites the pinned prefix nor any other retained field.
    #[test]
    fn when_compacting_history_given_retained_tail_reasoning_then_preserves_reasoning_for_synthetic_summary_request()
     {
        let script = Script::new(vec![
            reply(Some(HANDOVER_REPLY), &[]),
            reply(Some("done"), &[]),
        ]);
        let config = config(script.clone());
        let original = summary_history();
        let compacted = compact_history(&original).expect("the fixture is a paired history");
        assert!(
            compacted.messages[compacted.tail_start..]
                .iter()
                .any(|message| {
                    message.role == Role::Assistant && message.reasoning_content.is_some()
                }),
            "the fixture must retain reasoning in its tail"
        );
        let mut messages = original.clone();

        assert!(matches!(
            run_agent_loop(&mut messages, &config, Phase::Code, Source::None),
            Stop::TurnEnd
        ));
        assert_eq!(
            script.requests(),
            2,
            "one summary then one ordinary request"
        );

        let sent = script.sent();
        let summary_request = recorded_messages(&sent[0], "messages");
        assert_eq!(
            summary_request.len(),
            compacted.messages.len() + 1,
            "the instruction is appended to the compacted history"
        );
        assert_eq!(
            serde_json::to_value(&summary_request[..2]).unwrap(),
            serde_json::to_value(&original[..2]).unwrap(),
            "the pinned prefix is unchanged by the summary request"
        );
        for (index, expected) in compacted.messages.iter().enumerate() {
            assert_eq!(
                serde_json::to_value(&summary_request[index]).unwrap(),
                serde_json::to_value(expected).unwrap(),
                "retained message fields change at {index}"
            );
            if expected.role == Role::Assistant && expected.reasoning_content.is_some() {
                assert!(
                    summary_request[index].reasoning_content.is_some(),
                    "retained-tail reasoning is cleared at {index}"
                );
            }
        }

        let instruction = summary_request.last().unwrap();
        assert_eq!(instruction.role, Role::User);
        assert!(
            instruction
                .content
                .as_deref()
                .is_some_and(|content| content.contains(HANDOVER_SUMMARY_INSTRUCTION)),
            "the synthetic summary instruction is appended after the retained transcript"
        );
    }

    /// Edge-case table: a missing or malformed handover summary stops the
    /// phase before any ordinary request can carry the oversized history.
    #[test]
    fn when_summarizing_given_missing_or_malformed_summary_then_stops_without_ordinary_request() {
        let missing_label = format!(
            "{HANDOVER_SUMMARY_MARKER}\nCompleted/work context: compacted\nDecisions: keep the tail\nUnresolved work/blockers: none"
        );
        let wrong_marker = "not a handover summary\nCompleted/work context: compacted\nDecisions: keep the tail\nUnresolved work/blockers: none\nRelevant findings: none";
        let invalid_responses = [
            reply(None, &[]),
            reply(Some(""), &[]),
            reply(Some("   \n\t"), &[]),
            reply(Some(wrong_marker), &[]),
            reply(Some(missing_label.as_str()), &[]),
        ];

        for invalid in invalid_responses {
            let script = Script::new(vec![invalid]);
            let config = config(script.clone());
            let mut messages = summary_history();

            assert!(
                matches!(
                    run_agent_loop(&mut messages, &config, Phase::Code, Source::None),
                    Stop::Failed(FailureKind::ContextGuard)
                ),
                "an invalid summary stops the phase"
            );
            assert_eq!(
                script.requests(),
                1,
                "only the one summary request is made; no ordinary send follows"
            );
        }
    }

    /// Contract and invariant: the summary request goes through the active
    /// phase's provider, advertises no tools and asks for every exact label.
    #[test]
    fn when_summarizing_given_active_phase_provider_then_sends_one_tool_free_request_with_exact_labels()
     {
        let routes = [
            (
                Phase::Code,
                "http://script.test/v1/chat/completions",
                "mid-model",
                Auth::Bearer("mid-key".to_string()),
                reply(Some(HANDOVER_REPLY), &[]),
                reply(Some("done"), &[]),
            ),
            (
                Phase::Plan,
                "http://script.test/v1/responses",
                "giga-model",
                Auth::Bearer("giga-key".to_string()),
                responses_text(HANDOVER_REPLY),
                responses_text("done"),
            ),
            (
                Phase::Summary,
                "https://smoll.test/v1/messages",
                "smoll-model",
                Auth::Anthropic("smoll-key".to_string()),
                native_text(HANDOVER_REPLY),
                native_text("done"),
            ),
        ];

        for (phase, url, model, auth, summary_reply, ordinary_reply) in routes {
            let script = Script::new(vec![summary_reply, ordinary_reply]);
            let config = hybrid_config(script.clone());
            let mut messages = summary_history();

            let stop = run_agent_loop(&mut messages, &config, phase, Source::None);
            if phase == Phase::Summary {
                assert!(matches!(stop, Stop::Summary(Some(ref text)) if text == "done"));
            } else {
                assert!(matches!(stop, Stop::TurnEnd));
            }
            assert_eq!(script.requests(), 2, "phase {phase:?}");
            assert_eq!(
                script.urls()[0],
                url,
                "the summary goes to the active phase's route"
            );
            assert_eq!(
                script.auths()[0],
                auth,
                "the summary uses the active phase's authentication"
            );

            let bodies = script.sent();
            assert_eq!(
                bodies[0]["model"],
                serde_json::json!(model),
                "the summary uses the active phase's model"
            );
            assert_eq!(
                bodies[0]["tools"],
                serde_json::json!([]),
                "the summary advertises no tools"
            );
            let encoded = serde_json::to_string(&bodies[0]).expect("the body serializes");
            assert!(
                encoded.contains(HANDOVER_SUMMARY_MARKER),
                "the request names the marker: {encoded}"
            );
            for label in REQUIRED_HANDOVER_LABELS {
                assert!(
                    encoded.contains(label),
                    "the request asks for the {label} field"
                );
            }
            assert!(
                !bodies[1]["tools"].as_array().unwrap().is_empty(),
                "the ordinary request keeps the phase's tools"
            );
        }
    }

    /// Minimum valid boundary: exactly 200 KiB after Tier 1 skips the summary
    /// request, even though the original context crossed the Tier 1 watermark.
    #[test]
    fn when_summarizing_given_context_exactly_at_tier_two_watermark_then_skips_summary() {
        let script = Script::new(vec![reply(Some("done"), &[])]);
        let config = config(script.clone());
        let original =
            summary_history_compacted_to_context(&config, TIER_TWO_CONTEXT_BYTES, Phase::Code);
        let mut messages = original.clone();

        assert!(
            context_bytes(&config, &original, Phase::Code) > TIER_ONE_CONTEXT_BYTES,
            "the original context must cross Tier 1 so Tier 1 runs first"
        );
        assert!(matches!(
            run_agent_loop(&mut messages, &config, Phase::Code, Source::None),
            Stop::TurnEnd
        ));
        assert_eq!(
            script.requests(),
            1,
            "exactly at Tier 2 makes no summary request"
        );

        let sent = recorded_messages(&script.sent()[0], "messages");
        assert!(
            sent.iter().any(|message| {
                message.role == Role::Tool
                    && message.content.as_deref().is_some_and(is_tool_result_stub)
            }),
            "Tier 1 ran before the post-compaction measurement"
        );
    }

    /// Upper boundary: one byte above Tier 2 after Tier 1 sends exactly one
    /// tool-free summary request, then the ordinary request.
    #[test]
    fn when_summarizing_given_context_one_byte_above_tier_two_watermark_then_sends_exactly_one_summary_request()
     {
        let script = Script::new(vec![
            reply(Some(HANDOVER_REPLY), &[]),
            reply(Some("done"), &[]),
        ]);
        let config = config(script.clone());
        let original =
            summary_history_compacted_to_context(&config, TIER_TWO_CONTEXT_BYTES + 1, Phase::Code);
        let mut messages = original.clone();

        assert!(
            context_bytes(&config, &original, Phase::Code) > TIER_ONE_CONTEXT_BYTES,
            "the original context must cross Tier 1 so Tier 1 runs first"
        );
        assert!(matches!(
            run_agent_loop(&mut messages, &config, Phase::Code, Source::None),
            Stop::TurnEnd
        ));
        assert_eq!(
            script.requests(),
            2,
            "exactly one summary then one ordinary request"
        );

        let sent = script.sent();
        assert_eq!(
            sent[0]["tools"],
            serde_json::json!([]),
            "the one summary request is tool-free"
        );
        let encoded = serde_json::to_string(&sent[0]).expect("the body serializes");
        for label in REQUIRED_HANDOVER_LABELS {
            assert!(
                encoded.contains(label),
                "the request asks for the {label} field"
            );
        }

        let ordinary = recorded_messages(&sent[1], "messages");
        assert_eq!(
            serde_json::to_value(&ordinary[..2]).unwrap(),
            serde_json::to_value(&original[..2]).unwrap(),
            "the pinned prefix is byte-identical"
        );
        assert_eq!(
            ordinary[2].content.as_deref(),
            Some(HANDOVER_REPLY),
            "the one accepted summary replaces the compactable middle"
        );
        assert_eq!(
            serde_json::to_value(&ordinary[3..]).unwrap(),
            serde_json::to_value(&original[7..]).unwrap(),
            "the retained tail is byte-identical"
        );
        assert!(
            context_bytes(&config, &ordinary, Phase::Code) <= TIER_TWO_CONTEXT_BYTES,
            "the ordinary request is remeasured below Tier 2 after replacement"
        );
    }

    /// Empty/Zero/Nil: a summary response with no content stops with a context
    /// guard and is not retried.
    #[test]
    fn when_summarizing_given_missing_content_then_stops_with_context_guard_without_retry() {
        let script = Script::new(vec![reply(None, &[])]);
        let config = config(script.clone());
        let mut messages = summary_history();

        assert!(matches!(
            run_agent_loop(&mut messages, &config, Phase::Code, Source::None),
            Stop::Failed(FailureKind::ContextGuard)
        ));
        assert_eq!(
            script.requests(),
            1,
            "the missing summary is not retried and no ordinary request follows"
        );
    }

    /// Malformed/Invalid payload: a tool call or a summary missing any required
    /// label stops without an ordinary request.
    #[test]
    fn when_summarizing_given_tool_call_or_missing_required_label_then_stops_without_ordinary_send()
    {
        let missing_label = format!(
            "{HANDOVER_SUMMARY_MARKER}\nCompleted/work context: compacted\nDecisions: keep the tail\nUnresolved work/blockers: none"
        );
        let invalid_responses = [
            reply(None, &[("execute_script", "{}".to_string())]),
            reply(Some(missing_label.as_str()), &[]),
        ];

        for invalid in invalid_responses {
            let script = Script::new(vec![invalid]);
            let config = config(script.clone());
            let mut messages = summary_history();

            assert!(matches!(
                run_agent_loop(&mut messages, &config, Phase::Code, Source::None),
                Stop::Failed(FailureKind::ContextGuard)
            ));
            assert_eq!(
                script.requests(),
                1,
                "only the summary request is made; no ordinary send follows"
            );
        }
    }

    /// Unauthorized/Forbidden boundary: a 401 or 403 on the summary request
    /// stops as a context guard, with no retry and no ordinary send.
    #[test]
    fn when_summarizing_given_provider_returns_unauthorized_then_stops_without_summary_retry_or_ordinary_send()
     {
        for status in [401, 403] {
            let script = Script::with_errors(vec![CallError::Api {
                status,
                message: format!("denied {status}"),
            }]);
            let config = config(script.clone());
            let mut messages = summary_history();

            assert!(matches!(
                run_agent_loop(&mut messages, &config, Phase::Code, Source::None),
                Stop::Failed(FailureKind::ContextGuard)
            ));
            assert_eq!(
                script.requests(),
                1,
                "HTTP {status} on the summary is not retried and no ordinary send follows"
            );
        }
    }

    /// An above-watermark transcript with enough headroom that an unpaired
    /// mutation still leaves it over the watermark, plus a script that must
    /// never see a request.
    fn over_watermark_transcript_with_empty_script() -> (Arc<Script>, Config, Vec<Message>) {
        let script = Script::new(Vec::new());
        let config = config(script.clone());
        let messages = pad_to_context(
            &config,
            tier_one_history(8, 150 * 1024),
            TIER_ONE_CONTEXT_BYTES + 1024,
            Phase::Code,
        );
        (script, config, messages)
    }

    /// Assert every assistant tool call in a recorded request is answered by
    /// exactly one tool result, and every result names a pending call.
    fn assert_complete_tool_pairing(messages: &[Message]) {
        let mut pending: Vec<String> = Vec::new();
        for message in messages {
            match message.role {
                Role::Assistant => {
                    assert!(
                        pending.is_empty(),
                        "an assistant turn interrupted a tool-call group"
                    );
                    if let Some(calls) = &message.tool_calls {
                        pending.extend(calls.iter().map(|call| call.id.clone()));
                    }
                }
                Role::Tool => {
                    let id = message
                        .tool_call_id
                        .as_deref()
                        .expect("every tool result carries its call id");
                    let position = pending
                        .iter()
                        .position(|pending_id| pending_id == id)
                        .unwrap_or_else(|| panic!("a tool result answers no pending call: {id}"));
                    pending.remove(position);
                }
                Role::System | Role::User => assert!(
                    pending.is_empty(),
                    "a new turn interrupted a tool-call group"
                ),
            }
        }
        assert!(pending.is_empty(), "a tool call was left unanswered");
    }

    /// Empty and inclusive watermarks: zero bytes, exactly 200 KiB and exactly
    /// 400 KiB of provider-prepared context are sent as prepared, with no
    /// compaction and no summary request.
    #[test]
    fn when_compacting_given_zero_and_inclusive_watermarks_then_leaves_transcript_unchanged() {
        let script = Script::new(vec![
            reply(Some("done"), &[]),
            reply(Some("done"), &[]),
            reply(Some("done"), &[]),
        ]);
        let config = config(script.clone());

        let empty = Vec::new();
        let two_hundred = pad_to_context(
            &config,
            tier_one_history(8, 32),
            TIER_TWO_CONTEXT_BYTES,
            Phase::Code,
        );
        let four_hundred = pad_to_context(
            &config,
            tier_one_history(8, 32),
            TIER_ONE_CONTEXT_BYTES,
            Phase::Code,
        );

        for original in [&empty, &two_hundred, &four_hundred] {
            let mut messages = original.clone();
            assert!(matches!(
                run_agent_loop(&mut messages, &config, Phase::Code, Source::None),
                Stop::TurnEnd
            ));
            assert_eq!(
                serde_json::to_value(&messages[..original.len()]).unwrap(),
                serde_json::to_value(original).unwrap(),
                "compaction must not mutate the prepared transcript at or below the watermark"
            );
        }

        assert_eq!(script.requests(), 3, "three ordinary sends, no summary");
        let sent = script.sent();
        for (body, original) in sent.iter().zip([&empty, &two_hundred, &four_hundred]) {
            assert_eq!(
                body["messages"],
                serde_json::to_value(original).unwrap(),
                "the provider-prepared history is byte-stable at and below the watermark"
            );
        }
    }

    /// Zero content: an empty transcript is sent as an empty messages array,
    /// with no summary request and no invented history.
    #[test]
    fn when_sending_history_given_empty_history_then_sends_without_history_changes() {
        let script = Script::new(vec![reply(Some("done"), &[])]);
        let config = config(script.clone());
        let mut messages = Vec::new();

        assert!(matches!(
            run_agent_loop(&mut messages, &config, Phase::Code, Source::None),
            Stop::TurnEnd
        ));
        assert_eq!(
            script.requests(),
            1,
            "an empty history makes no summary request"
        );
        assert_eq!(
            script.sent()[0]["messages"],
            serde_json::json!([]),
            "the empty transcript is sent unchanged"
        );
        assert_eq!(messages.len(), 1, "only the ordinary reply is appended");
        assert_eq!(messages[0].role, Role::Assistant);
        assert_eq!(messages[0].content.as_deref(), Some("done"));
    }

    /// The minimum valid boundary: exactly the Tier 1 watermark is sent as
    /// prepared, with no stub, no cleared reasoning and no summary request.
    #[test]
    fn when_compacting_given_context_exactly_at_watermark_then_sends_unchanged_history() {
        let script = Script::new(vec![reply(Some("done"), &[])]);
        let config = config(script.clone());
        let original = pad_to_context(
            &config,
            tier_one_history(8, 150 * 1024),
            TIER_ONE_CONTEXT_BYTES,
            Phase::Code,
        );
        let mut messages = original.clone();

        assert_eq!(
            context_bytes(&config, &messages, Phase::Code),
            TIER_ONE_CONTEXT_BYTES
        );
        assert!(matches!(
            run_agent_loop(&mut messages, &config, Phase::Code, Source::None),
            Stop::TurnEnd
        ));
        assert_eq!(script.requests(), 1, "no summary request at the watermark");
        assert_eq!(
            script.sent()[0]["messages"],
            serde_json::to_value(&original).unwrap(),
            "the exact watermark is not mutated"
        );
        assert_eq!(
            serde_json::to_value(&messages[..original.len()]).unwrap(),
            serde_json::to_value(&original).unwrap(),
            "the prepared transcript keeps every byte"
        );
    }

    /// Core entry-point behavior, one byte above the watermark: Tier 1 pins the
    /// system prompt and kickoff, keeps the six newest complete tool-call steps
    /// and their results, clears older reasoning and stubs only older results.
    #[test]
    fn when_compacting_given_context_one_byte_above_watermark_then_preserves_pinned_prefix_and_six_complete_tool_steps()
     {
        let script = Script::new(vec![reply(Some("done"), &[])]);
        let config = config(script.clone());
        let original = pad_to_context(
            &config,
            tier_one_history(8, 150 * 1024),
            TIER_ONE_CONTEXT_BYTES + 1,
            Phase::Code,
        );
        let mut messages = original.clone();

        assert_eq!(
            context_bytes(&config, &messages, Phase::Code),
            TIER_ONE_CONTEXT_BYTES + 1
        );
        assert!(matches!(
            run_agent_loop(&mut messages, &config, Phase::Code, Source::None),
            Stop::TurnEnd
        ));
        assert_eq!(
            script.requests(),
            1,
            "Tier 1 brings the transcript under Tier 2, so the ordinary request is sent directly"
        );

        let sent = recorded_messages(&script.sent()[0], "messages");
        assert_eq!(
            sent.len(),
            original.len(),
            "compaction changes no message count"
        );
        assert_eq!(
            serde_json::to_value(&sent[..2]).unwrap(),
            serde_json::to_value(&original[..2]).unwrap(),
            "the system prompt and first kickoff are byte-identical"
        );
        assert_eq!(
            serde_json::to_value(&sent[6..]).unwrap(),
            serde_json::to_value(&original[6..]).unwrap(),
            "the six newest tool-call steps and their results are intact"
        );
        assert_eq!(
            assistant_tool_steps(&sent[6..]).len(),
            RETAINED_TOOL_STEPS,
            "the retained tail holds the six newest complete steps"
        );

        for index in [2_usize, 4] {
            assert_eq!(
                sent[index].reasoning_content, None,
                "older assistant reasoning is cleared at {index}"
            );
            assert_eq!(
                serde_json::to_value(&sent[index].tool_calls).unwrap(),
                serde_json::to_value(&original[index].tool_calls).unwrap(),
                "the older tool call and its id are untouched at {index}"
            );
        }
        for index in [3_usize, 5] {
            let stub = sent[index]
                .content
                .as_deref()
                .expect("a stubbed result keeps content");
            assert!(
                stub.starts_with("[tool result omitted:"),
                "old result {index} is a stub: {stub}"
            );
            assert!(
                stub.contains("execute_script"),
                "the function is named: {stub}"
            );
            assert!(stub.contains("return"), "the target is named: {stub}");
            assert!(
                stub.contains("153600 bytes omitted"),
                "the exact UTF-8 byte count is named: {stub}"
            );
            assert_eq!(
                sent[index].tool_call_id, original[index].tool_call_id,
                "the tool result id survives at {index}"
            );
        }
        assert_complete_tool_pairing(&sent);
    }

    /// Upper boundary: with more tool-call steps than the retained tail holds,
    /// only the six newest complete steps keep their calls, results and
    /// reasoning; older results become stubs.
    #[test]
    fn when_compacting_history_given_context_above_watermark_and_more_than_six_steps_then_preserves_pinned_prefix_and_newest_six_steps()
     {
        let script = Script::new(vec![reply(Some("done"), &[])]);
        let config = config(script.clone());
        let original = pad_to_context(
            &config,
            tier_one_history(9, 100 * 1024),
            TIER_ONE_CONTEXT_BYTES + 1,
            Phase::Code,
        );
        let mut messages = original.clone();

        assert!(matches!(
            run_agent_loop(&mut messages, &config, Phase::Code, Source::None),
            Stop::TurnEnd
        ));
        assert_eq!(
            script.requests(),
            1,
            "Tier 1 alone keeps the tail below Tier 2"
        );

        let sent = recorded_messages(&script.sent()[0], "messages");
        assert_eq!(
            serde_json::to_value(&sent[..2]).unwrap(),
            serde_json::to_value(&original[..2]).unwrap(),
            "the system prompt and kickoff remain byte-identical"
        );
        let assistant_indices: Vec<usize> = sent
            .iter()
            .enumerate()
            .filter(|(_, message)| {
                message.role == Role::Assistant
                    && message
                        .tool_calls
                        .as_ref()
                        .is_some_and(|calls| !calls.is_empty())
            })
            .map(|(index, _)| index)
            .collect();
        assert_eq!(assistant_indices.len(), 9, "no tool call is dropped");
        let tail_start = assistant_indices[assistant_indices.len() - RETAINED_TOOL_STEPS];
        assert_eq!(
            serde_json::to_value(&sent[tail_start..]).unwrap(),
            serde_json::to_value(&original[tail_start..]).unwrap(),
            "the newest six steps and every corresponding result are retained intact"
        );
        assert_eq!(
            assistant_tool_steps(&sent[tail_start..]).len(),
            RETAINED_TOOL_STEPS
        );

        for index in [3_usize, 5, 7] {
            let stub = sent[index].content.as_deref().unwrap();
            assert!(
                stub.starts_with("[tool result omitted:"),
                "the older result at {index} is stubbed: {stub}"
            );
            assert!(
                stub.contains("102400 bytes omitted"),
                "the omitted byte count is exact: {stub}"
            );
        }
        for index in assistant_indices.iter().take(3) {
            assert_eq!(
                sent[*index].reasoning_content, None,
                "older reasoning is cleared at {index}"
            );
        }
        for index in assistant_indices.iter().skip(3) {
            assert_eq!(
                sent[*index].reasoning_content, original[*index].reasoning_content,
                "the retained tail keeps its reasoning at {index}"
            );
        }
        assert_complete_tool_pairing(&sent);
    }

    /// Regression: a transcript that crosses the watermark more than once is
    /// compacted each time, but an older-result stub keeps the omitted-byte
    /// count it recorded on the first crossing.
    #[test]
    fn when_compacting_history_given_repeated_tool_result_stub_then_preserves_original_omitted_byte_count()
     {
        let script = Script::new(vec![
            reply(Some("first done"), &[]),
            reply(Some("second done"), &[]),
        ]);
        let config = config(script.clone());
        let mut messages = pad_to_context(
            &config,
            tier_one_history(8, 150 * 1024),
            TIER_ONE_CONTEXT_BYTES + 1,
            Phase::Code,
        );

        assert!(matches!(
            run_agent_loop(&mut messages, &config, Phase::Code, Source::None),
            Stop::TurnEnd
        ));
        assert_eq!(script.requests(), 1);
        let first = recorded_messages(&script.sent()[0], "messages");
        let first_stub_one = first[3]
            .content
            .clone()
            .expect("the oldest result is stubbed");
        let first_stub_two = first[5]
            .content
            .clone()
            .expect("the second-oldest result is stubbed");
        assert!(
            first_stub_one.contains("153600 bytes omitted"),
            "the first crossing records the original count: {first_stub_one}"
        );
        assert!(
            first_stub_two.contains("153600 bytes omitted"),
            "the second old result records its original count: {first_stub_two}"
        );

        // The six large results stay older than the retained tail, so the
        // second crossing must stub them while leaving the newest six steps
        // small enough to keep the compacted context under Tier 2.
        for index in 9..=20 {
            let bytes = if index <= 14 { 100 * 1024 } else { 32 };
            messages.extend(compaction_step(index, "n".repeat(bytes)));
        }
        assert!(
            context_bytes(&config, &messages, Phase::Code) > TIER_ONE_CONTEXT_BYTES,
            "the second run must cross the watermark again"
        );

        assert!(matches!(
            run_agent_loop(&mut messages, &config, Phase::Code, Source::None),
            Stop::TurnEnd
        ));
        assert_eq!(
            script.requests(),
            2,
            "the second crossing compacts and sends one ordinary request, with no summary"
        );
        let second = recorded_messages(&script.sent()[1], "messages");
        assert_eq!(
            second[3].content.as_deref(),
            Some(first_stub_one.as_str()),
            "the oldest stub keeps its original omitted-byte count"
        );
        assert_eq!(
            second[5].content.as_deref(),
            Some(first_stub_two.as_str()),
            "the second-oldest stub keeps its original omitted-byte count"
        );
        assert!(
            second[3]
                .content
                .as_deref()
                .is_some_and(|stub| stub.contains("153600 bytes omitted")),
            "the original count is never replaced by the stub's own length"
        );
        assert_eq!(
            serde_json::to_value(&second[2].tool_calls).unwrap(),
            serde_json::to_value(&first[2].tool_calls).unwrap(),
            "the compacted tool call and its id are unchanged"
        );
        assert!(
            second[7]
                .content
                .as_deref()
                .is_some_and(|stub| stub.starts_with("[tool result omitted:")),
            "the newly older result is stubbed on the second crossing"
        );
        assert_complete_tool_pairing(&second);
    }

    /// Contract and invariant: an unmatched tool result stops the pass as a
    /// context guard before any provider sees the invalid history.
    #[test]
    fn when_compacting_given_unpaired_tool_history_then_stops_without_provider_send() {
        let (script, config, mut messages) = over_watermark_transcript_with_empty_script();
        messages.push(Message::tool("unknown-call", "orphan result"));
        let original = messages.clone();

        assert!(matches!(
            run_agent_loop(&mut messages, &config, Phase::Code, Source::None),
            Stop::Failed(FailureKind::ContextGuard)
        ));
        assert_eq!(
            script.requests(),
            0,
            "an unmatched tool result is never sent to a provider"
        );
        assert_eq!(
            serde_json::to_value(&messages).unwrap(),
            serde_json::to_value(&original).unwrap(),
            "a refused history is not partly compacted"
        );

        let (script, config, mut messages) = over_watermark_transcript_with_empty_script();
        let last_result = messages.len() - 2;
        messages.remove(last_result);
        let original = messages.clone();

        assert!(matches!(
            run_agent_loop(&mut messages, &config, Phase::Code, Source::None),
            Stop::Failed(FailureKind::ContextGuard)
        ));
        assert_eq!(
            script.requests(),
            0,
            "a missing tool result is never sent to a provider"
        );
        assert_eq!(
            serde_json::to_value(&messages).unwrap(),
            serde_json::to_value(&original).unwrap()
        );

        let (script, config, mut messages) = over_watermark_transcript_with_empty_script();
        messages.insert(3, Message::user("interrupting turn"));
        let original = messages.clone();

        assert!(matches!(
            run_agent_loop(&mut messages, &config, Phase::Code, Source::None),
            Stop::Failed(FailureKind::ContextGuard)
        ));
        assert_eq!(
            script.requests(),
            0,
            "an interrupted tool-call group is never sent to a provider"
        );
        assert_eq!(
            serde_json::to_value(&messages).unwrap(),
            serde_json::to_value(&original).unwrap()
        );
    }

    /// Malformed/invalid payload boundary: a missing call id, a missing result
    /// id and a trailing unanswered call all stop before the provider send.
    #[test]
    fn when_compacting_given_missing_id_or_unanswered_tool_call_then_stops_without_send() {
        let (script, config, mut messages) = over_watermark_transcript_with_empty_script();
        messages[2].tool_calls.as_mut().unwrap()[0].id.clear();
        let original = messages.clone();

        assert!(matches!(
            run_agent_loop(&mut messages, &config, Phase::Code, Source::None),
            Stop::Failed(FailureKind::ContextGuard)
        ));
        assert_eq!(
            script.requests(),
            0,
            "a tool call without an id is not sent"
        );
        assert_eq!(
            serde_json::to_value(&messages).unwrap(),
            serde_json::to_value(&original).unwrap()
        );

        let (script, config, mut messages) = over_watermark_transcript_with_empty_script();
        messages[3].tool_call_id = None;
        let original = messages.clone();

        assert!(matches!(
            run_agent_loop(&mut messages, &config, Phase::Code, Source::None),
            Stop::Failed(FailureKind::ContextGuard)
        ));
        assert_eq!(
            script.requests(),
            0,
            "a tool result without a call id is not sent"
        );
        assert_eq!(
            serde_json::to_value(&messages).unwrap(),
            serde_json::to_value(&original).unwrap()
        );

        let (script, config, mut messages) = over_watermark_transcript_with_empty_script();
        messages.push(Message {
            role: Role::Assistant,
            content: Some("this call is never answered".to_string()),
            tool_calls: Some(vec![ToolCall {
                id: "call-unanswered".to_string(),
                kind: "function".to_string(),
                function: FunctionCall {
                    name: "execute_script".to_string(),
                    arguments: serde_json::json!({ "code": "return 0;" }).to_string(),
                },
            }]),
            tool_call_id: None,
            reasoning_content: None,
            extra: serde_json::Map::new(),
            truncated: false,
        });
        let original = messages.clone();

        assert!(matches!(
            run_agent_loop(&mut messages, &config, Phase::Code, Source::None),
            Stop::Failed(FailureKind::ContextGuard)
        ));
        assert_eq!(
            script.requests(),
            0,
            "an unanswered tool call is not sent to a provider"
        );
        assert_eq!(
            serde_json::to_value(&messages).unwrap(),
            serde_json::to_value(&original).unwrap()
        );
    }

    /// Contract and invariant: even when the context is too small to compact,
    /// an unpaired tool-call history is refused before a provider send.
    #[test]
    fn when_sending_given_unpaired_tool_history_then_stops_without_provider_send() {
        let script = Script::new(Vec::new());
        let config = config(script.clone());
        let mut messages = plan_transcript();
        messages.push(Message {
            role: Role::Assistant,
            content: Some("this call is never answered".to_string()),
            tool_calls: Some(vec![ToolCall {
                id: "call-unanswered".to_string(),
                kind: "function".to_string(),
                function: FunctionCall {
                    name: "execute_script".to_string(),
                    arguments: serde_json::json!({ "code": "return 0;" }).to_string(),
                },
            }]),
            tool_call_id: None,
            reasoning_content: None,
            extra: serde_json::Map::new(),
            truncated: false,
        });
        let original = messages.clone();

        assert!(matches!(
            run_agent_loop(&mut messages, &config, Phase::Code, Source::None),
            Stop::Failed(FailureKind::ContextGuard)
        ));
        assert_eq!(
            script.requests(),
            0,
            "an unpaired history below the watermark is never sent"
        );
        assert_eq!(
            serde_json::to_value(&messages).unwrap(),
            serde_json::to_value(&original).unwrap(),
            "a refused history is left untouched"
        );
    }

    /// Core entry-point behavior: a duplicate tool-call id inside one
    /// assistant group is refused at the pre-send boundary, so the provider is
    /// never asked to accept the invalid history and the input keeps every
    /// byte of its serialization.
    #[test]
    fn when_sending_history_given_duplicate_tool_call_ids_then_rejects_without_mutation() {
        let script = Script::new(Vec::new());
        let config = config(script.clone());
        let mut messages = plan_transcript();
        messages.push(call_group(&["call-dup", "call-dup"]));
        messages.push(Message::tool("call-dup", "first result"));
        messages.push(Message::tool("call-dup", "second result"));
        let before = serde_json::to_string(&messages).unwrap();

        assert!(matches!(
            run_agent_loop(&mut messages, &config, Phase::Code, Source::None),
            Stop::Failed(FailureKind::ContextGuard)
        ));
        assert_eq!(script.requests(), 0, "the invalid history is never sent");
        assert_eq!(
            serde_json::to_string(&messages).unwrap(),
            before,
            "the refused history keeps its exact serialization"
        );
    }

    /// Contract and invariant: the same call id repeated in two distinct
    /// assistant groups is refused, and the history's serialization survives
    /// the refusal byte-for-byte.
    #[test]
    fn when_sending_history_given_duplicate_call_ids_in_separate_groups_then_preserves_serialized_history()
     {
        let script = Script::new(Vec::new());
        let config = config(script.clone());
        let mut messages = plan_transcript();
        messages.push(call_group(&["call-dup"]));
        messages.push(Message::tool("call-dup", "first result"));
        messages.push(call_group(&["call-dup"]));
        messages.push(Message::tool("call-dup", "second result"));
        let before = serde_json::to_string(&messages).unwrap();

        assert!(matches!(
            run_agent_loop(&mut messages, &config, Phase::Code, Source::None),
            Stop::Failed(FailureKind::ContextGuard)
        ));
        assert_eq!(
            script.requests(),
            0,
            "a call id reused across groups is never sent"
        );
        assert_eq!(
            serde_json::to_string(&messages).unwrap(),
            before,
            "the refused history keeps its exact serialization"
        );
    }

    /// Regression: a duplicate call id is reported through the existing
    /// context-guard stop, naming the duplicate problem, before any provider
    /// endpoint or authentication is touched.
    #[test]
    fn when_a_transcript_reuses_call_ids_given_duplicate_identifiers_then_blocks_provider_dispatch()
    {
        let script = Script::new(Vec::new());
        let config = config(script.clone());
        let mut messages = plan_transcript();
        messages.push(call_group(&["call-dup", "call-dup"]));
        messages.push(Message::tool("call-dup", "first result"));
        messages.push(Message::tool("call-dup", "second result"));
        let before = serde_json::to_string(&messages).unwrap();

        let provider = config.provider(Phase::Code);
        let mut probed = messages.clone();
        let Err(detail) = prepare_for_send(&mut probed, &config, Phase::Code, provider.as_ref())
        else {
            panic!("a duplicate call id must be refused before preparation");
        };
        assert!(
            detail.contains("duplicate"),
            "the refusal identifies the duplicate-id problem: {detail}"
        );
        assert!(
            detail.contains("call-dup"),
            "the refusal names the duplicated id: {detail}"
        );
        assert_eq!(
            serde_json::to_string(&probed).unwrap(),
            before,
            "pre-send validation refuses without mutating the history"
        );

        assert!(matches!(
            run_agent_loop(&mut messages, &config, Phase::Code, Source::None),
            Stop::Failed(FailureKind::ContextGuard)
        ));
        assert_eq!(
            FailureKind::ContextGuard.label(),
            "context guard reached",
            "the existing guard label is the report the caller sees"
        );
        assert_eq!(script.requests(), 0, "no provider request is dispatched");
        assert!(script.urls().is_empty(), "no endpoint is contacted");
        assert!(
            script.auths().is_empty(),
            "no authentication is presented to a provider"
        );
    }

    /// Edge-case table: missing or empty call ids, missing result ids,
    /// unanswered calls, orphan results, duplicate results and an interrupted
    /// group are all refused before send and leave the history unchanged.
    #[test]
    fn when_sending_history_given_missing_or_orphaned_tool_result_ids_then_rejects_without_request()
    {
        let mut interrupted = plan_transcript();
        interrupted.push(call_group(&["call-interrupted"]));
        interrupted.push(Message::user("a new turn interrupts the pending group"));

        let mut duplicate_result = plan_transcript();
        duplicate_result.push(call_group(&["call-result"]));
        duplicate_result.push(Message::tool("call-result", "first result"));
        duplicate_result.push(Message::tool("call-result", "second result"));

        let mut missing_call_id = plan_transcript();
        missing_call_id.push(call_group(&[""]));
        missing_call_id.push(Message::tool("", "result"));

        let mut missing_result_id = plan_transcript();
        missing_result_id.push(call_group(&["call-result"]));
        missing_result_id.push(Message {
            role: Role::Tool,
            content: Some("result without an id".to_string()),
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            extra: serde_json::Map::new(),
            truncated: false,
        });

        let mut unanswered = plan_transcript();
        unanswered.push(call_group(&["call-unanswered"]));

        let mut orphan_result = plan_transcript();
        orphan_result.push(Message::tool("orphan-call", "orphan result"));

        for (case, messages) in [
            ("interrupted group", interrupted),
            ("duplicate result", duplicate_result),
            ("missing call id", missing_call_id),
            ("missing result id", missing_result_id),
            ("unanswered call", unanswered),
            ("orphan result", orphan_result),
        ] {
            let script = Script::new(Vec::new());
            let config = config(script.clone());
            let mut messages = messages;
            let before = serde_json::to_string(&messages).unwrap();

            assert!(
                matches!(
                    run_agent_loop(&mut messages, &config, Phase::Code, Source::None),
                    Stop::Failed(FailureKind::ContextGuard)
                ),
                "the {case} history must be refused"
            );
            assert_eq!(script.requests(), 0, "the {case} history is never sent");
            assert_eq!(
                serde_json::to_string(&messages).unwrap(),
                before,
                "the {case} history keeps its exact serialization"
            );
        }
    }

    /// Minimum valid boundary: one assistant tool call with exactly one
    /// matching result is accepted and reaches the provider unchanged.
    #[test]
    fn when_sending_history_given_one_complete_tool_call_pair_then_accepts_history() {
        let script = Script::new(vec![reply(Some("done"), &[])]);
        let config = config(script.clone());
        let mut messages = plan_transcript();
        messages.push(call_group(&["call-only"]));
        messages.push(Message::tool("call-only", "the one result"));
        let original = messages.clone();

        assert!(matches!(
            run_agent_loop(&mut messages, &config, Phase::Code, Source::None),
            Stop::TurnEnd
        ));
        assert_eq!(script.requests(), 1, "the valid pair is sent exactly once");
        assert_eq!(
            script.sent()[0]["messages"],
            serde_json::to_value(&original).unwrap(),
            "the single complete pair reaches the provider unchanged"
        );
        assert_eq!(
            messages.len(),
            original.len() + 1,
            "only the ordinary reply is appended"
        );
    }

    /// Unauthorized/Forbidden boundary: an ordinary provider 401 or 403 stays
    /// the same fatal API failure -- one attempt, unchanged authentication --
    /// whether Tier 1 compacted the history first or not.
    #[test]
    fn when_sending_given_ordinary_provider_returns_unauthorized_then_preserves_fatal_api_failure()
    {
        for status in [401, 403] {
            let ordinary_script = Script::with_errors(vec![CallError::Api {
                status,
                message: format!("denied {status}"),
            }]);
            let ordinary_config = config(ordinary_script.clone());
            let mut messages = plan_transcript();

            assert!(matches!(
                run_agent_loop(&mut messages, &ordinary_config, Phase::Code, Source::None),
                Stop::Failed(FailureKind::Api)
            ));
            assert_eq!(
                ordinary_script.requests(),
                1,
                "HTTP {status} is not retried"
            );
            assert_eq!(
                ordinary_script.auths(),
                vec![Auth::Bearer("test-key".to_string())],
                "the ordinary provider authentication is unchanged"
            );

            let compacted_script = Script::with_errors(vec![CallError::Api {
                status,
                message: format!("denied {status}"),
            }]);
            let compacted_config = config(compacted_script.clone());
            let mut messages = pad_to_context(
                &compacted_config,
                tier_one_history(8, 150 * 1024),
                TIER_ONE_CONTEXT_BYTES + 1,
                Phase::Code,
            );

            assert!(matches!(
                run_agent_loop(&mut messages, &compacted_config, Phase::Code, Source::None),
                Stop::Failed(FailureKind::Api)
            ));
            assert_eq!(
                compacted_script.requests(),
                1,
                "Tier 1 makes no request; the ordinary send is the only attempt"
            );
            assert_eq!(
                compacted_script.auths(),
                vec![Auth::Bearer("test-key".to_string())],
                "compaction does not alter the ordinary authentication"
            );
            assert!(
                messages.iter().any(|message| {
                    message.role == Role::Tool
                        && message
                            .content
                            .as_deref()
                            .is_some_and(|content| content.starts_with("[tool result omitted:"))
                }),
                "Tier 1 ran before the fatal ordinary failure"
            );
        }
    }

    /// Regression: the compaction summary goes through the active phase's own
    /// provider route and authentication, advertises no tools, and leaves the
    /// ordinary request's phase tool list untouched.
    #[test]
    fn when_compacting_given_active_phase_provider_then_uses_its_route_and_advertises_no_summary_tools()
     {
        struct Route {
            phase: Phase,
            url: &'static str,
            model: &'static str,
            auth: Auth,
            ordinary_tools: Vec<&'static str>,
        }

        let routes = [
            Route {
                phase: Phase::Code,
                url: "http://script.test/v1/chat/completions",
                model: "mid-model",
                auth: Auth::Bearer("mid-key".to_string()),
                ordinary_tools: vec!["bash", "execute_script", "task_complete"],
            },
            Route {
                phase: Phase::Plan,
                url: "http://script.test/v1/responses",
                model: "giga-model",
                auth: Auth::Bearer("giga-key".to_string()),
                ordinary_tools: vec!["execute_script", "finish_plan"],
            },
            Route {
                phase: Phase::Summary,
                url: "https://smoll.test/v1/messages",
                model: "smoll-model",
                auth: Auth::Anthropic("smoll-key".to_string()),
                ordinary_tools: vec!["execute_script"],
            },
        ];

        for route in routes {
            let (summary_reply, ordinary_reply) = match route.phase {
                Phase::Code => (reply(Some(HANDOVER_REPLY), &[]), reply(Some("done"), &[])),
                Phase::Plan => (responses_text(HANDOVER_REPLY), responses_text("done")),
                Phase::Summary => (native_text(HANDOVER_REPLY), native_text("done")),
                _ => unreachable!("the route table covers every tested phase"),
            };
            let script = Script::new(vec![summary_reply, ordinary_reply]);
            let config = hybrid_config(script.clone());
            let mut messages = summary_history();

            let stop = run_agent_loop(&mut messages, &config, route.phase, Source::None);
            if route.phase == Phase::Summary {
                assert!(matches!(stop, Stop::Summary(Some(ref text)) if text == "done"));
            } else {
                assert!(matches!(stop, Stop::TurnEnd));
            }
            assert_eq!(script.requests(), 2, "phase {:?}", route.phase);

            let bodies = script.sent();
            assert_eq!(
                script.urls(),
                vec![route.url, route.url],
                "phase {:?}",
                route.phase
            );
            assert_eq!(
                script.auths(),
                vec![route.auth.clone(), route.auth],
                "phase {:?}",
                route.phase
            );

            assert_eq!(
                bodies[0]["model"],
                serde_json::json!(route.model),
                "the summary uses the active phase's model"
            );
            assert_eq!(
                bodies[0]["tools"],
                serde_json::json!([]),
                "the summary advertises no tools"
            );

            let names: Vec<&str> = bodies[1]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|tool| {
                    tool.get("function")
                        .and_then(|function| function.get("name"))
                        .or_else(|| tool.get("name"))
                        .and_then(serde_json::Value::as_str)
                        .unwrap()
                })
                .collect();
            assert_eq!(
                names, route.ordinary_tools,
                "phase {:?} keeps its ordinary tool list",
                route.phase
            );
        }
    }

    /// Regression: the hard ceiling is inclusive -- exactly 4 MiB of encoded
    /// body is sent -- and the body guard never replaces the provider-prepared
    /// context measure the watermarks use.
    #[test]
    fn when_sending_given_encoded_body_at_four_mib_then_allows_request_and_preserves_provider_context_measure()
     {
        let messages = vec![Message::system("sys"), Message::user("work the ticket")];
        let effort_bytes = effort_bytes_for_body(&messages, CONTEXT_GUARD_BYTES);
        let script = Script::new(vec![reply(Some("done"), &[])]);
        let config = config_with_effort(script.clone(), &"x".repeat(effort_bytes));
        let provider = config.provider(Phase::Code);

        let prepared = provider.prepare(&messages, Phase::Code);
        assert_eq!(
            encoded_body_bytes(&prepared.outgoing.body),
            CONTEXT_GUARD_BYTES,
            "the fixture lands exactly on the four-MiB hard limit"
        );
        assert_eq!(
            prepared.context_bytes,
            context_bytes(&config, &messages, Phase::Code),
            "the provider's prepared context remains the watermark measure"
        );
        assert!(
            prepared.context_bytes <= TIER_ONE_CONTEXT_BYTES,
            "the body guard is tested below the compaction watermark"
        );
        let provider_measure = prepared.context_bytes;

        let mut prepared_messages = messages.clone();
        let guarded = prepare_for_send(
            &mut prepared_messages,
            &config,
            Phase::Code,
            provider.as_ref(),
        )
        .expect("exactly four MiB is permitted");
        assert_eq!(
            guarded.context_bytes, provider_measure,
            "the body guard does not replace the provider context measure"
        );

        let mut sent_messages = messages.clone();
        assert!(matches!(
            run_agent_loop(&mut sent_messages, &config, Phase::Code, Source::None),
            Stop::TurnEnd
        ));
        assert_eq!(script.requests(), 1, "exactly at the limit is sent");
        assert_eq!(
            script.sent()[0]["messages"],
            serde_json::to_value(&messages).unwrap(),
            "the transcript is neither mutated nor summarized"
        );
    }

    /// One byte over the hard ceiling is not sent, even though the prepared
    /// context is below the compaction watermark, so no transcript mutation
    /// could have brought it back.
    #[test]
    fn when_sending_given_encoded_body_above_hard_limit_then_stops_without_request() {
        let messages = vec![Message::system("sys"), Message::user("work the ticket")];
        let effort_bytes = effort_bytes_for_body(&messages, CONTEXT_GUARD_BYTES + 1);
        let script = Script::new(Vec::new());
        let config = config_with_effort(script.clone(), &"x".repeat(effort_bytes));

        let prepared = config.provider(Phase::Code).prepare(&messages, Phase::Code);
        assert_eq!(
            encoded_body_bytes(&prepared.outgoing.body),
            CONTEXT_GUARD_BYTES + 1
        );
        assert!(prepared.context_bytes <= TIER_ONE_CONTEXT_BYTES);

        let mut sent_messages = messages.clone();
        assert!(matches!(
            run_agent_loop(&mut sent_messages, &config, Phase::Code, Source::None),
            Stop::Failed(FailureKind::ContextGuard)
        ));
        assert_eq!(script.requests(), 0, "the oversized body is never sent");
        assert_eq!(
            serde_json::to_value(&sent_messages).unwrap(),
            serde_json::to_value(&messages).unwrap(),
            "a below-watermark transcript is not mutated"
        );
    }

    /// Malformed/Invalid payload: every unusable summary response and every
    /// history that cannot preserve pairing stops as a context guard, and no
    /// ordinary oversized request follows.
    #[test]
    fn when_summarizing_given_invalid_response_or_unpaired_history_then_stops_with_context_guard() {
        let invalid_responses = [
            reply(None, &[]),
            reply(Some(""), &[]),
            reply(Some("not a handover summary"), &[]),
            "not json".to_string(),
            reply(None, &[("execute_script", "{}".to_string())]),
        ];
        for invalid in invalid_responses {
            let script = Script::new(vec![invalid]);
            let configuration = config(script.clone());
            let mut messages = summary_history();

            assert!(matches!(
                run_agent_loop(&mut messages, &configuration, Phase::Code, Source::None),
                Stop::Failed(FailureKind::ContextGuard)
            ));
            assert_eq!(
                script.requests(),
                1,
                "only the one summary request is made; no ordinary send follows"
            );
        }

        // A summary that leaves the pinned prefix alone but the prepared
        // context still over Tier 2 is refused before the ordinary send.
        let script = Script::new(vec![reply(Some(HANDOVER_REPLY), &[])]);
        let configuration = config(script.clone());
        let mut messages = vec![
            Message::system("x".repeat(TIER_ONE_CONTEXT_BYTES + 1)),
            Message::user("work the ticket"),
        ];
        assert!(matches!(
            run_agent_loop(&mut messages, &configuration, Phase::Code, Source::None),
            Stop::Failed(FailureKind::ContextGuard)
        ));
        assert_eq!(
            script.requests(),
            1,
            "the summary is not followed by an ordinary send"
        );

        // A tool result without its call, and a call without its result, are
        // both invalid replacement histories: nothing is sent at all.
        let mut unpaired_result = summary_history();
        unpaired_result.pop();
        let mut unpaired_call = summary_history();
        unpaired_call.push(Message::tool("missing-call", "orphan"));
        for messages in [unpaired_result, unpaired_call] {
            let script = Script::new(Vec::new());
            let configuration = config(script.clone());
            let mut messages = messages;
            assert!(matches!(
                run_agent_loop(&mut messages, &configuration, Phase::Code, Source::None),
                Stop::Failed(FailureKind::ContextGuard)
            ));
            assert_eq!(
                script.requests(),
                0,
                "an unpaired history is never sent to a provider"
            );
        }
    }

    /// Unauthorized/Forbidden boundary: compaction does not alter authentication
    /// or the ordinary fatal API failure, and neither 401 nor 403 gains a retry.
    #[test]
    fn when_compacting_given_unauthorized_provider_response_then_preserves_fatal_provider_failure()
    {
        for status in [401, 403] {
            let script = Script::with_errors(vec![CallError::Api {
                status,
                message: format!("denied {status}"),
            }]);
            let config = config(script.clone());
            let mut messages = plan_transcript();

            assert!(matches!(
                run_agent_loop(&mut messages, &config, Phase::Code, Source::None),
                Stop::Failed(FailureKind::Api)
            ));
            assert_eq!(script.requests(), 1, "HTTP {status} is not retried");
            assert_eq!(
                script.auths(),
                vec![Auth::Bearer("test-key".to_string())],
                "the ordinary provider authentication is unchanged"
            );
        }

        // The compaction-only summary request is not retried either: its one
        // failure ends the phase as a context guard, before any ordinary send.
        let script = Script::with_errors(vec![CallError::Api {
            status: 401,
            message: "denied summary".to_string(),
        }]);
        let config = config(script.clone());
        let mut messages = summary_history();
        assert!(matches!(
            run_agent_loop(&mut messages, &config, Phase::Code, Source::None),
            Stop::Failed(FailureKind::ContextGuard)
        ));
        assert_eq!(script.requests(), 1, "the summary failure is not retried");
    }

    /// One configuration whose mid reasoning effort carries `effort`
    /// characters: the effort is excluded from `context_bytes` but included in
    /// the encoded body, so a test can put the body on an exact byte boundary
    /// without changing the transcript or the model the status line prints.
    fn config_with_effort(script: Arc<Script>, effort: &str) -> Config {
        Config::new(
            "http://script.test/v1/chat/completions",
            "test-model",
            Some("test-key".to_string()),
        )
        .with_smoll("https://smoll.test/v1/messages", "smoll-model", "smoll-key")
        .with_transport(script)
        .with_mid_reasoning_effort(effort)
    }

    /// How many effort characters make `messages`'s encoded chat body exactly
    /// `target` bytes. An effort character is one JSON byte, and the effort is
    /// excluded from the prepared context.
    fn effort_bytes_for_body(messages: &[Message], target: usize) -> usize {
        let baseline = Config::new(
            "http://script.test/v1/chat/completions",
            "test-model",
            Some("test-key".to_string()),
        )
        .with_smoll("https://smoll.test/v1/messages", "smoll-model", "smoll-key")
        .with_mid_reasoning_effort("");
        let prepared = baseline
            .provider(Phase::Code)
            .prepare(messages, Phase::Code);
        let base = encoded_body_bytes(&prepared.outgoing.body);
        assert!(target > base, "the target body must exceed {base} bytes");
        target - base
    }

    // -----------------------------------------------------------------------
    // Truncated tool-free reply recovery
    //
    // A provider can stop a reply at its output limit. When that reply also
    // carries no tool call, the loop asks the model once -- at most twice in a
    // row -- to continue in the same transcript before it treats the turn as
    // finished. Only the external transport is scripted; every request body is
    // observed at that boundary.
    // -----------------------------------------------------------------------

    /// The exact instruction the loop appends after a truncated tool-free
    /// reply. Duplicated in the tests as an independent literal so a change to
    /// the wording is a visible test failure, not a silently recomputed value.
    const TRUNCATION_RECOVERY: &str = "The assistant reply above was cut off. Continue from that partial reply and complete the intended response or tool call. Do not repeat completed content.";

    /// Core entry-point behavior and the minimum valid boundary: one truncated
    /// tool-free reply earns exactly one recovery prompt, appended after the
    /// partial assistant turn, with the earlier transcript preserved.
    #[test]
    fn when_reply_is_truncated_given_no_tool_calls_then_requests_recovery_in_same_conversation() {
        const PARTIAL: &str = "I will inspect the build and";
        let script = Script::new(vec![
            truncated_reply(Some(PARTIAL), &[]),
            reply(Some("done"), &[]),
        ]);
        let config = config(script.clone());
        let mut messages = plan_transcript();

        assert!(matches!(
            run_agent_loop(&mut messages, &config, Phase::Code, Source::None),
            Stop::TurnEnd
        ));
        assert_eq!(
            script.requests(),
            2,
            "the partial reply earns exactly one recovery request"
        );

        let sent = recorded_messages(&script.sent()[1], "messages");
        assert_eq!(
            sent.len(),
            4,
            "the transcript gained the reply and one prompt"
        );
        assert_eq!(sent[0].role, Role::System);
        assert_eq!(sent[0].content.as_deref(), Some("sys"));
        assert_eq!(sent[1].role, Role::User);
        assert_eq!(sent[1].content.as_deref(), Some("do it"));
        assert_eq!(sent[2].role, Role::Assistant);
        assert_eq!(
            sent[2].content.as_deref(),
            Some(PARTIAL),
            "the partial assistant reply is replayed unchanged"
        );
        assert_eq!(sent[3].role, Role::User);
        assert_eq!(
            sent[3].content.as_deref(),
            Some(TRUNCATION_RECOVERY),
            "the recovery prompt follows the partial reply"
        );
    }

    /// Core entry-point behavior and the maximum consecutive limit: two
    /// recovery prompts are allowed, and the third truncated tool-free reply
    /// ends the turn without another request. Summary returns its last answer;
    /// every other phase returns the ordinary turn end.
    #[test]
    fn when_reply_stays_truncated_given_two_recoveries_then_ends_without_third_request() {
        let script = Script::new(vec![
            truncated_reply(Some("partial one"), &[]),
            truncated_reply(Some("partial two"), &[]),
            truncated_reply(Some("partial three"), &[]),
        ]);
        let code_config = config(script.clone());
        let mut messages = plan_transcript();

        assert!(matches!(
            run_agent_loop(&mut messages, &code_config, Phase::Code, Source::None),
            Stop::TurnEnd
        ));
        assert_eq!(
            script.requests(),
            3,
            "two recovery requests follow the initial turn, and no third is sent"
        );
        let sent = recorded_messages(&script.sent()[2], "messages");
        let recoveries = sent
            .iter()
            .filter(|message| {
                message.role == Role::User
                    && message.content.as_deref() == Some(TRUNCATION_RECOVERY)
            })
            .count();
        assert_eq!(
            recoveries, 2,
            "both permitted prompts are in the transcript"
        );

        let script = Script::new(vec![
            native_truncated_reply(Some("partial one"), &[]),
            native_truncated_reply(Some("partial two"), &[]),
            native_truncated_reply(Some("partial three"), &[]),
        ]);
        let summary_config = config(script.clone());
        let mut messages = summary_transcript();

        match run_agent_loop(&mut messages, &summary_config, Phase::Summary, Source::None) {
            Stop::Summary(Some(answer)) => assert_eq!(answer, "partial three"),
            _ => panic!("the exhausted summary returns its last assistant content"),
        }
        assert_eq!(
            script.requests(),
            3,
            "the summary sends no request after its second recovery"
        );
        let bodies = script.sent();
        let sent = bodies[2]["messages"]
            .as_array()
            .expect("the native request carries a messages array");
        let recoveries = sent
            .iter()
            .filter(|message| {
                message["role"] == serde_json::json!("user")
                    && message["content"].as_array().is_some_and(|blocks| {
                        blocks
                            .iter()
                            .any(|block| block["text"].as_str() == Some(TRUNCATION_RECOVERY))
                    })
            })
            .count();
        assert_eq!(recoveries, 2, "the summary allows exactly two prompts");
    }

    /// Malformed/Invalid payload boundary: a truncated summary with absent or
    /// empty content ends with the absent answer preserved as `None`, and the
    /// loop never invents content for it.
    #[test]
    fn when_reply_has_no_content_given_truncated_summary_then_preserves_absent_answer() {
        for body in [
            native_truncated_reply(None, &[]),
            native_truncated_reply(Some(""), &[]),
        ] {
            let script = Script::new(vec![body.clone(), body.clone(), body]);
            let config = config(script.clone());
            let mut messages = summary_transcript();

            match run_agent_loop(&mut messages, &config, Phase::Summary, Source::None) {
                Stop::Summary(None) => {}
                _ => panic!("a truncated summary with no content has no answer to report"),
            }
            assert_eq!(
                script.requests(),
                3,
                "two recoveries precede the absent final answer"
            );
        }
    }

    /// Upper boundary: the existing step budget still governs. A truncated
    /// tool-free reply at the final available iteration cannot earn a request
    /// beyond `MAX_STEPS`, so the loop stops at the step limit.
    #[test]
    fn when_final_step_reply_is_truncated_given_recovery_would_exceed_budget_then_stops_at_step_limit()
     {
        let mut bodies = Vec::new();
        for index in 0..MAX_STEPS - 1 {
            // Every step needs its own call id and arguments: a repeated id is
            // an invalid transcript, and a repeated signature would trip the
            // repeated-tool-call guard before the step limit.
            let mut body: serde_json::Value = serde_json::from_str(&reply(
                Some("working"),
                &[("no_such_tool", format!(r#"{{"step":{index}}}"#))],
            ))
            .expect("the scripted reply is JSON");
            body["choices"][0]["message"]["tool_calls"][0]["id"] =
                serde_json::json!(format!("call-{index}"));
            bodies.push(body.to_string());
        }
        bodies.push(truncated_reply(Some("partial at the limit"), &[]));
        let script = Script::new(bodies);
        let config = config(script.clone());
        let mut messages = plan_transcript();

        assert!(matches!(
            run_agent_loop(&mut messages, &config, Phase::Code, Source::None),
            Stop::Failed(FailureKind::StepLimit)
        ));
        assert_eq!(
            script.requests(),
            MAX_STEPS,
            "recovery never sends a request beyond the step budget"
        );
    }

    /// Contract and invariant: a tool-call reply resets the consecutive
    /// recovery count even when the provider marked it truncated, and the
    /// existing tool handling still runs. After the reset, two further prompts
    /// are available again.
    #[test]
    fn when_recovery_is_followed_by_a_tool_call_given_a_truncated_reply_then_resets_recovery_count()
    {
        let script = Script::new(vec![
            truncated_reply(Some("partial one"), &[]),
            truncated_reply(
                Some("calling a tool"),
                &[("no_such_tool", "{}".to_string())],
            ),
            truncated_reply(Some("partial two"), &[]),
            truncated_reply(Some("partial three"), &[]),
            truncated_reply(Some("partial four"), &[]),
        ]);
        let config = config(script.clone());
        let mut messages = plan_transcript();

        assert!(matches!(
            run_agent_loop(&mut messages, &config, Phase::Code, Source::None),
            Stop::TurnEnd
        ));
        assert_eq!(
            script.requests(),
            5,
            "the reset makes two more prompts available after the tool call"
        );

        let after_tool_call = recorded_messages(&script.sent()[2], "messages");
        assert!(
            after_tool_call
                .iter()
                .any(|message| message.role == Role::Tool
                    && message.tool_call_id.as_deref() == Some("call-0")),
            "the truncated tool-call reply still ran the existing tool handling: {after_tool_call:?}"
        );
        let sent = recorded_messages(&script.sent()[4], "messages");
        let recoveries = sent
            .iter()
            .filter(|message| {
                message.role == Role::User
                    && message.content.as_deref() == Some(TRUNCATION_RECOVERY)
            })
            .count();
        assert_eq!(
            recoveries, 3,
            "one prompt before the tool call plus the two after the reset"
        );
    }

    /// Contract and invariant: a complete tool-free reply ends immediately even
    /// when a recovery was requested earlier, with the existing phase-specific
    /// result and no extra request.
    #[test]
    fn when_recovery_reply_is_complete_given_a_prior_recovery_then_ends_immediately() {
        let script = Script::new(vec![
            truncated_reply(Some("partial"), &[]),
            reply(Some("done"), &[]),
        ]);
        let code_config = config(script.clone());
        let mut messages = plan_transcript();
        assert!(matches!(
            run_agent_loop(&mut messages, &code_config, Phase::Code, Source::None),
            Stop::TurnEnd
        ));
        assert_eq!(
            script.requests(),
            2,
            "the complete reply needs no third turn"
        );

        let script = Script::new(vec![
            native_truncated_reply(Some("partial"), &[]),
            native_text("complete answer"),
        ]);
        let summary_config = config(script.clone());
        let mut messages = summary_transcript();
        match run_agent_loop(&mut messages, &summary_config, Phase::Summary, Source::None) {
            Stop::Summary(Some(answer)) => assert_eq!(answer, "complete answer"),
            _ => panic!("the complete summary reply is the answer"),
        }
        assert_eq!(
            script.requests(),
            2,
            "the complete summary reply ends the pass without another request"
        );
    }

    /// Regression: the recovery path does not touch authentication or the
    /// existing fatal API failure. A forbidden provider response is reported
    /// once, and no recovery prompt is appended.
    #[test]
    fn when_provider_forbids_access_given_recovery_is_enabled_then_returns_existing_api_failure() {
        for status in [401, 403] {
            let script = Script::with_errors(vec![CallError::Api {
                status,
                message: format!("denied {status}"),
            }]);
            let config = config(script.clone());
            let mut messages = plan_transcript();
            let original = messages.clone();

            assert!(matches!(
                run_agent_loop(&mut messages, &config, Phase::Code, Source::None),
                Stop::Failed(FailureKind::Api)
            ));
            assert_eq!(script.requests(), 1, "HTTP {status} is not retried");
            assert_eq!(
                serde_json::to_value(&messages).unwrap(),
                serde_json::to_value(&original).unwrap(),
                "a failed request appends no recovery prompt"
            );
        }
    }

    /// Empty/zero/nil boundary: an absent tool-call list and an explicitly
    /// empty one are both tool-free, so each truncated reply earns the same
    /// recovery request.
    #[test]
    fn when_tool_calls_are_absent_or_empty_given_a_truncated_reply_then_treats_reply_as_tool_free()
    {
        const PARTIAL: &str = "partial";
        for first in [
            truncated_reply(Some(PARTIAL), &[]),
            truncated_reply_without_tool_calls(PARTIAL),
        ] {
            let script = Script::new(vec![first, reply(Some("done"), &[])]);
            let config = config(script.clone());
            let mut messages = plan_transcript();

            assert!(matches!(
                run_agent_loop(&mut messages, &config, Phase::Code, Source::None),
                Stop::TurnEnd
            ));
            assert_eq!(
                script.requests(),
                2,
                "a tool-free truncated reply always earns one recovery request"
            );
            let sent = recorded_messages(&script.sent()[1], "messages");
            assert_eq!(sent.last().unwrap().role, Role::User);
            assert_eq!(
                sent.last().unwrap().content.as_deref(),
                Some(TRUNCATION_RECOVERY),
                "the recovery prompt is appended for the empty and the absent list"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Repeated tool-call guard
    // -----------------------------------------------------------------------

    /// The redirect notice's fixed leading text, duplicated here as an
    /// independent literal so these expectations do not read the loop constant.
    const REPEATED_CALL_REDIRECT_HEADER: &str = "[WARN: repeated tool calls] Your last turns issued the same tool calls. Choose a different action instead of repeating them. Canonical arguments preview: ";

    /// The stderr line logged when the third matching turn is redirected.
    const REPEATED_CALL_WARNING: &str =
        "[WARN] repeated tool calls; asking the model to choose a different action";

    /// The stderr line logged when the fifth matching turn is stopped.
    const REPEATED_CALL_ABORT_WARNING: &str =
        "[WARN] repeated tool calls; stopping before the step limit";

    fn redirect_notice(preview: &str) -> String {
        format!("{REPEATED_CALL_REDIRECT_HEADER}{preview}")
    }

    fn assert_no_redirect(content: &str) {
        assert!(
            !content.contains(REPEATED_CALL_REDIRECT_HEADER),
            "this tool result must not carry a redirect: {content:?}"
        );
    }

    /// Core entry point and upper boundary: the third matching turn runs and is
    /// redirected, the fourth still runs, and the fifth is stopped before any
    /// call is parsed or dispatched.
    #[test]
    fn when_tool_calls_repeat_given_five_matching_turns_then_stops_before_dispatch() {
        let arguments = r#"{"code":"return 'same';"}"#.to_string();
        let mut bodies = Vec::new();
        for turn in 0..5 {
            bodies.push(reply_numbered(
                turn * 10,
                Some("working"),
                &[("execute_script", arguments.clone())],
            ));
        }
        let script = Script::new(bodies);
        let settings = config(script.clone());
        let mut messages = plan_transcript();

        let stop = run_agent_loop(&mut messages, &settings, Phase::Code, Source::None);

        assert!(matches!(stop, Stop::Failed(FailureKind::StepLimit)));
        assert_eq!(
            script.requests(),
            5,
            "the fifth reply arrived and no sixth request was sent"
        );
        assert_eq!(
            assistant_tool_steps(&messages).len(),
            4,
            "the fifth response is never replayed into the transcript"
        );

        for id in ["call-0", "call-10", "call-30"] {
            let result = tool_result_for(&messages, id).expect("the turn dispatched its call");
            assert_eq!(result, "same", "turns one, two and four dispatch normally");
            assert_no_redirect(&result);
        }
        let third = tool_result_for(&messages, "call-20").expect("the third turn dispatched");
        assert_eq!(
            third,
            format!("same\n{}", redirect_notice(r#"{"code":"return 'same';"}"#)),
            "the third turn's last delivered result carries the redirect"
        );
        assert!(
            tool_result_for(&messages, "call-40").is_none(),
            "the fifth turn dispatched none of its calls and appended no result"
        );
    }

    /// Malformed-payload boundary: rejected calls are counted before parsing or
    /// dispatch, a changed signature starts a new sequence, and a tool-free turn
    /// breaks the consecutive tool-call sequence.
    #[test]
    fn when_rejected_calls_repeat_given_matching_turns_then_counts_each_turn() {
        // Five identical rejected turns still reach the abort threshold.
        let mut bodies = Vec::new();
        for turn in 0..5 {
            bodies.push(reply_numbered(
                turn * 10,
                Some("retrying"),
                &[("no_such_tool", r#"{}"#.to_string())],
            ));
        }
        let script = Script::new(bodies);
        let settings = config(script.clone());
        let mut messages = plan_transcript();

        assert!(matches!(
            run_agent_loop(&mut messages, &settings, Phase::Code, Source::None),
            Stop::Failed(FailureKind::StepLimit)
        ));
        assert_eq!(script.requests(), 5, "the fifth rejected turn is counted");
        assert_eq!(
            messages
                .iter()
                .filter(|message| message.role == Role::Tool)
                .count(),
            4,
            "a rejected call still delivers a result, and the fifth turn delivers none"
        );
        let third = tool_result_for(&messages, "call-20").expect("the third turn was rejected");
        assert!(third.ends_with(&redirect_notice("{}")));
        assert!(tool_result_for(&messages, "call-40").is_none());

        // A changed signature resets the count: only two turns share call two's
        // signature, so the third overall turn is not redirected.
        let script = Script::new(vec![
            reply_numbered(0, Some("one"), &[("no_such_tool", r#"{}"#.to_string())]),
            reply_numbered(10, Some("two"), &[("no_such_tool", r#"{}"#.to_string())]),
            reply_numbered(
                20,
                Some("three"),
                &[("other_missing_tool", r#"{}"#.to_string())],
            ),
            reply(Some("done"), &[]),
        ]);
        let settings = config(script.clone());
        let mut messages = plan_transcript();

        assert!(matches!(
            run_agent_loop(&mut messages, &settings, Phase::Code, Source::None),
            Stop::TurnEnd
        ));
        assert_eq!(script.requests(), 4);
        for id in ["call-0", "call-10", "call-20"] {
            assert_no_redirect(&tool_result_for(&messages, id).expect("the call ran"));
        }

        // A tool-free truncated turn breaks the sequence even though the loop
        // continues through its recovery prompt.
        let script = Script::new(vec![
            reply_numbered(0, Some("one"), &[("no_such_tool", r#"{}"#.to_string())]),
            reply_numbered(10, Some("two"), &[("no_such_tool", r#"{}"#.to_string())]),
            truncated_reply(Some("cut off"), &[]),
            reply_numbered(20, Some("three"), &[("no_such_tool", r#"{}"#.to_string())]),
            reply_numbered(30, Some("four"), &[("no_such_tool", r#"{}"#.to_string())]),
            reply_numbered(40, Some("five"), &[("no_such_tool", r#"{}"#.to_string())]),
            reply(Some("done"), &[]),
        ]);
        let settings = config(script.clone());
        let mut messages = plan_transcript();

        assert!(matches!(
            run_agent_loop(&mut messages, &settings, Phase::Code, Source::None),
            Stop::TurnEnd
        ));
        assert_eq!(script.requests(), 7);
        for id in ["call-0", "call-10", "call-20", "call-30"] {
            assert_no_redirect(&tool_result_for(&messages, id).expect("the call ran"));
        }
        let third_after_break =
            tool_result_for(&messages, "call-40").expect("the third turn was rejected");
        assert!(
            third_after_break.ends_with(&redirect_notice("{}")),
            "the first matching turn after the tool-free break starts at one and the third is redirected"
        );
    }

    /// Contract: recursively reordered object keys compare equal, while array
    /// order and JSON values remain significant.
    #[test]
    fn when_arguments_reorder_given_matching_json_objects_then_counts_the_same_turn() {
        let code = "return 'same';";
        let first = format!(r#"{{"code":"{code}","meta":{{"b":[1,{{"y":2,"x":1}}],"a":true}}}}"#);
        let second = format!(r#"{{"meta":{{"a":true,"b":[1,{{"x":1,"y":2}}]}},"code":"{code}"}}"#);
        let script = Script::new(vec![
            reply_numbered(0, Some("one"), &[("execute_script", first.clone())]),
            reply_numbered(10, Some("two"), &[("execute_script", second)]),
            reply_numbered(20, Some("three"), &[("execute_script", first)]),
            reply(Some("done"), &[]),
        ]);
        let settings = config(script.clone());
        let mut messages = plan_transcript();

        assert!(matches!(
            run_agent_loop(&mut messages, &settings, Phase::Code, Source::None),
            Stop::TurnEnd
        ));
        assert_eq!(script.requests(), 4);
        for id in ["call-0", "call-10"] {
            assert_no_redirect(&tool_result_for(&messages, id).expect("the call ran"));
        }
        let third = tool_result_for(&messages, "call-20").expect("the third turn ran");
        assert!(third.ends_with(&redirect_notice(
            r#"{"code":"return 'same';","meta":{"a":true,"b":[1,{"x":1,"y":2}]}}"#
        )));

        // Array order stays significant: a reordered array is a new signature,
        // so the third overall turn is only the second matching one.
        let first = format!(r#"{{"code":"{code}","tags":["a","b"]}}"#);
        let second = format!(r#"{{"code":"{code}","tags":["b","a"]}}"#);
        let script = Script::new(vec![
            reply_numbered(0, Some("one"), &[("execute_script", first.clone())]),
            reply_numbered(10, Some("two"), &[("execute_script", second.clone())]),
            reply_numbered(20, Some("three"), &[("execute_script", first)]),
            reply(Some("done"), &[]),
        ]);
        let settings = config(script.clone());
        let mut messages = plan_transcript();

        assert!(matches!(
            run_agent_loop(&mut messages, &settings, Phase::Code, Source::None),
            Stop::TurnEnd
        ));
        assert_eq!(script.requests(), 4);
        for id in ["call-0", "call-10", "call-20"] {
            assert_no_redirect(&tool_result_for(&messages, id).expect("the call ran"));
        }

        // JSON values stay significant for the same reason.
        let first = format!(r#"{{"code":"{code}","n":1}}"#);
        let second = format!(r#"{{"code":"{code}","n":2}}"#);
        let script = Script::new(vec![
            reply_numbered(0, Some("one"), &[("execute_script", first.clone())]),
            reply_numbered(10, Some("two"), &[("execute_script", second)]),
            reply_numbered(20, Some("three"), &[("execute_script", first)]),
            reply(Some("done"), &[]),
        ]);
        let settings = config(script.clone());
        let mut messages = plan_transcript();

        assert!(matches!(
            run_agent_loop(&mut messages, &settings, Phase::Code, Source::None),
            Stop::TurnEnd
        ));
        assert_eq!(script.requests(), 4);
        for id in ["call-0", "call-10", "call-20"] {
            assert_no_redirect(&tool_result_for(&messages, id).expect("the call ran"));
        }
    }

    /// Contract: the ordered signature includes every function name and
    /// canonical arguments in call order and excludes call ids and kind.
    #[test]
    fn when_calls_reorder_given_same_calls_in_different_order_then_starts_a_new_sequence() {
        let a = ("execute_script", r#"{"code":"return 'a';"}"#.to_string());
        let b = ("execute_script", r#"{"code":"return 'b';"}"#.to_string());

        // Swapping the calls changes the signature, so the third overall turn is
        // only the second matching one.
        let script = Script::new(vec![
            reply_numbered(0, Some("one"), &[a.clone(), b.clone()]),
            reply_numbered(10, Some("two"), &[b.clone(), a.clone()]),
            reply_numbered(20, Some("three"), &[b.clone(), a.clone()]),
            reply(Some("done"), &[]),
        ]);
        let settings = config(script.clone());
        let mut messages = plan_transcript();

        assert!(matches!(
            run_agent_loop(&mut messages, &settings, Phase::Code, Source::None),
            Stop::TurnEnd
        ));
        assert_eq!(script.requests(), 4);
        for id in [
            "call-0", "call-1", "call-10", "call-11", "call-20", "call-21",
        ] {
            assert_no_redirect(&tool_result_for(&messages, id).expect("the call ran"));
        }

        // A changed function name changes the signature, even when the
        // arguments and the order are identical.
        let same_arguments = r#"{"code":"return 'a';"}"#.to_string();
        let script = Script::new(vec![
            reply_numbered(
                0,
                Some("one"),
                &[("execute_script", same_arguments.clone())],
            ),
            reply_numbered(10, Some("two"), &[("no_such_tool", same_arguments.clone())]),
            reply_numbered(20, Some("three"), &[("no_such_tool", same_arguments)]),
            reply(Some("done"), &[]),
        ]);
        let settings = config(script.clone());
        let mut messages = plan_transcript();

        assert!(matches!(
            run_agent_loop(&mut messages, &settings, Phase::Code, Source::None),
            Stop::TurnEnd
        ));
        assert_eq!(script.requests(), 4);
        for id in ["call-0", "call-10", "call-20"] {
            assert_no_redirect(&tool_result_for(&messages, id).expect("the call ran"));
        }

        // Different call ids are excluded from the signature: three turns with
        // distinct ids still match and the third is redirected.
        let script = Script::new(vec![
            reply_numbered(0, Some("one"), &[a.clone(), b.clone()]),
            reply_numbered(10, Some("two"), &[a.clone(), b.clone()]),
            reply_numbered(20, Some("three"), &[a.clone(), b.clone()]),
            reply(Some("done"), &[]),
        ]);
        let settings = config(script.clone());
        let mut messages = plan_transcript();

        assert!(matches!(
            run_agent_loop(&mut messages, &settings, Phase::Code, Source::None),
            Stop::TurnEnd
        ));
        assert_eq!(script.requests(), 4);
        for id in ["call-0", "call-1", "call-10", "call-11", "call-20"] {
            assert_no_redirect(&tool_result_for(&messages, id).expect("the call ran"));
        }
        assert_eq!(
            tool_result_for(&messages, "call-21").expect("the last call ran"),
            format!(
                "b\n{}",
                redirect_notice(r#"{"code":"return 'a';"} | {"code":"return 'b';"}"#)
            ),
            "the last delivered result of the matching turn is annotated"
        );
    }

    /// Contract: invalid argument strings are compared byte-for-byte in their
    /// original form, with no normalization.
    #[test]
    fn when_arguments_are_invalid_given_repeated_calls_then_compares_original_bytes() {
        // Three identical malformed strings match and the third is redirected.
        let script = Script::new(vec![
            reply_numbered(0, Some("one"), &[("no_such_tool", r#"{"#.to_string())]),
            reply_numbered(10, Some("two"), &[("no_such_tool", r#"{"#.to_string())]),
            reply_numbered(20, Some("three"), &[("no_such_tool", r#"{"#.to_string())]),
            reply(Some("done"), &[]),
        ]);
        let settings = config(script.clone());
        let mut messages = plan_transcript();

        assert!(matches!(
            run_agent_loop(&mut messages, &settings, Phase::Code, Source::None),
            Stop::TurnEnd
        ));
        assert_eq!(script.requests(), 4);
        let third = tool_result_for(&messages, "call-20").expect("the third turn was rejected");
        assert!(third.ends_with(&redirect_notice("{")));

        // A different byte sequence is a different signature even though neither
        // string is valid JSON, so only the fourth overall turn is redirected.
        let script = Script::new(vec![
            reply_numbered(0, Some("one"), &[("no_such_tool", r#"{"#.to_string())]),
            reply_numbered(10, Some("two"), &[("no_such_tool", r#"{ "#.to_string())]),
            reply_numbered(20, Some("three"), &[("no_such_tool", r#"{ "#.to_string())]),
            reply_numbered(30, Some("four"), &[("no_such_tool", r#"{ "#.to_string())]),
            reply(Some("done"), &[]),
        ]);
        let settings = config(script.clone());
        let mut messages = plan_transcript();

        assert!(matches!(
            run_agent_loop(&mut messages, &settings, Phase::Code, Source::None),
            Stop::TurnEnd
        ));
        assert_eq!(script.requests(), 5);
        for id in ["call-0", "call-10", "call-20"] {
            assert_no_redirect(&tool_result_for(&messages, id).expect("the call was rejected"));
        }
        let fourth = tool_result_for(&messages, "call-30").expect("the fourth turn was rejected");
        assert!(fourth.ends_with(&redirect_notice("{ ")));
    }

    /// Empty boundary: empty argument strings compare byte-for-byte and are not
    /// normalized into whitespace-only strings.
    #[test]
    fn when_arguments_are_empty_given_repeated_calls_then_compares_original_bytes() {
        // Three identical empty strings match and the third is redirected.
        let script = Script::new(vec![
            reply_numbered(0, Some("one"), &[("no_such_tool", String::new())]),
            reply_numbered(10, Some("two"), &[("no_such_tool", String::new())]),
            reply_numbered(20, Some("three"), &[("no_such_tool", String::new())]),
            reply(Some("done"), &[]),
        ]);
        let settings = config(script.clone());
        let mut messages = plan_transcript();

        assert!(matches!(
            run_agent_loop(&mut messages, &settings, Phase::Code, Source::None),
            Stop::TurnEnd
        ));
        assert_eq!(script.requests(), 4);
        let third = tool_result_for(&messages, "call-20").expect("the third turn was rejected");
        assert!(third.ends_with(REPEATED_CALL_REDIRECT_HEADER));

        // An empty string and a whitespace-only string are different bytes, so
        // only the fourth overall turn is redirected.
        let script = Script::new(vec![
            reply_numbered(0, Some("one"), &[("no_such_tool", String::new())]),
            reply_numbered(10, Some("two"), &[("no_such_tool", " ".to_string())]),
            reply_numbered(20, Some("three"), &[("no_such_tool", " ".to_string())]),
            reply_numbered(30, Some("four"), &[("no_such_tool", " ".to_string())]),
            reply(Some("done"), &[]),
        ]);
        let settings = config(script.clone());
        let mut messages = plan_transcript();

        assert!(matches!(
            run_agent_loop(&mut messages, &settings, Phase::Code, Source::None),
            Stop::TurnEnd
        ));
        assert_eq!(script.requests(), 5);
        for id in ["call-0", "call-10", "call-20"] {
            assert_no_redirect(&tool_result_for(&messages, id).expect("the call was rejected"));
        }
        let fourth = tool_result_for(&messages, "call-30").expect("the fourth turn was rejected");
        assert!(fourth.ends_with(&redirect_notice(" ")));
    }

    /// Minimum valid boundary: one tool call is a new signature, so it dispatches
    /// normally and its result carries no redirect.
    #[test]
    fn when_one_tool_call_is_received_given_a_new_signature_then_dispatches_normally() {
        let script = Script::new(vec![
            reply(
                Some("running"),
                &[(
                    "execute_script",
                    serde_json::json!({ "code": "return 40 + 2;" }).to_string(),
                )],
            ),
            reply(Some("done"), &[]),
        ]);
        let settings = config(script.clone());
        let mut messages = plan_transcript();

        assert!(matches!(
            run_agent_loop(&mut messages, &settings, Phase::Code, Source::None),
            Stop::TurnEnd
        ));
        assert_eq!(script.requests(), 2);
        let result = tool_result_for(&messages, "call-0").expect("the call ran");
        assert_eq!(result, "42");
        assert_no_redirect(&result);
    }

    /// Upper boundary: the redirect keeps the ordered canonical-arguments
    /// preview within 500 UTF-8 bytes, stays at the tail of a capped result, and
    /// never pushes the delivered result over TOOL_RESULT_BYTES.
    #[test]
    fn when_redirect_is_added_given_a_large_multibyte_tool_result_then_keeps_notice_and_both_byte_limits()
     {
        let huge = "€".repeat(300);
        let code = format!("'{huge}'.repeat(2000)");
        let arguments = serde_json::json!({ "code": code }).to_string();
        let script = Script::new(vec![
            reply_numbered(0, Some("one"), &[("execute_script", arguments.clone())]),
            reply_numbered(10, Some("two"), &[("execute_script", arguments.clone())]),
            reply_numbered(20, Some("three"), &[("execute_script", arguments.clone())]),
            reply(Some("done"), &[]),
        ]);
        let settings = config(script.clone());
        let mut messages = plan_transcript();

        assert!(matches!(
            run_agent_loop(&mut messages, &settings, Phase::Code, Source::None),
            Stop::TurnEnd
        ));
        assert_eq!(script.requests(), 4);
        let third = tool_result_for(&messages, "call-20").expect("the third turn ran");
        assert!(
            third.len() <= TOOL_RESULT_BYTES,
            "the annotated result stays within the delivery cap: {} bytes",
            third.len()
        );
        let start = third
            .rfind(REPEATED_CALL_REDIRECT_HEADER)
            .expect("the redirect notice is present");
        let preview = &third[start + REPEATED_CALL_REDIRECT_HEADER.len()..];
        let expected_preview = format!("{{\"code\":\"'{}", "€".repeat(163));
        assert_eq!(
            preview, expected_preview,
            "the preview is the ordered canonical arguments cut at a UTF-8 boundary"
        );
        assert!(
            preview.len() <= 500,
            "the preview is at most 500 bytes, got {}",
            preview.len()
        );
        assert!(
            preview.ends_with('€'),
            "the cut lands on a code-point boundary inside the multibyte run"
        );
        assert!(
            third.ends_with(&redirect_notice(&expected_preview)),
            "the notice stays intact at the end of the capped result"
        );
        assert!(
            third.len() > TOOL_RESULT_BYTES - 1_000,
            "the result is still near the cap it was cut from, got {} bytes",
            third.len()
        );
    }

    /// Regression: the redirect warning is emitted even when a repeating turn
    /// has no tool result to annotate, and 401/403 provider failures stay API
    /// failures instead of entering repetition processing.
    #[test]
    fn when_repeated_turn_has_no_tool_result_given_the_warning_threshold_then_warns_without_annotation()
     {
        if is_timing_child() {
            start_timing_child();
            let arguments = r#"{"code":"return 'same';"}"#.to_string();
            let mut bodies = Vec::new();
            for turn in 0..5 {
                bodies.push(reply_numbered(
                    turn * 10,
                    Some("working"),
                    &[("execute_script", arguments.clone())],
                ));
            }
            let script = Script::new(bodies);
            let settings = config(script.clone());
            let mut messages = plan_transcript();

            assert!(matches!(
                run_agent_loop(&mut messages, &settings, Phase::Code, Source::None),
                Stop::Failed(FailureKind::StepLimit)
            ));
            assert_eq!(script.requests(), 5);
            assert_eq!(assistant_tool_steps(&messages).len(), 4);
            let third = tool_result_for(&messages, "call-20").expect("the third turn ran");
            assert!(third.ends_with(&redirect_notice(r#"{"code":"return 'same';"}"#)));
            assert!(
                tool_result_for(&messages, "call-40").is_none(),
                "the stopped fifth turn has no result to annotate"
            );
            return;
        }

        let transcript = timing_transcript(
            "when_repeated_turn_has_no_tool_result_given_the_warning_threshold_then_warns_without_annotation",
        );
        assert_eq!(
            transcript.matches(REPEATED_CALL_WARNING).count(),
            1,
            "the redirect warning is emitted even when no result exists to annotate:\n{transcript}"
        );
        assert_eq!(
            transcript.matches(REPEATED_CALL_ABORT_WARNING).count(),
            1,
            "the abort warning is emitted:\n{transcript}"
        );

        // Regression: unauthorized provider failures bypass repetition handling.
        for status in [401, 403] {
            let script = Script::with_errors(vec![CallError::Api {
                status,
                message: format!("denied {status}"),
            }]);
            let settings = config(script.clone());
            let mut messages = plan_transcript();
            let original = messages.clone();

            assert!(matches!(
                run_agent_loop(&mut messages, &settings, Phase::Code, Source::None),
                Stop::Failed(FailureKind::Api)
            ));
            assert_eq!(script.requests(), 1, "HTTP {status} is not retried");
            assert_eq!(
                serde_json::to_value(&messages).unwrap(),
                serde_json::to_value(&original).unwrap(),
                "an API failure never enters repetition processing"
            );
        }
    }

    #[test]
    fn when_summarizing_given_transient_503_on_first_summary_attempt_then_retries_and_succeeds() {
        let script = Script::with_replies(vec![
            Reply::Error(CallError::Api {
                status: 503,
                message: "service temporarily unavailable".to_string(),
            }),
            Reply::Body(reply(Some(HANDOVER_REPLY), &[])),
            Reply::Body(reply(Some("done"), &[])),
        ]);
        let settings = config(script.clone());
        let mut messages = summary_history();

        assert!(matches!(
            run_agent_loop(&mut messages, &settings, Phase::Code, Source::None),
            Stop::TurnEnd
        ));
        assert_eq!(
            script.requests(),
            3,
            "one failed summary attempt, one retried summary attempt, then one ordinary request"
        );
    }

    #[test]
    fn when_summarizing_given_prior_handover_checkpoint_then_instruction_requires_single_consolidated_summary()
     {
        assert!(HANDOVER_SUMMARY_INSTRUCTION.contains(HANDOVER_SUMMARY_MARKER));
        assert!(HANDOVER_SUMMARY_INSTRUCTION.contains("consolidated summary"));
        for label in HANDOVER_SUMMARY_LABELS {
            assert!(HANDOVER_SUMMARY_INSTRUCTION.contains(label));
        }
    }

    #[test]
    fn when_sending_turn_given_http_400_context_length_exceeded_below_watermark_then_compacts_and_retries()
     {
        let script = Script::with_replies(vec![
            Reply::Error(CallError::Api {
                status: 400,
                message: "context_length_exceeded: maximum context length is 32768 tokens"
                    .to_string(),
            }),
            Reply::Body(reply(Some("done"), &[])),
        ]);
        let settings = config(script.clone());
        let mut messages = tier_one_history(8, 16 * 1024);
        assert!(
            context_bytes(&settings, &messages, Phase::Code) <= TIER_ONE_CONTEXT_BYTES,
            "the transcript starts below the 400 KiB Tier 1 watermark"
        );

        assert!(matches!(
            run_agent_loop(&mut messages, &settings, Phase::Code, Source::None),
            Stop::TurnEnd
        ));
        assert_eq!(
            script.requests(),
            2,
            "the overflowed turn is compacted and retried once"
        );
        let retried = recorded_messages(&script.sent()[1], "messages");
        assert!(
            retried.iter().any(|message| {
                message.role == Role::Tool
                    && message.content.as_deref().is_some_and(is_tool_result_stub)
            }),
            "older tool results were stubbed before the retry"
        );
    }

    #[test]
    fn when_sending_turn_given_missing_reasoning_content_after_tier_one_then_summarizes_middle_and_retries()
     {
        let script = Script::with_replies(vec![
            Reply::Error(CallError::Api {
                status: 400,
                message:
                    "Missing reasoning_content field in the assistant message at message index 2"
                        .to_string(),
            }),
            Reply::Body(reply(Some(HANDOVER_REPLY), &[])),
            Reply::Body(reply(Some("done"), &[])),
        ]);
        let settings = config(script.clone());
        let mut messages = pad_to_context(
            &settings,
            tier_one_history(8, 150 * 1024),
            TIER_ONE_CONTEXT_BYTES + 1,
            Phase::Code,
        );

        assert!(matches!(
            run_agent_loop(&mut messages, &settings, Phase::Code, Source::None),
            Stop::TurnEnd
        ));
        assert_eq!(
            script.requests(),
            3,
            "Tier 1 send rejected by DeepSeek, one summary request, then one ordinary retry"
        );
        let retried = recorded_messages(&script.sent()[2], "messages");
        assert_eq!(retried[2].content.as_deref(), Some(HANDOVER_REPLY));
        for message in &retried[3..] {
            if message.role == Role::Assistant {
                assert!(
                    message.reasoning_content.is_some(),
                    "every retained tool-call assistant message after the summary keeps reasoning_content"
                );
            }
        }
    }

    #[test]
    fn when_loop_finishes_given_ejaj_session_dir_then_writes_numbered_traj_json_under_trajectories()
    {
        let root = std::env::temp_dir().join(format!(
            "ejaj-traj-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let session_dir = root.join(".ejaj").join("20261108-190000");
        let script = Script::new(vec![
            reply(None, &[("task_complete", "{}".to_string())]),
            reply(Some("stopped without task_complete"), &[]),
        ]);
        let settings = config(script);

        let mut first_messages = vec![
            Message::system("system prompt"),
            Message::user("first ticket"),
        ];
        let first_stop = run_agent_loop_with_context(
            &mut first_messages,
            &settings,
            Phase::Code,
            Source::None,
            InvocationContext::new(&root, &session_dir),
        );
        assert!(matches!(first_stop, Stop::Complete));

        let mut second_messages = vec![
            Message::system("system prompt"),
            Message::user("second attempt"),
        ];
        let second_stop = run_agent_loop_with_context(
            &mut second_messages,
            &settings,
            Phase::Code,
            Source::None,
            InvocationContext::new(&root, &session_dir),
        );
        assert!(matches!(second_stop, Stop::TurnEnd));

        let traj_dir = session_dir.join("trajectories");
        let first_path = traj_dir.join("code.traj.json");
        let second_path = traj_dir.join("code-1.traj.json");
        assert!(first_path.is_file(), "first trajectory file must exist");
        assert!(second_path.is_file(), "second trajectory file must exist");
        assert!(
            root.join(".ejaj").join(".gitignore").is_file(),
            ".ejaj/.gitignore must be prepared"
        );

        let first_json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&first_path).unwrap()).unwrap();
        assert_eq!(first_json["phase"], "code");
        assert_eq!(first_json["role"], "mid");
        assert_eq!(first_json["model"], "test-model");
        assert_eq!(first_json["steps_taken"], 1);
        assert_eq!(first_json["exit_status"], "complete");
        assert!(first_json["messages"].is_array());

        let second_json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&second_path).unwrap()).unwrap();
        assert_eq!(second_json["phase"], "code");
        assert_eq!(second_json["steps_taken"], 1);
        assert_eq!(second_json["exit_status"], "turn_end");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn when_loop_finishes_given_no_session_dir_or_plan_turn_end_then_writes_no_trajectory_file() {
        let root = std::env::temp_dir().join(format!(
            "ejaj-traj-skip-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let ejaj_session = root.join(".ejaj").join("plan-turn-end");
        let non_ejaj_session = root.join("artifacts");
        let script = Script::new(vec![
            reply(Some("What scope should we use?"), &[]),
            reply(Some("done"), &[]),
        ]);
        let settings = config(script);

        let mut plan_messages = vec![Message::system("plan"), Message::user("ask")];
        let plan_stop = run_agent_loop_with_context(
            &mut plan_messages,
            &settings,
            Phase::Plan,
            Source::None,
            InvocationContext::new(&root, &ejaj_session),
        );
        assert!(matches!(plan_stop, Stop::TurnEnd));
        assert!(
            !ejaj_session.exists(),
            "a plan turn end must not create the session directory"
        );

        let mut code_messages = vec![Message::system("code"), Message::user("work")];
        let code_stop = run_agent_loop_with_context(
            &mut code_messages,
            &settings,
            Phase::Code,
            Source::None,
            InvocationContext::new(&root, &non_ejaj_session),
        );
        assert!(matches!(code_stop, Stop::TurnEnd));
        assert!(
            !non_ejaj_session.join("trajectories").exists(),
            "a session directory outside .ejaj/ must not get trajectories/"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// One assistant tool call for the activity-trail tests.
    fn activity_call(id: &str, name: &str, arguments: &str) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            kind: "function".to_string(),
            function: FunctionCall {
                name: name.to_string(),
                arguments: arguments.to_string(),
            },
        }
    }

    /// One assistant message issuing calls in order.
    fn activity_assistant(calls: Vec<ToolCall>) -> Message {
        Message {
            role: Role::Assistant,
            content: Some("calling".to_string()),
            tool_calls: Some(calls),
            tool_call_id: None,
            reasoning_content: None,
            extra: serde_json::Map::new(),
            truncated: false,
        }
    }

    /// One tool result addressed to an id; a None content is the provider's
    /// empty-result shape.
    fn activity_result(id: &str, content: Option<&str>) -> Message {
        Message {
            role: Role::Tool,
            content: content.map(str::to_string),
            tool_calls: None,
            tool_call_id: Some(id.to_string()),
            reasoning_content: None,
            extra: serde_json::Map::new(),
            truncated: false,
        }
    }

    /// The activity trail for one assistant call paired with one result.
    fn single_activity(name: &str, arguments: &str, result: Option<&str>) -> Option<String> {
        summarize_tool_activity(&[
            activity_assistant(vec![activity_call("call-only", name, arguments)]),
            activity_result("call-only", result),
        ])
    }

    /// Core entry point: an outcome-only attempt carries no narration and no
    /// tool activity, whether it was built directly or converted from an
    /// outcome, while the code session's ordinary entry point still returns
    /// only the outcome.
    #[test]
    fn when_constructing_given_outcome_only_result_then_leaves_attempt_activity_empty() {
        use crate::loops::code::{Attempt, Outcome, Session};

        for outcome in [
            Outcome::Complete,
            Outcome::TurnEnd,
            Outcome::Failed(FailureKind::StepLimit),
        ] {
            let direct = Attempt::new(outcome);
            assert_eq!(direct.outcome, outcome);
            assert_eq!(direct.narration, None);
            assert_eq!(direct.tool_activity, None);

            let converted: Attempt = outcome.into();
            assert_eq!(converted.outcome, outcome);
            assert_eq!(converted.narration, None);
            assert_eq!(converted.tool_activity, None);
        }

        let script = Script::new(vec![reply(Some("finished"), &[])]);
        let config = config(script.clone());
        let session = Session::new(&config, "scope");
        let outcome: Outcome = session.run_ticket("do the work");
        assert_eq!(outcome, Outcome::TurnEnd);
        assert_eq!(script.requests(), 1);
    }

    /// Edge case: transcripts with no delivered pair -- empty, tool-free,
    /// unanswered, mismatched, and result-before-call -- carry no trail.
    #[test]
    fn when_summarizing_given_calls_without_matching_deliveries_then_returns_no_trail() {
        assert_eq!(summarize_tool_activity(&[]), None);
        assert_eq!(
            summarize_tool_activity(&[Message::system("sys"), Message::assistant("hello")]),
            None
        );

        let unanswered = vec![
            activity_assistant(vec![activity_call(
                "call-a",
                "bash",
                r#"{"command":"printf a"}"#,
            )]),
            activity_result("call-b", Some("a")),
        ];
        assert_eq!(summarize_tool_activity(&unanswered), None);

        let unpaired_handoff = vec![activity_assistant(vec![activity_call(
            "call-plan",
            "finish_plan",
            r##"{"plan":"# Plan"}"##,
        )])];
        assert_eq!(summarize_tool_activity(&unpaired_handoff), None);

        let result_before_call = vec![
            activity_result("call-a", Some("ok")),
            activity_assistant(vec![activity_call(
                "call-a",
                "bash",
                r#"{"command":"printf a"}"#,
            )]),
        ];
        assert_eq!(summarize_tool_activity(&result_before_call), None);
    }

    /// Edge case: calls pair by exact id in call order, not by result order,
    /// unanswered calls in the same group are skipped, and only the last three
    /// delivered pairs survive in chronological order.
    #[test]
    fn when_summarizing_given_mixed_tool_deliveries_then_keeps_last_three_in_chronological_order() {
        let transcript = vec![
            activity_assistant(vec![
                activity_call("call-a", "bash", r#"{"command":"printf a"}"#),
                activity_call("call-b", "execute_script", r#"{"code":"return \"b\";"}"#),
                activity_call("call-x", "bash", r#"{"command":"printf x"}"#),
            ]),
            activity_result("call-b", Some("b")),
            activity_result("call-a", Some("a")),
            activity_assistant(vec![activity_call(
                "call-c",
                "bash",
                r#"{"command":"printf c"}"#,
            )]),
            activity_result("call-c", Some("c")),
            activity_assistant(vec![
                activity_call("call-d", "bash", r#"{"command":"printf d"}"#),
                activity_call("call-e", "bash", r#"{"command":"printf e"}"#),
            ]),
            activity_result("call-e", Some("e")),
            activity_result("call-d", Some("d")),
        ];

        assert_eq!(
            summarize_tool_activity(&transcript).as_deref(),
            Some("- bash(printf c) -> ok\n- bash(printf d) -> ok\n- bash(printf e) -> ok")
        );
    }

    /// Regression: undelivered calls -- including an aborted repeated-call turn
    /// and an accepted control-tool handoff -- never enter the trail, and a
    /// real attempt carries the delivered activity beside its stop and
    /// narration.
    #[test]
    fn when_summarizing_given_unanswered_or_aborted_calls_then_excludes_undelivered_activity() {
        let transcript = vec![
            activity_assistant(vec![activity_call(
                "call-delivered",
                "bash",
                r#"{"command":"printf delivered"}"#,
            )]),
            activity_result("call-delivered", Some("delivered")),
            activity_assistant(vec![activity_call(
                "call-aborted",
                "bash",
                r#"{"command":"printf aborted"}"#,
            )]),
            activity_assistant(vec![activity_call("call-handoff", "task_complete", "{}")]),
        ];
        assert_eq!(
            summarize_tool_activity(&transcript).as_deref(),
            Some("- bash(printf delivered) -> ok")
        );

        let arguments = serde_json::json!({ "code": "return 1;" }).to_string();
        let script = Script::new(vec![
            reply(None, &[("execute_script", arguments)]),
            reply(Some("done"), &[]),
        ]);
        let settings = config(script.clone());
        let mut messages = vec![Message::system("sys"), Message::user("work")];
        let attempt =
            run_agent_loop_attempt(&mut messages, &settings, Phase::Code, Source::None, None);
        assert!(matches!(attempt.stop, Stop::TurnEnd));
        assert_eq!(attempt.narration.as_deref(), Some("done"));
        assert_eq!(
            attempt.tool_activity.as_deref(),
            Some("- execute_script(return 1;) -> ok")
        );

        let arguments = serde_json::json!({ "code": "return 2;" }).to_string();
        let script = Script::new(vec![
            reply(None, &[("execute_script", arguments)]),
            reply(Some("finished"), &[]),
        ]);
        let settings = config(script.clone());
        let session = crate::loops::code::Session::new(&settings, "scope");
        let attempt = session.run_ticket_attempt("do the work", None, None);
        assert_eq!(attempt.outcome, crate::loops::code::Outcome::TurnEnd);
        assert_eq!(attempt.narration.as_deref(), Some("finished"));
        assert_eq!(
            attempt.tool_activity.as_deref(),
            Some("- execute_script(return 2;) -> ok")
        );
    }

    /// Edge case table for call previews: known Bash and execute-script
    /// strings unwrap and collapse, every unsupported shape falls back to the
    /// raw arguments, and the parse, fallback and preview byte bounds never
    /// split a UTF-8 character.
    #[test]
    fn when_summarizing_given_tool_arguments_at_boundary_sizes_then_formats_bounded_previews() {
        assert_eq!(
            single_activity(
                "bash",
                "{\"command\":\"printf\\n   hello\\tworld\"}",
                Some("ok"),
            )
            .as_deref(),
            Some("- bash(printf hello world) -> ok")
        );
        assert_eq!(
            single_activity("execute_script", "{\"code\":\"return\\n  1;\"}", Some("ok"),)
                .as_deref(),
            Some("- execute_script(return 1;) -> ok")
        );

        assert_eq!(
            single_activity("bash", "not json", Some("ok")).as_deref(),
            Some("- bash(not json) -> ok")
        );
        assert_eq!(
            single_activity("bash", r#"{"cmd":"x"}"#, Some("ok")).as_deref(),
            Some("- bash({\"cmd\":\"x\"}) -> ok")
        );
        assert_eq!(
            single_activity("bash", r#"{"command":42}"#, Some("ok")).as_deref(),
            Some("- bash({\"command\":42}) -> ok")
        );

        for arguments in ["", "   \n ", "{}"] {
            assert_eq!(
                single_activity("bash", arguments, Some("ok")).as_deref(),
                Some("- bash() -> ok"),
                "arguments {arguments:?} render as an empty call"
            );
        }

        let prefix = r#"{"command":"hi""#;
        let at_limit = format!("{prefix}{}{}", " ".repeat(16_384 - prefix.len() - 1), "}");
        assert_eq!(at_limit.len(), 16_384);
        assert_eq!(
            single_activity("bash", &at_limit, Some("ok")).as_deref(),
            Some("- bash(hi) -> ok")
        );

        let over_limit = format!("{prefix}{}{}", " ".repeat(16_385 - prefix.len() - 1), "}");
        assert_eq!(over_limit.len(), 16_385);
        assert_eq!(
            single_activity("bash", &over_limit, Some("ok")).as_deref(),
            Some("- bash({\"command\":\"hi\") -> ok")
        );

        let whitespace_then_text = format!("{}after", " ".repeat(600));
        assert_eq!(
            single_activity("bash", &whitespace_then_text, Some("ok")).as_deref(),
            Some("- bash() -> ok"),
            "only the first 512 raw bytes are inspected before whitespace collapses"
        );

        let multibyte_at_fallback_edge = format!("{}{}", " ".repeat(511), "€".repeat(10));
        assert_eq!(
            single_activity("bash", &multibyte_at_fallback_edge, Some("ok")).as_deref(),
            Some("- bash() -> ok"),
            "the 512-byte fallback cut never splits a UTF-8 character"
        );

        let exact_preview = format!("bash({})", "a".repeat(154));
        assert_eq!(exact_preview.len(), 160);
        assert_eq!(
            single_activity(
                "bash",
                &serde_json::json!({ "command": "a".repeat(154) }).to_string(),
                Some("ok"),
            )
            .as_deref(),
            Some(format!("- {exact_preview} -> ok").as_str())
        );

        let over_preview = format!("bash({})", "a".repeat(155));
        assert_eq!(over_preview.len(), 161);
        let bounded_preview = format!("bash({}…", "a".repeat(152));
        assert_eq!(
            single_activity(
                "bash",
                &serde_json::json!({ "command": "a".repeat(155) }).to_string(),
                Some("ok"),
            )
            .as_deref(),
            Some(format!("- {bounded_preview} -> ok").as_str())
        );

        let multibyte_preview = format!("bash({}…", "€".repeat(50));
        assert_eq!(multibyte_preview.len(), 158);
        assert_eq!(
            single_activity(
                "bash",
                &serde_json::json!({ "command": "€".repeat(100) }).to_string(),
                Some("ok"),
            )
            .as_deref(),
            Some(format!("- {multibyte_preview} -> ok").as_str())
        );
    }

    /// Contract and invariant table for result statuses: empty and stub
    /// results, both error forms, exit/timeout diagnostics, trailing warnings,
    /// spill footers, and the UTF-8-safe 240-byte status and 1,500-byte trail
    /// limits.
    #[test]
    fn when_summarizing_given_tool_results_with_errors_warnings_and_spills_then_preserves_bounded_statuses()
     {
        for content in [None, Some(""), Some("   \n  ")] {
            assert_eq!(
                single_activity("bash", r#"{"command":"printf x"}"#, content).as_deref(),
                Some("- bash(printf x) -> ok")
            );
        }

        let stub = "  [tool result omitted: bash target printf x; 1234 bytes omitted]  ";
        assert_eq!(
            single_activity("bash", r#"{"command":"printf x"}"#, Some(stub)).as_deref(),
            Some(
                "- bash(printf x) -> [tool result omitted: bash target printf x; 1234 bytes omitted]"
            )
        );

        for (content, status) in [
            (
                "[ERROR: execute_script failed]\nError: boom",
                "[ERROR: execute_script failed]: Error: boom",
            ),
            (
                "[ERROR: unexpected failure]\nsecond line",
                "[ERROR: unexpected failure]",
            ),
            ("Error: plain failure\nmore detail", "Error: plain failure"),
            ("[ERROR] tagged failure", "tagged failure"),
        ] {
            assert_eq!(
                single_activity("bash", r#"{"command":"printf x"}"#, Some(content)).as_deref(),
                Some(format!("- bash(printf x) -> {status}").as_str())
            );
        }

        let footer = "[full output saved to .ejaj/run/tools-output/step-9-call-1.log; inspect it with tools.read through execute_script]";
        let exit_with_diagnostics = format!(
            "[ERROR: exit code 101 for cargo test]\nstartup banner\n[diagnostics]\nerror[E0425]: cannot find value x\ntail\n{footer}\n[WARN: step 99/100; 1 steps remaining before the step limit]"
        );
        let trail = single_activity(
            "bash",
            r#"{"command":"printf x"}"#,
            Some(&exit_with_diagnostics),
        )
        .expect("the delivered pair keeps its trail");
        assert_eq!(
            trail,
            format!(
                "- bash(printf x) -> [ERROR: exit code 101 for cargo test] — error[E0425]: cannot find value x {footer}"
            )
        );
        assert_eq!(
            trail.matches(footer).count(),
            1,
            "the spill footer is preserved exactly once: {trail:?}"
        );

        let timed_out = "[ERROR: exit code -1 for sleep 1]\ncommand timed out after 3s\n[WARN: repeated tool calls] Your last turns issued the same tool calls.";
        assert_eq!(
            single_activity("bash", r#"{"command":"printf x"}"#, Some(timed_out)).as_deref(),
            Some(
                "- bash(printf x) -> [ERROR: exit code -1 for sleep 1] — command timed out after 3s"
            )
        );

        for warning in [
            "[WARN: step 99/100; 1 steps remaining before the step limit]",
            "[WARN: repeated tool calls] Your last turns issued the same tool calls.",
        ] {
            let content = format!("command output\n{warning}");
            assert_eq!(
                single_activity("bash", r#"{"command":"printf x"}"#, Some(&content)).as_deref(),
                Some("- bash(printf x) -> ok"),
                "the trailing loop warning never enters the status"
            );
        }

        let successful_spill = format!("command output\n{footer}");
        let trail = single_activity("bash", r#"{"command":"printf x"}"#, Some(&successful_spill))
            .expect("the delivered pair keeps its trail");
        assert_eq!(trail, format!("- bash(printf x) -> ok {footer}"));
        assert_eq!(trail.matches(footer).count(), 1);

        let multibyte_error = format!("Error: {}", "€".repeat(200));
        assert_eq!(
            single_activity("bash", r#"{"command":"printf x"}"#, Some(&multibyte_error)).as_deref(),
            Some(format!("- bash(printf x) -> Error: {}…", "€".repeat(76)).as_str())
        );

        let diagnostic_head = "[ERROR: exit code 1 for x] — ";
        let diagnostic_full = format!("{diagnostic_head}{}", "d".repeat(500));
        let error_with_footer = format!("{diagnostic_full}\n{footer}");
        let expected_head = format!("{diagnostic_head}{}…", "d".repeat(91));
        assert_eq!(
            single_activity(
                "bash",
                r#"{"command":"printf x"}"#,
                Some(&error_with_footer)
            )
            .as_deref(),
            Some(format!("- bash(printf x) -> {expected_head} {footer}").as_str())
        );

        let long_footer = format!("[full output saved to {}]", "p".repeat(300));
        let error_with_long_footer = format!("Error: boom\n{long_footer}");
        let expected_footer = format!("{}…", &long_footer[..237]);
        assert_eq!(
            single_activity(
                "bash",
                r#"{"command":"printf x"}"#,
                Some(&error_with_long_footer),
            )
            .as_deref(),
            Some(format!("- bash(printf x) -> {expected_footer}").as_str())
        );

        let long_stub = format!("[tool result omitted: x{}", "€".repeat(600));
        let trail = single_activity("bash", r#"{"command":"printf x"}"#, Some(&long_stub))
            .expect("the delivered pair keeps its trail");
        let expected_trail = format!(
            "- bash(printf x) -> [tool result omitted: x{}…",
            "€".repeat(484)
        );
        assert_eq!(trail, expected_trail);
        assert!(
            trail.len() <= 1_500,
            "the complete trail is {} bytes",
            trail.len()
        );
    }
}
