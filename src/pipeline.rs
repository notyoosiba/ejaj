//! The pipeline: the sequence `main` used to run inline after a handoff.
//!
//! This is the only place the agents are composed. [`Pipeline`] owns the plan
//! agent for the life of the process; one [`Pipeline::ask`] drives one REPL
//! line all the way through: the plan interview's `ask`, the plan handoff write,
//! one fresh spec pass, the spec handoff write, one fresh tickets pass, the
//! ticket writes, one fresh triage pass over the original draft set, the ticket
//! queue over the fresh code agent -- but only when the validated triage report
//! marks every draft ticket ready -- then a bounded review loop: one review of
//! that queue, and, whenever a review reports fix tickets, one more queue round
//! over exactly those fix tickets followed by the next review. At most three
//! review-generated fix queues run, each followed by its own review, so the run
//! reaches at most four reviews; any passing review ends the loop, and a queue
//! that stops or drains incomplete ends the run without another review. Finally,
//! after every queue outcome, one fresh summary pass over the report that very
//! invocation generated. The return value tells `main` what to do next -- keep
//! the REPL going, exit, or report a failure.
//!
//! Every one of those phases is timed around its own call: each plan ask, the
//! spec, tickets and triage passes, every queue round, every review round and
//! the summary pass reports one additive elapsed line as soon as that call
//! returns, whatever it returned. The queue and review rounds carry one-based
//! round numbers, and nothing else is timed -- not the preflight, not an
//! artifact write, not the run as a whole.
//!
//! The library owns no artifact-directory policy and never exits: the directory
//! is a parameter and every failure is reported to the caller, which owns the
//! console error line and the process exit.

use crate::Config;
use crate::git;
use crate::loops::{InvocationContext, plan, review, spec, summary, tickets, triage};
use crate::orchestrator;
use crate::plan_document::Plan;
use crate::review::{ReviewReport, render_reviews};
use crate::specification::Specification;
use crate::status::{self, OutputCoordinator, OutputKind, OutputSink};
use crate::tickets::{Ticket, TicketSet};
use crate::triage::{TRIAGE_ARTIFACT, TriageReport};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Upper bound on review-generated fix queue rounds in one [`Pipeline::ask`]:
/// each of these rounds runs a review pass, queues the fix set that review
/// emitted, runs the queue, and then reviews again.
pub const MAX_FIX_QUEUE_ROUNDS: usize = 3;
/// Upper bound on review passes in one [`Pipeline::ask`]: the review of the
/// original queue plus one review after each review-generated fix queue round,
/// so the last permitted fix set is followed by the fourth review that can pass
/// the run.
pub const MAX_REVIEW_ROUNDS: usize = MAX_FIX_QUEUE_ROUNDS + 1;
/// What the REPL should do after one [`Pipeline::ask`].
#[derive(Debug)]
pub enum Step {
    /// Back at `plan>>`: nothing handed off this line, or the run ended without
    /// resolving every ticket.
    Retry,
    /// The queue resolved every ticket: the only step that claims success, and
    /// the REPL then exits.
    Complete,
    /// Git could not record the run, or the run could not start: the one fatal
    /// result every Git failure arrives as. `stage` names the operation that
    /// failed -- `"preflight"` for the status read that begins every
    /// invocation, before any agent is asked anything or any artifact is
    /// written; `"branch-detection"` for the current/default branch identity
    /// read that decides how the run is recorded (including a detached HEAD, a
    /// non-branch target, or a missing, unreadable, dangling or out-of-prefix
    /// `origin/HEAD`); `"branch"` for the summary-branch uniqueness query and
    /// the branch creation; `"stash"` for saving the run's visible changes with an
    /// untracked-including stash and verifying the new entry; `"switch"` for
    /// switching to the default branch; `"pull"` for the fast-forward-only
    /// update of that branch; `"stash-pop"` for reapplying the saved changes;
    /// `"stage"` for the `git add --all` that stages the whole working tree;
    /// `"commit"` for the commit that follows it; and `"push"` for the
    /// non-overwriting publication of the summary branch -- and `reason` is
    /// Git's own diagnostic, or the readable placeholder when Git wrote no
    /// stderr, or the readable status preflight needed and could not get. This
    /// is fatal at every stage: a run whose root could not be preflighted, a run
    /// whose branch identity could not be read, a default-branch run whose save,
    /// switch, pull, branch query, branch creation, reapply, stage, commit or
    /// push could not complete, and a run whose changes were not committed, all
    /// never report completion. No failed stage rolls anything back.
    GitFailed { stage: &'static str, reason: String },
    /// A handoff document could not be written. `artifact` is `"plan"`, `"spec"`,
    /// `"ticket"`, `"triage"`, `"review"` or `"change-summary"`.
    WriteFailed {
        artifact: &'static str,
        error: io::Error,
    },
    /// The summary phase produced no usable summary. The queue might have
    /// resolved, drained or stopped; either way no change summary exists, and
    /// this is fatal rather than a recoverable `Retry`. `reason` names what was
    /// wrong -- an agent failure, or an answer the format refused.
    SummaryFailed { reason: String },
}

/// The five agents composed into one run. The plan agent's transcript lives for
/// the whole process; each spec pass and each tickets pass is fresh, the code
/// agent is driven only by the queue -- one ticket per fresh transcript -- and
/// the summary pass runs once, in its own transcript, after the queue.
pub struct Pipeline {
    config: Config,
    scope: String,
    /// The one directory the run is anchored to: the project root the process
    /// resolved. Every invocation reads this root's Git status before it does
    /// anything else, so no agent is ever asked anything while the tree the
    /// run would change is not clean.
    root: PathBuf,
    plan_agent: plan::Session,
}

impl Pipeline {
    /// Seed the plan agent and hold the config, the scope and the project root
    /// the spec and tickets passes and the queue need. They each own their own
    /// copies of the config; the root is kept so that every `ask` can prove the
    /// tree this run would change is clean before any agent is asked anything.
    pub fn new(config: &Config, scope: &str, root: impl AsRef<Path>) -> Pipeline {
        Pipeline {
            config: config.clone(),
            scope: scope.to_string(),
            root: root.as_ref().to_path_buf(),
            plan_agent: plan::Session::new(config, scope),
        }
    }

    /// Drive one REPL line: project-root preflight → plan interview → plan
    /// handoff write → spec pass → spec handoff write → tickets pass → ticket
    /// writes → triage pass → ticket queue. `dir` is the directory every
    /// artifact lands in; it is created only when an artifact is actually
    /// written.
    ///
    /// Each phase call is wrapped on its own, so one line reports every
    /// invocation that actually happened: the plan ask is timed per REPL line,
    /// each queue round and each review round carries its own one-based number,
    /// the summary pass covers its whole generation -- a correction inside the
    /// pass included -- and every returned outcome, not only a handoff, reports
    /// the line of the phase it came from.
    pub fn ask(&mut self, input: &str, dir: &Path) -> Step {
        // The first operation of every invocation, before the plan agent is
        // asked anything and before the first artifact write: the root this run
        // is anchored to must have a clean working tree. A run that began over
        // someone else's uncommitted work could not tell its own changes from
        // theirs -- and a status that cannot be read is a failure too, never an
        // empty, clean-looking one. The failure arrives in the same staged shape
        // as the staging and the commit that record a finished run, so every
        // fatal Git failure reaches the caller as one result, distinguished only
        // by the stage it names.
        if let Err(failure) = git::preflight(&self.root) {
            return Step::GitFailed {
                stage: failure.stage,
                reason: failure.reason,
            };
        }

        let plan = match self.plan_handoff(input, dir) {
            Ok(plan) => plan,
            Err(step) => return step,
        };

        // A validated handoff ends the interview. The plan session keeps only
        // its pinned system prompt and one assistant message carrying the typed
        // plan's canonical rendering, so no exploratory turn or handoff tool
        // call can leak into the next plan request. This changes only the
        // in-memory conversation: plan.md and the typed plan are untouched.
        self.plan_agent.accept_handoff(&plan);

        // The plan is rendered from the object the plan agent handed over --
        // never the model's raw text -- so `plan.md` parses back into the same
        // typed plan. A write failure is reported before anything is printed.
        if let Err(step) = write_artifact(
            dir,
            "plan.md",
            &plan.to_markdown(),
            "plan",
            "plan written to",
        ) {
            return step;
        }

        let specification = match self.spec_pass(&plan, dir) {
            Ok(specification) => specification,
            Err(step) => return step,
        };
        let set = match self.tickets_pass(&plan, &specification, dir) {
            Ok(set) => set,
            Err(step) => return step,
        };

        // One file per ticket, in the order the set was emitted, each holding
        // exactly the rendered ticket the queue will hand to the code agent.
        let tickets_dir = dir.join("tickets");
        if let Err(step) = write_tickets(&tickets_dir, &set.tickets) {
            return step;
        }

        if let Err(step) = self.triage_pass(&plan, &specification, &set, dir) {
            return step;
        }

        let run = match self.queue_and_review(&specification, set, &tickets_dir, dir) {
            Ok(run) => run,
            Err(step) => return step,
        };

        // The summary phase, exactly once, immediately after the queue/review
        // loop and never before it: one fresh read-only pass over the report
        // content this invocation generated -- `run.report` and
        // `run.review_markdown`, never a reread of files an earlier invocation
        // left behind. The whole pass is one interval: the initial generation,
        // each correction a rejected candidate buys, every tool step in between
        // and the canonicalization are all inside the call this measures.
        let text = match self.summary_pass(&run, dir) {
            Ok(text) => text,
            Err(step) => return step,
        };

        if let Err(step) = write_artifact(
            dir,
            "change-summary.md",
            &text,
            "change-summary",
            "change summary written to",
        ) {
            return step;
        }

        // Only a queue that resolved every ticket and passed its review is
        // recorded; the finalizer reads the current and default branch
        // identities, then stages the whole working tree and creates one
        // ordinary commit whose message is the summary the run just wrote. On a
        // non-default branch that recording stays local. Every other
        // termination keeps its summary artifact and its retryable or fatal
        // step, and runs no Git operation at all.
        if let Err(step) = self.record_run(&run, dir, &text) {
            return step;
        }

        // The summary reaches the console only after the commit that records it
        // succeeded, so a fatal Git failure is never presented as a successful
        // completion.
        self.finish(run, &plan, &text)
    }

    /// Ask the plan interview one turn and keep only a validated handoff.
    fn plan_handoff(&mut self, input: &str, dir: &Path) -> Result<Plan, Step> {
        let invocation = InvocationContext::new(&self.root, dir);
        match status::timed("plan-agent ask", || {
            self.plan_agent.ask_in(input, invocation)
        }) {
            plan::Outcome::Handoff(plan) => Ok(plan),
            // Nothing was handed off: no `finish_plan`, so no artifact and no
            // spec pass. Back at plan>> with no output. This is a plan
            // conversation retry, never a downstream handover: no agreed plan
            // exists to carry forward.
            plan::Outcome::TurnEnd => Err(Step::Retry),
            plan::Outcome::Failed(kind) => {
                let _ = status::shared().stderr_log(&format!(
                    "[WARN] Plan agent ended early ({}); still at plan>>.",
                    kind.label()
                ));
                Err(Step::Retry)
            }
        }
    }

    /// Run the one fresh specification pass and write its artifact.
    fn spec_pass(&mut self, plan: &Plan, dir: &Path) -> Result<Specification, Step> {
        let invocation = InvocationContext::new(&self.root, dir);
        // Unattended, once, in its own transcript: the spec agent turns the plan
        // the user just confirmed into a self-contained technical specification.
        // No user turn happens in between.
        let _ = status::shared().stdout_log("\n=== Spec agent ===\n");
        match status::timed("spec pass", || {
            spec::to_spec_in(&self.config, &self.scope, plan, invocation)
        }) {
            spec::Outcome::Handoff(specification) => {
                // The specification artifact is the object's rendering, on the
                // unchanged path. The object itself is kept: the tickets agent
                // renders it on its side of the boundary.
                write_artifact(
                    dir,
                    "spec.md",
                    &specification.to_markdown(),
                    "spec",
                    "spec written to",
                )?;
                Ok(specification)
            }
            // No specification was produced, so there is nothing to slice: the
            // tickets agent must not start on the plan alone. Back at plan>>
            // with no break.
            spec::Outcome::TurnEnd => {
                let _ = status::shared().stderr_log(
                    "[INFO] Spec agent ended its turn without handing over a specification; back \
                     at plan>>. Call `finish_plan` again with the revised plan.",
                );
                Err(self.downstream_retry(
                    plan,
                    "spec",
                    "ended its turn without handing over a specification",
                ))
            }
            spec::Outcome::Failed(kind) => {
                let _ = status::shared().stderr_log(&format!(
                    "[WARN] Spec agent ended early ({}); back at plan>>. Call `finish_plan` again \
                     with the revised plan.",
                    kind.label()
                ));
                Err(self.downstream_retry(plan, "spec", format!("ended early ({})", kind.label())))
            }
        }
    }

    /// Run the one fresh tickets pass over the specification.
    fn tickets_pass(
        &mut self,
        plan: &Plan,
        specification: &Specification,
        dir: &Path,
    ) -> Result<TicketSet, Step> {
        let invocation = InvocationContext::new(&self.root, dir);
        // Unattended, once, in its own transcript: the tickets agent turns the
        // specification into the ordered set the queue runs. There is no direct
        // specification-to-code path any more.
        let _ = status::shared().stdout_log("\n=== Tickets agent ===\n");
        match status::timed("tickets pass", || {
            tickets::to_tickets_in(&self.config, &self.scope, specification, invocation)
        }) {
            tickets::Outcome::Handoff(set) => Ok(set),
            tickets::Outcome::TurnEnd => {
                let _ = status::shared().stderr_log(
                    "[INFO] Tickets agent ended its turn without handing over a ticket set; back \
                     at plan>>. There is no direct specification-to-code path: call \
                     `finish_plan` again so the tickets agent can call `finish_tickets`.",
                );
                Err(self.downstream_retry(
                    plan,
                    "tickets",
                    "ended its turn without handing over a ticket set",
                ))
            }
            tickets::Outcome::Failed(kind) => {
                let _ = status::shared().stderr_log(&format!(
                    "[WARN] Tickets agent ended early ({}); back at plan>>. There is no direct \
                     specification-to-code path: call `finish_plan` again so the tickets agent \
                     can call `finish_tickets`.",
                    kind.label()
                ));
                Err(self.downstream_retry(
                    plan,
                    "tickets",
                    format!("ended early ({})", kind.label()),
                ))
            }
        }
    }

    /// Run the one fresh read-only triage pass, write its report and decide
    /// whether the whole draft set may be queued.
    fn triage_pass(
        &mut self,
        plan: &Plan,
        specification: &Specification,
        set: &TicketSet,
        dir: &Path,
    ) -> Result<TriageReport, Step> {
        let invocation = InvocationContext::new(&self.root, dir);
        // Unattended, once, in its own transcript: the triage agent audits the
        // original draft set -- the specification it was sliced from and every
        // draft ticket, unchanged -- before the queue is constructed or run. The
        // pass is offered no `bash` and its scripts run read-only.
        let _ = status::shared().stdout_log("\n=== Triage agent ===\n");
        let report = match status::timed("triage pass", || {
            triage::audit_in(&self.config, &self.scope, specification, set, invocation)
        }) {
            triage::Outcome::Handoff(report) => report,
            // No report exists, so the draft set was not audited and the queue
            // must not run: back at plan>> with no break and no execution.
            triage::Outcome::TurnEnd => {
                let _ = status::shared().stderr_log(
                    "[INFO] Triage agent ended its turn without handing over a triage report; \
                     the draft set was not audited, so no ticket ran. The run is incomplete and \
                     retryable: back at plan>>. Call `finish_plan` again to start a new run.",
                );
                return Err(self.downstream_retry(
                    plan,
                    "triage",
                    "ended its turn without handing over a triage report; the draft set was not audited",
                ));
            }
            triage::Outcome::Failed(kind) => {
                let _ = status::shared().stderr_log(&format!(
                    "[WARN] Triage agent ended early ({}); the draft set was not audited, so no \
                     ticket ran. The run is incomplete and retryable: back at plan>>. Call \
                     `finish_plan` again to start a new run.",
                    kind.label()
                ));
                return Err(self.downstream_retry(
                    plan,
                    "triage",
                    format!(
                        "ended early ({}); the draft set was not audited",
                        kind.label()
                    ),
                ));
            }
        };

        // The artifact is the typed report's own rendering, in draft-ticket
        // order, written beside `plan.md` and `spec.md` for every valid report
        // -- ready or blocked. A write that fails is reported before anything is
        // queued, so no ticket can run without its audit on record.
        write_artifact(
            dir,
            TRIAGE_ARTIFACT,
            &report.to_markdown(set),
            "triage",
            "triage report written to",
        )?;

        // The validated typed report is the sole authority on whether the queue
        // runs: one classification per draft ticket, every one ready-for-agent.
        // Anything else -- a ticket needing a human answer, or a `wontfix` --
        // blocks the whole set. No subset runs, and nothing waits for an
        // interactive answer.
        if !authorizes_queue(&report) {
            let _ = status::shared().stdout_log(&report.to_markdown(set));
            let _ = status::shared().stderr_log(
                "[WARN] Triage did not mark every draft ticket ready-for-agent, so no ticket \
                 ran; the findings above say what blocks each one. The run is incomplete and \
                 retryable: no subset of the draft set executes, and no answer is waited for. \
                 Deal with the blockers and call `finish_plan` again to start a new run.",
            );
            return Err(self.downstream_retry(plan, "triage", triage_blockers(&report)));
        }

        Ok(report)
    }

    /// Drive the bounded queue-and-review loop to its termination, keeping the
    /// content and review state the summary pass is handed.
    fn queue_and_review(
        &mut self,
        specification: &Specification,
        set: TicketSet,
        tickets_dir: &Path,
        dir: &Path,
    ) -> Result<RunReview, Step> {
        let status_path = tickets_dir.join("queue-status.json");
        // The report sits beside the plan and the specification: the `tickets`
        // directory holds one markdown file per ticket id and nothing else.
        let report_path = dir.join("queue-report.md");
        let mut progress = ReviewProgress::new(set);

        loop {
            progress.queue_round += 1;
            let result = self.run_queue_round(&progress, dir, &status_path, &report_path);

            match &result.outcome {
                orchestrator::Outcome::Resolved => {}
                orchestrator::Outcome::Incomplete => {
                    let _ = status::shared().stderr_log(
                        "[WARN] the run did not resolve every ticket; the plan tool must be called \
                         again to start a new run.",
                    );
                    return Ok(RunReview::terminated(
                        RunTermination::Queue(orchestrator::Outcome::Incomplete),
                        result.report,
                        &progress,
                    ));
                }
                orchestrator::Outcome::Stopped { ticket, reason } => {
                    let _ = status::shared().stderr_log(&format!(
                        "[WARN] Queue stopped on ticket `{}`: {}; the queue stopped and the plan \
                         tool must be called again to start a new run.",
                        ticket, reason
                    ));
                    return Ok(RunReview::terminated(
                        RunTermination::Queue(orchestrator::Outcome::Stopped {
                            ticket: ticket.clone(),
                            reason: reason.clone(),
                        }),
                        result.report,
                        &progress,
                    ));
                }
            }

            progress
                .prior_completed
                .extend(progress.current_set.tickets.iter().cloned());
            progress.review_round += 1;

            match self.review_round(
                &mut progress,
                specification,
                &result.report,
                dir,
                tickets_dir,
            )? {
                ReviewRound::Queued => {}
                ReviewRound::Passed => {
                    return Ok(RunReview::terminated(
                        RunTermination::Queue(orchestrator::Outcome::Resolved),
                        result.report,
                        &progress,
                    ));
                }
                ReviewRound::Terminated(termination) => {
                    return Ok(RunReview::terminated(termination, result.report, &progress));
                }
            }
        }
    }

    /// Invoke one queue round and report its total and residual elapsed lines.
    fn run_queue_round(
        &self,
        progress: &ReviewProgress,
        dir: &Path,
        status_path: &Path,
        report_path: &Path,
    ) -> orchestrator::QueueResult {
        let _ = status::shared().stdout_log("\n=== Ticket queue ===\n");
        // The whole invocation is one interval: the ticket attempts, their
        // verification gates, every persistence write and the report are all
        // inside the call this measures, and none of them is timed apart. A
        // fresh accumulator is created for this one invocation and discarded
        // after return, so no earlier round can contribute to this round's
        // residual.
        let mut contributions = crate::timing::QueueContributions::new();
        let (result, elapsed) = crate::timing::measured(|| {
            orchestrator::run_with_prior_contributions_in_session(
                &self.config,
                &self.scope,
                &self.root,
                Some(dir),
                &progress.prior_completed,
                &progress.current_set,
                orchestrator::QueueInvocation {
                    status_path,
                    report_path,
                    contributions: &mut contributions,
                },
            )
        });
        // Report the existing total first, then the residual: the part of that
        // same measured interval not already attributed to a model attempt, an
        // executed command or a verification gate. The subtraction saturates,
        // so an interval shorter than its measured parts reports zero rather
        // than wrapping. A failed informational write changes neither the result
        // nor the control flow.
        let total_label = format!("ticket-queue round {}", progress.queue_round);
        let _ = status::shared().elapsed_log(&total_label, elapsed);
        let overhead_label = format!("ticket-queue round {} overhead", progress.queue_round);
        let _ = status::shared().elapsed_log(&overhead_label, contributions.residual(elapsed));
        result
    }

    /// Run one review round, write its cumulative artifact and either queue its
    /// fixes or report how the loop ended.
    fn review_round(
        &mut self,
        progress: &mut ReviewProgress,
        specification: &Specification,
        queue_report: &str,
        dir: &Path,
        tickets_dir: &Path,
    ) -> Result<ReviewRound, Step> {
        let invocation = InvocationContext::new(&self.root, dir);
        let _ = status::shared().stdout_log("\n=== Review agent ===\n");
        let existing_ids: Vec<String> = progress
            .all_tickets
            .iter()
            .map(|ticket| ticket.id.trim().to_string())
            .collect();
        // The round number is the loop's own one-based review state: the
        // original queue's review is round one, and every review-generated fix
        // queue adds exactly one more.
        let round = progress.review_round;
        let review_report = match status::timed(&format!("review round {round}"), || {
            review::audit_in(
                &self.config,
                review::ReviewInputs {
                    scope: &self.scope,
                    specification,
                    tickets: &progress.all_tickets,
                    report: queue_report,
                    prior_reviews: &progress.reviews,
                    existing_ticket_ids: &existing_ids,
                },
                invocation,
            )
        }) {
            review::Outcome::Handoff(report) => report,
            review::Outcome::TurnEnd => {
                let _ = status::shared().stderr_log(
                    "[WARN] Review agent ended its turn without handing over a review report; \
                     back at plan>>. Call `finish_plan` again to start a new run.",
                );
                return Ok(ReviewRound::Terminated(RunTermination::ReviewIncomplete {
                    reason: "the review agent ended its turn without calling `finish_review`"
                        .to_string(),
                }));
            }
            review::Outcome::Failed(kind) => {
                let _ = status::shared().stderr_log(&format!(
                    "[WARN] Review agent ended early ({}); back at plan>>. Call `finish_plan` \
                     again to start a new run.",
                    kind.label()
                ));
                return Ok(ReviewRound::Terminated(RunTermination::ReviewIncomplete {
                    reason: format!("the review agent ended early ({})", kind.label()),
                }));
            }
        };

        progress.reviews.push(review_report.clone());
        progress.review_markdown = render_reviews(&progress.reviews);
        write_artifact(
            dir,
            "review.md",
            &progress.review_markdown,
            "review",
            "review written to",
        )?;

        if review_report.fixes.is_empty() {
            let _ = status::shared().stdout_log(&format!("[INFO] review round {round} passed"));
            return Ok(ReviewRound::Passed);
        }

        // The last permitted review still reported fixes: every one of the
        // three review-generated fix queues has run, and the final review's fix
        // set is deliberately not queued. The run stays incomplete and
        // retryable, exactly as any other non-passing review does.
        if round >= MAX_REVIEW_ROUNDS {
            let _ = status::shared().stderr_log(&format!(
                "[WARN] review still reported {} finding(s) after {MAX_REVIEW_ROUNDS} review \
                 rounds; the plan tool must be called again to start a new run.",
                review_report.findings.len()
            ));
            return Ok(ReviewRound::Terminated(RunTermination::ReviewCapReached {
                rounds: round,
                findings: review_report.findings.len(),
            }));
        }

        write_tickets(tickets_dir, &review_report.fixes)?;
        progress
            .all_tickets
            .extend(review_report.fixes.iter().cloned());
        progress.current_set = TicketSet {
            tickets: review_report.fixes,
        };
        Ok(ReviewRound::Queued)
    }

    /// Run the one summary pass over this invocation's own report and reviews.
    fn summary_pass(&self, run: &RunReview, dir: &Path) -> Result<String, Step> {
        let invocation = InvocationContext::new(&self.root, dir);
        match status::timed("summary pass", || {
            summary::summarize_in(
                &self.config,
                &self.scope,
                &termination_outcome(&run.termination),
                &run.report,
                &run.review_markdown,
                &run.generated_ids,
                invocation,
            )
        }) {
            summary::Outcome::Summary(text) => Ok(text),
            summary::Outcome::Invalid(reason) => Err(Step::SummaryFailed { reason }),
            summary::Outcome::Failed(kind) => Err(Step::SummaryFailed {
                reason: format!("the summary agent ended early ({})", kind.label()),
            }),
        }
    }

    /// Record a resolved queue that passed its review. Any other termination
    /// keeps its summary artifact and runs no Git operation at all.
    fn record_run(&self, run: &RunReview, dir: &Path, text: &str) -> Result<(), Step> {
        if !matches!(
            &run.termination,
            RunTermination::Queue(orchestrator::Outcome::Resolved)
        ) {
            return Ok(());
        }
        let Err(failure) = git::finalize(&self.root, text) else {
            return Ok(());
        };

        // The default-branch recording saves every visible change -- this run's
        // summary artifact among them -- in a stash before it switches, pulls,
        // creates the summary branch and reapplies that stash. A fatal operation
        // in between leaves the artifact only in the retained stash, and a
        // tracked artifact can be restored to its committed bytes by the stash
        // itself. Every finalize failure therefore rewrites the canonical text
        // to the path the run used, whether or not that path is still present; a
        // path that is still there with stale restored bytes is overwritten too.
        // The Git failure is still the result: this is not a rollback, touches no
        // Git state, and no later Git operation runs. A recovery write that
        // fails is ignored, so the original stage and reason survive.
        let _ = write_handoff(dir, "change-summary.md", text);
        Err(Step::GitFailed {
            stage: failure.stage,
            reason: failure.reason,
        })
    }

    /// Emit the summary after any required Git recording, then report the
    /// caller-visible step for the run's termination.
    fn finish(&mut self, run: RunReview, plan: &Plan, text: &str) -> Step {
        let _ = emit_summary(&mut status::shared(), text);

        match &run.termination {
            RunTermination::Queue(orchestrator::Outcome::Resolved) => Step::Complete,
            RunTermination::Queue(outcome) => {
                self.downstream_retry(plan, "queue", queue_retry_detail(outcome, &run.report))
            }
            RunTermination::ReviewIncomplete { reason } => {
                self.downstream_retry(plan, "review", format!("did not complete: {reason}"))
            }
            RunTermination::ReviewCapReached { rounds, findings } => self.downstream_retry(
                plan,
                "review",
                format!(
                    "still reported {findings} unresolved finding(s) after {rounds} review rounds"
                ),
            ),
        }
    }

    /// Record one concise downstream phase outcome in the plan agent's
    /// assistant handover and return the recoverable step.
    ///
    /// Every downstream retry path goes through here after a validated plan
    /// handoff, so the next plan request resumes from the agreed plan plus what
    /// sent the run back. A plan-conversation retry before the handoff, a fatal
    /// Git, artifact-write or summary failure never takes this path.
    fn downstream_retry(
        &mut self,
        plan: &Plan,
        phase: &'static str,
        detail: impl AsRef<str>,
    ) -> Step {
        self.plan_agent
            .record_downstream_retry(plan, phase, detail.as_ref());
        Step::Retry
    }
}

/// One concise triage outcome naming every non-ready draft ticket and its
/// blocker. The plan handover keeps the first part of the blocker when the
/// explanation is long, so the reason the run returned is never lost entirely.
fn triage_blockers(report: &TriageReport) -> String {
    let blockers: Vec<String> = report
        .tickets
        .iter()
        .filter(|classification| !classification.state.is_ready())
        .map(|classification| {
            let explanation = classification
                .blocker_explanation
                .as_deref()
                .unwrap_or("no blocker explanation");
            format!(
                "{} ({}): {}",
                classification.ticket_id.trim(),
                classification.state.wire(),
                explanation
            )
        })
        .collect();

    if blockers.is_empty() {
        "the report did not mark every draft ticket ready-for-agent".to_string()
    } else {
        blockers.join("; ")
    }
}

/// One concise queue outcome for the plan handover. The report's own failed
/// ticket sections supply the relevant gate/agent result, so a drained queue
/// names what actually went wrong instead of only restating its status.
fn queue_retry_detail(outcome: &orchestrator::Outcome, report: &str) -> String {
    match outcome {
        orchestrator::Outcome::Resolved => "completed every ticket".to_string(),
        orchestrator::Outcome::Incomplete => {
            let failures = queue_failures(report);
            if failures.is_empty() {
                "drained without completing every ticket".to_string()
            } else {
                format!("drained without completing every ticket ({failures})")
            }
        }
        orchestrator::Outcome::Stopped { ticket, reason } => {
            let ticket = ticket.trim();
            if ticket.is_empty() {
                format!("stopped before any ticket started: {reason}")
            } else {
                format!("stopped on ticket `{ticket}`: {reason}")
            }
        }
    }
}

/// The failed ticket sections of a queue report, as "id: ending" fragments in
/// emitted order.
fn queue_failures(report: &str) -> String {
    let mut failures = Vec::new();
    let mut id: Option<String> = None;
    let mut status: Option<String> = None;
    let mut ended: Option<String> = None;

    for line in report.lines() {
        if let Some(section) = line.strip_prefix("## ") {
            flush_queue_failure(&mut failures, &mut id, &mut status, &mut ended);
            id = Some(section.trim().to_string());
            status = None;
            ended = None;
        } else if let Some(value) = line.strip_prefix("- Status: ") {
            status = Some(value.trim().to_string());
        } else if let Some(value) = line.strip_prefix("- Ended: ") {
            ended = Some(value.trim().to_string());
        }
    }
    flush_queue_failure(&mut failures, &mut id, &mut status, &mut ended);
    failures.join("; ")
}

fn flush_queue_failure(
    failures: &mut Vec<String>,
    id: &mut Option<String>,
    status: &mut Option<String>,
    ended: &mut Option<String>,
) {
    let Some(ticket) = id.take() else {
        return;
    };
    if status.as_deref() == Some("failed") {
        let ending = ended.take().unwrap_or_else(|| "failed".to_string());
        failures.push(format!("{ticket}: {ending}"));
    }
    *status = None;
    *ended = None;
}

/// Whether the validated triage report authorizes the queue: every draft ticket
/// classified, and every classification `ready-for-agent`.
///
/// The report is the only authority. The check reads the typed report and never
/// the model's markdown or narration, so no prose can authorize a run the
/// boundary refused, and a single non-ready classification -- a ticket needing a
/// human answer, or a `wontfix` -- blocks the whole set.
fn authorizes_queue(report: &TriageReport) -> bool {
    !report.tickets.is_empty()
        && report
            .tickets
            .iter()
            .all(|classification| classification.state.is_ready())
}

/// How the bounded queue-and-review loop ended.
enum RunTermination {
    /// Ended on the queue's own outcome (or after a passing review for `Resolved`).
    Queue(orchestrator::Outcome),
    /// The queue resolved, but the review pass ended without `finish_review`.
    ReviewIncomplete { reason: String },
    /// The last permitted review still emitted fix tickets, so its fix set is
    /// reported but never queued: three review-generated fix queues have
    /// already run.
    ReviewCapReached { rounds: usize, findings: usize },
}

/// The mutable state one queue/review loop accumulates: the accepted reviews,
/// the cumulative review artifact, the current queue set and both one-based
/// round numbers.
struct ReviewProgress {
    prior_completed: Vec<Ticket>,
    original_tickets: Vec<Ticket>,
    all_tickets: Vec<Ticket>,
    current_set: TicketSet,
    reviews: Vec<ReviewReport>,
    review_markdown: String,
    review_round: usize,
    queue_round: usize,
}

impl ReviewProgress {
    /// Seed the loop from the original draft set, keeping the original typed
    /// tickets apart for the summary's generated-id gate.
    fn new(set: TicketSet) -> ReviewProgress {
        let original_tickets = set.tickets.clone();
        ReviewProgress {
            prior_completed: Vec::new(),
            all_tickets: original_tickets.clone(),
            original_tickets,
            current_set: set,
            reviews: Vec::new(),
            review_markdown: String::new(),
            review_round: 0,
            queue_round: 0,
        }
    }
}

/// One review round's caller-visible result.
enum ReviewRound {
    /// The review reported fixes, and they were written and queued.
    Queued,
    /// The review reported no fixes: the run passed.
    Passed,
    /// The review ended without a usable report, or the review cap was reached.
    Terminated(RunTermination),
}

/// What the queue/review loop leaves for the summary and the final step.
struct RunReview {
    termination: RunTermination,
    report: String,
    review_markdown: String,
    generated_ids: Vec<String>,
}

impl RunReview {
    /// Keep the loop's final queue report and cumulative review content, with
    /// the generated-id set built from the original tickets and every accepted
    /// review report.
    fn terminated(
        termination: RunTermination,
        report: String,
        progress: &ReviewProgress,
    ) -> RunReview {
        RunReview {
            termination,
            report,
            review_markdown: progress.review_markdown.clone(),
            generated_ids: generated_ticket_ids(&progress.original_tickets, &progress.reviews),
        }
    }
}

/// Every generated ticket id this run created, for the summary's id gate: the
/// original typed tickets, then every accepted review report's fix tickets.
///
/// The review reports include the final allowed round's report, so a fix ticket
/// the cap round reported but never queued or wrote as a ticket file is still
/// part of the set: the summary must not name that one either.
fn generated_ticket_ids(original: &[Ticket], reviews: &[ReviewReport]) -> Vec<String> {
    let mut ids: Vec<String> = original
        .iter()
        .map(|ticket| ticket.id.trim().to_string())
        .collect();
    for review in reviews {
        ids.extend(
            review
                .fixes
                .iter()
                .map(|ticket| ticket.id.trim().to_string()),
        );
    }
    ids
}

/// Format the run termination in the terms the summary pass receives.
fn termination_outcome(termination: &RunTermination) -> String {
    match termination {
        RunTermination::Queue(outcome) => queue_outcome(outcome),
        RunTermination::ReviewIncomplete { reason } => format!(
            "Incomplete: the queue completed every ticket, but the review pass did not complete: \
             {reason}."
        ),
        RunTermination::ReviewCapReached { rounds, findings } => format!(
            "Incomplete: the review pass still reported {findings} unresolved finding(s) after \
             the maximum of {rounds} review rounds."
        ),
    }
}

/// The queue's outcome in the terms the summary pass is handed: its
/// classification, and -- for a stopped queue -- the ticket it stopped on and
/// the reason, so the pass knows how far the run got instead of guessing from
/// the report alone.
fn queue_outcome(outcome: &orchestrator::Outcome) -> String {
    match outcome {
        orchestrator::Outcome::Resolved => {
            "Resolved: the queue completed every ticket.".to_string()
        }
        orchestrator::Outcome::Incomplete => {
            "Incomplete: the queue drained without completing every ticket; the report below \
             names what failed, what was blocked and what was never reached."
                .to_string()
        }
        orchestrator::Outcome::Stopped { ticket, reason } => {
            let ticket = ticket.trim();
            if ticket.is_empty() {
                format!(
                    "Stopped: the queue stopped before it could drain, before any ticket \
                     started, and no ticket is named: {reason}"
                )
            } else {
                format!(
                    "Stopped: the queue stopped before it could drain, on ticket `{ticket}`: \
                     {reason}"
                )
            }
        }
    }
}

/// Write the canonical summary text to `coordinator` as one unmodified
/// multi-line message.
///
/// The artifact ends with exactly one LF and [`OutputCoordinator`] supplies the
/// line terminator of every message it writes, so the payload is the artifact's
/// own bytes without that final LF and nothing else is transformed. What lands
/// on screen is therefore the artifact itself, once.
fn emit_summary<S: OutputSink>(
    coordinator: &mut OutputCoordinator<S>,
    text: &str,
) -> io::Result<()> {
    let payload = text.strip_suffix('\n').unwrap_or(text);
    coordinator.stdout(OutputKind::Log, payload)
}

/// Write one run artifact and name it on the console. The artifact name is the
/// one a write failure reports; the console label is the phase's own.
fn write_artifact(
    dir: &Path,
    name: &str,
    text: &str,
    artifact: &'static str,
    label: &str,
) -> Result<PathBuf, Step> {
    let path =
        write_handoff(dir, name, text).map_err(|error| Step::WriteFailed { artifact, error })?;
    let _ = status::shared().stdout_log(&format!("[INFO] {label} {}", path.display()));
    Ok(path)
}

/// Write one ticket file per ticket, in the order the tickets were emitted.
fn write_tickets(tickets_dir: &Path, tickets: &[Ticket]) -> Result<(), Step> {
    for ticket in tickets {
        write_artifact(
            tickets_dir,
            &ticket.file_name(),
            &ticket.to_markdown(),
            "ticket",
            "ticket written to",
        )?;
    }
    Ok(())
}

/// Write one handoff artifact into `dir`. One directory per handoff, so every
/// revision of the plan, its specification and its tickets is preserved rather
/// than overwritten (the ticket files go into the `tickets` subdirectory).
/// Returns the path so the caller can name the file on the console.
fn write_handoff(dir: &Path, name: &str, text: &str) -> io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    ignore_run_artifacts(dir)?;
    let path = dir.join(name);
    fs::write(&path, text)?;
    Ok(path)
}

/// Ensure the exact `.ejaj` ancestor of `dir` carries a self-ignoring
/// `.gitignore` with exactly `*\n`, so a run's artifacts never surface as
/// untracked changes in the target repository. Only a component whose whole
/// name is `.ejaj` matches: a lookalike such as `.ejaj-extra` is an ordinary
/// destination and is left alone. An existing regular ignore file is never
/// repaired or overwritten; any other entry at that path makes creation fail.
/// The destination directory must already exist, so the ignore file is created
/// after it and before any artifact lands inside it.
fn ignore_run_artifacts(dir: &Path) -> io::Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A sink that records the bytes written to stdout, so the exact payload the
    /// coordinator is handed can be asserted without touching the real streams.
    #[derive(Default)]
    struct RecordingSink {
        stdout: String,
    }

    impl OutputSink for RecordingSink {
        fn write_stdout(&mut self, bytes: &str) -> io::Result<()> {
            self.stdout.push_str(bytes);
            Ok(())
        }

        fn flush_stdout(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn write_stderr(&mut self, _bytes: &str) -> io::Result<()> {
            Ok(())
        }

        fn flush_stderr(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// The printed summary is the artifact itself, once: the payload is the
    /// canonical text without its terminal LF, and the coordinator's own line
    /// terminator supplies that LF -- so what reaches stdout is byte for byte
    /// what `change-summary.md` holds, and nothing else is transformed.
    #[test]
    fn when_emitting_summary_given_validated_answer_then_preserves_canonical_bytes_once() {
        let text = "feat(api): add retry backoff\n\nA body line.\n\nCo-authored-by: Ada Lovelace <ada@example.com>\n";
        let mut out = OutputCoordinator::new(RecordingSink::default());

        emit_summary(&mut out, text).expect("the write succeeds");

        assert_eq!(
            out.sink().stdout,
            text,
            "the whole multi-line summary must be written, once"
        );
        assert_eq!(
            out.last(),
            Some(OutputKind::Log),
            "the summary is one framed message"
        );
    }

    /// The accepted publication path: a final answer the summary validator
    /// accepts is published once, in canonical form, and nothing else emits it.
    /// Combined with the loop-boundary policy that withholds the summary final
    /// turn from narration, this is the only emission the accepted text gets.
    #[test]
    fn when_summary_is_accepted_given_final_answer_then_emits_canonical_text_once() {
        let answer = "feat(api): add retry backoff\n\nRetry the call twice so a flaky endpoint stops failing the run.\n";
        let canonical = summary::canonicalize(answer).expect("the answer is a valid summary");
        let mut out = OutputCoordinator::new(RecordingSink::default());

        emit_summary(&mut out, &canonical).expect("the write succeeds");

        assert_eq!(
            out.sink().stdout,
            canonical,
            "the accepted canonical bytes reach stdout unchanged"
        );
        assert_eq!(
            out.sink()
                .stdout
                .matches("feat(api): add retry backoff")
                .count(),
            1,
            "the accepted summary is emitted exactly once"
        );
        assert_eq!(
            out.sink().stdout.matches("Retry the call twice").count(),
            1,
            "the body is emitted once as well"
        );
    }

    /// CRLF is normalized to LF before the write, and the print is the
    /// artifact: the canonical text of a CRLF answer for a header, a body and a
    /// footer is what the file holds and -- minus the coordinator's terminator
    /// -- what the screen gets.
    #[test]
    fn a_crlf_answer_is_canonicalized_once_for_the_file_and_the_screen() {
        let answer = "feat(api): add retry backoff\r\n\r\nFirst line.  \r\nSecond line.\r\n\r\nCo-authored-by: Ada Lovelace <ada@example.com>\r\n";
        let canonical = summary::canonicalize(answer).expect("the answer is a valid summary");

        assert_eq!(
            canonical,
            "feat(api): add retry backoff\n\nFirst line.\nSecond line.\n\nCo-authored-by: Ada Lovelace <ada@example.com>\n"
        );
        assert!(!canonical.contains('\r'), "the artifact is LF only");

        let mut out = OutputCoordinator::new(RecordingSink::default());
        emit_summary(&mut out, &canonical).expect("the write succeeds");
        assert_eq!(
            out.sink().stdout,
            canonical,
            "the printed summary is the artifact itself"
        );
    }
    /// The only edit is the dropped terminal LF: every other byte of a canonical
    /// summary -- internal blank lines and a footer included -- survives, and a
    /// text that arrives without its LF is written as it stands.
    #[test]
    fn the_summary_payload_loses_only_the_terminal_lf() {
        let mut out = OutputCoordinator::new(RecordingSink::default());
        emit_summary(&mut out, "test(pipeline): a header\n\nBody.\n").unwrap();
        assert_eq!(out.sink().stdout, "test(pipeline): a header\n\nBody.\n");

        let mut out = OutputCoordinator::new(RecordingSink::default());
        emit_summary(&mut out, "test(pipeline): a header\n\nBody.").unwrap();
        assert_eq!(out.sink().stdout, "test(pipeline): a header\n\nBody.\n");
    }

    /// The outcome half of the summary's input names the queue's own
    /// classification, and for a stopped queue the ticket it stopped on and the
    /// reason it gave.
    #[test]
    fn the_summary_outcome_names_the_stopped_ticket_and_its_reason() {
        let resolved = queue_outcome(&orchestrator::Outcome::Resolved);
        assert!(resolved.contains("Resolved"), "{resolved}");

        let incomplete = queue_outcome(&orchestrator::Outcome::Incomplete);
        assert!(incomplete.contains("Incomplete"), "{incomplete}");

        let stopped = queue_outcome(&orchestrator::Outcome::Stopped {
            ticket: "2-second".to_string(),
            reason: "could not persist the queue status: disk full".to_string(),
        });
        assert!(stopped.contains("Stopped"), "{stopped}");
        assert!(stopped.contains("2-second"), "{stopped}");
        assert!(
            stopped.contains("could not persist the queue status: disk full"),
            "{stopped}"
        );

        // A queue that stopped before any ticket started names no ticket, and
        // says so rather than naming an empty one.
        let stopped = queue_outcome(&orchestrator::Outcome::Stopped {
            ticket: String::new(),
            reason: "could not persist the queue status: disk full".to_string(),
        });
        assert!(stopped.contains("no ticket is named"), "{stopped}");
        assert!(!stopped.contains("``"), "{stopped}");
    }

    /// Create a fresh temporary directory for one handoff test, so no test can
    /// observe another test's artifacts or ignore files.
    fn scratch_dir(label: &str) -> PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "ejaj-pipeline-test-{}-{id}-{label}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("temp dir should be creatable");
        dir
    }

    /// Core entry-point behavior, minimum valid boundary: a handoff beneath the
    /// exact `.ejaj` ancestor prepares its destination, writes the exact
    /// self-ignoring bytes into that ancestor and then writes the artifact.
    #[test]
    fn when_writing_handoff_given_ejaj_destination_then_creates_exact_ignore_file() {
        let root = scratch_dir("handoff-ejaj");
        let dir = root.join(".ejaj").join("run-1");
        let text = "# Plan\n";

        let path = write_handoff(&dir, "plan.md", text).expect("the handoff should be written");

        assert_eq!(path, dir.join("plan.md"));
        assert!(dir.is_dir(), "the destination directory must be prepared");
        assert_eq!(
            fs::read(root.join(".ejaj").join(".gitignore")).unwrap(),
            b"*\n".as_slice(),
            "the matching .ejaj ancestor must own exactly the self-ignoring bytes"
        );
        assert_eq!(
            fs::read(&path).unwrap(),
            text.as_bytes(),
            "the handoff contents must be written after the ignore file"
        );

        let _ = fs::remove_dir_all(&root);
    }

    /// Contract and invariant, malformed ignore path: a directory where the
    /// `.ejaj` ignore file belongs fails the handoff through the existing I/O
    /// error path, so no artifact claims to have been written. A directory
    /// obstructs creation on every runner, unlike a permission bit that an
    /// elevated runner can ignore.
    #[test]
    fn when_writing_handoff_given_unwritable_ejaj_ignore_path_then_returns_io_error_without_artifact()
     {
        let root = scratch_dir("handoff-unwritable-ignore");
        let ejaj = root.join(".ejaj");
        let dir = ejaj.join("run");
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir(ejaj.join(".gitignore")).unwrap();

        let error = write_handoff(&dir, "plan.md", "plan")
            .expect_err("a directory at the ignore path must fail the handoff write");

        assert_eq!(
            error.kind(),
            io::ErrorKind::IsADirectory,
            "the existing I/O error path must carry the obstruction: {error}"
        );
        assert!(
            !dir.join("plan.md").exists(),
            "a failed ignore preparation must not leave an artifact behind"
        );
        assert!(
            ejaj.join(".gitignore").is_dir(),
            "the failed writer must neither repair nor replace the obstructing directory"
        );

        let _ = fs::remove_dir_all(&root);
    }

    /// Contract and invariant: an existing regular ignore file is preserved
    /// byte for byte rather than repaired or overwritten.
    #[test]
    fn when_writing_handoff_given_existing_regular_ignore_file_then_preserves_contents() {
        let root = scratch_dir("handoff-existing-ignore");
        let ejaj = root.join(".ejaj");
        let dir = ejaj.join("run");
        fs::create_dir_all(&ejaj).unwrap();
        fs::write(ejaj.join(".gitignore"), "keep this ignore line\n").unwrap();

        let path = write_handoff(&dir, "spec.md", "spec").expect("the handoff should be written");

        assert_eq!(
            fs::read_to_string(ejaj.join(".gitignore")).unwrap(),
            "keep this ignore line\n",
            "an existing regular ignore file must not be replaced"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "spec");

        let _ = fs::remove_dir_all(&root);
    }

    /// Contract and invariant, minimum valid boundary: ordinary destinations
    /// and lookalike component names never gain an ignore file.
    #[test]
    fn when_writing_handoff_given_destination_without_exact_ejaj_component_then_leaves_ignore_files_unchanged()
     {
        let root = scratch_dir("handoff-no-ejaj");
        let ordinary = root.join("artifacts").join("run");
        write_handoff(&ordinary, "plan.md", "plan")
            .expect("the ordinary handoff should be written");
        assert!(
            !ordinary.join(".gitignore").exists(),
            "an ordinary destination must not gain an ignore file"
        );
        assert!(
            !root.join(".gitignore").exists(),
            "an ordinary destination must not change a parent ignore file"
        );

        let lookalike = root.join(".ejaj-extra").join("run");
        write_handoff(&lookalike, "spec.md", "spec")
            .expect("the lookalike handoff should be written");
        assert!(
            !lookalike.join(".gitignore").exists(),
            "a lookalike component must not gain an ignore file"
        );
        assert!(
            !root.join(".ejaj-extra").join(".gitignore").exists(),
            "a lookalike component must not be matched as .ejaj"
        );
        assert!(
            !root.join(".ejaj").exists(),
            "a lookalike component must not create a real .ejaj ancestor"
        );

        let _ = fs::remove_dir_all(&root);
    }

    /// Ordering contract: the destination and its ignore file are prepared
    /// before the artifact write, so a failed artifact write still leaves the
    /// destination self-ignoring. The obstruction is a directory at the
    /// artifact path, which fails on every runner rather than only when the
    /// runner lacks the privilege to write.
    #[test]
    fn when_writing_handoff_given_unwritable_artifact_path_then_prepares_ignore_before_the_artifact()
     {
        let root = scratch_dir("handoff-ordering");
        let ejaj = root.join(".ejaj");
        let dir = ejaj.join("run");
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir(dir.join("plan.md")).unwrap();

        let result = write_handoff(&dir, "plan.md", "plan");

        assert!(
            result.is_err(),
            "a directory at the artifact path must report the artifact write failure"
        );
        assert_eq!(
            fs::read(ejaj.join(".gitignore")).unwrap(),
            b"*\n".as_slice(),
            "ignore preparation must precede the artifact write"
        );
        assert!(
            dir.join("plan.md").is_dir(),
            "the failed artifact must not replace the obstructing directory"
        );

        let _ = fs::remove_dir_all(&root);
    }

    /// A scripted transport for the plan interview: every request is answered
    /// with one plain assistant turn, so the interview ends without a handoff.
    #[derive(Default)]
    struct PlanTurnEndTransport {
        requests: std::sync::atomic::AtomicUsize,
    }

    impl crate::api::Transport for PlanTurnEndTransport {
        fn send(
            &self,
            _outgoing: &crate::api::Outgoing,
        ) -> Result<crate::api::RawResponse, crate::api::CallError> {
            self.requests
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(crate::api::RawResponse {
                status: 200,
                body: serde_json::json!({
                    "choices": [{
                        "message": {
                            "role": "assistant",
                            "content": "I need more detail before I can plan.",
                            "tool_calls": [],
                        }
                    }]
                })
                .to_string(),
            })
        }
    }

    /// Initialize a clean temporary Git repository at `root`. Git and the
    /// filesystem stay real; only the provider transport is scripted.
    fn init_clean_repository(root: &Path) {
        let output = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(root)
            .output()
            .expect("git must be runnable");
        assert!(
            output.status.success(),
            "git init must succeed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Core entry-point behavior: the plan interview ending its turn without a
    /// handoff is a plan-conversation retry. No downstream agent is asked, and
    /// no run artifact is written.
    #[test]
    fn when_plan_interview_ends_given_no_handoff_then_returns_retry_without_artifacts() {
        let root = scratch_dir("plan-turn-end");
        init_clean_repository(&root);
        let transport = std::sync::Arc::new(PlanTurnEndTransport::default());
        let config = Config::new(
            "https://provider.test/v1/chat/completions",
            "scripted-model",
            None,
        )
        .with_transport(transport.clone());
        let dir = root.join(".ejaj").join("run-1");
        let mut pipeline = Pipeline::new(&config, "test scope", &root);

        let step = pipeline.ask("Plan a small change.", &dir);

        assert!(
            matches!(step, Step::Retry),
            "a plan turn end retries: {step:?}"
        );
        assert_eq!(
            transport.requests.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "only the plan interview may be asked"
        );
        assert!(
            !dir.exists(),
            "a plan turn end must not create a run directory"
        );
        assert!(!dir.join("plan.md").exists());
        assert!(!dir.join("spec.md").exists());
        assert!(!dir.join("tickets").exists());

        let _ = fs::remove_dir_all(&root);
    }
}
