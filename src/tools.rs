//! Tools the agent can run: their argument types, their OpenAI-compatible
//! definitions, and their dispatch.
//!
//! Every tool has one argument struct (`Serialize + Deserialize + JsonSchema`),
//! and the schema the API receives is derived from that struct with
//! `schemars::schema_for!`, so the definition sent to the model and the
//! arguments it parses cannot drift apart. The command tools wrap their
//! arguments directly; the handoff tools take a typed payload on the wire and
//! hold the parsed document the harness keeps from it.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use crate::api::{FunctionDefinition, ToolDefinition};
use crate::plan_document::Plan;
use crate::review::ReviewReport;
use crate::script::{Capabilities, QuickJsEngine, ScriptEngine, format_script_error};
use crate::specification::Specification;
use crate::tickets::TicketSet;
use crate::triage::TriageReport;

/// The maximum UTF-8 byte length of any tool result delivered to the model.
pub const TOOL_OUTPUT_LIMIT_BYTES: usize = crate::script::OUTPUT_LIMIT_BYTES;

/// The agent loop's step budget. The Bash digest reserves room for the
/// step-budget warning the loop appends near this boundary.
pub const MAX_STEPS: usize = 100;

/// Remaining-step threshold at or below which the loop appends a step-budget
/// warning to every tool result.
pub const STEP_WARN_REMAINING: usize = 10;

/// The head bytes a spilled Bash digest keeps.
const BASH_DIGEST_HEAD_BYTES: usize = 4 * 1024;
/// The tail bytes a spilled Bash digest keeps.
const BASH_DIGEST_TAIL_BYTES: usize = 4 * 1024;
/// The most diagnostic bytes a spilled Bash digest carries.
const BASH_DIGEST_DIAGNOSTIC_BYTES: usize = 8 * 1024;
/// The room a spilled Bash digest leaves for the loop's warning when applicable.
const STEP_WARNING_RESERVE_BYTES: usize = 128;
/// The directory every complete raw Bash log is spilled into, under an
/// explicitly supplied session directory.
const SPILL_DIRECTORY: &str = "tools-output";
/// The most bytes one spill failure footer may carry.
const SPILL_FAILURE_FOOTER_BYTES: usize = 512;
/// The most bytes of a failing command an error prefix may quote. A long
/// command must not push the complete failure result over the delivery cap.
const BASH_FAILURE_COMMAND_BYTES: usize = 512;

/// The deadline for one shell command. Both the model's `bash` tool and the
/// verification gate run through `shell_output_in`, so this single bound keeps
/// neither path waiting indefinitely.
pub const SHELL_TIMEOUT: Duration = Duration::from_secs(120);

/// The fixed pause between `try_wait` checks: short enough to notice a finished
/// command promptly, long enough not to spin a core.
const SHELL_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// The public identity of one dispatch through the agent loop: the one-based
/// model step and the one-based index of this Bash call within that step. The
/// pair names the spill file for oversized output, so two calls -- or two
/// runs -- never overwrite each other's complete log. The invocation's
/// optional session directory travels beside this context, never inside it, so
/// the context stays `Copy` and a context-free dispatch keeps its old shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolContext {
    pub step: usize,
    pub call_index: usize,
}

impl Default for ToolContext {
    fn default() -> Self {
        Self {
            step: 1,
            call_index: 1,
        }
    }
}

/// Which agent is talking to the model. The five agents are offered different
/// tools: only the plan agent can hand off the plan, only the spec agent can
/// hand over the specification, only the tickets agent can hand over a ticket
/// set, and only the code agent can declare a ticket complete. The summary
/// agent is offered no control tool at all: that pass ends when the model stops
/// calling tools and its final text is the summary. The phase is the single
/// source of truth for both [`Tool::definitions`] and [`Tool::parse`], so a tool
/// the model was never offered cannot silently succeed either. `Phase::Triage`
/// and `Phase::Summary` are read-only inspection passes: they are offered
/// `execute_script` and never `bash`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Plan,
    Spec,
    Tickets,
    /// The read-only triage pass over the draft ticket set: it runs after the
    /// tickets handoff and before the ticket queue, and it classifies the draft
    /// tickets the tickets agent handed over.
    Triage,
    Code,
    /// The read-only review pass after a resolved ticket-queue round.
    Review,
    /// The read-only change-summary pass. It inspects the repository through
    /// `execute_script` and ends by writing the summary itself, so it offers no
    /// tool that could complete, hand off or ask anything.
    Summary,
}

impl Phase {
    /// The canonical lowercase label a report names this phase by:
    /// `plan`, `spec`, `tickets`, `triage`, `code`, `review` or
    /// `summary`. It is deliberately independent of the enum's debug spelling,
    /// so no Rust identifier reaches a user-visible report.
    pub fn label(self) -> &'static str {
        match self {
            Phase::Plan => "plan",
            Phase::Spec => "spec",
            Phase::Tickets => "tickets",
            Phase::Triage => "triage",
            Phase::Code => "code",
            Phase::Review => "review",
            Phase::Summary => "summary",
        }
    }
}

/// The typed upstream object a pass validates its control-tool handoff
/// against.
///
/// The specification pass is handed the confirmed [`Plan`] and the tickets
/// pass is handed the typed [`Specification`]; the shared tool-validation path
/// then checks that the handoff still accounts for that source before it is
/// accepted. The triage pass is handed both halves of its boundary -- the
/// [`Specification`] the draft set was sliced from and the original
/// [`TicketSet`] -- because a triage report is validated against the draft
/// tickets themselves. The plan interview and the code pass have no upstream document to
/// preserve -- their control tools carry no payload to check -- so they pass
/// [`Source::None`], and a pass ignores a source that belongs to another phase
/// rather than misreading it.
#[derive(Debug, Clone, Copy)]
pub enum Source<'a> {
    /// Nothing to cover: the plan interview (`finish_plan`) and the code pass
    /// (`task_complete`).
    None,
    /// The confirmed plan whose decisions the specification pass must cover.
    Plan(&'a Plan),
    /// The typed specification whose stories the tickets pass must cover.
    Specification(&'a Specification),
    /// The typed triage context: the specification the draft set was sliced
    /// from and the original draft tickets a report must classify exactly once.
    Triage {
        specification: &'a Specification,
        tickets: &'a TicketSet,
    },
    /// The ticket ids already executed in this run, which fix tickets emitted by
    /// `finish_review` must not duplicate.
    Review(&'a [String]),
}

/// Arguments of the `bash` tool.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct BashArgs {
    /// The bash command to run.
    pub command: String,
}

/// Arguments of the `execute_script` tool.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ExecuteScriptArgs {
    /// The JavaScript program to run in the project sandbox.
    pub code: String,
}

/// Arguments of the `finish_plan` tool: the agreed plan as a markdown document.
///
/// The plan is a required argument because the spec agent never sees the
/// interview that produced it. Nothing is recovered from the transcript, so
/// whatever the plan agent writes here is exactly -- and only -- what the
/// spec agent is handed. The harness parses the document back into a [`Plan`]
/// with [`Plan::from_markdown`], so the typed value -- not the model's text --
/// is what crosses the harness boundary.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct FinishPlanArgs {
    /// The complete plan as a markdown document, following the template in the system prompt.
    pub plan: String,
}

/// Arguments of the `finish_spec` tool: the specification as a markdown document.
///
/// The specification is a required argument because the tickets agent never sees the plan or the
/// spec pass that produced it: it receives this document and nothing else. The harness parses the
/// document back into a [`Specification`] with [`Specification::from_markdown`], so the typed
/// value -- not the model's text -- is what crosses the harness boundary.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct FinishSpecArgs {
    /// The complete specification as a markdown document, following the template in the system
    /// prompt.
    pub spec: String,
}

/// Arguments of the `finish_tickets` tool: the whole ticket set the tickets agent hands over.
///
/// The wire form is the typed object, not a markdown document, so there is no reverse parser to
/// keep in step with the ticket renderer. The single required field holds the set; every ticket
/// it contains is handed to the code agent one at a time.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct FinishTicketsArgs {
    /// The whole set of tickets, in implementation order. Every ticket it contains is handed to
    /// the code agent one at a time, and the orchestrator runs each ticket's verification gate
    /// itself.
    pub tickets: TicketSet,
}

/// Arguments of the `finish_triage` tool: the whole triage report for the draft
/// ticket set.
///
/// The wire form is the typed report, not a markdown document, so there is no
/// reverse parser to keep in step. The single required field is the `triage`
/// object that holds the report; every classification it carries is validated
/// against the draft ticket set the pass was handed before the handoff is
/// accepted.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct FinishTriageArgs {
    /// The complete triage report, in the required `triage` object: exactly
    /// one classification per draft ticket, carrying its ticket_id, its
    /// ticket_title, its category, its state, its audit notes and -- depending on
    /// the state -- its Agent Brief or its blocker explanation.
    pub triage: TriageReport,
}

/// Arguments of the `finish_review` tool: the review report for the current round.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct FinishReviewArgs {
    /// The complete review report for this round: its pre-flight audit scorecard,
    /// any findings, and the fix tickets to run (or empty `findings` and `fixes`
    /// when the review passes).
    pub review: ReviewReport,
}

/// Arguments of the `task_complete` tool. Empty on purpose: completion is a
/// statement, not a payload.
///
/// The `properties` extension is load-bearing: `schemars` omits the key for a
/// struct with no fields, and a provider that validates the tool list strictly
/// (LM Studio) rejects a parameter schema without an object `properties` with
/// `parameters.properties: Required`. An explicit empty object keeps
/// `task_complete` as admissible as the tools that do take arguments.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[schemars(extend("properties" = {}))]
pub struct TaskCompleteArgs {}

/// Every tool the agent offers. One variant per tool, holding the already
/// parsed and validated argument payload.
// The specification and ticket-set payloads are large, but they are moved
// straight into the loop's exit value: boxing them would only change the types
// the spec fixes here.
#[allow(clippy::large_enum_variant)]
pub enum Tool {
    Bash(BashArgs),
    ExecuteScript(ExecuteScriptArgs),
    FinishPlan(Plan),
    FinishSpec(Specification),
    FinishTickets(TicketSet),
    FinishTriage(TriageReport),
    FinishReview(ReviewReport),
    TaskComplete(TaskCompleteArgs),
}

impl Tool {
    /// The OpenAI-compatible tool definitions advertised in a request. Each
    /// `parameters` schema is generated from the matching argument struct.
    /// `finish_plan` exists only in [`Phase::Plan`], `finish_spec` only in
    /// [`Phase::Spec`], `finish_tickets` only in [`Phase::Tickets`], and
    /// `task_complete` only in [`Phase::Code`]: a plan agent has nothing to
    /// specify, a spec agent has nothing to complete, and a plan agent that
    /// could "complete" the task would never start it. Advertising a tool an
    /// agent must never call only invites a wasted turn.
    ///
    /// [`Phase::Summary`] is offered `execute_script` and nothing else: it is
    /// a read-only inspection pass, so it has nothing to complete, nothing to
    /// hand over, and no `bash` to run. [`Phase::Triage`] is the other
    /// read-only pass: it is offered `execute_script` and its own
    /// `finish_triage` handoff, and no `bash` either.
    pub fn definitions(phase: Phase) -> Vec<ToolDefinition> {
        let bash = ToolDefinition {
            kind: "function",
            function: FunctionDefinition {
                name: "bash",
                description: "Runs a bash command.",
                parameters: schemars::schema_for!(BashArgs),
            },
        };

        let execute_script = ToolDefinition {
            kind: "function",
            function: FunctionDefinition {
                name: "execute_script",
                description: "Runs a plain JavaScript program in the project sandbox against the \
                              `tools` namespace (`read`, `write`, `replace`, `list`, `grep`, \
                              `diff`) and returns the result (plain text for a string, JSON for \
                              other defined values, or the captured `console.log` output when the \
                              script returns `undefined`). Batch related file reads, searches, and \
                              edits into a single script call rather than making separate calls.",
                parameters: schemars::schema_for!(ExecuteScriptArgs),
            },
        };

        match phase {
            Phase::Plan => vec![
                execute_script,
                ToolDefinition {
                    kind: "function",
                    function: FunctionDefinition {
                        name: "finish_plan",
                        description: "Ends the design interview and hands the agreed plan to the \
                                      spec agent, which turns it into a technical specification \
                                      for the code agent. Call it immediately, in the same turn, \
                                      as soon as no blocking decision remains -- do not summarize \
                                      the plan and wait for a separate confirmation turn, because \
                                      a confirmation of decisions the user has already settled is \
                                      redundant. Ask the user and wait only while a genuinely \
                                      blocking decision is still open; never implement anything \
                                      yourself. The `plan` argument must hold the complete plan as \
                                      a markdown document following the template in the system \
                                      prompt: it is all the spec agent receives.",
                        parameters: schemars::schema_for!(FinishPlanArgs),
                    },
                },
            ],
            Phase::Spec => vec![
                execute_script,
                ToolDefinition {
                    kind: "function",
                    function: FunctionDefinition {
                        name: "finish_spec",
                        description: "Ends the specification pass and hands the technical \
                                      specification to the tickets agent. Call it once the plan has \
                                      been turned into a complete specification -- never implement \
                                      anything yourself. The tickets agent receives the document and \
                                      nothing else, and slices it into the tickets the code agent \
                                      implements; the code agent never sees the specification. The \
                                      `spec` argument must hold the complete specification as a \
                                      markdown document following the template in the system \
                                      prompt: it is all the tickets agent receives.",
                        parameters: schemars::schema_for!(FinishSpecArgs),
                    },
                },
            ],
            Phase::Tickets => vec![
                execute_script,
                ToolDefinition {
                    kind: "function",
                    function: FunctionDefinition {
                        name: "finish_tickets",
                        description: "Ends the tickets pass: the ticket set is the tickets \
                                      agent's handoff to the orchestrator, which writes it and \
                                      runs each ticket's verification gate itself. Emit the \
                                      tickets in implementation order: each ticket is a discrete \
                                      end-to-end slice that one agent can implement, test and \
                                      verify inside a single context window. Every `blocked_by` \
                                      and `blocks` entry must name an id that exists in the set, \
                                      and the set must be acyclic. Because the orchestrator runs \
                                      each verification gate itself, the gate must be a real \
                                      command that exits 0.",
                        parameters: schemars::schema_for!(FinishTicketsArgs),
                    },
                },
            ],
            Phase::Triage => vec![
                execute_script,
                ToolDefinition {
                    kind: "function",
                    function: FunctionDefinition {
                        name: "finish_triage",
                        description: "Ends the triage pass and hands the typed report in the required `triage` object to the \
                                      pipeline. Classify every draft ticket exactly once, \
                                      retaining its id, its title and its dependencies, and \
                                      carry the Agent Brief for every ready-for-agent ticket or \
                                      a blocker explanation for every other state -- never both, \
                                      never neither. The report is an audit beside the ticket \
                                      files, not a replacement for them: the queue still runs \
                                      the original tickets. A rejected payload comes back as a \
                                      tool error, so correct it and call this again; never end \
                                      the triage pass without it, and never ask the user \
                                      anything.",
                        parameters: schemars::schema_for!(FinishTriageArgs),
                    },
                },
            ],
            Phase::Review => vec![
                execute_script,
                ToolDefinition {
                    kind: "function",
                    function: FunctionDefinition {
                        name: "finish_review",
                        description: "Ends the review pass and hands the review report to the \
                                      pipeline. Populate every field of `scorecard`. When the \
                                      test suite and implementation satisfy the specification, \
                                      the tickets, the 4 keeper categories, the 5-dimension \
                                      boundary matrix, edge-only mocking, and \
                                      `when_<action>_given_<context>_then_<outcome>` test naming, \
                                      pass empty `findings` and empty `fixes` so the pipeline \
                                      proceeds to the change summary. When defects remain, list \
                                      each defect in `findings` and emit the ordered `fixes` \
                                      tickets that resolve them; the pipeline will run the fix \
                                      tickets through the queue and return for another review \
                                      round.",
                        parameters: schemars::schema_for!(FinishReviewArgs),
                    },
                },
            ],
            // The summary pass is read-only and ends with its own text, so it
            // is offered the inspection tool and no control tool at all.
            Phase::Summary => vec![execute_script],
            Phase::Code => vec![
                bash,
                execute_script,
                ToolDefinition {
                    kind: "function",
                    function: FunctionDefinition {
                        name: "task_complete",
                        description: "Ends the ticket: call it only once the ticket's verification \
                                      gate has exited 0. You are unattended, so there is no user to \
                                      ask: a turn that ends without this call fails the ticket.",
                        parameters: schemars::schema_for!(TaskCompleteArgs),
                    },
                },
            ],
        }
    }

    /// Match the model's `function.name` to a variant and parse its raw
    /// `arguments` JSON into that tool's struct. Anything wrong here -- a
    /// malformed payload, a missing field, an unknown tool, a tool the current
    /// phase does not offer, or a handoff that dropped upstream content --
    /// comes back as `Err(String)` so the loop can hand it to the model as a
    /// tool result and let it correct itself on the next step.
    ///
    /// `source` is the typed upstream object of the current pass. It is
    /// consulted only by the control tool that owns the phase: `finish_spec`
    /// checks a [`Source::Plan`], `finish_tickets` checks a
    /// [`Source::Specification`] and `finish_triage` checks a
    /// [`Source::Triage`], so no other phase's control tool changes
    /// behaviour.
    pub fn parse(
        name: &str,
        arguments: &str,
        phase: Phase,
        source: Source<'_>,
    ) -> Result<Tool, String> {
        match name {
            "bash" if phase != Phase::Code => Err(
                "[ERROR] `bash` is only available in the code phase; use `execute_script` \
                 (`tools.list`, `tools.grep`, `tools.read`) instead"
                    .to_string(),
            ),
            "bash" => serde_json::from_str(arguments)
                .map(Tool::Bash)
                .map_err(|e| e.to_string()),
            "execute_script" => serde_json::from_str(arguments)
                .map(Tool::ExecuteScript)
                .map_err(|e| e.to_string()),
            // A control tool belongs to exactly one phase. In any other phase it
            // is a mistake to recover from, not a second (fake) success. The
            // message names only the tool and the phase, so it reads the same
            // -- and is equally true -- in all four phases.
            name @ ("finish_plan" | "finish_spec" | "finish_tickets" | "finish_triage"
            | "finish_review")
                if !matches!(
                    (name, phase),
                    ("finish_plan", Phase::Plan)
                        | ("finish_spec", Phase::Spec)
                        | ("finish_tickets", Phase::Tickets)
                        | ("finish_triage", Phase::Triage)
                        | ("finish_review", Phase::Review)
                ) =>
            {
                Err(format!(
                    "[ERROR] `{name}` is the {} agent's handoff and is not available in this \
                     phase",
                    &name["finish_".len()..]
                ))
            }
            // A payload that does not parse as the plan document is not a
            // handoff: the spec agent would receive nothing to specify. The
            // first failure is the wire format (the argument must be a JSON
            // string), the second is the document itself; both are reported as
            // a parse error, exactly like any other bad tool call, so the model
            // fixes its arguments on its next step.
            "finish_plan" => Self::parse_finish_plan(arguments),
            // A payload that is not a complete specification document is not a
            // handoff: the tickets agent would receive nothing to slice. The
            // first failure is the wire format (the argument must be a JSON
            // string), the second is the document itself; both are reported as
            // a parse error, exactly like any other bad tool call, so the model
            // fixes its arguments on its next step.
            "finish_spec" => Self::parse_finish_spec(arguments, source),
            // A payload that does not deserialize is not a handoff, and neither
            // is a set that fails the six structural rules: either way the
            // orchestrator would have nothing executable. Both are reported as
            // an error naming what to fix, so the tickets agent corrects its
            // arguments on its next step instead of ending the pass with an
            // unusable set.
            "finish_tickets" => Self::parse_finish_tickets(arguments, source),
            // A payload that does not deserialize is not a handoff, and neither
            // is a report that drops a draft ticket, invents one, rewrites a
            // title, or carries a payload its own state does not allow: either
            // way the pipeline would have no usable audit of the draft set.
            // Both are reported as a tool error naming what to fix, so the
            // triage agent corrects its arguments on its next step instead of
            // ending the pass with a report the boundary cannot accept.
            "finish_triage" => Self::parse_finish_triage(arguments, source),
            "finish_review" => Self::parse_finish_review(arguments, source),
            // Completion belongs to the code agent: cutting any earlier pass
            // short would trade the design, specification or tickets for no code
            // at all. The message names only the tool and the phase.
            "task_complete" if phase != Phase::Code => Err(
                "[ERROR] `task_complete` is the code agent's completion and is not available in \
                 this phase"
                    .to_string(),
            ),
            "task_complete" if no_args(arguments) => Ok(Tool::TaskComplete(TaskCompleteArgs {})),
            "task_complete" => serde_json::from_str(arguments)
                .map(Tool::TaskComplete)
                .map_err(|e| e.to_string()),
            other => Err(format!("[ERROR] unknown tool `{}`", other)),
        }
    }

    /// Parse and validate the plan handoff. The wire form is the markdown
    /// document; the typed value is what crosses the harness boundary.
    fn parse_finish_plan(arguments: &str) -> Result<Tool, String> {
        let parsed: FinishPlanArgs = serde_json::from_str(arguments).map_err(|e| {
            format!(
                "[ERROR] `finish_plan` needs the plan as a markdown document in its \
                 `plan` argument, but the argument is not a valid JSON string: {e}"
            )
        })?;
        Plan::from_markdown(&parsed.plan)
            .map(Tool::FinishPlan)
            .map_err(|e| {
                format!(
                    "[ERROR] `finish_plan` needs the complete plan as a markdown document \
                     in its `plan` argument; the document could not be parsed: {e}"
                )
            })
    }

    /// Parse and validate the specification handoff, including plan coverage
    /// when the current pass carries a plan source.
    fn parse_finish_spec(arguments: &str, source: Source<'_>) -> Result<Tool, String> {
        let parsed: FinishSpecArgs = serde_json::from_str(arguments).map_err(|e| {
            format!(
                "[ERROR] `finish_spec` needs the specification as a markdown document in \
                 its `spec` argument, but the argument is not a valid JSON string: {e}"
            )
        })?;
        let specification = Specification::from_markdown(&parsed.spec).map_err(|e| {
            format!(
                "[ERROR] `finish_spec` needs the complete specification as a markdown \
                     document in its `spec` argument; the document could not be parsed: {e}"
            )
        })?;
        // The specification is only a handoff if it still accounts for the plan
        // it came from. An uncovered decision is an ordinary tool error, so the
        // spec agent adds it on its next step.
        if let Source::Plan(plan) = source {
            plan_to_spec(plan, &specification)?;
        }
        Ok(Tool::FinishSpec(specification))
    }

    /// Parse and structurally validate the ticket-set handoff, including
    /// specification coverage when the current pass carries a specification
    /// source.
    fn parse_finish_tickets(arguments: &str, source: Source<'_>) -> Result<Tool, String> {
        let parsed: FinishTicketsArgs = serde_json::from_str(arguments).map_err(|e| {
            format!(
                "[ERROR] `finish_tickets` needs the ticket set as a JSON object in its \
                 `tickets` argument, but the argument could not be parsed: {e}"
            )
        })?;
        parsed.tickets.validate()?;
        // A structurally valid set is still not a handoff if it drops a story
        // or rewrites a criterion: the tickets agent gets the coverage failure
        // as a tool result and repairs the set.
        if let Source::Specification(specification) = source {
            spec_to_tickets(specification, &parsed.tickets)?;
        }
        Ok(Tool::FinishTickets(parsed.tickets))
    }

    /// Parse the triage handoff and validate its report against the draft set
    /// the pass was handed. A missing draft set is refused rather than accepted
    /// unvalidated.
    fn parse_finish_triage(arguments: &str, source: Source<'_>) -> Result<Tool, String> {
        let parsed: FinishTriageArgs = serde_json::from_str(arguments).map_err(|e| {
            format!(
                "[ERROR] `finish_triage` needs the triage report as a JSON object in its \
                 `triage` argument, but the argument could not be parsed: {e}"
            )
        })?;
        match source {
            Source::Triage {
                specification,
                tickets,
            } => parsed.triage.validate_against(specification, tickets)?,
            // Every report invariant is decided by the draft set, so a handoff
            // that names no draft set has nothing to be checked against.
            _ => {
                return Err(
                    "[ERROR] `finish_triage` needs the draft ticket set it classifies; \
                     the triage source is missing, so the report cannot be validated"
                        .to_string(),
                );
            }
        }
        Ok(Tool::FinishTriage(parsed.triage))
    }

    /// Parse and validate the review handoff. Only a review source supplies the
    /// earlier round's ticket ids the fix tickets must not duplicate.
    fn parse_finish_review(arguments: &str, source: Source<'_>) -> Result<Tool, String> {
        let parsed: FinishReviewArgs = serde_json::from_str(arguments).map_err(|e| {
            format!(
                "[ERROR] `finish_review` needs the review report as a JSON object in its \
                 `review` argument, but the argument could not be parsed: {e}"
            )
        })?;
        parsed.review.validate()?;
        let existing = match source {
            Source::Review(ids) => ids,
            _ => &[],
        };
        review_to_fixes(&parsed.review, existing)?;
        Ok(Tool::FinishReview(parsed.review))
    }

    /// What the status line shows while this tool runs.
    pub fn target(&self) -> &str {
        match self {
            Tool::Bash(args) => &args.command,
            Tool::ExecuteScript(args) => &args.code,
            Tool::FinishPlan(_) => "finish_plan",
            Tool::FinishSpec(_) => "finish_spec",
            Tool::FinishTickets(_) => "finish_tickets",
            Tool::FinishTriage(_) => "finish_triage",
            Tool::FinishReview(_) => "finish_review",
            Tool::TaskComplete(_) => "task_complete",
        }
    }

    /// Run the tool at the identity a manual call gives it (step 1, Bash call
    /// 1).
    ///
    /// The agent loop uses [`Tool::run_with_context`] so an oversized Bash log
    /// is named for the step and call that produced it.
    pub fn run(&self, phase: Phase) -> Result<String, String> {
        self.run_with_context(phase, ToolContext::default())
    }

    /// The same dispatch anchored to an explicit project root and an explicit
    /// optional invocation session directory. A script runs with the phase's
    /// capabilities rooted at this project, so a caller that runs from a
    /// directory other than the project root still reads and writes the project
    /// it selected. Bash runs in this root and resolves an oversized log's
    /// destination against it, so the reported spill path is root-relative. The
    /// session directory stays a separate argument so the public [`ToolContext`]
    /// remains context-free and `Copy`.
    pub(crate) fn run_with_context_in(
        &self,
        root: &Path,
        phase: Phase,
        context: ToolContext,
        session_dir: Option<&Path>,
    ) -> Result<String, String> {
        match self {
            Tool::Bash(args) => bash_in(root, args, context, session_dir),
            Tool::ExecuteScript(args) => {
                let caps = Capabilities::for_phase(phase).with_root(root);
                QuickJsEngine::new()
                    .run(&args.code, &caps)
                    .map_err(format_script_error)
            }
            _ => self.run_with_context(phase, context),
        }
    }

    /// Run the tool at the one-based model step and Bash-call index of the
    /// dispatch. `execute_script` runs against the production
    /// [`QuickJsEngine`] with the capabilities of `phase`. This context-free
    /// entry point carries no invocation session directory, so an oversized
    /// Bash result is reported as a save failure rather than written to a
    /// project-global location. A session-aware caller passes the directory
    /// separately through [`Tool::run_with_context_in`]. There is no
    /// injectable engine, so the dispatch a unit test exercises is exactly the
    /// dispatch the agent loop runs.
    pub fn run_with_context(&self, phase: Phase, context: ToolContext) -> Result<String, String> {
        match self {
            Tool::Bash(args) => {
                let root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                bash_in(&root, args, context, None)
            }
            Tool::ExecuteScript(args) => {
                let caps = Capabilities::for_phase(phase);
                QuickJsEngine::new()
                    .run(&args.code, &caps)
                    .map_err(format_script_error)
            }
            // Unreachable by construction: the loop recognises the control
            // variants in the parsed tool and leaves the loop before running
            // anything. A loud error (rather than a fake success) keeps that
            // guarantee honest if the dispatch ever changes.
            Tool::FinishPlan(_) => Err(
                "[ERROR] `finish_plan` is not a command and cannot be run; the handoff is handled \
                 by the loop"
                    .to_string(),
            ),
            Tool::FinishSpec(_) => Err(
                "[ERROR] `finish_spec` is not a command and cannot be run; the handoff is handled \
                 by the loop"
                    .to_string(),
            ),
            Tool::FinishTickets(_) => Err(
                "[ERROR] `finish_tickets` is not a command and cannot be run; the handoff is \
                 handled by the loop"
                    .to_string(),
            ),
            Tool::FinishTriage(_) => Err(
                "[ERROR] `finish_triage` is not a command and cannot be run; the handoff is \
                 handled by the loop"
                    .to_string(),
            ),
            Tool::FinishReview(_) => Err(
                "[ERROR] `finish_review` is not a command and cannot be run; the handoff is \
                 handled by the loop"
                    .to_string(),
            ),
            Tool::TaskComplete(_) => Err(
                "[ERROR] `task_complete` is not a command and cannot be run; completion is handled \
                 by the loop"
                    .to_string(),
            ),
        }
    }
}

/// Run a command through the same `bash -c` path the model's own `bash` tool uses.
///
/// The orchestrator runs a ticket's verification gate through this, so the gate is executed
/// exactly the way the model's commands are -- same working directory, same captured output, same
/// error shape -- and the two paths cannot diverge. The gate's complete output is returned: it is
/// not a model result, so the delivery cap and the spill area do not apply to it.
pub fn run_command_in(root: &Path, command: &str) -> Result<String, String> {
    let output = shell_output_in(root, command)?;

    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));

    if output.status.success() {
        Ok(text)
    } else {
        let code = output.status.code().unwrap_or(-1);
        Err(format!(
            "[ERROR: exit code {} for `{}`]\n{}",
            code, command, text
        ))
    }
}

/// Run a command in the process-relative `.` directory.
///
/// This is the historical verification entry point: callers that do not select
/// a project root keep running exactly where they always have.
pub fn run_command(command: &str) -> Result<String, String> {
    run_command_in(Path::new("."), command)
}

/// A no-argument tool is encoded inconsistently: an empty string, the literal
/// `null`, and a JSON object are all seen in the wild. Accepting each as "no
/// arguments" is the difference between the control tool working and a parse
/// error the model has to recover from.
fn no_args(arguments: &str) -> bool {
    let trimmed = arguments.trim();
    trimmed.is_empty() || trimmed == "null" || trimmed == "{}"
}

/// Run `command` through `bash -c` in `root` with the shared deadline,
/// capturing stdout, stderr and the exit code. Both the model's `bash` tool and
/// the verification gate enter here, so neither can wait indefinitely and the
/// two paths cannot diverge.
fn shell_output_in(root: &Path, command: &str) -> Result<Output, String> {
    shell_output_with_timeout(root, command, SHELL_TIMEOUT)
}

/// Run `command` through `bash -c` in `root`, capturing stdout, stderr and the
/// exit code while bounding the wait to `timeout`.
///
/// Stdin is explicitly closed so a command that reads it observes end-of-file
/// instead of waiting for the caller's terminal. Both streams are piped and
/// drained on their own threads, so a command that writes more than a pipe
/// buffer to either stream cannot deadlock against the poll loop. A command
/// still running when the deadline passes is killed and reaped, and the caller
/// receives the established `exit code -1` error shape describing the timeout.
/// A failed spawn keeps that same prefix followed by its cause.
fn shell_output_with_timeout(
    root: &Path,
    command: &str,
    timeout: Duration,
) -> Result<Output, String> {
    let mut child = Command::new("bash")
        .arg("-c")
        .arg(command)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| shell_failure(command, error))?;

    // The deadline starts at a successful spawn, so a slow spawn is never
    // charged to the command's own budget.
    let started = Instant::now();

    let stdout = child
        .stdout
        .take()
        .expect("piped stdout should be available after spawn");
    let stderr = child
        .stderr
        .take()
        .expect("piped stderr should be available after spawn");
    let stdout_reader = spawn_stream_reader(stdout);
    let stderr_reader = spawn_stream_reader(stderr);

    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(shell_failure(command, error));
            }
        }

        // A zero deadline therefore permits no polling wait: an already-exited
        // child was observed above, and a running one is terminated here.
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(shell_timeout(command, timeout));
        }

        std::thread::sleep(SHELL_POLL_INTERVAL);
    };

    // The child's exit status is not enough: a descendant can keep an output
    // pipe open after Bash itself has exited. Waiting for the readers under the
    // same overall deadline keeps that case bounded instead of blocking forever
    // in a thread join.
    let stdout = collect_stream(stdout_reader, command, started, timeout)?;
    let stderr = collect_stream(stderr_reader, command, started, timeout)?;

    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

/// The failure shape a run that never produced an exit status reports. A spawn
/// failure and a timeout share it, and the cause follows the prefix.
fn shell_failure(command: &str, cause: impl std::fmt::Display) -> String {
    format!("[ERROR: exit code -1 for `{command}`]\n{cause}")
}

/// The exact timeout result the shell contract reports when the deadline
/// expires, whether the Bash child is still running or a descendant is holding
/// an output pipe open.
fn shell_timeout(command: &str, timeout: Duration) -> String {
    format!(
        "[ERROR: exit code -1 for `{command}`]\ncommand timed out after {}s",
        timeout.as_secs()
    )
}

/// Read one piped stream to end of file on its own thread, reporting either the
/// complete bytes or the error that stopped the read. The outcome travels over
/// a channel so the poll loop can abandon an unfinished reader at the deadline
/// instead of joining it unconditionally.
fn spawn_stream_reader(
    mut stream: impl std::io::Read + Send + 'static,
) -> Receiver<std::io::Result<Vec<u8>>> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stream.read_to_end(&mut bytes).map(|_| bytes);
        let _ = sender.send(result);
    });
    receiver
}

/// Wait for one stream reader's outcome until the shell's overall deadline.
///
/// A reader that finishes promptly yields its complete bytes. A read error or a
/// reader thread that disappears before reporting yields the established
/// failure prefix, and a read still blocked at the deadline yields the exact
/// timeout error rather than partial bytes.
fn collect_stream(
    reader: Receiver<std::io::Result<Vec<u8>>>,
    command: &str,
    started: Instant,
    timeout: Duration,
) -> Result<Vec<u8>, String> {
    match reader.recv_timeout(timeout.saturating_sub(started.elapsed())) {
        Ok(Ok(bytes)) => Ok(bytes),
        Ok(Err(error)) => Err(shell_failure(command, error)),
        Err(RecvTimeoutError::Timeout) => Err(shell_timeout(command, timeout)),
        Err(RecvTimeoutError::Disconnected) => Err(shell_failure(
            command,
            "the output stream reader stopped before reporting",
        )),
    }
}

/// Run one model-issued Bash command, bounding what the model receives.
///
/// The raw stdout bytes are followed by the raw stderr bytes, exactly as they
/// were captured. At or below the delivery cap the lossy UTF-8 rendering is
/// returned unchanged. Above it the complete raw bytes are written to the
/// `tools-output` directory of the separately supplied invocation session
/// directory before the digest is formed, so the model can inspect the omitted
/// middle with `tools.read` through `execute_script`. Neither dispatch nor
/// persistence falls back to a global location when no session was supplied.
fn bash_in(
    root: &Path,
    args: &BashArgs,
    context: ToolContext,
    session_dir: Option<&Path>,
) -> Result<String, String> {
    let output = shell_output_in(root, &args.command)?;

    let mut raw = Vec::with_capacity(output.stdout.len() + output.stderr.len());
    raw.extend_from_slice(&output.stdout);
    raw.extend_from_slice(&output.stderr);

    let oversized = raw.len() > TOOL_OUTPUT_LIMIT_BYTES;
    let rendered = (!oversized).then(|| String::from_utf8_lossy(&raw).into_owned());

    if output.status.success() {
        return Ok(match rendered {
            Some(text) => text,
            None => spill_and_digest(
                root,
                &raw,
                &args.command,
                context,
                session_dir,
                TOOL_OUTPUT_LIMIT_BYTES,
            ),
        });
    }

    let code = output.status.code().unwrap_or(-1);
    let prefix = bash_failure_prefix(code, &args.command);
    let text = match rendered {
        Some(text) => text,
        None => {
            // The command failure information is part of the delivered
            // result, so the digest is bounded to what remains after it.
            let budget = TOOL_OUTPUT_LIMIT_BYTES.saturating_sub(prefix.len());
            spill_and_digest(root, &raw, &args.command, context, session_dir, budget)
        }
    };
    Err(format!("{prefix}{text}"))
}

/// The command failure information a Bash tool error begins with. The quoted
/// command is bounded on a UTF-8 character boundary so a long command cannot
/// push the bounded digest out of the delivery cap.
fn bash_failure_prefix(code: i32, command: &str) -> String {
    let shown = truncate_utf8_prefix(command, BASH_FAILURE_COMMAND_BYTES);
    if shown.len() == command.len() {
        format!("[ERROR: exit code {code} for `{shown}`]\n")
    } else {
        format!("[ERROR: exit code {code} for `{shown}...`]\n")
    }
}

/// Persist the complete raw bytes behind a command preamble and form a digest
/// that fits `limit`. A failed save still produces a bounded digest, but its
/// footer reports the failure and names no path.
fn spill_and_digest(
    root: &Path,
    raw: &[u8],
    command: &str,
    context: ToolContext,
    session_dir: Option<&Path>,
    limit: usize,
) -> String {
    let rendered = String::from_utf8_lossy(raw);
    match save_spill(root, command, raw, context, session_dir) {
        Ok(relative) => {
            let footer = format!(
                "[full output saved to {relative}; inspect it with tools.read through execute_script]"
            );
            form_bash_digest(&rendered, &footer, context.step, limit)
        }
        Err(error) => {
            let footer = bounded_footer(format!(
                "[ERROR: the complete output could not be saved; no spill file was written: {error}]"
            ));
            form_bash_digest(&rendered, &footer, context.step, limit)
        }
    }
}

/// Resolve the only directory an oversized log may be written to: the
/// `tools-output` child of the explicitly supplied session directory. A
/// relative session path is interpreted under the project root.
fn spill_directory(root: &Path, session_dir: &Path) -> PathBuf {
    let session = if session_dir.is_absolute() {
        session_dir.to_path_buf()
    } else {
        root.join(session_dir)
    };
    session.join(SPILL_DIRECTORY)
}

/// Render a saved path relative to the project root when it lives there.
fn root_relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

/// Write the originating command and the complete raw bytes to a brand-new file
/// under `<session-dir>/tools-output/`, returning its path relative to the
/// project root. The session directory is the only possible destination; an
/// absent session is a save failure, never a fallback write. Exclusive creation
/// is what makes the numeric collision suffix safe: an existing log is never
/// truncated or overwritten. The command is written verbatim, with no
/// truncation or substitution.
fn save_spill(
    root: &Path,
    command: &str,
    raw: &[u8],
    context: ToolContext,
    session_dir: Option<&Path>,
) -> Result<String, String> {
    let session_dir = session_dir.ok_or_else(|| {
        "no session directory was supplied, so the output was not saved".to_string()
    })?;
    let directory = spill_directory(root, session_dir);
    fs::create_dir_all(&directory)
        .map_err(|error| format!("could not create the spill directory: {error}"))?;
    ignore_run_artifacts(&directory)
        .map_err(|error| format!("could not prepare the self-ignoring .ejaj directory: {error}"))?;

    let step = context.step.max(1);
    let call = context.call_index.max(1);
    for suffix in 0..10_000usize {
        let file_name = if suffix == 0 {
            format!("step-{step}-call-{call}.log")
        } else {
            format!("step-{step}-call-{call}-{suffix}.log")
        };
        let path = directory.join(&file_name);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                let mut contents = Vec::with_capacity(command.len() + raw.len() + 19);
                contents.extend_from_slice(b"Command:\n");
                contents.extend_from_slice(command.as_bytes());
                contents.extend_from_slice(b"\n\nOutput:\n");
                contents.extend_from_slice(raw);
                if let Err(error) = file.write_all(&contents) {
                    let _ = fs::remove_file(&path);
                    return Err(format!("could not write the spill file: {error}"));
                }
                return Ok(root_relative_path(root, &path));
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("could not create the spill file: {error}")),
        }
    }

    Err("could not create a unique spill file after 10000 attempts".to_string())
}

/// Ensure the exact `.ejaj` ancestor of `dir` carries a self-ignoring
/// `.gitignore` with exactly `*\n`, so a run's artifacts never surface as
/// untracked changes in the target repository. Only a component whose whole
/// name is `.ejaj` matches: a lookalike such as `.ejaj-extra` is an ordinary
/// destination and is left alone. An existing regular ignore file is never
/// repaired or overwritten; any other entry at that path makes creation fail.
/// The spill directory must already exist, so the ignore file is created after
/// it and before the exclusive spill log.
fn ignore_run_artifacts(dir: &Path) -> std::io::Result<()> {
    let Some(ejaj) = ejaj_ancestor(dir) else {
        return Ok(());
    };
    let ignore = ejaj.join(".gitignore");
    if !ignore.is_file() {
        fs::write(&ignore, "*\n")?;
    }
    Ok(())
}

/// The path through the exact `.ejaj` component of `dir`, resolved from the
/// path components so relative and absolute destinations behave alike.
fn ejaj_ancestor(dir: &Path) -> Option<PathBuf> {
    let mut ancestor = PathBuf::new();
    for component in dir.components() {
        ancestor.push(component.as_os_str());
        if component.as_os_str() == ".ejaj" {
            return Some(ancestor);
        }
    }
    None
}

/// Bound a failure footer to something a digest can always carry.
fn bounded_footer(footer: String) -> String {
    if footer.len() <= SPILL_FAILURE_FOOTER_BYTES {
        return footer;
    }
    truncate_utf8_prefix(&footer, SPILL_FAILURE_FOOTER_BYTES).to_string()
}

/// Form the digest the model receives for oversized Bash output: a UTF-8-safe
/// head, the extracted diagnostics, a UTF-8-safe tail and the footer. The
/// footer, section separators and any step warning are reserved first; only
/// then is the diagnostic block reduced to fit the remaining budget.
fn form_bash_digest(rendered: &str, footer: &str, step: usize, limit: usize) -> String {
    let head = truncate_utf8_prefix(rendered, BASH_DIGEST_HEAD_BYTES);
    let tail = truncate_utf8_suffix(rendered, BASH_DIGEST_TAIL_BYTES);
    let diagnostics = diagnostic_text(rendered);
    let warning_reserve = step_warning_reserve(step);

    // The diagnostics heading, the newline before the tail and the newline
    // before the footer make seventeen separator bytes.
    let diagnostic_separators = 17;
    let no_diagnostic_overhead = head.len() + tail.len() + footer.len() + 2 + warning_reserve;
    let diagnostic_overhead =
        head.len() + tail.len() + footer.len() + diagnostic_separators + warning_reserve;

    let diagnostic_budget = if diagnostics.is_empty() || diagnostic_overhead >= limit {
        0
    } else {
        (limit - diagnostic_overhead).min(BASH_DIGEST_DIAGNOSTIC_BYTES)
    };
    let diagnostic = if diagnostic_budget == 0 {
        String::new()
    } else {
        truncate_utf8_prefix(&diagnostics, diagnostic_budget).to_string()
    };

    let digest = if diagnostic.is_empty() {
        format!("{head}\n{tail}\n{footer}")
    } else {
        format!("{head}\n[diagnostics]\n{diagnostic}\n{tail}\n{footer}")
    };

    debug_assert!(
        digest.len() <= limit || no_diagnostic_overhead > limit,
        "the Bash digest is {} bytes, over the {limit}-byte cap",
        digest.len()
    );
    if digest.len() <= limit {
        return digest;
    }

    // A safety net for a footer that somehow outgrew its reserve: keep the
    // footer and the head, and give the tail whatever remains.
    let fixed = head.len() + footer.len() + 2 + warning_reserve;
    let tail_budget = limit.saturating_sub(fixed);
    let tail = truncate_utf8_suffix(tail, tail_budget);
    format!("{head}\n{tail}\n{footer}")
}

/// The room a digest at `step` leaves for the loop's step-budget warning. The
/// warning is only appended from `STEP_WARN_REMAINING` steps onward, so the
/// reserve is taken only when it applies.
fn step_warning_reserve(step: usize) -> usize {
    if MAX_STEPS.saturating_sub(step) <= STEP_WARN_REMAINING {
        STEP_WARNING_RESERVE_BYTES
    } else {
        0
    }
}

/// Extract the nonblank diagnostic blocks from the rendered output in original
/// order, de-duplicated. The extraction is deliberately lexical: the model
/// gets the lines that already name a failure, not a reinterpretation of them.
fn diagnostic_text(rendered: &str) -> String {
    let lines: Vec<&str> = rendered.lines().collect();
    let mut blocks: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut index = 0;

    while index < lines.len() {
        if lines[index].trim().is_empty() || !is_diagnostic_line(lines[index]) {
            index += 1;
            continue;
        }

        let start = index;
        while index < lines.len() && !lines[index].trim().is_empty() {
            index += 1;
        }
        let block = lines[start..index].join("\n");
        if seen.insert(block.clone()) {
            blocks.push(block);
        }
    }

    blocks.join("\n")
}

/// Whether a line starts a diagnostic block the model should see. Matching is
/// case-insensitive, and Cargo's `---- <test> stdout ----` section marker is
/// recognised alongside the ordinary failure words.
fn is_diagnostic_line(line: &str) -> bool {
    let trimmed = line.trim();
    let lowered = trimmed.to_ascii_lowercase();
    lowered.contains("[error")
        || lowered.contains("error:")
        || lowered.contains("failed")
        || lowered.contains("failures:")
        || lowered.contains("panicked at")
        || lowered.contains("panic:")
        || (trimmed.starts_with("---- ") && trimmed.ends_with(" stdout ----"))
}

/// The longest prefix of `text` of at most `max_bytes`, cut on a UTF-8
/// character boundary.
fn truncate_utf8_prefix(text: &str, max_bytes: usize) -> &str {
    let mut end = max_bytes.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// The longest suffix of `text` of at most `max_bytes`, starting on a UTF-8
/// character boundary.
fn truncate_utf8_suffix(text: &str, max_bytes: usize) -> &str {
    let mut start = text.len().saturating_sub(max_bytes);
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

// ---------------------------------------------------------------------------
// Source-aware handoff coverage
//
// A typed handoff is only as good as the upstream document it carries
// forward. Before `finish_spec` ends a specification pass the harness checks
// that the specification's own content still accounts for every decision of
// the confirmed plan, and before `finish_tickets` ends a tickets pass it
// checks that the ticket set still accounts for every user story of the typed
// specification. A failure is an ordinary `Err(String)` tool result, so the
// running agent retries its handoff instead of ending the pass with a document
// that dropped content.
//
// The check is lexical on purpose. It never asks the downstream document to
// repeat the upstream text verbatim: an item counts as covered when a large
// enough share of its significant words appear somewhere in the downstream
// content. Two documents that say the same thing in different words therefore
// agree, while an item that was left out entirely has no overlap to match.

/// A word shorter than this carries too little substance to decide coverage;
/// the stop words below remove the frequent ones that are long enough.
const MIN_WORD_CHARS: usize = 3;
/// The share of an upstream item's significant words that must appear in the
/// downstream document for the item to count as covered: `matched / total >=
/// COVERAGE_NUMERATOR / COVERAGE_DENOMINATOR`. Two fifths is low enough that a
/// specification which paraphrases a plan decision still agrees, and high
/// enough that an item left out entirely has nothing to match.
const COVERAGE_NUMERATOR: usize = 2;
const COVERAGE_DENOMINATOR: usize = 5;

/// The words that carry no topic, so their presence says nothing about whether
/// an upstream item survived. Kept lower-case and matched after lower-casing;
/// the words short enough to be dropped by [`MIN_WORD_CHARS`] stay in the list
/// anyway, so the intent reads in one place.
const STOP_WORDS: &[&str] = &[
    "the",
    "and",
    "for",
    "with",
    "that",
    "this",
    "from",
    "into",
    "when",
    "must",
    "each",
    "one",
    "new",
    "not",
    "are",
    "its",
    "has",
    "have",
    "any",
    "all",
    "out",
    "use",
    "can",
    "may",
    "who",
    "how",
    "why",
    "also",
    "per",
    "via",
    "but",
    "you",
    "your",
    "they",
    "their",
    "them",
    "then",
    "than",
    "there",
    "here",
    "what",
    "which",
    "while",
    "would",
    "could",
    "should",
    "will",
    "shall",
    "does",
    "did",
    "doing",
    "done",
    "make",
    "makes",
    "made",
    "more",
    "most",
    "some",
    "such",
    "over",
    "under",
    "again",
    "further",
    "once",
    "same",
    "too",
    "very",
    "own",
    "now",
    "our",
    "ours",
    "we",
    "us",
    "it",
    "is",
    "a",
    "an",
    "of",
    "to",
    "in",
    "on",
    "at",
    "by",
    "as",
    "be",
    "been",
    "being",
    "or",
    "if",
    "no",
    "nor",
    "so",
    "do",
    "was",
    "were",
    "having",
    "about",
    "after",
    "before",
    "between",
    "during",
    "without",
    "within",
    "where",
    "whose",
    "these",
    "those",
    "other",
    "others",
    "just",
    "only",
    "both",
    "either",
    "neither",
    "every",
    "because",
    "since",
    "although",
    "however",
    "therefore",
    "thereby",
    "thus",
    "yet",
];

/// Check a specification against the plan it must preserve.
///
/// Every decision of `plan` must have its substance represented in
/// `specification`: its significant words must appear in the specification's
/// own content, whether in a user story or in the implementation and testing
/// decisions. A decision left out entirely is reported by title, so the spec
/// agent can repair the handoff on its next loop iteration. `Ok(())` means
/// every decision survived.
pub fn plan_to_spec(plan: &Plan, specification: &Specification) -> Result<(), String> {
    let target = specification_words(specification);

    let uncovered: Vec<&str> = plan
        .decisions
        .iter()
        .filter(|decision| {
            let source = format!("{}\n{}", decision.title, decision.detail);
            !covers(&target, &source)
        })
        .map(|decision| decision.title.trim())
        .collect();

    if uncovered.is_empty() {
        return Ok(());
    }

    Err(uncovered_decisions_error(&uncovered))
}

/// Check a ticket set against the specification it must preserve.
///
/// Every user story must survive into the ticket set. A story that carries
/// acceptance criteria is represented when the union of every ticket's
/// `acceptance_criteria` holds each criterion as the same text; a story with
/// no criteria is represented when some ticket's own content makes its
/// substance discernible. A criterion compares after trimming and collapsing
/// inner whitespace, so a reflowed line is not an alteration. An uncovered
/// story is reported by title, and a criterion the tickets dropped or rewrote
/// is reported by its exact text --
/// both as ordinary tool errors, so the tickets agent repairs the set on its
/// next loop iteration. `Ok(())` means every story and every criterion
/// survived.
pub fn spec_to_tickets(specification: &Specification, tickets: &TicketSet) -> Result<(), String> {
    let carried: HashSet<String> = tickets
        .tickets
        .iter()
        .flat_map(|ticket| ticket.acceptance_criteria.iter())
        .map(|criterion| normalized_criterion(criterion))
        .filter(|criterion| !criterion.is_empty())
        .collect();
    let ticket_content = ticket_content_words(tickets);

    let mut uncovered: Vec<&str> = Vec::new();
    let mut missing: Vec<(&str, &str)> = Vec::new();
    for story in &specification.user_stories {
        let criteria: Vec<&str> = story
            .acceptance_criteria
            .iter()
            .map(|criterion| criterion.trim())
            .filter(|criterion| !criterion.is_empty())
            .collect();

        if criteria.is_empty() {
            let source = format!("{}\n{}", story.title, story.requirement);
            if !covers(&ticket_content, &source) {
                uncovered.push(story.title.trim());
            }
            continue;
        }

        for criterion in criteria {
            if !carried.contains(&normalized_criterion(criterion)) {
                missing.push((story.title.trim(), criterion));
            }
        }
    }

    if !uncovered.is_empty() {
        return Err(uncovered_stories_error(&uncovered));
    }

    if !missing.is_empty() {
        return Err(missing_criteria_error(&missing));
    }

    Ok(())
}

/// The significant words of the whole specification: its prose, its stories
/// and criteria, its implementation and testing decisions, and its remaining
/// lists. Coverage may be expressed anywhere the specification says what to
/// build, so every content field is searched and no heading-only text is.
fn specification_words(specification: &Specification) -> HashSet<String> {
    let mut text = String::new();
    push_line(&mut text, &specification.feature_name);
    push_line(&mut text, &specification.problem_statement);
    push_line(&mut text, &specification.solution_overview);
    for story in &specification.user_stories {
        push_line(&mut text, &story.title);
        push_line(&mut text, &story.requirement);
        for criterion in &story.acceptance_criteria {
            push_line(&mut text, criterion);
        }
    }
    push_line(
        &mut text,
        &specification.implementation_decisions.module_architecture,
    );
    push_line(
        &mut text,
        &specification
            .implementation_decisions
            .data_models_and_contracts,
    );
    push_line(
        &mut text,
        &specification.implementation_decisions.adr_compliance,
    );
    push_line(&mut text, &specification.testing_decisions.test_seams);
    push_line(
        &mut text,
        &specification.testing_decisions.coverage_strategy,
    );
    push_line(&mut text, &specification.testing_decisions.prior_art);
    for entry in &specification.out_of_scope {
        push_line(&mut text, entry);
    }
    push_line(&mut text, &specification.deferred_items.operational_risks);
    push_line(&mut text, &specification.deferred_items.follow_up_work);

    significant_words(&text)
}

/// The significant words of the whole ticket set: each ticket's title,
/// objective, prerequisites, boundaries, scope, tests and criteria. A story
/// without explicit criteria counts as covered when this content makes its
/// substance discernible.
fn ticket_content_words(tickets: &TicketSet) -> HashSet<String> {
    let mut text = String::new();
    for ticket in &tickets.tickets {
        push_line(&mut text, &ticket.title);
        push_line(&mut text, &ticket.objective);
        push_line(&mut text, &ticket.prerequisites_assumed);
        for entry in &ticket.out_of_scope {
            push_line(&mut text, entry);
        }
        for entry in &ticket.scope_of_changes {
            push_line(&mut text, entry);
        }
        for entry in &ticket.test_expectations {
            push_line(&mut text, entry);
        }
        for criterion in &ticket.acceptance_criteria {
            push_line(&mut text, criterion);
        }
    }

    significant_words(&text)
}

/// Append `line` and a newline, so adjacent fields cannot fuse into one token.
fn push_line(text: &mut String, line: &str) {
    text.push_str(line);
    text.push('\n');
}

/// Whether `source`'s substance is discernible in `target`: at least
/// `COVERAGE_NUMERATOR / COVERAGE_DENOMINATOR` of its significant words are
/// present. An item with no significant words of its own carries no substance
/// to check and is covered by definition.
fn covers(target: &HashSet<String>, source: &str) -> bool {
    let source = significant_words(source);
    if source.is_empty() {
        return true;
    }

    let matched = source.iter().filter(|word| target.contains(*word)).count();
    matched * COVERAGE_DENOMINATOR >= source.len() * COVERAGE_NUMERATOR
}

/// The set of significant words in `text`: the lower-cased runs of ASCII
/// letters and digits that are long enough and are not stop words. Splitting on
/// every other character means code spans (`plan_document`, `--version`) and
/// punctuation cannot smuggle a token past the check or fuse two words.
fn significant_words(text: &str) -> HashSet<String> {
    let mut words = HashSet::new();
    let mut current = String::new();

    for character in text.chars() {
        if character.is_ascii_alphanumeric() {
            current.push(character.to_ascii_lowercase());
        } else {
            push_word(&mut words, std::mem::take(&mut current));
        }
    }
    push_word(&mut words, current);

    words
}

/// Keep `word` when it is long enough and not one of [`STOP_WORDS`].
fn push_word(words: &mut HashSet<String>, word: String) {
    if word.chars().count() >= MIN_WORD_CHARS && !STOP_WORDS.contains(&word.as_str()) {
        words.insert(stem(&word).to_string());
    }
}

/// A very light stemmer: drop one trailing inflection so "prints" and "print",
/// "renders" and "render", "exits" and "exit" compare equal. Coverage is about
/// meaning, and a paraphrase that only changes the verb form must not read as a
/// lost item. The rule is deliberately tiny -- one suffix, never below three
/// characters of stem -- so it cannot fold two unrelated words together.
fn stem(word: &str) -> &str {
    for suffix in ["ing", "ed", "s"] {
        if let Some(base) = word.strip_suffix(suffix)
            && base.len() >= MIN_WORD_CHARS
            && !(suffix == "s" && base.ends_with('s'))
        {
            return base;
        }
    }
    word
}

/// The error for the decisions a specification failed to cover: one sentence
/// naming every uncovered decision title, so the spec agent knows exactly what
/// to add.
fn uncovered_decisions_error(titles: &[&str]) -> String {
    let list = quoted_titles(titles);
    if titles.len() == 1 {
        format!(
            "[ERROR] the specification leaves an uncovered plan decision: {list}; every plan \
             decision must be represented, in substance, by a user story or by the implementation \
             or testing decisions"
        )
    } else {
        format!(
            "[ERROR] the specification leaves {} uncovered plan decisions: {list}; every plan \
             decision must be represented, in substance, by a user story or by the implementation \
             or testing decisions",
            titles.len()
        )
    }
}

/// Normalise one criterion for comparison: trimmed, with every inner run of
/// whitespace collapsed to a single space. Formatting differences between the
/// specification and a ticket must not read as an altered criterion.
fn normalized_criterion(criterion: &str) -> String {
    criterion.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The error for the stories a ticket set failed to cover.
fn uncovered_stories_error(titles: &[&str]) -> String {
    let list = quoted_titles(titles);
    if titles.len() == 1 {
        format!(
            "[ERROR] the ticket set leaves an uncovered specification story: {list}; every user \
             story must be represented by at least one ticket, through its criteria or through \
             the ticket's own content"
        )
    } else {
        format!(
            "[ERROR] the ticket set leaves {} uncovered specification stories: {list}; every user \
             story must be represented by at least one ticket, through its criteria or through \
             the ticket's own content",
            titles.len()
        )
    }
}

/// The error for the criteria the ticket set dropped or rewrote. Every one is
/// quoted with its story, so the tickets agent can copy the exact text back
/// without guessing.
fn missing_criteria_error(missing: &[(&str, &str)]) -> String {
    let entries: Vec<String> = missing
        .iter()
        .map(|(story, criterion)| format!("\"{criterion}\" (story \"{story}\")"))
        .collect();
    let list = match entries.as_slice() {
        [] => String::new(),
        [only] => only.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    };

    if missing.len() == 1 {
        format!(
            "[ERROR] the ticket set is missing the specification criterion for story \"{}\": \
             \"{}\"; every criterion must appear exactly in the `acceptance_criteria` of the \
             ticket that implements its story, so missing or altered criterion text is rejected",
            missing[0].0, missing[0].1
        )
    } else {
        format!(
            "[ERROR] the ticket set is missing or altered {} specification criteria: {list}; \
             every criterion must appear exactly in the `acceptance_criteria` of the ticket that \
             implements its story, so missing or altered criterion text is rejected",
            missing.len()
        )
    }
}

/// `"a", "b" and "c"` -- the uncovered titles, quoted so a title that is a
/// sentence still reads as one item.
fn quoted_titles(titles: &[&str]) -> String {
    let quoted: Vec<String> = titles.iter().map(|title| format!("\"{title}\"")).collect();
    match quoted.as_slice() {
        [] => String::new(),
        [only] => only.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// Check a review report's fix tickets against the findings they must cover and
/// the ticket ids already used in earlier rounds of the same run.
pub fn review_to_fixes(
    review: &ReviewReport,
    existing_ticket_ids: &[String],
) -> Result<(), String> {
    if review.fixes.is_empty() {
        return Ok(());
    }

    let existing: HashSet<&str> = existing_ticket_ids.iter().map(|id| id.trim()).collect();
    for ticket in &review.fixes {
        let id = ticket.id.trim();
        if existing.contains(id) {
            return Err(format!(
                "[ERROR] fix ticket `{id}` duplicates a ticket id from an earlier round; every \
                 fix ticket must use a new unique id"
            ));
        }
    }

    let fix_set = TicketSet {
        tickets: review.fixes.clone(),
    };
    let target = ticket_content_words(&fix_set);
    let uncovered: Vec<&str> = review
        .findings
        .iter()
        .filter(|finding| {
            let source = format!(
                "{}\n{}\n{}",
                finding.category, finding.target, finding.detail
            );
            !covers(&target, &source)
        })
        .map(|finding| finding.id.trim())
        .collect();

    if !uncovered.is_empty() {
        return Err(uncovered_findings_error(&uncovered));
    }

    Ok(())
}

/// The error for the review findings that no fix ticket covers in substance.
fn uncovered_findings_error(ids: &[&str]) -> String {
    let list = quoted_titles(ids);
    if ids.len() == 1 {
        format!(
            "[ERROR] the review fix tickets leave an uncovered finding: {list}; every review \
             finding must be represented, in substance, by at least one fix ticket in `fixes`"
        )
    } else {
        format!(
            "[ERROR] the review fix tickets leave {} uncovered findings: {list}; every review \
             finding must be represented, in substance, by at least one fix ticket in `fixes`",
            ids.len()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Create a fresh temporary directory for one test. Every Bash output test
    /// runs against its own root so its spill area cannot collide with another
    /// test's.
    fn scratch_dir(label: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "ejaj-tools-test-{}-{id}-{label}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir should be creatable");
        dir
    }

    /// The explicit session directory a test supplies to Bash dispatch. It
    /// lives under the scratch project root, so every saved path must render
    /// relative to that root.
    fn session_at(root: &std::path::Path, name: &str) -> std::path::PathBuf {
        root.join(".ejaj").join(name)
    }

    /// The `tools-output` directory under one supplied session directory.
    fn session_output(session: &std::path::Path) -> std::path::PathBuf {
        session.join("tools-output")
    }

    /// Run one real Bash command at the one-based model step and Bash-call
    /// index the agent would pass through dispatch, inside an explicit session
    /// directory carried separately from the public context.
    fn run_bash_in_session(
        root: &std::path::Path,
        session: &std::path::Path,
        command: &str,
        step: usize,
        call_index: usize,
    ) -> Result<String, String> {
        bash_in(
            root,
            &BashArgs {
                command: command.to_string(),
            },
            ToolContext { step, call_index },
            Some(session),
        )
    }

    /// Run one real Bash command through the context-free dispatch, which
    /// carries no invocation session directory.
    fn run_bash_without_session(
        root: &std::path::Path,
        command: &str,
        step: usize,
        call_index: usize,
    ) -> Result<String, String> {
        bash_in(
            root,
            &BashArgs {
                command: command.to_string(),
            },
            ToolContext { step, call_index },
            None,
        )
    }

    /// The one-line command that prints exactly the requested number of bytes.
    fn repeat_command(byte: char, bytes: usize) -> String {
        format!("printf '%{bytes}s' '' | tr ' ' '{byte}'")
    }

    /// The exact bytes a saved spill must carry: the command preamble followed
    /// by the unchanged captured stdout-then-stderr bytes.
    fn spilled_bytes(command: &str, output: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"Command:\n");
        bytes.extend_from_slice(command.as_bytes());
        bytes.extend_from_slice(b"\n\nOutput:\n");
        bytes.extend_from_slice(output);
        bytes
    }

    /// Core entry-point behavior: a real Bash command over the delivery cap
    /// writes its complete log under the explicitly supplied session directory
    /// and names that file relative to the project root.
    #[test]
    fn when_bash_output_given_oversized_session_command_then_saves_under_session_and_reports_root_relative_path()
     {
        let root = scratch_dir("bash-oversized-session");
        let session = session_at(&root, "run-42");
        let command = "printf 'HEAD-MARK\\n'; printf '[ERROR] demo failure\\n'; printf 'x%.0s' {1..20000}; printf '\\nTAIL-MARK\\n'; printf 'STDERR-MARK\\n' >&2";

        let result = run_bash_in_session(&root, &session, command, 90, 1)
            .expect("the command should succeed");

        let relative = ".ejaj/run-42/tools-output/step-90-call-1.log";
        let warning = "[WARN: step 90/100; 10 steps remaining before the step limit]";
        assert!(
            result.len() + warning.len() < crate::script::OUTPUT_LIMIT_BYTES,
            "the digest and the step warning must fit the delivery cap: {}",
            result.len()
        );
        assert!(
            result.contains(relative),
            "the digest must name the root-relative path under the supplied session: {result:?}"
        );
        assert!(
            result.contains("tools.read") && result.contains("execute_script"),
            "the digest must advise inspecting the spill through execute_script: {result:?}"
        );
        assert!(
            !result.contains(".ejaj/tool-output"),
            "the digest must never name the legacy global spill directory: {result:?}"
        );
        assert!(
            !result.contains("Command:") && !result.contains("Output:"),
            "the digest must be formed from captured output only: {result:?}"
        );

        let spill = session_output(&session).join("step-90-call-1.log");
        assert!(
            spill.exists(),
            "the complete log must be written under the supplied session"
        );
        assert!(
            !root.join(".ejaj/tool-output").exists(),
            "an explicitly supplied session is the only spill destination"
        );

        let expected_output = format!(
            "HEAD-MARK\n[ERROR] demo failure\n{}\nTAIL-MARK\nSTDERR-MARK\n",
            "x".repeat(20_000)
        );
        assert_eq!(
            std::fs::read(&spill).unwrap(),
            spilled_bytes(command, expected_output.as_bytes()),
            "the spill file must carry the exact command preamble and complete output"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Consolidated boundary table: empty output, output at the delivery cap,
    /// one byte over it and substantially oversized output all select spill or
    /// no spill from the captured bytes alone. A command longer than the cap
    /// with output at the cap never spills.
    #[test]
    fn when_bash_output_given_boundary_sizes_then_spills_only_above_limit() {
        struct BoundaryCase {
            label: &'static str,
            command: String,
            expected_output: String,
            expect_spill: bool,
        }

        let limit = crate::script::OUTPUT_LIMIT_BYTES;
        let long_command_at_limit = format!(
            ": '{}'; {}",
            "x".repeat(limit + 1),
            repeat_command('d', limit)
        );
        assert!(
            long_command_at_limit.len() > limit,
            "the long-command case must exceed the output cap in command text"
        );

        let cases = [
            BoundaryCase {
                label: "zero-bytes",
                command: "true".to_string(),
                expected_output: String::new(),
                expect_spill: false,
            },
            BoundaryCase {
                label: "exact-limit",
                command: repeat_command('a', limit),
                expected_output: "a".repeat(limit),
                expect_spill: false,
            },
            BoundaryCase {
                label: "one-byte-over",
                command: repeat_command('b', limit + 1),
                expected_output: "b".repeat(limit + 1),
                expect_spill: true,
            },
            BoundaryCase {
                label: "substantially-oversized",
                command: repeat_command('c', limit * 4),
                expected_output: "c".repeat(limit * 4),
                expect_spill: true,
            },
            BoundaryCase {
                label: "long-command-at-limit",
                command: long_command_at_limit,
                expected_output: "d".repeat(limit),
                expect_spill: false,
            },
        ];

        for case in cases {
            let root = scratch_dir(&format!("bash-boundary-{}", case.label));
            let session = session_at(&root, "run");
            let result = run_bash_in_session(&root, &session, &case.command, 4, 2)
                .unwrap_or_else(|error| panic!("the {} case should succeed: {error}", case.label));
            let spill = session_output(&session).join("step-4-call-2.log");

            if case.expect_spill {
                assert!(
                    result.contains(".ejaj/run/tools-output/step-4-call-2.log"),
                    "the {} case must name the session spill file: {result:?}",
                    case.label
                );
                assert!(
                    spill.exists(),
                    "the {} case must create a file under the supplied session",
                    case.label
                );
                assert_eq!(
                    std::fs::read(&spill).unwrap(),
                    spilled_bytes(&case.command, case.expected_output.as_bytes()),
                    "the {} case must spill the exact command preamble and captured bytes",
                    case.label
                );
            } else {
                assert_eq!(
                    result, case.expected_output,
                    "the {} case must return its captured output unchanged and without a header",
                    case.label
                );
                assert!(
                    !session.exists(),
                    "the {} case must not create the supplied session directory",
                    case.label
                );
                assert!(
                    !spill.exists(),
                    "the {} case must not create a spill file",
                    case.label
                );
            }

            let _ = std::fs::remove_dir_all(&root);
        }
    }

    /// Invalid payload and collision table: invalid UTF-8 bytes survive
    /// verbatim after the complete command preamble, and an existing primary
    /// log is preserved while the next file takes an incrementing suffix.
    #[test]
    fn when_bash_output_given_invalid_bytes_and_collisions_then_preserves_raw_log_and_adds_suffix()
    {
        let root = scratch_dir("bash-invalid-bytes");
        let session = session_at(&root, "invalid");
        let command =
            "printf 'BEGIN\\n'; head -c 20000 /dev/zero | tr '\\0' '\\377'; printf '\\nEND\\n'";

        let result = run_bash_in_session(&root, &session, command, 3, 1)
            .expect("the command should succeed");

        assert!(
            result.len() <= crate::script::OUTPUT_LIMIT_BYTES,
            "the digest must stay within the delivery cap: {}",
            result.len()
        );
        assert!(result.is_char_boundary(result.len()));
        assert!(
            result.contains("BEGIN") && result.contains("END"),
            "the digest must keep a UTF-8-safe head and tail: {result:?}"
        );

        let mut expected_output = b"BEGIN\n".to_vec();
        expected_output.extend(std::iter::repeat_n(0xff, 20_000));
        expected_output.extend_from_slice(b"\nEND\n");
        let header = format!("Command:\n{command}\n\nOutput:\n").into_bytes();
        let raw = std::fs::read(session_output(&session).join("step-3-call-1.log"))
            .expect("the spill file must exist");
        assert_eq!(
            raw,
            spilled_bytes(command, &expected_output),
            "the spill file must carry the exact preamble and stdout-then-stderr bytes"
        );
        assert_eq!(
            &raw[header.len()..],
            expected_output.as_slice(),
            "the bytes after the output header must be the captured bytes unchanged"
        );
        assert!(
            raw.contains(&0xff),
            "the spill file must keep the invalid UTF-8 byte verbatim"
        );

        let collisions = session_at(&root, "collisions");
        let limit = crate::script::OUTPUT_LIMIT_BYTES;
        let first_command = repeat_command('a', limit + 1);
        let second_command = repeat_command('b', limit + 1);
        let first = run_bash_in_session(&root, &collisions, &first_command, 12, 2)
            .expect("the command should succeed");
        let second = run_bash_in_session(&root, &collisions, &second_command, 12, 2)
            .expect("the command should succeed");

        assert!(
            first.contains(".ejaj/collisions/tools-output/step-12-call-2.log"),
            "the first spill must use the step-and-call name in the session: {first:?}"
        );
        assert!(
            second.contains(".ejaj/collisions/tools-output/step-12-call-2-1.log"),
            "a collision must use a numeric suffix in the same session: {second:?}"
        );
        let first_log = session_output(&collisions).join("step-12-call-2.log");
        let second_log = session_output(&collisions).join("step-12-call-2-1.log");
        let first_bytes = std::fs::read(&first_log).unwrap();
        let second_bytes = std::fs::read(&second_log).unwrap();
        assert_eq!(
            first_bytes,
            spilled_bytes(&first_command, "a".repeat(limit + 1).as_bytes()),
            "the first spill keeps the exact command preamble and complete output"
        );
        assert_eq!(
            second_bytes,
            spilled_bytes(&second_command, "b".repeat(limit + 1).as_bytes()),
            "the collision file keeps its own exact command preamble and complete output"
        );
        assert!(
            first_bytes.ends_with(b"aaaa"),
            "the collision must not overwrite the existing log"
        );
        assert!(
            second_bytes.ends_with(b"bbbb"),
            "the collision file must carry its own complete output"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Contract and invariant: a failing oversized command keeps its failure
    /// prefix and complete log, while a save error never names a path it did
    /// not create and never changes the command's success or error status.
    #[test]
    fn when_bash_output_given_failure_or_save_error_then_preserves_status_without_false_path() {
        let root = scratch_dir("bash-failure-save-error");
        let failure_session = session_at(&root, "failure");
        let command = "printf 'FAIL-HEAD\\n'; printf '[ERROR] deliberate failure\\n'; printf 'x%.0s' {1..20000}; printf '\\nFAIL-TAIL\\n'; printf 'STDERR-DIAG\\n' >&2; exit 7";

        let error = run_bash_in_session(&root, &failure_session, command, 90, 1)
            .expect_err("a failing command must return its diagnostics as a tool error");

        assert!(
            error.starts_with("[ERROR: exit code 7 for `"),
            "the command failure information must precede the digest: {error:?}"
        );
        assert!(
            error.len() <= crate::script::OUTPUT_LIMIT_BYTES,
            "the failure result is {} bytes, over the delivery cap: {error:?}",
            error.len()
        );
        assert!(
            error.contains("[ERROR] deliberate failure"),
            "the digest must keep the extracted diagnostics: {error:?}"
        );
        assert!(
            error.contains("FAIL-HEAD") && error.contains("FAIL-TAIL"),
            "the digest must keep a head and a tail: {error:?}"
        );
        assert!(
            error.contains(".ejaj/failure/tools-output/step-90-call-1.log"),
            "the digest must name the session spill path: {error:?}"
        );
        let warning = "[WARN: step 90/100; 10 steps remaining before the step limit]";
        assert!(
            error.len() + warning.len() < crate::script::OUTPUT_LIMIT_BYTES,
            "a step warning must still fit beside the complete failure digest: {}",
            error.len()
        );

        let expected_output = format!(
            "FAIL-HEAD\n[ERROR] deliberate failure\n{}\nFAIL-TAIL\nSTDERR-DIAG\n",
            "x".repeat(20_000)
        );
        let raw = std::fs::read(session_output(&failure_session).join("step-90-call-1.log"))
            .expect("the spill file must exist");
        assert_eq!(
            raw,
            spilled_bytes(command, expected_output.as_bytes()),
            "the spill file must carry the exact command preamble and complete output"
        );
        assert!(raw.len() > crate::script::OUTPUT_LIMIT_BYTES);
        assert!(
            error.starts_with("[ERROR: exit code 7"),
            "the failure status must be preserved when the spill succeeds"
        );

        // A file where the tools-output directory belongs makes directory
        // creation fail deterministically, on success and on failure alike.
        let blocked_session = session_at(&root, "blocked");
        std::fs::create_dir_all(&blocked_session).unwrap();
        let blocked_output = session_output(&blocked_session);
        std::fs::write(&blocked_output, "not a directory").unwrap();

        let success_command =
            "printf 'SUCCESS-HEAD\\n'; printf 's%.0s' {1..20000}; printf '\\nSUCCESS-TAIL\\n'";
        let result = run_bash_in_session(&root, &blocked_session, success_command, 7, 1)
            .expect("a successful command must keep its success status when the spill fails");
        assert!(
            result.len() <= crate::script::OUTPUT_LIMIT_BYTES,
            "the failure digest must stay within the delivery cap: {}",
            result.len()
        );
        assert!(result.is_char_boundary(result.len()));
        assert!(result.contains("SUCCESS-HEAD") && result.contains("SUCCESS-TAIL"));
        let lowered = result.to_lowercase();
        assert!(
            lowered.contains("could not save") || lowered.contains("no spill file"),
            "the digest must report the spill failure explicitly: {result:?}"
        );
        assert!(
            !result.contains("saved to"),
            "a failed spill must not claim a saved path: {result:?}"
        );
        assert!(
            !result.contains("step-7-call-1.log"),
            "a failed spill must not name a path it did not save: {result:?}"
        );
        assert!(
            !blocked_output.join("step-7-call-1.log").exists(),
            "a failed spill must not leave a file behind: {result:?}"
        );
        assert!(
            !root.join(".ejaj/tool-output").exists(),
            "a failed spill must not fall back to the global location"
        );

        let failure_command =
            "printf 'FAIL-HEAD\\n'; printf 'f%.0s' {1..20000}; printf '\\nFAIL-TAIL\\n'; exit 9";
        let error = run_bash_in_session(&root, &blocked_session, failure_command, 8, 1)
            .expect_err("a failing command must keep its error status when the spill fails");
        assert!(
            error.starts_with("[ERROR: exit code 9 for `"),
            "the command failure information must precede the digest: {error:?}"
        );
        assert!(
            error.len() <= crate::script::OUTPUT_LIMIT_BYTES,
            "the failure digest must stay within the delivery cap: {}",
            error.len()
        );
        assert!(error.contains("FAIL-HEAD") && error.contains("FAIL-TAIL"));
        let lowered = error.to_lowercase();
        assert!(
            lowered.contains("could not save") || lowered.contains("no spill file"),
            "the digest must report the spill failure explicitly: {error:?}"
        );
        assert!(
            !error.contains("saved to"),
            "a failed spill must not claim a saved path: {error:?}"
        );
        assert!(
            !error.contains("step-8-call-1.log"),
            "a failed spill must not name a path it did not save: {error:?}"
        );
        assert!(
            !blocked_output.join("step-8-call-1.log").exists(),
            "a failed spill must not leave a file behind: {error:?}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Core entry-point behavior: the context-free dispatch carries only the
    /// existing one-based step and call index -- it stays copyable and has no
    /// session destination -- so an oversized result reports the missing
    /// destination without falling back to a project-global write.
    #[test]
    fn when_bash_output_given_existing_call_indices_then_keeps_context_free_dispatch_contract() {
        let context = ToolContext {
            step: 2,
            call_index: 1,
        };
        let copied = context;
        assert_eq!(
            copied, context,
            "the public tool context must keep its copy semantics"
        );
        assert_eq!(
            ToolContext::default(),
            ToolContext {
                step: 1,
                call_index: 1,
            },
            "the default context must keep the existing one-based minimum"
        );

        let public = Tool::Bash(BashArgs {
            command: "printf 'legacy'".to_string(),
        })
        .run_with_context(Phase::Code, context)
        .expect("the context-free dispatch must still run an ordinary Bash command");
        assert_eq!(
            public, "legacy",
            "the context-free dispatch must return the command output unchanged"
        );

        let root = scratch_dir("bash-no-session");
        let command = repeat_command('n', crate::script::OUTPUT_LIMIT_BYTES + 1);

        let result = run_bash_without_session(&root, &command, 2, 1)
            .expect("a successful command must keep its success status when no session exists");

        assert!(
            result.len() <= crate::script::OUTPUT_LIMIT_BYTES,
            "the failure digest must stay within the delivery cap: {}",
            result.len()
        );
        let lowered = result.to_lowercase();
        assert!(
            lowered.contains("could not save") || lowered.contains("no spill file"),
            "the digest must report that no spill was written: {result:?}"
        );
        assert!(
            !result.contains("saved to"),
            "no session must not produce a saved path: {result:?}"
        );
        // The candidate file name the dispatch would have used at this
        // step/call index, had any unscoped fallback been taken.
        let candidate = "step-2-call-1.log";
        let expected_candidate = root.join(".ejaj/tool-output").join(candidate);

        assert!(
            !root.join("tools-output").exists(),
            "no session must not write to a project-root tools-output directory: {result:?}"
        );
        assert!(
            !root.join(".ejaj/tool-output").exists(),
            "no session must not fall back to the global spill directory"
        );
        assert!(
            !expected_candidate.exists(),
            "no session must not leave the expected candidate spill file {}: {result:?}",
            expected_candidate.display()
        );
        assert!(
            !root.join(".ejaj").join(candidate).exists(),
            "no session must not write a spill file directly under .ejaj/: {result:?}"
        );
        let direct_entries: Vec<_> = std::fs::read_dir(root.join(".ejaj"))
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.path())
                    .filter(|path| path.is_file())
                    .collect()
            })
            .unwrap_or_default();
        assert!(
            direct_entries.is_empty(),
            "no session must not leave any direct spill files under .ejaj/: {direct_entries:?}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Verification-gate regression: the separate gate path returns complete
    /// output and never spills it.
    #[test]
    fn when_verification_gate_given_oversized_output_then_returns_complete_without_spill() {
        let bytes = 20_000;
        let command = format!(
            "printf 'GATE-HEAD\\n'; printf 'g%.0s' {{1..{bytes}}}; printf '\\nGATE-TAIL\\n'"
        );

        let result = run_command(&command).expect("the verification gate should succeed");

        let expected = format!("GATE-HEAD\n{}\nGATE-TAIL\n", "g".repeat(bytes));
        assert_eq!(
            result, expected,
            "the verification gate must return its complete output unchanged"
        );
        assert!(
            result.len() > crate::script::OUTPUT_LIMIT_BYTES,
            "the regression case must exceed the model delivery cap"
        );
        assert!(
            !result.contains("full output saved"),
            "the verification gate must never spill or name a saved file"
        );
    }

    /// Empty/zero boundary: an empty result is empty, and no session or spill
    /// directory is created just to carry nothing.
    #[test]
    fn when_bash_output_given_empty_result_then_returns_without_spill() {
        let root = scratch_dir("bash-empty");
        let session = session_at(&root, "run");

        let result = run_bash_in_session(&root, &session, "true", 1, 1)
            .expect("the empty command should succeed");

        assert_eq!(result, "");
        assert!(
            !session.exists(),
            "an empty result must not create the supplied session directory"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Minimum valid boundary: output of exactly the delivery cap is handed
    /// back unchanged, with no session directory or spill file beside it.
    #[test]
    fn when_bash_output_given_exact_limit_then_returns_unchanged_without_spill() {
        let root = scratch_dir("bash-limit");
        let session = session_at(&root, "run");
        let command = repeat_command('a', crate::script::OUTPUT_LIMIT_BYTES);

        let result = run_bash_in_session(&root, &session, &command, 1, 1)
            .expect("the command should succeed");

        assert_eq!(result, "a".repeat(crate::script::OUTPUT_LIMIT_BYTES));
        assert!(
            !session.exists(),
            "output at the cap must not create the supplied session directory"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Upper boundary: one byte over the cap is spilled with the exact command
    /// preamble and complete output inside the supplied session directory.
    #[test]
    fn when_bash_output_given_active_session_then_saves_under_that_session_directory() {
        let root = scratch_dir("bash-one-byte-over");
        let session = session_at(&root, "run");
        let command = repeat_command('b', crate::script::OUTPUT_LIMIT_BYTES + 1);

        let result = run_bash_in_session(&root, &session, &command, 2, 3)
            .expect("the command should succeed");

        assert!(
            result.contains(".ejaj/run/tools-output/step-2-call-3.log"),
            "one byte over the cap must name the session spill file: {result:?}"
        );
        assert!(
            !result.contains("Command:") && !result.contains("Output:"),
            "the digest must not carry the file preamble: {result:?}"
        );
        let spill = session_output(&session).join("step-2-call-3.log");
        assert_eq!(
            std::fs::read(&spill).unwrap(),
            spilled_bytes(
                &command,
                "b".repeat(crate::script::OUTPUT_LIMIT_BYTES + 1).as_bytes()
            ),
            "the spill file must carry the exact command preamble and complete output"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Malformed payload boundary: malformed Bash arguments stay a parse error,
    /// and invalid raw output bytes stay intact in the saved log.
    #[test]
    fn when_bash_output_given_malformed_arguments_then_retains_parse_error() {
        let parse_error = match Tool::parse("bash", r#"{"command":42}"#, Phase::Code, Source::None)
        {
            Err(error) => error,
            Ok(_) => panic!("malformed Bash arguments must stay a parse error"),
        };
        assert!(
            !parse_error.is_empty(),
            "a malformed payload must be reported to the model"
        );

        let root = scratch_dir("bash-malformed");
        let session = session_at(&root, "run");
        let command =
            "printf 'RAW\\n'; head -c 20000 /dev/zero | tr '\\0' '\\376'; printf '\\nRAW-END\\n'";
        let result = run_bash_in_session(&root, &session, command, 5, 1)
            .expect("the command should succeed");
        assert!(
            result.len() <= crate::script::OUTPUT_LIMIT_BYTES,
            "the digest must stay within the delivery cap: {}",
            result.len()
        );

        let mut expected_output = b"RAW\n".to_vec();
        expected_output.extend(std::iter::repeat_n(0xfe, 20_000));
        expected_output.extend_from_slice(b"\nRAW-END\n");
        let raw = std::fs::read(session_output(&session).join("step-5-call-1.log"))
            .expect("the spill file must exist");
        assert_eq!(
            raw,
            spilled_bytes(command, &expected_output),
            "invalid raw output bytes must remain intact after the complete command preamble"
        );
        assert!(
            raw.contains(&0xfe),
            "the spill file must keep an invalid UTF-8 byte verbatim"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Authorisation boundary: a read-only pass never gains the Bash tool, no
    /// matter that a session spill area now exists beside it.
    #[test]
    fn when_bash_is_requested_given_read_only_phase_then_remains_forbidden() {
        for phase in [
            Phase::Plan,
            Phase::Spec,
            Phase::Tickets,
            Phase::Triage,
            Phase::Review,
            Phase::Summary,
        ] {
            let error = match Tool::parse("bash", r#"{"command":"echo hi"}"#, phase, Source::None) {
                Err(error) => error,
                Ok(_) => panic!("bash must stay forbidden in {phase:?}"),
            };
            assert!(
                error.contains("only available in the code phase"),
                "the read-only boundary must be named for {phase:?}: {error}"
            );
        }
    }

    /// Upper boundary: a long originating command is saved verbatim, never
    /// truncated, inside the supplied session directory.
    #[test]
    fn when_bash_output_given_long_command_then_saves_command_without_truncation() {
        let root = scratch_dir("bash-long-command");
        let session = session_at(&root, "run");
        let command = format!(
            "{}true; {}",
            "true && ".repeat(400),
            repeat_command('q', crate::script::OUTPUT_LIMIT_BYTES + 1)
        );
        assert!(command.len() > BASH_FAILURE_COMMAND_BYTES);

        let result = run_bash_in_session(&root, &session, &command, 5, 3)
            .expect("the command should succeed");

        assert!(
            result.len() <= crate::script::OUTPUT_LIMIT_BYTES,
            "the digest must stay within the delivery cap: {}",
            result.len()
        );
        assert!(
            result.contains(".ejaj/run/tools-output/step-5-call-3.log"),
            "the digest must name the root-relative session spill path: {result:?}"
        );
        let expected_output = "q".repeat(crate::script::OUTPUT_LIMIT_BYTES + 1);
        let raw = std::fs::read(session_output(&session).join("step-5-call-3.log"))
            .expect("the spill file must exist");
        assert_eq!(
            raw,
            spilled_bytes(&command, expected_output.as_bytes()),
            "the spill file must carry the whole long command and complete output"
        );
        assert!(
            raw.starts_with(format!("Command:\n{command}\n\nOutput:\n").as_bytes()),
            "the long command must not be truncated in the preamble"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// One valid `finish_spec` payload: the specification as a markdown document.
    fn valid_spec_arguments() -> String {
        let spec = "\
# Typed specification handoff

## 1. Problem Statement

The spec handoff is free text.

## 2. Solution Overview

The handoff carries a typed document.

## 3. User Stories & Acceptance Criteria

1. **Typed handoff**

The spec agent hands over a specification object.

- A complete document parses.
- An incomplete document is a parse error.

## 4. Implementation Decisions

### Module Architecture

A new `specification` module owns the document.

### Data Models & Contracts

`Specification` with one field per section.

### ADR Compliance

Mirrors the typed tool-argument convention.

## 5. Testing Decisions & Seams

### Test Seams

`Specification::to_markdown`.

### Coverage Strategy

Unit tests over the renderer.

### Prior Art

The `mod tests` block in `tools.rs`.

## 6. Out of Scope

- Content validation.

## 7. Deferred Items & Operational Risks

### Operational Risks

Providers may reject nested schemas.

### Follow-Up Work

None.
";
        serde_json::json!({ "spec": spec }).to_string()
    }

    /// One valid `finish_tickets` payload: a two-ticket set with an edge between them.
    fn valid_ticket_arguments() -> String {
        let set = serde_json::json!({
            "tickets": {
                "tickets": [
                    {
                        "id": "1-first",
                        "acceptance_criteria": ["The first criterion."],
                        "blocked_by": [],
                        "blocks": ["2-second"],
                        "title": "First",
                        "objective": "Do the first thing.",
                        "prerequisites_assumed": "Nothing.",
                        "out_of_scope": [],
                        "scope_of_changes": ["One change."],
                        "test_expectations": ["when_first_runs_given_setup_then_succeeds"],
                        "verification_gate": "cargo check"
                    },
                    {
                        "id": "2-second",
                        "acceptance_criteria": ["The second criterion."],
                        "blocked_by": ["1-first"],
                        "blocks": [],
                        "title": "Second",
                        "objective": "Do the second thing.",
                        "prerequisites_assumed": "The first ticket landed.",
                        "out_of_scope": [],
                        "scope_of_changes": ["Another change."],
                        "test_expectations": ["when_second_runs_given_first_then_succeeds"],
                        "verification_gate": "cargo test"
                    }
                ]
            }
        });
        set.to_string()
    }

    /// The names advertised to the model, in order.
    fn names(phase: Phase) -> Vec<&'static str> {
        Tool::definitions(phase)
            .into_iter()
            .map(|definition| definition.function.name)
            .collect()
    }

    #[test]
    fn each_phase_gets_only_its_own_control_tool() {
        assert_eq!(names(Phase::Plan), ["execute_script", "finish_plan"]);
        assert_eq!(names(Phase::Spec), ["execute_script", "finish_spec"]);
        assert_eq!(names(Phase::Tickets), ["execute_script", "finish_tickets"]);
        assert_eq!(names(Phase::Review), ["execute_script", "finish_review"]);
        assert_eq!(names(Phase::Summary), ["execute_script"]);
        assert_eq!(
            names(Phase::Code),
            ["bash", "execute_script", "task_complete"]
        );
    }

    /// The summary pass is a read-only inspection pass: it is offered
    /// `execute_script` and nothing else -- no `bash`, no completion, no
    /// handoff -- and its script capabilities are read-only.
    #[test]
    fn the_summary_phase_offers_only_a_read_only_execute_script() {
        assert_eq!(names(Phase::Summary), ["execute_script"]);

        for name in [
            "bash",
            "finish_plan",
            "finish_spec",
            "finish_tickets",
            "finish_review",
            "task_complete",
        ] {
            let arguments = if name == "bash" {
                r#"{"command":"ls"}"#
            } else {
                "{}"
            };
            let error = match Tool::parse(name, arguments, Phase::Summary, Source::None) {
                Err(error) => error,
                Ok(_) => panic!("{name} must not parse in the summary phase"),
            };
            assert!(error.contains(name), "{name}: {error}");
        }

        assert!(matches!(
            Tool::parse(
                "execute_script",
                r#"{"code":"return tools.diff();"}"#,
                Phase::Summary,
                Source::None
            ),
            Ok(Tool::ExecuteScript(_))
        ));

        let caps = Capabilities::for_phase(Phase::Summary);
        assert!(
            !caps.writable,
            "the summary phase must have read-only script capabilities"
        );
    }

    #[test]
    fn bash_is_rejected_outside_the_code_phase() {
        for phase in [
            Phase::Plan,
            Phase::Spec,
            Phase::Tickets,
            Phase::Review,
            Phase::Summary,
        ] {
            let err = match Tool::parse("bash", r#"{"command":"ls"}"#, phase, Source::None) {
                Err(e) => e,
                Ok(_) => panic!("`bash` must be rejected in {phase:?}"),
            };
            assert!(
                err.contains("only available in the code phase"),
                "{phase:?}: {err}"
            );
            assert!(err.contains("execute_script"), "{phase:?}: {err}");
        }
    }

    /// A strict provider (LM Studio) validates the tool list itself and rejects
    /// a `parameters` schema without an object `properties` -- which is what
    /// `schemars` emits for the empty argument struct of `task_complete`. Every
    /// advertised tool must be admissible, so every schema must carry the key.
    #[test]
    fn every_advertised_tool_schema_has_properties() {
        for phase in [
            Phase::Plan,
            Phase::Spec,
            Phase::Tickets,
            Phase::Code,
            Phase::Review,
            Phase::Summary,
        ] {
            for definition in Tool::definitions(phase) {
                let schema = serde_json::to_value(&definition.function.parameters).unwrap();
                assert!(
                    schema["properties"].is_object(),
                    "`{phase:?}` advertised `{}` with a schema that has no `properties` object: \
                     {schema}",
                    definition.function.name
                );
            }
        }
    }

    #[test]
    fn no_arg_encodings_all_parse() {
        for arguments in ["", "{}", "null", "  null  "] {
            assert!(matches!(
                Tool::parse("task_complete", arguments, Phase::Code, Source::None),
                Ok(Tool::TaskComplete(_))
            ));
        }
    }

    /// One valid `finish_plan` payload: the agreed plan as a markdown document.
    const VALID_PLAN_ARGUMENTS: &str = r##"{"plan":"# Typed plan handoff\n\n## 1. Decisions\n\n1. **Add the document type**\n\nA new `plan_document` module owns the contract.\n\n2. **Render at the boundary**\n\n`main` renders the plan into `plan.md`.\n\n## 2. Defaults Chosen\n\n- Keep `write_handoff`'s `text: &str` signature.\n\n## 3. Constraints\n\n- No new dependencies.\n\n## 4. Open Questions\n\n- Should parsing validate bullet content? The fact needed to settle it: whether the spec agent depends on it.\n"}"##;

    /// The handoff carries the parsed plan and nothing else: the document the
    /// spec agent receives is re-rendered from it.
    #[test]
    fn finish_plan_carries_the_plan_it_was_given() {
        match Tool::parse(
            "finish_plan",
            VALID_PLAN_ARGUMENTS,
            Phase::Plan,
            Source::None,
        ) {
            Ok(Tool::FinishPlan(plan)) => {
                assert_eq!(plan.goal, "Typed plan handoff");
                assert_eq!(plan.decisions.len(), 2);
                assert_eq!(plan.decisions[0].title, "Add the document type");
                assert_eq!(
                    plan.decisions[1].detail,
                    "`main` renders the plan into `plan.md`."
                );
                assert_eq!(
                    plan.defaults,
                    ["Keep `write_handoff`'s `text: &str` signature."]
                );
                assert_eq!(plan.constraints, ["No new dependencies."]);
                assert_eq!(
                    plan.open_questions,
                    [
                        "Should parsing validate bullet content? The fact needed to settle it: whether the spec agent depends on it."
                    ]
                );
            }
            _ => panic!("expected `finish_plan` with its plan payload"),
        }
    }

    /// No plan document means no handoff: the interview is not ended and no
    /// blank task reaches the spec agent.
    #[test]
    fn finish_plan_without_a_complete_plan_is_an_error() {
        for arguments in ["", "null", "{}", r#"{"plan":{}}"#, r#"{"plan":[]}"#] {
            let error = match Tool::parse("finish_plan", arguments, Phase::Plan, Source::None) {
                Err(e) => e,
                Ok(_) => panic!("`{}` must not hand off a plan", arguments),
            };
            assert!(
                error.contains("not a valid JSON string"),
                "`{}` must tell the model the argument is a JSON string: {error}",
                arguments
            );
        }

        let no_headings = r##"{"plan":"the plan as prose, with no headings"}"##;
        let error = match Tool::parse("finish_plan", no_headings, Phase::Plan, Source::None) {
            Err(e) => e,
            Ok(_) => panic!("prose must not hand off a plan"),
        };
        assert!(error.contains("could not be parsed"), "{error}");
        assert!(error.contains("missing title"), "{error}");

        let missing_section =
            r##"{"plan":"# g\n\n## 2. Defaults Chosen\n\nNone.\n\n## 3. Constraints\n\nNone.\n"}"##;
        let error = match Tool::parse("finish_plan", missing_section, Phase::Plan, Source::None) {
            Err(e) => e,
            Ok(_) => panic!("a missing section must not hand off a plan"),
        };
        assert!(
            error.contains("missing section \"## 1. Decisions\""),
            "{error}"
        );
    }

    /// Only the template is required: a document whose sections are all `None.`
    /// is still a valid handoff, because content is not validated.
    #[test]
    fn a_plan_of_empty_sections_parses() {
        let arguments = r##"{"plan":"# \n\n## 1. Decisions\n\nNone.\n\n## 2. Defaults Chosen\n\nNone.\n\n## 3. Constraints\n\nNone.\n\n## 4. Open Questions\n\nNone.\n"}"##;
        assert!(matches!(
            Tool::parse("finish_plan", arguments, Phase::Plan, Source::None),
            Ok(Tool::FinishPlan(_))
        ));
    }

    /// The handoff carries the parsed specification object and nothing else: the
    /// document the tickets agent receives is rendered from it.
    #[test]
    fn finish_spec_carries_the_spec_it_was_given() {
        match Tool::parse(
            "finish_spec",
            &valid_spec_arguments(),
            Phase::Spec,
            Source::None,
        ) {
            Ok(Tool::FinishSpec(spec)) => {
                assert_eq!(spec.feature_name, "Typed specification handoff");
                assert_eq!(spec.user_stories.len(), 1);
            }
            _ => panic!("expected `finish_spec` with its spec payload"),
        }
    }

    /// No structurally valid specification document means no handoff: the spec
    /// pass is not ended and no empty task reaches the tickets agent.
    #[test]
    fn finish_spec_without_a_usable_spec_is_an_error() {
        for arguments in ["", "null", "{}", r#"{"spec":{}}"#, r#"{"spec":[]}"#] {
            let error = match Tool::parse("finish_spec", arguments, Phase::Spec, Source::None) {
                Err(e) => e,
                Ok(_) => panic!("`{}` must not hand off a specification", arguments),
            };
            assert!(
                error.contains("not a valid JSON string"),
                "`{}` must tell the model the argument is a JSON string: {error}",
                arguments
            );
        }

        let no_headings = r#"{"spec":"the specification as prose, with no headings"}"#;
        let error = match Tool::parse("finish_spec", no_headings, Phase::Spec, Source::None) {
            Err(e) => e,
            Ok(_) => panic!("prose must not hand over a specification"),
        };
        assert!(error.contains("could not be parsed"), "{error}");
        assert!(error.contains("missing title"), "{error}");

        let missing_section =
            serde_json::json!({ "spec": "# g\n\n## 2. Solution Overview\n\nNone.\n" }).to_string();
        let error = match Tool::parse("finish_spec", &missing_section, Phase::Spec, Source::None) {
            Err(e) => e,
            Ok(_) => panic!("a missing section must not hand over a specification"),
        };
        assert!(
            error.contains("missing section \"## 1. Problem Statement\""),
            "{error}"
        );
    }

    /// The handoff carries exactly the set it was given: the order of the
    /// tickets and both dependency lists survive the parse.
    #[test]
    fn finish_tickets_carries_the_set_it_was_given() {
        match Tool::parse(
            "finish_tickets",
            &valid_ticket_arguments(),
            Phase::Tickets,
            Source::None,
        ) {
            Ok(Tool::FinishTickets(set)) => {
                assert_eq!(set.tickets.len(), 2);
                assert_eq!(set.tickets[0].id, "1-first");
                assert_eq!(set.tickets[1].id, "2-second");
                assert_eq!(set.tickets[0].blocks, ["2-second"]);
                assert_eq!(set.tickets[1].blocked_by, ["1-first"]);
                assert_eq!(set.tickets[0].acceptance_criteria, ["The first criterion."]);
                assert_eq!(
                    set.tickets[1].acceptance_criteria,
                    ["The second criterion."]
                );
            }
            _ => panic!("expected `finish_tickets` with its ticket set"),
        }
    }

    /// A payload that does not deserialize, and a payload that violates any of
    /// the six validation rules, both come back as an error naming what to fix.
    #[test]
    fn finish_tickets_without_a_usable_set_is_an_error() {
        for arguments in ["", "null", "{}", r#"{"tickets":{}}"#, r#"{"tickets":[]}"#] {
            let error = match Tool::parse("finish_tickets", arguments, Phase::Tickets, Source::None)
            {
                Err(e) => e,
                Ok(_) => panic!("`{}` must not hand over tickets", arguments),
            };
            assert!(
                error.contains("could not be parsed"),
                "`{}` must tell the model what to fix: {error}",
                arguments
            );
        }

        let empty_set = serde_json::json!({ "tickets": { "tickets": [] } }).to_string();
        let error = match Tool::parse("finish_tickets", &empty_set, Phase::Tickets, Source::None) {
            Err(e) => e,
            Ok(_) => panic!("an empty set must not hand over tickets"),
        };
        assert!(error.contains("empty"), "{error}");

        let mut set: serde_json::Value = serde_json::from_str(&valid_ticket_arguments()).unwrap();
        set["tickets"]["tickets"][1]["verification_gate"] = serde_json::json!("  ");
        let error = match Tool::parse(
            "finish_tickets",
            &set.to_string(),
            Phase::Tickets,
            Source::None,
        ) {
            Err(e) => e,
            Ok(_) => panic!("a blank gate must not hand over tickets"),
        };
        assert!(error.contains("verification gate"), "{error}");
    }

    /// A control tool is only available in its own phase; in any other phase the
    /// message says exactly that and carries no phase-specific state.
    #[test]
    fn a_control_tool_from_the_other_phase_is_an_error() {
        for phase in [
            Phase::Plan,
            Phase::Spec,
            Phase::Tickets,
            Phase::Code,
            Phase::Review,
            Phase::Summary,
        ] {
            for name in [
                "finish_plan",
                "finish_spec",
                "finish_tickets",
                "finish_review",
                "task_complete",
            ] {
                let own = matches!(
                    (name, phase),
                    ("finish_plan", Phase::Plan)
                        | ("finish_spec", Phase::Spec)
                        | ("finish_tickets", Phase::Tickets)
                        | ("finish_review", Phase::Review)
                        | ("task_complete", Phase::Code)
                );
                if own {
                    continue;
                }
                let error = match Tool::parse(name, "{}", phase, Source::None) {
                    Err(error) => error,
                    Ok(_) => panic!("`{name}` must not parse in {phase:?}"),
                };
                assert!(error.contains(name), "{name} in {phase:?}: {error}");
                assert!(
                    error.contains("not available in this phase"),
                    "{name} in {phase:?}: {error}"
                );
            }
        }
    }

    #[test]
    fn control_tools_are_never_run_as_commands() {
        assert!(
            Tool::parse(
                "finish_plan",
                VALID_PLAN_ARGUMENTS,
                Phase::Plan,
                Source::None
            )
            .unwrap()
            .run(Phase::Plan)
            .is_err()
        );
        assert!(
            Tool::parse(
                "finish_spec",
                &valid_spec_arguments(),
                Phase::Spec,
                Source::None
            )
            .unwrap()
            .run(Phase::Spec)
            .is_err()
        );
        assert!(
            Tool::parse(
                "finish_tickets",
                &valid_ticket_arguments(),
                Phase::Tickets,
                Source::None
            )
            .unwrap()
            .run(Phase::Tickets)
            .is_err()
        );
        assert!(
            Tool::parse("task_complete", "", Phase::Code, Source::None)
                .unwrap()
                .run(Phase::Code)
                .is_err()
        );
    }

    #[test]
    fn an_unknown_tool_is_still_an_error() {
        assert!(Tool::parse("nope", "{}", Phase::Plan, Source::None).is_err());
    }

    /// The planning tool must carry the same immediate-handoff criterion as the
    /// system prompt -- call it as soon as nothing blocks immediate work, never
    /// as a redundant confirmation turn -- while retaining its role and the
    /// complete markdown plan contract the spec agent receives.
    #[test]
    fn the_plan_tool_description_carries_the_immediate_handoff_criterion() {
        let definition = Tool::definitions(Phase::Plan)
            .into_iter()
            .find(|definition| definition.function.name == "finish_plan")
            .expect("the plan phase advertises `finish_plan`");
        let description = definition.function.description.to_lowercase();

        assert!(
            description.contains("immediately") && description.contains("blocking decision"),
            "the tool must hand off as soon as no blocking decision remains: {description}"
        );
        assert!(
            description.contains("do not summarize") && description.contains("confirmation"),
            "the tool must reject a redundant confirmation turn: {description}"
        );
        assert!(
            description.contains("never implement anything yourself"),
            "the tool must keep its role: the plan agent never implements: {description}"
        );
        assert!(
            description.contains("complete plan as a markdown document"),
            "the complete markdown plan contract must survive: {description}"
        );
    }

    /// The `finish_spec` handoff identifies its true reader: the tickets agent
    /// receives the specification and slices it into tickets, while the code
    /// agent receives tickets and never sees the specification.
    #[test]
    fn the_finish_spec_description_names_the_tickets_agent_as_the_reader() {
        let definition = Tool::definitions(Phase::Spec)
            .into_iter()
            .find(|definition| definition.function.name == "finish_spec")
            .expect("the spec phase advertises `finish_spec`");
        let description = definition.function.description.to_lowercase();

        assert!(
            description.contains("tickets agent") && description.contains("receives"),
            "the tool must name the tickets agent as the reader: {description}"
        );
        assert!(
            description.contains("slices it into the tickets"),
            "the tool must say the tickets agent slices the specification into tickets: \
             {description}"
        );
        assert!(
            description.contains("code agent") && description.contains("never sees"),
            "the tool must say the code agent never sees the specification: {description}"
        );
    }

    /// The generated ticket schema is the contract the tickets agent fills: it
    /// requires the acceptance criteria and no longer exposes `spec_ref`.
    #[test]
    fn the_finish_tickets_schema_requires_acceptance_criteria_and_drops_spec_ref() {
        let definition = Tool::definitions(Phase::Tickets)
            .into_iter()
            .find(|definition| definition.function.name == "finish_tickets")
            .expect("the tickets phase advertises `finish_tickets`");
        let schema = serde_json::to_value(&definition.function.parameters).unwrap();
        let ticket = &schema["$defs"]["Ticket"];

        let required: Vec<&str> = ticket["required"]
            .as_array()
            .expect("the ticket schema lists required fields")
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect();
        assert!(
            required.contains(&"acceptance_criteria"),
            "the ticket schema must require `acceptance_criteria`: {required:?}"
        );
        assert!(
            !required.contains(&"spec_ref"),
            "the ticket schema must not require `spec_ref`: {required:?}"
        );

        let properties = ticket["properties"]
            .as_object()
            .expect("the ticket schema lists properties");
        assert!(properties.contains_key("acceptance_criteria"));
        assert!(!properties.contains_key("spec_ref"));
    }

    // -----------------------------------------------------------------------
    // Source-aware handoff coverage
    // -----------------------------------------------------------------------

    use crate::plan_document::PlanDecision;
    use crate::specification::{
        DeferredItems, ImplementationDecisions, TestingDecisions, UserStory,
    };
    use crate::tickets::Ticket;

    /// A plan whose single decision is exactly what `valid_spec_arguments`
    /// describes, so the valid specification covers it.
    fn covering_plan() -> Plan {
        Plan {
            goal: "Typed specification handoff".to_string(),
            decisions: vec![PlanDecision {
                title: "Add the document type".to_string(),
                detail: "A new `specification` module owns the document.".to_string(),
            }],
            defaults: Vec::new(),
            constraints: Vec::new(),
            open_questions: Vec::new(),
        }
    }

    /// The typed specification of the valid `finish_spec` payload, ready to be
    /// used as the source of a tickets pass.
    fn covering_specification() -> Specification {
        match Tool::parse(
            "finish_spec",
            &valid_spec_arguments(),
            Phase::Spec,
            Source::None,
        ) {
            Ok(Tool::FinishSpec(specification)) => specification,
            _ => panic!("the valid specification payload must parse"),
        }
    }

    /// The specification's only story carries two criteria; this payload copies
    /// both of them exactly.
    fn covering_ticket_arguments() -> String {
        serde_json::json!({
            "tickets": {
                "tickets": [
                    {
                        "id": "1-typed-handoff",
                        "acceptance_criteria": [
                            "A complete document parses.",
                            "An incomplete document is a parse error."
                        ],
                        "blocked_by": [],
                        "blocks": [],
                        "title": "Hand over the typed specification",
                        "objective": "Carry the specification document across the boundary.",
                        "prerequisites_assumed": "Nothing.",
                        "out_of_scope": [],
                        "scope_of_changes": ["Parse the specification document."],
                        "test_expectations": ["when_parsing_spec_given_complete_document_then_succeeds"],
                        "verification_gate": "cargo check"
                    }
                ]
            }
        })
        .to_string()
    }

    /// The plan source reaches `finish_spec`: a specification that covers each
    /// decision is accepted.
    #[test]
    fn finish_spec_is_accepted_when_it_covers_the_plan() {
        let plan = covering_plan();
        assert!(matches!(
            Tool::parse(
                "finish_spec",
                &valid_spec_arguments(),
                Phase::Spec,
                Source::Plan(&plan)
            ),
            Ok(Tool::FinishSpec(_))
        ));
    }

    /// A decision the specification never accounts for is rejected, and the
    /// error names the uncovered decision by title.
    #[test]
    fn finish_spec_is_rejected_when_it_drops_a_plan_decision() {
        let mut plan = covering_plan();
        plan.decisions.push(PlanDecision {
            title: "Ship telemetry".to_string(),
            detail: "Report anonymised usage counts to the fleet endpoint.".to_string(),
        });

        let error = match Tool::parse(
            "finish_spec",
            &valid_spec_arguments(),
            Phase::Spec,
            Source::Plan(&plan),
        ) {
            Err(error) => error,
            Ok(_) => panic!("an uncovered decision must not hand over a specification"),
        };
        assert!(error.contains("Ship telemetry"), "{error}");
        assert!(!error.contains("Add the document type"), "{error}");
    }

    /// The specification source reaches `finish_tickets`: a set that carries
    /// every story criterion exactly is accepted.
    #[test]
    fn finish_tickets_is_accepted_when_it_covers_the_specification() {
        let specification = covering_specification();
        assert!(matches!(
            Tool::parse(
                "finish_tickets",
                &covering_ticket_arguments(),
                Phase::Tickets,
                Source::Specification(&specification)
            ),
            Ok(Tool::FinishTickets(_))
        ));
    }

    /// A criterion the set dropped is rejected, and the error quotes the exact
    /// specification text.
    #[test]
    fn finish_tickets_is_rejected_when_it_drops_a_criterion() {
        let specification = covering_specification();
        let mut set: serde_json::Value =
            serde_json::from_str(&covering_ticket_arguments()).unwrap();
        set["tickets"]["tickets"][0]["acceptance_criteria"] =
            serde_json::json!(["A complete document parses."]);

        let error = match Tool::parse(
            "finish_tickets",
            &set.to_string(),
            Phase::Tickets,
            Source::Specification(&specification),
        ) {
            Err(error) => error,
            Ok(_) => panic!("a dropped criterion must not hand over tickets"),
        };
        assert!(
            error.contains("An incomplete document is a parse error."),
            "{error}"
        );
        assert!(error.contains("Typed handoff"), "{error}");
    }

    /// A criterion the set rewrote is a different string and is rejected the
    /// same way.
    #[test]
    fn finish_tickets_is_rejected_when_it_alters_a_criterion() {
        let specification = covering_specification();
        let mut set: serde_json::Value =
            serde_json::from_str(&covering_ticket_arguments()).unwrap();
        set["tickets"]["tickets"][0]["acceptance_criteria"] = serde_json::json!([
            "A complete document parses.",
            "An incomplete document is an error."
        ]);

        let error = match Tool::parse(
            "finish_tickets",
            &set.to_string(),
            Phase::Tickets,
            Source::Specification(&specification),
        ) {
            Err(error) => error,
            Ok(_) => panic!("an altered criterion must not hand over tickets"),
        };
        assert!(
            error.contains("An incomplete document is a parse error."),
            "{error}"
        );
    }

    /// Formatting is not alteration: a criterion reflowed or re-spaced between
    /// the specification and the ticket represents the same requirement.
    #[test]
    fn finish_tickets_accepts_a_reflowed_criterion() {
        let specification = covering_specification();
        let mut set: serde_json::Value =
            serde_json::from_str(&covering_ticket_arguments()).unwrap();
        set["tickets"]["tickets"][0]["acceptance_criteria"] = serde_json::json!([
            "A complete document   parses.",
            "An incomplete document is a parse error."
        ]);

        assert!(matches!(
            Tool::parse(
                "finish_tickets",
                &set.to_string(),
                Phase::Tickets,
                Source::Specification(&specification)
            ),
            Ok(Tool::FinishTickets(_))
        ));
    }

    /// The source is phase-appropriate: a plan source does not make the tickets
    /// control tool consult it, and vice versa. The unrelated phases therefore
    /// behave exactly as before.
    #[test]
    fn a_source_from_another_phase_is_not_consulted() {
        let plan = covering_plan();
        assert!(matches!(
            Tool::parse(
                "finish_tickets",
                &valid_ticket_arguments(),
                Phase::Tickets,
                Source::Plan(&plan)
            ),
            Ok(Tool::FinishTickets(_))
        ));

        let specification = covering_specification();
        assert!(matches!(
            Tool::parse(
                "finish_spec",
                &valid_spec_arguments(),
                Phase::Spec,
                Source::Specification(&specification)
            ),
            Ok(Tool::FinishSpec(_))
        ));
    }

    /// The plan and code control tools never need the upstream source, so they
    /// behave the same with or without one.
    #[test]
    fn unrelated_phases_do_not_require_a_source() {
        assert!(matches!(
            Tool::parse(
                "finish_plan",
                VALID_PLAN_ARGUMENTS,
                Phase::Plan,
                Source::None
            ),
            Ok(Tool::FinishPlan(_))
        ));
        assert!(matches!(
            Tool::parse("task_complete", "{}", Phase::Code, Source::None),
            Ok(Tool::TaskComplete(_))
        ));
    }

    // -----------------------------------------------------------------------
    // Coverage unit tests
    // -----------------------------------------------------------------------

    /// A plan whose one decision is the flag.
    fn flag_plan() -> Plan {
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

    /// A specification whose implementation decisions cover the flag decision.
    fn flag_specification() -> Specification {
        Specification {
            feature_name: "A version flag".to_string(),
            problem_statement: "There is no way to print the version.".to_string(),
            solution_overview: "Print the crate version for the --version flag.".to_string(),
            user_stories: vec![UserStory {
                title: "Version output".to_string(),
                requirement: "The binary prints its crate version and exits.".to_string(),
                acceptance_criteria: vec!["`--version` prints the version and exits.".to_string()],
            }],
            implementation_decisions: ImplementationDecisions {
                module_architecture: "Reuse the existing parser.".to_string(),
                data_models_and_contracts: "The version comes from `env!`.".to_string(),
                adr_compliance: "No new dependency.".to_string(),
            },
            testing_decisions: TestingDecisions {
                test_seams: "The parser seam.".to_string(),
                coverage_strategy: "One unit test.".to_string(),
                prior_art: "The CLI tests.".to_string(),
            },
            out_of_scope: vec!["Alternate formats.".to_string()],
            deferred_items: DeferredItems {
                operational_risks: String::new(),
                follow_up_work: String::new(),
            },
        }
    }

    #[test]
    fn a_covered_decision_passes() {
        assert_eq!(
            plan_to_spec(&flag_plan(), &flag_specification()),
            Ok(()),
            "the specification covers the only decision"
        );
    }

    /// Coverage is a matter of substance, not verbatim repetition: the plan
    /// says "add", the specification says "introduce", and the check still
    /// agrees.
    #[test]
    fn coverage_does_not_require_verbatim_repetition() {
        let mut specification = flag_specification();
        specification.user_stories[0].requirement =
            "The tool reports the crate version and quits when asked.".to_string();
        specification.implementation_decisions.module_architecture =
            "Introduce a flag handled by the existing argument parser.".to_string();
        specification
            .implementation_decisions
            .data_models_and_contracts = "Embed the crate version at compile time.".to_string();

        assert_eq!(plan_to_spec(&flag_plan(), &specification), Ok(()));
    }

    /// A decision covered only by the implementation decisions, with no story
    /// saying anything about it, is still covered.
    #[test]
    fn a_decision_covered_in_implementation_content_passes() {
        let mut specification = flag_specification();
        specification.user_stories[0].requirement =
            "The tool answers the version request.".to_string();
        specification.implementation_decisions.module_architecture =
            "Add the flag: the binary prints its crate version and exits.".to_string();

        assert_eq!(plan_to_spec(&flag_plan(), &specification), Ok(()));
    }

    /// A decision covered only by a user story, with the implementation silent
    /// about it, is covered too.
    #[test]
    fn a_decision_covered_in_a_user_story_passes() {
        let mut specification = flag_specification();
        specification.user_stories[0].requirement =
            "Add the flag: the binary prints its crate version and exits.".to_string();
        specification.implementation_decisions.module_architecture =
            "Reuse the parser.".to_string();

        assert_eq!(plan_to_spec(&flag_plan(), &specification), Ok(()));
    }

    /// A plan decision the specification never accounts for is reported by
    /// title.
    #[test]
    fn an_uncovered_decision_is_reported_by_title() {
        let mut plan = flag_plan();
        plan.decisions.push(PlanDecision {
            title: "Ship telemetry".to_string(),
            detail: "Report anonymised usage counts to the fleet endpoint.".to_string(),
        });

        let error = plan_to_spec(&plan, &flag_specification()).expect_err("must be rejected");
        assert!(error.contains("Ship telemetry"), "{error}");
        assert!(!error.contains("Add the flag"), "{error}");
    }

    /// Every uncovered decision is named, not only the first.
    #[test]
    fn every_uncovered_decision_is_named() {
        let mut plan = flag_plan();
        plan.decisions.push(PlanDecision {
            title: "Ship telemetry".to_string(),
            detail: "Report anonymised usage counts to the fleet endpoint.".to_string(),
        });
        plan.decisions.push(PlanDecision {
            title: "Translate the CLI".to_string(),
            detail: "Localise the help output for every locale.".to_string(),
        });

        let error = plan_to_spec(&plan, &flag_specification()).expect_err("must be rejected");
        assert!(error.contains("Ship telemetry"), "{error}");
        assert!(error.contains("Translate the CLI"), "{error}");
        assert!(error.contains("2 uncovered plan decisions"), "{error}");
    }

    /// Several decisions pass when every one of them is covered, proving the
    /// check is per decision rather than per document.
    #[test]
    fn several_covered_decisions_pass() {
        let mut plan = flag_plan();
        plan.decisions.push(PlanDecision {
            title: "Reuse the parser".to_string(),
            detail: "Reuse the existing parser, adding no dependency.".to_string(),
        });

        assert_eq!(plan_to_spec(&plan, &flag_specification()), Ok(()));
    }

    /// The fixture ticket set the tickets-side checks are built on.
    fn ticket_set(criteria: Vec<&str>) -> TicketSet {
        TicketSet {
            tickets: vec![Ticket {
                id: "1-version".to_string(),
                blocked_by: Vec::new(),
                blocks: Vec::new(),
                title: "Add the version flag".to_string(),
                objective: "Print the crate version and exit.".to_string(),
                acceptance_criteria: criteria.into_iter().map(str::to_string).collect(),
                prerequisites_assumed: "The parser builds.".to_string(),
                out_of_scope: Vec::new(),
                scope_of_changes: vec!["Add the flag handle.".to_string()],
                test_expectations: vec![
                    "when_printing_version_given_flag_then_exits_zero".to_string(),
                ],
                verification_gate: "cargo test".to_string(),
            }],
        }
    }

    #[test]
    fn a_criterion_copied_exactly_passes() {
        assert_eq!(
            spec_to_tickets(
                &flag_specification(),
                &ticket_set(vec!["`--version` prints the version and exits."])
            ),
            Ok(())
        );
    }

    /// The criterion is compared after trimming, so surrounding whitespace is
    /// not an alteration.
    #[test]
    fn a_criterion_is_compared_after_trimming() {
        assert_eq!(
            spec_to_tickets(
                &flag_specification(),
                &ticket_set(vec!["  `--version` prints the version and exits.  "])
            ),
            Ok(())
        );
    }

    #[test]
    fn a_missing_criterion_is_reported_with_its_text() {
        let error = spec_to_tickets(&flag_specification(), &ticket_set(vec!["Something else."]))
            .expect_err("must be rejected");
        assert!(
            error.contains("`--version` prints the version and exits."),
            "{error}"
        );
        assert!(error.contains("Version output"), "{error}");
        assert!(error.contains("missing"), "{error}");
    }

    /// Every missing criterion is named, each with its story.
    #[test]
    fn every_missing_criterion_is_named() {
        let mut specification = flag_specification();
        specification.user_stories.push(UserStory {
            title: "Quiet mode".to_string(),
            requirement: "The binary stays quiet unless asked.".to_string(),
            acceptance_criteria: vec!["No output without the verbose flag.".to_string()],
        });

        let error =
            spec_to_tickets(&specification, &ticket_set(Vec::new())).expect_err("must be rejected");
        assert!(
            error.contains("`--version` prints the version and exits."),
            "{error}"
        );
        assert!(
            error.contains("No output without the verbose flag."),
            "{error}"
        );
        assert!(
            error.contains("Quiet mode") && error.contains("Version output"),
            "{error}"
        );
    }

    /// An altered criterion is the same failure: the exact string is absent.
    #[test]
    fn an_altered_criterion_is_rejected() {
        let error = spec_to_tickets(
            &flag_specification(),
            &ticket_set(vec!["`--version` prints the version."]),
        )
        .expect_err("must be rejected");
        assert!(
            error.contains("`--version` prints the version and exits."),
            "{error}"
        );
        assert!(error.contains("altered"), "{error}");
    }

    /// Additional ticket-specific criteria are permitted: they do not replace
    /// the specification's own criterion.
    #[test]
    fn extra_criteria_do_not_substitute_for_the_specification_criterion() {
        let error = spec_to_tickets(
            &flag_specification(),
            &ticket_set(vec!["An extra house rule.", "Another extra rule."]),
        )
        .expect_err("must be rejected");
        assert!(error.contains("prints the version and exits"), "{error}");
    }

    /// A story with no criteria is covered when a ticket's own content makes
    /// its substance discernible.
    #[test]
    fn a_story_without_criteria_is_covered_by_ticket_content() {
        let mut specification = flag_specification();
        specification.user_stories[0].acceptance_criteria.clear();

        assert_eq!(
            spec_to_tickets(&specification, &ticket_set(Vec::new())),
            Ok(())
        );
    }

    /// The same story is uncovered when no ticket mentions it.
    #[test]
    fn an_uncovered_story_is_reported_by_title() {
        let mut specification = flag_specification();
        specification.user_stories[0] = UserStory {
            title: "Stream the output".to_string(),
            requirement: "Stream each log record to the remote collector.".to_string(),
            acceptance_criteria: Vec::new(),
        };

        let error =
            spec_to_tickets(&specification, &ticket_set(Vec::new())).expect_err("must be rejected");
        assert!(error.contains("Stream the output"), "{error}");
        assert!(!error.contains("Version output"), "{error}");
    }

    /// Every uncovered story is named.
    #[test]
    fn every_uncovered_story_is_named() {
        let mut specification = flag_specification();
        specification.user_stories[0].acceptance_criteria.clear();
        specification.user_stories.push(UserStory {
            title: "Stream the output".to_string(),
            requirement: "Stream each log record to the remote collector.".to_string(),
            acceptance_criteria: Vec::new(),
        });

        let error =
            spec_to_tickets(&specification, &ticket_set(Vec::new())).expect_err("must be rejected");
        assert!(error.contains("Stream the output"), "{error}");
    }

    /// A story that carries criteria is represented by those criteria: the
    /// ticket need not repeat the story title.
    #[test]
    fn criteria_alone_represent_their_story() {
        let mut specification = flag_specification();
        specification.user_stories[0].title = "Output requirement".to_string();
        specification.user_stories[0].requirement = "Unsullied prose.".to_string();

        assert_eq!(
            spec_to_tickets(
                &specification,
                &ticket_set(vec!["`--version` prints the version and exits."])
            ),
            Ok(())
        );
    }

    /// The plan and code control tools take no upstream document: a source that
    /// belongs to another phase is ignored rather than consulted, so those
    /// phases keep their original behaviour.
    #[test]
    fn the_plan_and_code_phases_ignore_an_unrelated_source() {
        let specification = covering_specification();
        assert!(matches!(
            Tool::parse(
                "finish_plan",
                VALID_PLAN_ARGUMENTS,
                Phase::Plan,
                Source::Specification(&specification)
            ),
            Ok(Tool::FinishPlan(_))
        ));

        let plan = covering_plan();
        assert!(matches!(
            Tool::parse("task_complete", "{}", Phase::Code, Source::Plan(&plan)),
            Ok(Tool::TaskComplete(_))
        ));
    }

    /// A story the ticket set does not represent is refused at the boundary,
    /// and the error names the story so the tickets agent can repair the set.
    #[test]
    fn finish_tickets_is_rejected_when_it_drops_a_story() {
        let mut specification = covering_specification();
        specification.user_stories = vec![UserStory {
            title: "Stream the output".to_string(),
            requirement: "Stream each log record to the remote collector.".to_string(),
            acceptance_criteria: Vec::new(),
        }];

        let error = match Tool::parse(
            "finish_tickets",
            &covering_ticket_arguments(),
            Phase::Tickets,
            Source::Specification(&specification),
        ) {
            Err(error) => error,
            Ok(_) => panic!("an uncovered story must not hand over tickets"),
        };
        assert!(error.contains("Stream the output"), "{error}");
    }

    /// A blank criterion in the ticket set carries no content, so it cannot
    /// stand in for the specification's own criterion.
    #[test]
    fn a_blank_ticket_criterion_does_not_cover_a_real_one() {
        let error = spec_to_tickets(&flag_specification(), &ticket_set(vec!["   "]))
            .expect_err("a blank criterion must be rejected");
        assert!(
            error.contains("`--version` prints the version and exits."),
            "{error}"
        );
    }

    /// A blank criterion in the specification is not a criterion at all: the
    /// story is covered by the ticket content exactly as a story with no
    /// criteria would be.
    #[test]
    fn a_blank_specification_criterion_is_not_required() {
        let mut specification = flag_specification();
        specification.user_stories[0].acceptance_criteria = vec!["   ".to_string()];

        assert_eq!(
            spec_to_tickets(&specification, &ticket_set(Vec::new())),
            Ok(())
        );
    }

    #[test]
    fn execute_script_parses_in_every_phase_and_rejects_bad_arguments() {
        for phase in [
            Phase::Plan,
            Phase::Spec,
            Phase::Tickets,
            Phase::Code,
            Phase::Review,
            Phase::Summary,
        ] {
            let tool = Tool::parse(
                "execute_script",
                r#"{"code":"return 1 + 1;"}"#,
                phase,
                Source::None,
            )
            .expect("valid execute_script arguments must parse");
            assert_eq!(tool.target(), "return 1 + 1;");
            match tool {
                Tool::ExecuteScript(args) => assert_eq!(args.code, "return 1 + 1;"),
                _ => panic!("expected Tool::ExecuteScript"),
            }

            for bad in ["", "{}", r#"{"code":42}"#, "not json"] {
                assert!(
                    Tool::parse("execute_script", bad, phase, Source::None).is_err(),
                    "bad payload {bad:?} in {phase:?} must fail"
                );
            }
        }
    }

    /// The code phase is the only phase whose scripts may mutate the project.
    /// The real QuickJS sandbox installs the two mutating bindings for it, so
    /// the capability is observable on the running namespace instead of read
    /// back out of a double.
    #[test]
    fn when_running_script_given_code_phase_then_allows_code_capabilities() {
        let tool = Tool::parse(
            "execute_script",
            r#"{"code":"return Object.keys(tools).sort().join(',');"}"#,
            Phase::Code,
            Source::None,
        )
        .expect("valid execute_script arguments must parse");

        assert_eq!(
            tool.run(Phase::Code),
            Ok("diff,grep,list,read,replace,write".to_string()),
            "the code phase must expose the mutating bindings too"
        );
    }

    /// Every other phase runs real read-only scripts: the mutating bindings do
    /// not exist in the sandbox at all, so a write cannot even be attempted.
    #[test]
    fn when_running_script_given_read_only_phase_then_omits_mutating_bindings() {
        for phase in [
            Phase::Plan,
            Phase::Spec,
            Phase::Tickets,
            Phase::Triage,
            Phase::Review,
            Phase::Summary,
        ] {
            let tool = Tool::parse(
                "execute_script",
                r#"{"code":"return Object.keys(tools).sort().join(',');"}"#,
                phase,
                Source::None,
            )
            .expect("valid execute_script arguments must parse");

            assert_eq!(
                tool.run(phase),
                Ok("diff,grep,list,read".to_string()),
                "the {phase:?} phase must expose only the read-only bindings"
            );
        }
    }

    /// A script that throws surfaces the engine's own error, wrapped in the
    /// self-correcting shape the model recovers from: a runtime reference error
    /// and a parse error both come back as `Err`, never as a fake success.
    #[test]
    fn when_running_script_given_invalid_javascript_then_returns_execution_error() {
        for (code, expected) in [
            (r#"{"code":"return x;"}"#, "x is not defined"),
            (
                r#"{"code":"return 1 +;"}"#,
                "unexpected token in expression",
            ),
        ] {
            let tool = Tool::parse("execute_script", code, Phase::Code, Source::None)
                .expect("valid execute_script arguments must parse");

            let error = tool
                .run(Phase::Code)
                .expect_err("invalid JavaScript must surface as an error");
            assert!(
                error.starts_with("[ERROR: execute_script failed]"),
                "unexpected error shape for {code}: {error}"
            );
            assert!(
                error.contains(expected),
                "expected a {expected} for {code}: {error}"
            );
        }
    }

    #[test]
    fn when_parsing_finish_review_given_valid_pass_or_fixes_then_validates_and_covers() {
        let pass_args = serde_json::json!({
            "review": {
                "scorecard": {
                    "keeper_coverage": "All 4 keeper categories present.",
                    "sediment_and_coupling": "Zero internal mocks.",
                    "inferred_assumptions": "Default status is 0.",
                    "boundary_matrix": "All 5 dimensions covered.",
                    "code_and_scope": "Matches specification."
                },
                "findings": [],
                "fixes": []
            }
        })
        .to_string();

        match Tool::parse("finish_review", &pass_args, Phase::Review, Source::None) {
            Ok(Tool::FinishReview(report)) => assert!(report.passed()),
            other => panic!(
                "expected passing ReviewReport, got {}",
                other.err().unwrap_or_default()
            ),
        }

        let fix_args = serde_json::json!({
            "review": {
                "scorecard": {
                    "keeper_coverage": "Missing empty boundary check.",
                    "sediment_and_coupling": "Zero internal mocks.",
                    "inferred_assumptions": "Default status is 0.",
                    "boundary_matrix": "Empty dimension missing.",
                    "code_and_scope": "Parser needs empty guard."
                },
                "findings": [{
                    "id": "f1-empty-boundary",
                    "category": "boundary-matrix",
                    "target": "src/parser.rs",
                    "detail": "Parser does not reject empty input slice."
                }],
                "fixes": [{
                    "id": "r1-1-empty-boundary",
                    "blocked_by": [],
                    "blocks": [],
                    "title": "Add empty input boundary check",
                    "objective": "Reject an empty input slice in the parser and test it.",
                    "acceptance_criteria": ["Parser rejects empty input slice."],
                    "prerequisites_assumed": "Parser exists.",
                    "out_of_scope": [],
                    "scope_of_changes": ["Update `src/parser.rs` for empty input slice."],
                    "test_expectations": ["when_parsing_input_given_empty_slice_then_returns_error"],
                    "verification_gate": "cargo test parser"
                }]
            }
        })
        .to_string();

        let existing = vec!["1-initial".to_string()];
        match Tool::parse(
            "finish_review",
            &fix_args,
            Phase::Review,
            Source::Review(&existing),
        ) {
            Ok(Tool::FinishReview(report)) => assert_eq!(report.fixes.len(), 1),
            other => panic!(
                "expected fix ReviewReport, got {}",
                other.err().unwrap_or_default()
            ),
        }

        // Colliding with an earlier round's ticket id is rejected.
        let colliding = vec!["r1-1-empty-boundary".to_string()];
        let err = match Tool::parse(
            "finish_review",
            &fix_args,
            Phase::Review,
            Source::Review(&colliding),
        ) {
            Err(err) => err,
            Ok(_) => panic!("colliding fix ticket id must be rejected"),
        };
        assert!(
            err.contains("duplicates a ticket id from an earlier round"),
            "{err}"
        );
    }

    /// Only a review source supplies the ticket ids a fix must not reuse. An
    /// unrelated source -- here the triage draft set, which carries the same id
    /// as the fix -- is ignored, so its ids never make a valid fix collide.
    #[test]
    fn when_handing_off_given_non_review_source_and_colliding_fix_id_then_ignores_unrelated_ids() {
        let fix_args = serde_json::json!({
            "review": {
                "scorecard": {
                    "keeper_coverage": "All 4 keeper categories present.",
                    "sediment_and_coupling": "Zero internal mocks.",
                    "inferred_assumptions": "Default status is 0.",
                    "boundary_matrix": "All 5 dimensions covered.",
                    "code_and_scope": "Matches specification."
                },
                "findings": [{
                    "id": "f1-empty-boundary",
                    "category": "boundary-matrix",
                    "target": "src/parser.rs",
                    "detail": "Parser does not reject empty input slice."
                }],
                "fixes": [{
                    "id": "1-first",
                    "blocked_by": [],
                    "blocks": [],
                    "title": "Add empty input boundary check",
                    "objective": "Reject an empty input slice in the parser and test it.",
                    "acceptance_criteria": ["Parser rejects empty input slice."],
                    "prerequisites_assumed": "Parser exists.",
                    "out_of_scope": [],
                    "scope_of_changes": ["Update `src/parser.rs` for empty input slice."],
                    "test_expectations": ["when_parsing_input_given_empty_slice_then_returns_error"],
                    "verification_gate": "cargo test parser"
                }]
            }
        })
        .to_string();

        let colliding = vec!["1-first".to_string()];
        let error = match Tool::parse(
            "finish_review",
            &fix_args,
            Phase::Review,
            Source::Review(&colliding),
        ) {
            Err(error) => error,
            Ok(_) => panic!("a review source must still reject a colliding fix id"),
        };
        assert!(
            error.contains("duplicates a ticket id from an earlier round"),
            "{error}"
        );

        let tickets = triage_draft_tickets();
        let specification = covering_specification();
        let unrelated = triage_source(&specification, &tickets);
        assert_eq!(tickets.tickets[0].id, "1-first");
        for source in [Source::None, unrelated] {
            match Tool::parse("finish_review", &fix_args, Phase::Review, source) {
                Ok(Tool::FinishReview(report)) => {
                    assert_eq!(report.fixes[0].id, "1-first");
                }
                Err(error) => {
                    panic!("a non-review source must not supply existing ticket ids: {error}")
                }
                Ok(other) => panic!(
                    "expected the review handoff, got a different tool: {}",
                    other.target()
                ),
            }
        }
    }

    // -----------------------------------------------------------------------
    // The triage phase: a read-only pass over the draft ticket set
    // -----------------------------------------------------------------------

    use crate::specification::NONE;
    use crate::triage::{AgentBrief, TriageCategory, TriageReport, TriageState, TriageTicket};

    /// One draft ticket, carrying the fields the triage report preserves.
    fn triage_draft_ticket(id: &str, title: &str, blocked_by: &[String]) -> Ticket {
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

    /// The draft set the triage tests classify: two tickets with one edge.
    fn triage_draft_tickets() -> TicketSet {
        TicketSet {
            tickets: vec![
                triage_draft_ticket("1-first", "First slice", &[]),
                triage_draft_ticket("2-second", "Second slice", &["1-first".to_string()]),
            ],
        }
    }

    /// One classification the contract accepts: a ready ticket with the
    /// smallest brief that carries every field.
    fn triage_ready_ticket(id: &str, title: &str, blocked_by: &[String]) -> TriageTicket {
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
                out_of_scope: vec![NONE.to_string()],
                blocked_by: blocked_by.to_vec(),
            }),
        }
    }

    /// The typed triage source: the specification the draft set was sliced from
    /// and the original draft tickets a report must classify exactly once.
    fn triage_source<'a>(specification: &'a Specification, tickets: &'a TicketSet) -> Source<'a> {
        Source::Triage {
            specification,
            tickets,
        }
    }

    /// The wire arguments of one finish_triage call carrying the report in the
    /// required triage object.
    fn finish_triage_arguments(report: &TriageReport) -> String {
        serde_json::json!({ "triage": report }).to_string()
    }

    /// Every string constant a JSON schema pins, at any depth: the members of an
    /// enum array and the const value of a oneOf branch.
    fn schema_constants(value: &serde_json::Value, out: &mut Vec<String>) {
        match value {
            serde_json::Value::Object(map) => {
                for (key, child) in map {
                    match key.as_str() {
                        "enum" => {
                            for item in child.as_array().into_iter().flatten() {
                                if let Some(text) = item.as_str() {
                                    out.push(text.to_string());
                                }
                            }
                        }
                        "const" => {
                            if let Some(text) = child.as_str() {
                                out.push(text.to_string());
                            }
                        }
                        _ => schema_constants(child, out),
                    }
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    schema_constants(item, out);
                }
            }
            _ => {}
        }
    }

    /// The triage pass is offered the read-only inspection tool and its own
    /// handoff, and nothing else: no bash, no user-question workflow, no
    /// writable binding. The handoff schema pins exactly the report's closed
    /// enum wire values, the phase restriction is enforced on the way in, and a
    /// rejected payload is a tool-result correction rather than the end of the
    /// pass.
    #[test]
    fn when_offering_tools_given_triage_phase_then_exposes_only_read_only_script_and_finish_triage()
    {
        assert_eq!(names(Phase::Triage), ["execute_script", "finish_triage"]);

        let definition = Tool::definitions(Phase::Triage)
            .into_iter()
            .find(|definition| definition.function.name == "finish_triage")
            .expect("the triage phase offers its handoff tool");
        let schema = serde_json::to_value(&definition.function.parameters)
            .expect("the handoff schema serializes");
        assert!(
            schema["properties"].is_object(),
            "a strict provider rejects a schema without a properties object: {schema}"
        );
        assert!(
            schema["properties"]["triage"].is_object(),
            "the handoff takes the report in its triage argument: {schema}"
        );
        assert!(
            schema["properties"]["report"].is_null(),
            "the retired report argument is not advertised any more: {schema}"
        );

        let mut constants = Vec::new();
        schema_constants(&schema, &mut constants);
        constants.sort();
        constants.dedup();
        let mut expected: Vec<String> = TriageCategory::ALL
            .iter()
            .map(|value| value.wire().to_string())
            .chain(
                TriageState::ALL
                    .iter()
                    .map(|value| value.wire().to_string()),
            )
            .collect();
        expected.sort();
        assert_eq!(
            constants, expected,
            "the schema must pin exactly the closed category and state wire values: {schema}"
        );

        let tickets = triage_draft_tickets();
        let specification = covering_specification();
        let source = triage_source(&specification, &tickets);
        let report = TriageReport {
            tickets: vec![
                triage_ready_ticket("1-first", "First slice", &[]),
                triage_ready_ticket("2-second", "Second slice", &["1-first".to_string()]),
            ],
        };
        let arguments = finish_triage_arguments(&report);

        match Tool::parse("finish_triage", &arguments, Phase::Triage, source) {
            Ok(Tool::FinishTriage(parsed)) => assert_eq!(parsed, report),
            Err(error) => panic!("the valid handoff must parse: {error}"),
            Ok(other) => panic!(
                "expected the triage handoff, got a different tool: {}",
                other.target()
            ),
        }

        for phase in [
            Phase::Plan,
            Phase::Spec,
            Phase::Tickets,
            Phase::Code,
            Phase::Review,
            Phase::Summary,
        ] {
            let error = match Tool::parse("finish_triage", &arguments, phase, Source::None) {
                Err(error) => error,
                Ok(_) => panic!("finish_triage must be rejected in {phase:?}"),
            };
            assert!(error.contains("finish_triage"), "{phase:?}: {error}");
            assert!(
                error.contains("not available in this phase"),
                "{phase:?}: {error}"
            );
        }

        let mut malformed = report.clone();
        malformed.tickets[0].ticket_title = "First slice, reworded".to_string();
        let error = match Tool::parse(
            "finish_triage",
            &finish_triage_arguments(&malformed),
            Phase::Triage,
            source,
        ) {
            Err(error) => error,
            Ok(_) => panic!("a mismatched title must be rejected"),
        };
        assert!(error.contains("1-first"), "{error}");
        assert!(error.contains("title"), "{error}");
        match Tool::parse("finish_triage", &arguments, Phase::Triage, source) {
            Ok(Tool::FinishTriage(corrected)) => assert_eq!(corrected, report),
            Err(error) => panic!("the corrected handoff must parse: {error}"),
            Ok(other) => panic!(
                "expected the triage handoff, got a different tool: {}",
                other.target()
            ),
        }

        // The inspection tool runs against the real QuickJS engine with the
        // triage phase's capabilities, so the read-only boundary is observable
        // instead of read back out of a double: the namespace holds exactly the
        // four inspection bindings, and a mutating binding does not exist at
        // all.
        assert!(
            !Capabilities::for_phase(Phase::Triage).writable,
            "the triage phase must have read-only script capabilities"
        );
        let inspect = Tool::parse(
            "execute_script",
            "{\"code\":\"return Object.keys(tools).sort();\"}",
            Phase::Triage,
            source,
        )
        .expect("the triage phase offers its inspection tool");
        assert_eq!(
            inspect.run(Phase::Triage),
            Ok(r#"["diff","grep","list","read"]"#.to_string()),
            "the triage pass must run its scripts with read-only capabilities"
        );

        let mutating = Tool::parse(
            "execute_script",
            r#"{"code":"tools.write(\"note.txt\", \"mutated\");"}"#,
            Phase::Triage,
            source,
        )
        .expect("the triage phase offers its inspection tool");
        let error = mutating
            .run(Phase::Triage)
            .expect_err("the triage pass must not be able to write");
        assert!(error.contains("not a function"), "{error}");

        let handoff = match Tool::parse("finish_triage", &arguments, Phase::Triage, source) {
            Ok(tool) => tool,
            Err(error) => panic!("the valid handoff must parse: {error}"),
        };
        assert_eq!(handoff.target(), "finish_triage");
        assert!(
            handoff.run(Phase::Triage).is_err(),
            "a handoff is not a command and cannot be run"
        );
    }

    /// The pass is offered no bash at all, and a bash call is refused on the way
    /// in, so the read-only boundary is enforced on both sides.
    #[test]
    fn when_offering_tools_given_triage_phase_then_omits_bash() {
        let advertised: Vec<&'static str> = Tool::definitions(Phase::Triage)
            .into_iter()
            .map(|definition| definition.function.name)
            .collect();
        assert_eq!(advertised, ["execute_script", "finish_triage"]);
        assert!(!advertised.contains(&"bash"));

        let error = match Tool::parse("bash", "{\"command\":\"ls\"}", Phase::Triage, Source::None) {
            Err(error) => error,
            Ok(_) => panic!("the triage pass must not run bash"),
        };
        assert!(
            error.contains("only available in the code phase"),
            "{error}"
        );
        assert!(error.contains("execute_script"), "{error}");
    }

    /// Every state-specific payload rule the report contract fixes is refused on
    /// the handoff path, as a tool error naming the ticket and the field, so the
    /// triage agent can correct the payload on its next turn.
    #[test]
    fn when_handing_off_triage_given_invalid_state_brief_or_dependency_then_returns_tool_error() {
        let tickets = triage_draft_tickets();
        let specification = covering_specification();
        let source = triage_source(&specification, &tickets);
        let second = triage_ready_ticket("2-second", "Second slice", &["1-first".to_string()]);

        let mut ready_with_explanation = second.clone();
        ready_with_explanation.blocker_explanation = Some("The ticket is blocked.".to_string());

        let mut ready_without_brief = second.clone();
        ready_without_brief.agent_brief = None;

        let mut blank_target_behavior = second.clone();
        blank_target_behavior
            .agent_brief
            .as_mut()
            .expect("the helper carries a brief")
            .target_behavior = "   ".to_string();

        let mut blank_criteria = second.clone();
        blank_criteria
            .agent_brief
            .as_mut()
            .expect("the helper carries a brief")
            .acceptance_criteria = vec!["  ".to_string()];

        let mut blank_boundaries = second.clone();
        blank_boundaries
            .agent_brief
            .as_mut()
            .expect("the helper carries a brief")
            .out_of_scope = vec!["   ".to_string()];

        let mut not_ready_without_explanation = second.clone();
        not_ready_without_explanation.state = TriageState::NeedsInfo;
        not_ready_without_explanation.agent_brief = None;

        let mut not_ready_with_brief = second.clone();
        not_ready_with_brief.state = TriageState::Wontfix;
        not_ready_with_brief.blocker_explanation = Some("Nothing to execute.".to_string());

        let mut repeated_dependency = second.clone();
        repeated_dependency
            .agent_brief
            .as_mut()
            .expect("the helper carries a brief")
            .blocked_by = vec!["1-first".to_string(), "1-first".to_string()];

        let mut dropped_dependency = second.clone();
        dropped_dependency
            .agent_brief
            .as_mut()
            .expect("the helper carries a brief")
            .blocked_by = Vec::new();

        let mut extra_dependency = second.clone();
        extra_dependency
            .agent_brief
            .as_mut()
            .expect("the helper carries a brief")
            .blocked_by = vec!["1-first".to_string(), "2-second".to_string()];

        let cases: [(&str, TriageTicket, &str); 10] = [
            (
                "a ready classification that carries a blocker explanation",
                ready_with_explanation,
                "blocker explanation",
            ),
            (
                "a ready classification without a brief",
                ready_without_brief,
                "Agent Brief",
            ),
            (
                "a brief with a blank target behavior",
                blank_target_behavior,
                "target_behavior",
            ),
            (
                "a brief with blank acceptance criteria",
                blank_criteria,
                "acceptance_criteria",
            ),
            (
                "a brief with a blank out-of-scope boundary",
                blank_boundaries,
                "out_of_scope",
            ),
            (
                "a non-ready classification without an explanation",
                not_ready_without_explanation,
                "explanation",
            ),
            (
                "a non-ready classification that carries a brief",
                not_ready_with_brief,
                "Agent Brief",
            ),
            (
                "a brief that repeats a dependency",
                repeated_dependency,
                "more than once",
            ),
            (
                "a brief that drops a dependency",
                dropped_dependency,
                "dependencies",
            ),
            (
                "a brief that widens the dependency list",
                extra_dependency,
                "dependencies",
            ),
        ];

        for (case, classification, expected) in cases {
            let report = TriageReport {
                tickets: vec![
                    triage_ready_ticket("1-first", "First slice", &[]),
                    classification,
                ],
            };
            let error = match Tool::parse(
                "finish_triage",
                &finish_triage_arguments(&report),
                Phase::Triage,
                source,
            ) {
                Err(error) => error,
                Ok(_) => panic!("{case} must be rejected"),
            };
            assert!(
                error.contains("2-second"),
                "{case}: must name the ticket: {error}"
            );
            assert!(
                error.contains(expected),
                "{case}: must name the failing field: {error}"
            );
        }
    }

    /// A handoff with no triage object -- a missing argument, a null one, an
    /// empty object, a null classification list, a retired report argument -- is
    /// refused, so no payload without the required object can end the pass.
    #[test]
    fn when_handing_off_triage_given_null_or_missing_triage_then_rejects_call() {
        let tickets = triage_draft_tickets();
        let specification = covering_specification();
        let source = triage_source(&specification, &tickets);
        let report = TriageReport {
            tickets: vec![
                triage_ready_ticket("1-first", "First slice", &[]),
                triage_ready_ticket("2-second", "Second slice", &["1-first".to_string()]),
            ],
        };

        for payload in [
            "".to_string(),
            "{}".to_string(),
            "null".to_string(),
            "{\"triage\":null}".to_string(),
            "{\"triage\":{}}".to_string(),
            "{\"triage\":{\"tickets\":null}}".to_string(),
            "{\"triage\":{\"tickets\":[null]}}".to_string(),
            "{\"tickets\":[]}".to_string(),
            serde_json::json!({ "report": report }).to_string(),
        ] {
            match Tool::parse("finish_triage", &payload, Phase::Triage, source) {
                Err(_) => {}
                Ok(_) => panic!("{payload} must not end the pass"),
            }
        }

        let empty = TriageReport {
            tickets: Vec::new(),
        };
        let error = match Tool::parse(
            "finish_triage",
            &finish_triage_arguments(&empty),
            Phase::Triage,
            source,
        ) {
            Err(error) => error,
            Ok(_) => panic!("an empty report cannot cover the draft set"),
        };
        assert!(error.contains("no triage classification"), "{error}");
    }

    /// A valid report can only be validated against the draft set it classifies.
    /// A missing or unrelated source is refused with the exact actionable error
    /// instead of being accepted unvalidated.
    #[test]
    fn when_handing_off_given_missing_triage_source_then_returns_validation_error() {
        let specification = covering_specification();
        let report = TriageReport {
            tickets: vec![
                triage_ready_ticket("1-first", "First slice", &[]),
                triage_ready_ticket("2-second", "Second slice", &["1-first".to_string()]),
            ],
        };
        let arguments = finish_triage_arguments(&report);
        let expected = "[ERROR] `finish_triage` needs the draft ticket set it classifies; the \
                        triage source is missing, so the report cannot be validated";

        let plan = covering_plan();
        let review_ids = vec!["9-reviewed".to_string()];
        for source in [
            Source::None,
            Source::Plan(&plan),
            Source::Specification(&specification),
            Source::Review(&review_ids),
        ] {
            let error = match Tool::parse("finish_triage", &arguments, Phase::Triage, source) {
                Err(error) => error,
                Ok(_) => panic!("a triage handoff without its triage source must be rejected"),
            };
            assert_eq!(error, expected, "the missing-source error must be exact");
        }
    }

    /// The smallest valid handoff: one draft ticket, one ready classification,
    /// every brief field filled in. It parses into the typed report the caller
    /// keeps, and the report validates against the typed source it was checked
    /// against.
    #[test]
    fn when_handing_off_triage_given_single_valid_classification_then_returns_typed_report() {
        let tickets = TicketSet {
            tickets: vec![triage_draft_ticket("1-only", "Only slice", &[])],
        };
        let specification = covering_specification();
        let source = triage_source(&specification, &tickets);
        let report = TriageReport {
            tickets: vec![TriageTicket {
                ticket_id: "1-only".to_string(),
                ticket_title: "Only slice".to_string(),
                category: TriageCategory::Bug,
                state: TriageState::ReadyForAgent,
                notes: vec!["x".to_string()],
                blocker_explanation: None,
                agent_brief: Some(AgentBrief {
                    target_behavior: "x".to_string(),
                    acceptance_criteria: vec!["y".to_string()],
                    out_of_scope: vec![NONE.to_string()],
                    blocked_by: Vec::new(),
                }),
            }],
        };

        match Tool::parse(
            "finish_triage",
            &finish_triage_arguments(&report),
            Phase::Triage,
            source,
        ) {
            Ok(Tool::FinishTriage(parsed)) => {
                assert_eq!(parsed, report);
                assert_eq!(parsed.tickets[0].category, TriageCategory::Bug);
                assert_eq!(parsed.tickets[0].state, TriageState::ReadyForAgent);
                assert_eq!(
                    parsed.tickets[0]
                        .agent_brief
                        .as_ref()
                        .expect("the brief survives the handoff")
                        .acceptance_criteria,
                    ["y"]
                );
                assert_eq!(parsed.validate_against(&specification, &tickets), Ok(()));
            }
            Err(error) => panic!("the smallest valid handoff must parse: {error}"),
            Ok(other) => panic!(
                "expected the triage handoff, got a different tool: {}",
                other.target()
            ),
        }
    }

    /// A malformed payload -- an unknown enum value, an unknown, duplicated,
    /// missing or extra ticket identity, a title that is not the ticket's -- is
    /// refused as a tool error and the corrected payload parses on the next
    /// turn.
    #[test]
    fn when_handing_off_triage_given_unknown_enum_or_extra_ticket_then_returns_tool_error() {
        let tickets = triage_draft_tickets();
        let specification = covering_specification();
        let source = triage_source(&specification, &tickets);
        let report = TriageReport {
            tickets: vec![
                triage_ready_ticket("1-first", "First slice", &[]),
                triage_ready_ticket("2-second", "Second slice", &["1-first".to_string()]),
            ],
        };
        let valid = finish_triage_arguments(&report);
        let payload: serde_json::Value = serde_json::from_str(&valid).expect("the report is JSON");

        let mut unknown_state = payload.clone();
        unknown_state["triage"]["tickets"][0]["state"] = serde_json::json!("maybe-later");

        let mut unknown_category = payload.clone();
        unknown_category["triage"]["tickets"][0]["category"] = serde_json::json!("feature");

        let mut extra_ticket = payload.clone();
        extra_ticket["triage"]["tickets"]
            .as_array_mut()
            .expect("the report carries a classification list")
            .push(
                serde_json::to_value(triage_ready_ticket("9-absent", "Absent slice", &[])).unwrap(),
            );

        let mut duplicate_ticket = payload.clone();
        duplicate_ticket["triage"]["tickets"]
            .as_array_mut()
            .expect("the report carries a classification list")
            .push(
                serde_json::to_value(triage_ready_ticket("1-first", "First slice", &[])).unwrap(),
            );

        let mut missing_ticket = payload.clone();
        missing_ticket["triage"]["tickets"]
            .as_array_mut()
            .expect("the report carries a classification list")
            .pop();

        let mut wrong_title = payload.clone();
        wrong_title["triage"]["tickets"][1]["ticket_title"] =
            serde_json::json!("Second slice, reworded");

        let cases: [(&str, serde_json::Value, &str); 6] = [
            ("an unknown state value", unknown_state, "maybe-later"),
            ("an unknown category value", unknown_category, "feature"),
            (
                "an extra ticket identity",
                extra_ticket,
                "not a draft ticket",
            ),
            ("a duplicated ticket identity", duplicate_ticket, "twice"),
            (
                "a missing ticket identity",
                missing_ticket,
                "no triage classification",
            ),
            ("a title that is not the ticket's", wrong_title, "title"),
        ];

        for (case, malformed, expected) in cases {
            let error = match Tool::parse(
                "finish_triage",
                &malformed.to_string(),
                Phase::Triage,
                source,
            ) {
                Err(error) => error,
                Ok(_) => panic!("{case} must be rejected"),
            };
            assert!(error.contains(expected), "{case}: {error}");
        }

        match Tool::parse("finish_triage", &valid, Phase::Triage, source) {
            Ok(Tool::FinishTriage(parsed)) => assert_eq!(parsed.tickets.len(), 2),
            Err(error) => panic!("the corrected payload must parse: {error}"),
            Ok(other) => panic!(
                "expected the triage handoff, got a different tool: {}",
                other.target()
            ),
        }
    }

    /// The wire handoff is finish_triage with one required triage object holding
    /// the report: the exact shape parses into the typed report, and a payload
    /// that renames, omits or nulls that object is refused as a tool error.
    #[test]
    fn when_handing_off_triage_given_required_wire_shape_then_returns_typed_report() {
        let tickets = triage_draft_tickets();
        let specification = covering_specification();
        let source = triage_source(&specification, &tickets);
        let report = TriageReport {
            tickets: vec![
                triage_ready_ticket("1-first", "First slice", &[]),
                triage_ready_ticket("2-second", "Second slice", &["1-first".to_string()]),
            ],
        };
        let arguments = finish_triage_arguments(&report);

        // The advertised schema requires the triage object and advertises no
        // other argument for the report.
        let definition = Tool::definitions(Phase::Triage)
            .into_iter()
            .find(|definition| definition.function.name == "finish_triage")
            .expect("the triage phase offers its handoff tool");
        let schema = serde_json::to_value(&definition.function.parameters)
            .expect("the handoff schema serializes");
        assert!(
            schema["properties"]["triage"].is_object(),
            "the handoff takes the report in its triage argument: {schema}"
        );
        assert!(
            schema["properties"]["report"].is_null(),
            "the retired report argument is gone: {schema}"
        );
        let required: Vec<String> = schema["required"]
            .as_array()
            .expect("the handoff schema lists its required arguments")
            .iter()
            .filter_map(|name| name.as_str().map(str::to_string))
            .collect();
        assert_eq!(
            required,
            ["triage"],
            "the triage object is the handoff's one required argument: {schema}"
        );

        // The wire payload carries exactly that one field.
        let carried: serde_json::Value =
            serde_json::from_str(&arguments).expect("the payload is JSON");
        let keys: Vec<String> = carried
            .as_object()
            .expect("the payload is an object")
            .keys()
            .cloned()
            .collect();
        assert_eq!(keys, ["triage"], "the handoff carries one required object");

        match Tool::parse("finish_triage", &arguments, Phase::Triage, source) {
            Ok(Tool::FinishTriage(parsed)) => assert_eq!(parsed, report),
            Err(error) => panic!("the required wire shape must parse: {error}"),
            Ok(other) => panic!(
                "expected the triage handoff, got a different tool: {}",
                other.target()
            ),
        }

        // A payload that renames, omits or nulls the required object is refused.
        for malformed in [
            "{}".to_string(),
            serde_json::json!({ "report": report }).to_string(),
            serde_json::json!({ "triage": null }).to_string(),
            serde_json::json!({ "triage": {} }).to_string(),
        ] {
            let error = match Tool::parse("finish_triage", &malformed, Phase::Triage, source) {
                Err(error) => error,
                Ok(_) => panic!("{malformed} must not end the pass"),
            };
            assert!(error.contains("finish_triage"), "{error}");
        }
    }

    /// Every phase has one canonical lowercase label, independent of the
    /// enum's debug spelling, so reports can name the phase without leaking a
    /// Rust identifier.
    #[test]
    fn when_phase_is_labelled_given_all_seven_variants_then_returns_canonical_lowercase_label() {
        for (phase, expected) in [
            (Phase::Plan, "plan"),
            (Phase::Spec, "spec"),
            (Phase::Tickets, "tickets"),
            (Phase::Triage, "triage"),
            (Phase::Code, "code"),
            (Phase::Review, "review"),
            (Phase::Summary, "summary"),
        ] {
            let label = phase.label();
            assert_eq!(label, expected, "phase {phase:?}");
            assert_eq!(label, label.to_ascii_lowercase(), "phase {phase:?}");
            assert!(
                !label.is_empty() && !label.contains(' '),
                "phase {phase:?} label must be one lowercase word: {label:?}"
            );
        }
    }

    /// Core entry-point behavior, minimum valid boundary: a spill under the
    /// exact `.ejaj` ancestor creates the exact self-ignoring bytes and keeps
    /// the deepest normal spill path and its complete payload.
    #[test]
    fn when_saving_spill_given_session_under_ejaj_then_creates_exact_ignore_file() {
        let root = scratch_dir("spill-ejaj");
        let session = session_at(&root, "run");
        let command = repeat_command('s', crate::script::OUTPUT_LIMIT_BYTES + 1);

        let result = run_bash_in_session(&root, &session, &command, 4, 2)
            .expect("the command should succeed");

        assert!(
            result.contains(".ejaj/run/tools-output/step-4-call-2.log"),
            "the deepest normal spill path must be preserved: {result:?}"
        );
        assert_eq!(
            std::fs::read(root.join(".ejaj").join(".gitignore")).unwrap(),
            b"*\n".as_slice(),
            "the matching .ejaj ancestor must own exactly the self-ignoring bytes"
        );
        assert_eq!(
            std::fs::read(session_output(&session).join("step-4-call-2.log")).unwrap(),
            spilled_bytes(
                &command,
                "s".repeat(crate::script::OUTPUT_LIMIT_BYTES + 1).as_bytes()
            ),
            "the complete spill bytes must be preserved"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Empty/zero boundary and regression: without a session the existing
    /// error is reported before any directory or ignore file exists.
    #[test]
    fn when_saving_spill_given_no_session_then_creates_no_directories_or_ignore_file() {
        let root = scratch_dir("spill-no-session");
        let command = repeat_command('n', crate::script::OUTPUT_LIMIT_BYTES + 1);

        let result = run_bash_without_session(&root, &command, 2, 1)
            .expect("a successful command must keep its success status when no session exists");

        let lowered = result.to_lowercase();
        assert!(
            lowered.contains("could not save") || lowered.contains("no spill file"),
            "the existing no-session error must be reported: {result:?}"
        );
        assert!(
            !result.contains("saved to"),
            "no session must not produce a saved path: {result:?}"
        );
        assert!(
            !root.join(".ejaj").exists(),
            "no session must not create the .ejaj directory"
        );
        assert!(
            !root.join(".ejaj").join(".gitignore").exists(),
            "no session must not create an ignore file"
        );
        assert!(
            !root.join("tools-output").exists(),
            "no session must not create a spill directory"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Upper boundary and collision contract: a deep destination keeps its
    /// collision-suffixed logs while an existing ignore file stays untouched.
    #[test]
    fn when_saving_spill_given_deep_destination_and_collision_then_preserves_log_and_ignore_contents()
     {
        let root = scratch_dir("spill-deep-collision");
        let ejaj = root.join(".ejaj");
        std::fs::create_dir_all(&ejaj).unwrap();
        std::fs::write(ejaj.join(".gitignore"), "# keep this ignore line\n").unwrap();
        let session = ejaj.join("deep").join("nested").join("run");
        let command = repeat_command('c', crate::script::OUTPUT_LIMIT_BYTES + 1);
        let output = "c".repeat(crate::script::OUTPUT_LIMIT_BYTES + 1);

        let first = run_bash_in_session(&root, &session, &command, 9, 3)
            .expect("the first spill should succeed");
        let second = run_bash_in_session(&root, &session, &command, 9, 3)
            .expect("the collision spill should succeed");

        assert!(
            first.contains(".ejaj/deep/nested/run/tools-output/step-9-call-3.log"),
            "the deepest normal spill path must be named: {first:?}"
        );
        assert!(
            second.contains(".ejaj/deep/nested/run/tools-output/step-9-call-3-1.log"),
            "the collision must use the numeric suffix form: {second:?}"
        );
        assert_eq!(
            std::fs::read_to_string(ejaj.join(".gitignore")).unwrap(),
            "# keep this ignore line\n",
            "an existing ignore file must not be overwritten or repaired"
        );
        assert_eq!(
            std::fs::read(session_output(&session).join("step-9-call-3.log")).unwrap(),
            spilled_bytes(&command, output.as_bytes()),
            "the first log must keep its complete bytes"
        );
        assert_eq!(
            std::fs::read(session_output(&session).join("step-9-call-3-1.log")).unwrap(),
            spilled_bytes(&command, output.as_bytes()),
            "the collision log must keep its complete bytes"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Malformed ignore path: a directory where the `.ejaj` ignore file
    /// belongs returns the existing string-error path and never writes a spill
    /// log. A directory obstructs creation on every runner, unlike a permission
    /// bit that an elevated runner can ignore.
    #[test]
    fn when_saving_spill_given_unwritable_ejaj_ignore_path_then_returns_error_without_artifact() {
        let root = scratch_dir("spill-unwritable-ignore");
        let ejaj = root.join(".ejaj");
        let session = session_at(&root, "blocked");
        let output = session_output(&session);
        std::fs::create_dir_all(&output).unwrap();
        std::fs::create_dir(ejaj.join(".gitignore")).unwrap();
        let command = repeat_command('u', crate::script::OUTPUT_LIMIT_BYTES + 1);

        let result = run_bash_in_session(&root, &session, &command, 6, 1)
            .expect("a successful command must keep its success status when the spill fails");

        let lowered = result.to_lowercase();
        assert!(
            lowered.contains("could not save") || lowered.contains("no spill file"),
            "the existing spill error must be reported: {result:?}"
        );
        assert!(
            !result.contains("saved to"),
            "a failed ignore preparation must not claim a saved path: {result:?}"
        );
        assert!(
            !result.contains("step-6-call-1.log"),
            "a failed ignore preparation must not name a spill path: {result:?}"
        );
        assert!(
            !output.join("step-6-call-1.log").exists(),
            "a failed ignore preparation must not write a spill log"
        );
        assert!(
            ejaj.join(".gitignore").is_dir(),
            "the failed writer must neither repair nor replace the obstructing directory"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Contract and invariant: an existing regular ignore file survives a
    /// successful spill byte for byte.
    #[test]
    fn when_saving_spill_given_existing_regular_ignore_file_then_preserves_contents() {
        let root = scratch_dir("spill-existing-ignore");
        let ejaj = root.join(".ejaj");
        std::fs::create_dir_all(&ejaj).unwrap();
        std::fs::write(ejaj.join(".gitignore"), "# keep this ignore line\n").unwrap();
        let session = session_at(&root, "run");
        let command = repeat_command('e', crate::script::OUTPUT_LIMIT_BYTES + 1);

        let result =
            run_bash_in_session(&root, &session, &command, 3, 1).expect("the spill should succeed");

        assert!(
            result.contains(".ejaj/run/tools-output/step-3-call-1.log"),
            "the spill must still be written and named: {result:?}"
        );
        assert_eq!(
            std::fs::read_to_string(ejaj.join(".gitignore")).unwrap(),
            "# keep this ignore line\n",
            "an existing regular ignore file must not be replaced"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A unique relative file name for a test that needs a same-named file in
    /// two different roots.
    fn fixture_name(label: &str) -> String {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        format!(
            "ejaj-tools-root-fixture-{}-{id}-{label}.txt",
            std::process::id()
        )
    }

    /// The absolute path a relative file name resolves to in the process
    /// working directory.
    fn process_cwd_path(name: &str) -> std::path::PathBuf {
        std::env::current_dir()
            .expect("the process working directory should be readable")
            .join(name)
    }

    /// A process-working-directory fixture that removes itself when the test
    /// ends, even if an assertion fails first, so the repository working tree
    /// is never left dirty.
    struct ProcessCwdCleanup(std::path::PathBuf);

    impl ProcessCwdCleanup {
        fn create(name: &str, contents: &str) -> Self {
            let path = process_cwd_path(name);
            std::fs::write(&path, contents)
                .expect("the process-directory fixture should be writable");
            Self(path)
        }

        fn absent(name: &str) -> Self {
            Self(process_cwd_path(name))
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for ProcessCwdCleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// Dispatch an `execute_script` call through the explicit-root entry point
    /// with the real QuickJS engine.
    fn run_script_in(root: &std::path::Path, phase: Phase, code: &str) -> Result<String, String> {
        let arguments = serde_json::json!({ "code": code }).to_string();
        let tool = Tool::parse("execute_script", &arguments, phase, Source::None)
            .expect("valid execute_script arguments must parse");
        tool.run_with_context_in(root, phase, ToolContext::default(), None)
    }

    /// Dispatch a `bash` call through the explicit-root entry point with a real
    /// subprocess.
    fn run_bash_tool_in(root: &std::path::Path, command: &str) -> Result<String, String> {
        let arguments = serde_json::json!({ "command": command }).to_string();
        let tool = Tool::parse("bash", &arguments, Phase::Code, Source::None)
            .expect("valid bash arguments must parse");
        tool.run_with_context_in(root, Phase::Code, ToolContext::default(), None)
    }

    /// Render a path as a JavaScript string literal.
    fn js_string(value: &str) -> String {
        serde_json::to_string(value).expect("a path should serialize to a JSON string")
    }

    /// Core entry-point behavior: an explicit-root script reads the file in the
    /// supplied project, never a same-named file in the process working
    /// directory.
    #[test]
    fn when_script_inspection_given_explicit_project_root_then_reads_only_that_project() {
        let root = scratch_dir("script-explicit-root");
        let name = fixture_name("script-explicit-root");
        std::fs::write(root.join(&name), "ROOT-CONTENT")
            .expect("the root fixture should be writable");
        let process_fixture = ProcessCwdCleanup::create(&name, "PROCESS-CONTENT");

        let code = format!("return tools.read({});", js_string(&name));
        let result = run_script_in(&root, Phase::Plan, &code)
            .expect("the explicit-root script read should succeed");

        assert_eq!(
            result, "ROOT-CONTENT",
            "the script must read the supplied project root, not the process directory"
        );
        assert_eq!(
            std::fs::read_to_string(process_fixture.path()).unwrap(),
            "PROCESS-CONTENT",
            "the process-directory fixture must stay untouched"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Core entry-point behavior: explicit-root Bash dispatch runs `pwd` in the
    /// supplied project.
    #[test]
    fn when_bash_execution_given_explicit_project_root_then_uses_that_directory() {
        let root = scratch_dir("bash-explicit-root");

        let output = run_bash_tool_in(&root, "pwd").expect("`pwd` should succeed");

        let actual =
            std::fs::canonicalize(output.trim()).expect("`pwd` must name an existing directory");
        let expected = std::fs::canonicalize(&root).expect("the root should canonicalize");
        assert_eq!(
            actual, expected,
            "Bash must run in the supplied project root"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Core entry-point behavior: a verification command runs in the supplied
    /// root even when it differs from the process working directory.
    #[test]
    fn when_verification_command_given_explicit_project_root_then_uses_that_directory() {
        let root = scratch_dir("verification-explicit-root");

        let output = run_command_in(&root, "pwd").expect("`pwd` should succeed");

        let actual =
            std::fs::canonicalize(output.trim()).expect("`pwd` must name an existing directory");
        let expected = std::fs::canonicalize(&root).expect("the root should canonicalize");
        assert_eq!(
            actual, expected,
            "the verification command must run in the supplied project root"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Regression: the legacy entry point keeps the process-relative `.` root
    /// and its complete-output and nonzero-exit behavior.
    #[test]
    fn when_legacy_verification_command_given_process_relative_root_then_preserves_compatibility() {
        let cwd =
            std::env::current_dir().expect("the process working directory should be readable");

        let output = run_command("pwd").expect("`pwd` should succeed");
        let actual =
            std::fs::canonicalize(output.trim()).expect("`pwd` must name an existing directory");
        let expected = std::fs::canonicalize(&cwd).expect("the process directory should exist");
        assert_eq!(
            actual, expected,
            "run_command must stay anchored to the process-relative `.` root"
        );

        let output = run_command("printf 'legacy '; printf 'complete output'")
            .expect("the successful legacy command should return its complete output");
        assert_eq!(output, "legacy complete output");

        let command = "printf 'LEGACY-OUT\\n'; printf 'LEGACY-ERR\\n' >&2; exit 7";
        assert_eq!(
            run_command(command),
            Err(format!(
                "[ERROR: exit code 7 for `{command}`]\nLEGACY-OUT\nLEGACY-ERR\n"
            )),
            "the legacy command path must preserve its nonzero-exit shape"
        );
    }

    /// Empty/zero boundary: an empty successful verification command returns an
    /// empty success rather than an error or a spill.
    #[test]
    fn when_verification_command_given_empty_output_then_returns_empty_success() {
        let root = scratch_dir("verification-empty");

        assert_eq!(run_command_in(&root, "true"), Ok(String::new()));

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Edge-case boundary: a command that reads stdin observes immediate EOF
    /// through both the Bash and verification entry points.
    #[test]
    fn when_shell_command_given_closed_standard_input_then_reads_end_of_file() {
        let root = scratch_dir("shell-closed-stdin");
        let command =
            "if read -r line; then printf 'DATA:%s\\n' \"$line\"; else printf 'EOF\\n'; fi";

        let started = std::time::Instant::now();
        let bash_output = run_bash_tool_in(&root, command)
            .expect("Bash dispatch should observe EOF instead of waiting for input");
        assert_eq!(bash_output, "EOF\n", "Bash stdin must be closed");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "closed stdin must complete without waiting for interactive input"
        );

        let started = std::time::Instant::now();
        let gate_output =
            run_command_in(&root, command).expect("the verification command should observe EOF");
        assert_eq!(gate_output, "EOF\n", "verification stdin must be closed");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "closed stdin must complete without waiting for interactive input"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Edge-case boundary: an explicit-root read never falls back to a
    /// same-named file in the process working directory.
    #[test]
    fn when_script_inspection_given_missing_root_file_then_rejects_process_directory_fallback() {
        let root = scratch_dir("script-missing-root");
        let name = fixture_name("script-missing-root");
        let process_fixture = ProcessCwdCleanup::create(&name, "PROCESS-ONLY-CONTENT");

        let code = format!("return tools.read({});", js_string(&name));
        let error = run_script_in(&root, Phase::Code, &code)
            .expect_err("an explicit-root read must not fall back to the process directory");

        assert!(
            error.starts_with("[ERROR: execute_script failed]"),
            "the script failure must keep its existing formatted shape: {error}"
        );
        assert!(
            error.contains("tools.read"),
            "the failure must name the failed capability: {error}"
        );
        assert!(
            !error.contains("PROCESS-ONLY-CONTENT"),
            "a process-directory file must never satisfy an explicit-root read: {error}"
        );
        assert_eq!(
            std::fs::read_to_string(process_fixture.path()).unwrap(),
            "PROCESS-ONLY-CONTENT",
            "the process-directory fixture must stay untouched"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Malformed payload boundary: invalid JavaScript keeps the existing
    /// formatted script-error shape through explicit-root dispatch.
    #[test]
    fn when_script_inspection_given_invalid_javascript_then_preserves_formatted_error() {
        let root = scratch_dir("script-invalid-js");

        let error = run_script_in(&root, Phase::Code, "return missingIdentifier;")
            .expect_err("invalid JavaScript must surface as an error");

        assert!(
            error.starts_with("[ERROR: execute_script failed]"),
            "invalid JavaScript must keep the formatted script error: {error}"
        );
        assert!(
            error.contains("missingIdentifier is not defined"),
            "the engine detail must survive the formatted error: {error}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Failure boundary: a nonzero command preserves both the exit code and the
    /// complete stdout-then-stderr payload.
    #[test]
    fn when_shell_command_given_nonzero_exit_then_preserves_exit_code_and_output() {
        let root = scratch_dir("shell-nonzero");
        let command = "printf 'OUT-LINE\\n'; printf 'ERR-LINE\\n' >&2; exit 4";
        let expected = format!("[ERROR: exit code 4 for `{command}`]\nOUT-LINE\nERR-LINE\n");

        assert_eq!(
            run_command_in(&root, command),
            Err(expected.clone()),
            "the verification path must preserve the nonzero-exit shape"
        );
        assert_eq!(
            run_bash_tool_in(&root, command),
            Err(expected),
            "Bash dispatch must preserve the same nonzero-exit shape"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Failure boundary: an unusable working directory preserves the existing
    /// exit-code `-1` spawn-error shape.
    #[test]
    fn when_shell_command_given_unusable_working_directory_then_preserves_spawn_error_shape() {
        let root = scratch_dir("shell-unusable-root");
        std::fs::remove_dir_all(&root).expect("the scratch root should be removable");
        let command = "pwd";
        let prefix = format!("[ERROR: exit code -1 for `{command}`]\n");

        let error = run_command_in(&root, command)
            .expect_err("spawning Bash in a missing directory must fail");
        assert!(
            error.starts_with(&prefix),
            "unexpected verification spawn error shape: {error}"
        );
        assert!(
            error.len() > prefix.len(),
            "the spawn failure must report its cause: {error}"
        );

        let error = run_bash_tool_in(&root, command)
            .expect_err("Bash dispatch in a missing directory must fail");
        assert!(
            error.starts_with(&prefix),
            "unexpected Bash spawn error shape: {error}"
        );
        assert!(
            error.len() > prefix.len(),
            "the spawn failure must report its cause: {error}"
        );
    }

    /// Contract and invariant: successful output is the captured stdout
    /// followed by the captured stderr.
    #[test]
    fn when_shell_command_given_stdout_and_stderr_then_returns_stdout_before_stderr() {
        let root = scratch_dir("shell-output-order");
        let command = "printf 'ERR-LINE\\n' >&2; printf 'OUT-LINE\\n'";

        assert_eq!(
            run_command_in(&root, command),
            Ok("OUT-LINE\nERR-LINE\n".to_string()),
            "the verification path must place captured stderr after stdout"
        );
        assert_eq!(
            run_bash_tool_in(&root, command),
            Ok("OUT-LINE\nERR-LINE\n".to_string()),
            "Bash dispatch must place captured stderr after stdout"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Empty/zero boundary: a successful command with no output captures an
    /// empty stdout and stderr with a success status.
    #[test]
    fn when_shell_command_given_no_output_then_returns_empty_success() {
        let root = scratch_dir("shell-no-output");

        let output = shell_output_in(&root, "true").expect("`true` should succeed");

        assert!(output.status.success(), "`true` should report success");
        assert!(
            output.stdout.is_empty(),
            "a silent command must capture no stdout bytes: {:?}",
            output.stdout
        );
        assert!(
            output.stderr.is_empty(),
            "a silent command must capture no stderr bytes: {:?}",
            output.stderr
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Minimum valid boundary: a short successful command captures exactly the
    /// bytes it wrote and nothing else.
    #[test]
    fn when_shell_command_given_short_finite_output_then_captures_exact_bytes() {
        let root = scratch_dir("shell-short-output");

        let output = shell_output_in(&root, "printf 'SHORT-OUT'")
            .expect("a short command should complete successfully");

        assert!(output.status.success(), "the short command should succeed");
        assert_eq!(
            output.stdout,
            b"SHORT-OUT".to_vec(),
            "the captured stdout must be exactly the bytes the command wrote"
        );
        assert!(
            output.stderr.is_empty(),
            "a command that writes no stderr must capture none: {:?}",
            output.stderr
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Malformed payload boundary: a completed command with invalid shell
    /// syntax is a nonzero captured result, not a timeout.
    #[test]
    fn when_shell_command_given_invalid_shell_syntax_then_captures_nonzero_diagnostic() {
        let root = scratch_dir("shell-invalid-syntax");

        let output = shell_output_in(&root, "if true; then")
            .expect("a completed invalid command is still a captured result");

        assert!(
            !output.status.success(),
            "invalid shell syntax must report a failing status"
        );
        let code = output
            .status
            .code()
            .expect("a completed command must report an exit code");
        assert_ne!(code, 0, "invalid shell syntax must not report success");
        let diagnostic = String::from_utf8_lossy(&output.stderr);
        assert!(
            diagnostic.contains("syntax error"),
            "the diagnostic must explain the syntax error: {diagnostic}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Upper boundary and contract: the shared default deadline is two minutes,
    /// and a command that finishes inside it keeps its normal capture.
    #[test]
    fn when_shell_execution_given_default_configuration_then_uses_two_minute_timeout() {
        assert_eq!(
            SHELL_TIMEOUT,
            std::time::Duration::from_secs(120),
            "the shared shell deadline must be two minutes"
        );

        let root = scratch_dir("shell-default-timeout");
        let output = shell_output_in(&root, "printf 'WITHIN-DEADLINE'")
            .expect("a command far inside the default deadline should complete");

        assert!(
            output.status.success(),
            "the bounded command should report success"
        );
        assert_eq!(
            output.stdout,
            b"WITHIN-DEADLINE".to_vec(),
            "the bounded runner must preserve its normal capture"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Core entry-point behavior: a command still running at the supplied
    /// deadline is killed, reaped, and reported in the established error shape.
    #[test]
    fn when_shell_command_given_expired_deadline_then_returns_timeout_error() {
        let root = scratch_dir("shell-timeout");
        let command = "exec sleep 5";
        let timeout = std::time::Duration::from_millis(50);

        let started = std::time::Instant::now();
        let error = shell_output_with_timeout(&root, command, timeout)
            .expect_err("a still-running command must not be awaited indefinitely");
        let elapsed = started.elapsed();

        assert_eq!(
            error,
            format!("[ERROR: exit code -1 for `{command}`]\ncommand timed out after 0s"),
            "the timeout must keep the established error shape and whole seconds"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "the timeout must kill and reap the command promptly, took {elapsed:?}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Zero boundary: a zero deadline permits no polling wait, so whichever
    /// first-poll outcome occurs is the one reported and never a longer wait.
    #[test]
    fn when_shell_command_given_zero_duration_deadline_then_returns_first_poll_outcome() {
        let root = scratch_dir("shell-zero-timeout");
        let timeout = std::time::Duration::ZERO;

        let running = "exec sleep 5";
        let started = std::time::Instant::now();
        assert_eq!(
            shell_output_with_timeout(&root, running, timeout),
            Err(format!(
                "[ERROR: exit code -1 for `{running}`]\ncommand timed out after 0s"
            )),
            "a command still running at a zero deadline must be killed and reported"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "a zero deadline must not wait for the command"
        );

        let fast = "printf 'FIRST-POLL'";
        let started = std::time::Instant::now();
        match shell_output_with_timeout(&root, fast, timeout) {
            Ok(output) => {
                assert!(
                    output.status.success(),
                    "an already-exited first poll must keep its normal status"
                );
                assert_eq!(
                    output.stdout,
                    b"FIRST-POLL".to_vec(),
                    "an already-exited first poll must keep its captured stdout"
                );
                assert!(
                    output.stderr.is_empty(),
                    "an already-exited first poll must keep its captured stderr"
                );
            }
            Err(error) => assert_eq!(
                error,
                format!("[ERROR: exit code -1 for `{fast}`]\ncommand timed out after 0s"),
                "a still-running first poll must report the zero-deadline timeout"
            ),
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "a zero deadline must never wait for a command"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Maximum-output boundary: both streams larger than a typical pipe buffer
    /// are drained concurrently, so the command completes instead of blocking
    /// on a full pipe.
    #[test]
    fn when_shell_command_given_large_output_on_both_streams_then_captures_without_blocking() {
        let root = scratch_dir("shell-large-streams");
        let bytes = 256 * 1024;
        let command = format!(
            "printf '%{bytes}s' '' | tr ' ' 'o'; printf '%{bytes}s' '' | tr ' ' 'e' >&2"
        );

        let output = shell_output_with_timeout(&root, &command, SHELL_TIMEOUT)
            .expect("a command that fills both pipes must complete instead of deadlocking");

        assert!(
            output.status.success(),
            "the large-output command should succeed"
        );
        assert_eq!(
            output.stdout.len(),
            bytes,
            "the complete stdout stream must be captured"
        );
        assert_eq!(
            output.stdout,
            vec![b'o'; bytes],
            "the captured stdout must be exactly the bytes the command wrote"
        );
        assert_eq!(
            output.stderr.len(),
            bytes,
            "the complete stderr stream must be captured separately"
        );
        assert_eq!(
            output.stderr,
            vec![b'e'; bytes],
            "the captured stderr must be exactly the bytes the command wrote"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Edge-case boundary: a Bash that exits while a background descendant
    /// still holds an output pipe open must not keep the caller blocked in the
    /// reader wait past the supplied deadline.
    #[test]
    fn when_shell_command_given_descendant_holds_output_pipe_then_returns_by_deadline() {
        let root = scratch_dir("shell-descendant-pipe");
        let command = "sleep 2 & printf 'BASH-EXITED\\n'";
        let timeout = std::time::Duration::from_millis(50);

        let started = std::time::Instant::now();
        let error = shell_output_with_timeout(&root, command, timeout)
            .expect_err("a descendant holding the pipe must not outlive the deadline");
        let elapsed = started.elapsed();

        assert_eq!(
            error,
            format!("[ERROR: exit code -1 for `{command}`]\ncommand timed out after 0s"),
            "a descendant-held pipe must expire into the established timeout shape"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "the reader wait must respect the deadline instead of the descendant, took {elapsed:?}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Edge-case table across the shell and explicit-root entry points: stdin
    /// EOF, distinct process and project roots, a missing-root read without a
    /// process-directory fallback, malformed scripts, empty and nonzero command
    /// results, and unusable-root spawn failures each keep their observable
    /// contract.
    #[test]
    fn when_shell_edge_cases_given_execution_contexts_then_preserves_contracts() {
        #[derive(Clone, Copy)]
        enum EntryPoint {
            Bash,
            Verification,
            Script,
        }

        enum Expectation {
            Output(String),
            WorkingDirectory,
            ErrorExact(String),
            ErrorPrefix {
                prefix: String,
                must_contain: Vec<String>,
                must_not_contain: Vec<String>,
            },
        }

        struct EdgeCase {
            context: &'static str,
            command: String,
            entry: EntryPoint,
            unusable_root: bool,
            process_fixture: Option<(ProcessCwdCleanup, &'static str)>,
            expected: Expectation,
        }

        let stdin_command =
            "if read -r line; then printf 'DATA:%s\\n' \"$line\"; else printf 'EOF\\n'; fi";
        let nonzero_command = "printf 'OUT-LINE\\n'; printf 'ERR-LINE\\n' >&2; exit 4";
        let nonzero_expected =
            format!("[ERROR: exit code 4 for `{nonzero_command}`]\nOUT-LINE\nERR-LINE\n");
        let spawn_prefix = "[ERROR: exit code -1 for `pwd`]\n";
        let script_error_prefix = "[ERROR: execute_script failed]";

        let missing_name = fixture_name("table-missing-root");
        let missing_code = format!("return tools.read({});", js_string(&missing_name));
        let missing_fixture = ProcessCwdCleanup::create(&missing_name, "PROCESS-ONLY-CONTENT");

        let process_cwd =
            std::env::current_dir().expect("the process working directory should be readable");

        let cases = vec![
            EdgeCase {
                context: "table-bash-explicit-root",
                command: "pwd".to_string(),
                entry: EntryPoint::Bash,
                unusable_root: false,
                process_fixture: None,
                expected: Expectation::WorkingDirectory,
            },
            EdgeCase {
                context: "table-verification-explicit-root",
                command: "pwd".to_string(),
                entry: EntryPoint::Verification,
                unusable_root: false,
                process_fixture: None,
                expected: Expectation::WorkingDirectory,
            },
            EdgeCase {
                context: "table-bash-closed-stdin",
                command: stdin_command.to_string(),
                entry: EntryPoint::Bash,
                unusable_root: false,
                process_fixture: None,
                expected: Expectation::Output("EOF\n".to_string()),
            },
            EdgeCase {
                context: "table-verification-closed-stdin",
                command: stdin_command.to_string(),
                entry: EntryPoint::Verification,
                unusable_root: false,
                process_fixture: None,
                expected: Expectation::Output("EOF\n".to_string()),
            },
            EdgeCase {
                context: "table-verification-empty-output",
                command: "true".to_string(),
                entry: EntryPoint::Verification,
                unusable_root: false,
                process_fixture: None,
                expected: Expectation::Output(String::new()),
            },
            EdgeCase {
                context: "table-script-missing-root-file",
                command: missing_code,
                entry: EntryPoint::Script,
                unusable_root: false,
                process_fixture: Some((missing_fixture, "PROCESS-ONLY-CONTENT")),
                expected: Expectation::ErrorPrefix {
                    prefix: script_error_prefix.to_string(),
                    must_contain: vec!["tools.read".to_string()],
                    must_not_contain: vec!["PROCESS-ONLY-CONTENT".to_string()],
                },
            },
            EdgeCase {
                context: "table-script-invalid-javascript",
                command: "return missingIdentifier;".to_string(),
                entry: EntryPoint::Script,
                unusable_root: false,
                process_fixture: None,
                expected: Expectation::ErrorPrefix {
                    prefix: script_error_prefix.to_string(),
                    must_contain: vec!["missingIdentifier is not defined".to_string()],
                    must_not_contain: Vec::new(),
                },
            },
            EdgeCase {
                context: "table-bash-nonzero-exit",
                command: nonzero_command.to_string(),
                entry: EntryPoint::Bash,
                unusable_root: false,
                process_fixture: None,
                expected: Expectation::ErrorExact(nonzero_expected.clone()),
            },
            EdgeCase {
                context: "table-verification-nonzero-exit",
                command: nonzero_command.to_string(),
                entry: EntryPoint::Verification,
                unusable_root: false,
                process_fixture: None,
                expected: Expectation::ErrorExact(nonzero_expected),
            },
            EdgeCase {
                context: "table-bash-unusable-root",
                command: "pwd".to_string(),
                entry: EntryPoint::Bash,
                unusable_root: true,
                process_fixture: None,
                expected: Expectation::ErrorPrefix {
                    prefix: spawn_prefix.to_string(),
                    must_contain: Vec::new(),
                    must_not_contain: Vec::new(),
                },
            },
            EdgeCase {
                context: "table-verification-unusable-root",
                command: "pwd".to_string(),
                entry: EntryPoint::Verification,
                unusable_root: true,
                process_fixture: None,
                expected: Expectation::ErrorPrefix {
                    prefix: spawn_prefix.to_string(),
                    must_contain: Vec::new(),
                    must_not_contain: Vec::new(),
                },
            },
        ];

        for case in cases {
            let root = scratch_dir(case.context);
            if case.unusable_root {
                std::fs::remove_dir_all(&root).expect("the scratch root should be removable");
            }

            let result = match case.entry {
                EntryPoint::Bash => run_bash_tool_in(&root, &case.command),
                EntryPoint::Verification => run_command_in(&root, &case.command),
                EntryPoint::Script => run_script_in(&root, Phase::Plan, &case.command),
            };

            match &case.expected {
                Expectation::Output(expected) => assert_eq!(
                    result,
                    Ok(expected.clone()),
                    "{} must preserve its successful output",
                    case.context
                ),
                Expectation::WorkingDirectory => {
                    let output = result.unwrap_or_else(|error| {
                        panic!("{} must run in its project root: {error}", case.context)
                    });
                    let actual = std::fs::canonicalize(output.trim()).unwrap_or_else(|error| {
                        panic!(
                            "{} must report an existing directory: {error}",
                            case.context
                        )
                    });
                    let expected = std::fs::canonicalize(&root).unwrap_or_else(|error| {
                        panic!("{} scratch root should canonicalize: {error}", case.context)
                    });
                    assert_eq!(
                        actual, expected,
                        "{} must use the explicit project root",
                        case.context
                    );
                    assert_ne!(
                        actual,
                        std::fs::canonicalize(&process_cwd)
                            .expect("the process working directory should canonicalize"),
                        "{} must not use the process working directory",
                        case.context
                    );
                }
                Expectation::ErrorExact(expected) => assert_eq!(
                    result,
                    Err(expected.clone()),
                    "{} must preserve its formatted failure",
                    case.context
                ),
                Expectation::ErrorPrefix {
                    prefix,
                    must_contain,
                    must_not_contain,
                } => {
                    let error = match result {
                        Ok(value) => panic!(
                            "{} must fail through the established error path, got {value:?}",
                            case.context
                        ),
                        Err(error) => error,
                    };
                    assert!(
                        error.starts_with(prefix),
                        "{} must keep the formatted error prefix `{prefix}`: {error}",
                        case.context
                    );
                    assert!(
                        error.len() > prefix.len(),
                        "{} must report its failure cause after the prefix: {error}",
                        case.context
                    );
                    for fragment in must_contain {
                        assert!(
                            error.contains(fragment),
                            "{} must report `{fragment}`: {error}",
                            case.context
                        );
                    }
                    for fragment in must_not_contain {
                        assert!(
                            !error.contains(fragment),
                            "{} must not observe `{fragment}`: {error}",
                            case.context
                        );
                    }
                }
            }

            if let Some((fixture, contents)) = case.process_fixture {
                assert_eq!(
                    std::fs::read_to_string(fixture.path())
                        .expect("the process-directory fixture should stay readable"),
                    contents,
                    "{} must leave the process-directory fixture untouched",
                    case.context
                );
            }

            if !case.unusable_root {
                let _ = std::fs::remove_dir_all(&root);
            }
        }
    }

    /// Marker set only in the isolated child process that exercises the legacy
    /// entry point.
    const LEGACY_SPAWN_CHILD_ENV: &str = "EJAJ_LEGACY_SPAWN_ERROR_CHILD";
    /// Scratch directory handed to the isolated child so it can enter and make
    /// its process directory unusable.
    const LEGACY_SPAWN_ROOT_ENV: &str = "EJAJ_LEGACY_SPAWN_ERROR_ROOT";

    /// Regression: the public rootless command API keeps the established
    /// spawn-error prefix and cause formatting even when its process-relative
    /// working directory cannot be used. The check runs in a spawned child test
    /// process so changing the working directory cannot race sibling tests.
    #[test]
    fn when_legacy_verification_command_given_unusable_process_directory_then_preserves_spawn_error_shape()
     {
        if std::env::var_os(LEGACY_SPAWN_CHILD_ENV).is_some() {
            let root = std::env::var_os(LEGACY_SPAWN_ROOT_ENV)
                .map(std::path::PathBuf::from)
                .expect("the child must receive the scratch directory to enter");
            std::env::set_current_dir(&root)
                .expect("the child test should enter its scratch directory");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o000))
                    .expect("the child should make its process directory unusable");
            }

            let command = "pwd";
            let prefix = format!("[ERROR: exit code -1 for `{command}`]\n");
            let error = run_command(command)
                .expect_err("an unusable process directory must fail the rootless command API");

            assert!(
                error.starts_with(&prefix),
                "the legacy spawn error must keep its prefix: {error}"
            );
            assert!(
                error.len() > prefix.len(),
                "the legacy spawn error must report its cause: {error}"
            );

            return;
        }

        let root = scratch_dir("legacy-unusable-process-dir");
        let executable = std::env::current_exe().expect("the test executable should be locatable");
        let output = std::process::Command::new(executable)
            .arg("--exact")
            .arg(
                "tools::tests::when_legacy_verification_command_given_unusable_process_directory_then_preserves_spawn_error_shape",
            )
            .arg("--nocapture")
            .env(LEGACY_SPAWN_CHILD_ENV, "1")
            .env(LEGACY_SPAWN_ROOT_ENV, &root)
            .env("PATH", &root)
            .output()
            .expect("the isolated child test process should start");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700));
        }
        let _ = std::fs::remove_dir_all(&root);

        let child_stdout = String::from_utf8_lossy(&output.stdout);
        let child_stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "the isolated legacy spawn-error test failed: stdout={child_stdout}\nstderr={child_stderr}"
        );
        assert!(
            child_stdout.contains("1 passed"),
            "the isolated child must run exactly the legacy spawn-error test: stdout={child_stdout}\nstderr={child_stderr}"
        );
    }

    /// Contract and invariant: a read-only phase reads inside the explicit root
    /// but has no write binding at all.
    #[test]
    fn when_script_inspection_given_read_only_phase_and_explicit_root_then_reads_without_write_access()
     {
        let root = scratch_dir("script-read-only-root");
        let read_name = fixture_name("read-only-read");
        let write_name = fixture_name("read-only-write");
        std::fs::write(root.join(&read_name), "READ-ONLY-CONTENT")
            .expect("the read fixture should be writable");
        let _process_write = ProcessCwdCleanup::absent(&write_name);

        let read = run_script_in(
            &root,
            Phase::Plan,
            &format!("return tools.read({});", js_string(&read_name)),
        )
        .expect("a read-only phase must read inside the explicit root");
        assert_eq!(read, "READ-ONLY-CONTENT");

        let error = run_script_in(
            &root,
            Phase::Plan,
            &format!(
                "tools.write({}, 'FORBIDDEN'); return 'wrote';",
                js_string(&write_name)
            ),
        )
        .expect_err("a read-only phase must not write");
        assert!(
            error.starts_with("[ERROR: execute_script failed]"),
            "the rejected write must keep the formatted script-error shape: {error}"
        );
        assert!(
            !root.join(&write_name).exists(),
            "the read-only phase must not create a file in the explicit root"
        );
        assert!(
            !process_cwd_path(&write_name).exists(),
            "the read-only phase must not create a file in the process directory"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Contract and invariant: the code phase writes and reads inside the
    /// explicit root, never the process working directory.
    #[test]
    fn when_script_inspection_given_code_phase_and_explicit_root_then_writes_within_that_project() {
        let root = scratch_dir("script-code-write-root");
        let name = fixture_name("code-write");
        let _process_cleanup = ProcessCwdCleanup::absent(&name);
        let code = format!(
            "tools.write({}, 'CODE-WRITE'); return tools.read({});",
            js_string(&name),
            js_string(&name)
        );

        let result = run_script_in(&root, Phase::Code, &code)
            .expect("the code phase must write and read inside the explicit root");

        assert_eq!(result, "CODE-WRITE");
        assert_eq!(
            std::fs::read_to_string(root.join(&name))
                .expect("the write must land in the explicit root"),
            "CODE-WRITE"
        );
        assert!(
            !process_cwd_path(&name).exists(),
            "the code-phase write must not land in the process directory"
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
