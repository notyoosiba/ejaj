//! The ticket queue: a pure scheduler and a minimal, single-threaded runner.
//!
//! Deliberately minimal -- no concurrency, no resume. The runner persists each
//! ticket's status to the JSON snapshot at the caller's path before it starts
//! work and after every transition, and a failed write stops the queue. One
//! ticket that ends without a declaration earns a single retry and then fails
//! alone: its dependents can never become executable, but every other branch
//! still runs, and the queue ends by reporting how each ticket turned out. A
//! later change can extract the whole queue into a separately replaceable
//! component without touching the agent pipeline, because the agent pipeline
//! only ever calls [`run`]. The scheduler is model-free, so its ordering is
//! unit-testable without a config or an agent.

use crate::Config;
use crate::loops::{FailureKind, InvocationContext, code};
use crate::tickets::{Ticket, TicketSet};
use crate::timing::QueueContributions;
use crate::tools::run_command_in;

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// The model-free scheduler over one ticket set.
///
/// A ticket is executable when every one of its `blocked_by` entries has
/// completed. Among the executable tickets the greatest remaining downstream
/// chain wins, where a ticket's chain counts itself and is one plus the largest
/// chain among its still-undecided direct dependents; dependents are derived by
/// inverting the `blocked_by` edges. Ties break to the lowest index in the
/// emitted order. A failed ticket leaves its dependents permanently
/// unexecutable -- their blocker never completes -- so the frontier passes to
/// the other branches instead. The set validator already guarantees acyclicity,
/// so the chain helper cannot recurse forever.
pub struct Scheduler {
    /// The tickets in emitted order, so a tie can fall back to the lowest index.
    tickets: Vec<Ticket>,
    /// One completed flag per ticket, indexed like `tickets`: its verification
    /// gate passed and the completion is durable.
    completed: Vec<bool>,
    /// One failed flag per ticket, indexed like `tickets`: the ticket is decided
    /// and must never be scheduled again.
    failed: Vec<bool>,
    /// `dependents[i]` holds every ticket that lists `tickets[i]` in its
    /// `blocked_by`: the inversion of the blocking edges.
    dependents: Vec<Vec<usize>>,
    /// The index of every id, used to resolve a dependency name to a ticket.
    index_of: HashMap<String, usize>,
}

impl Scheduler {
    /// Build a scheduler over `set`. The set has already been validated, so
    /// every dependency names an id in the set.
    pub fn new(set: &TicketSet) -> Scheduler {
        let tickets = set.tickets.clone();
        let index_of: HashMap<String, usize> = tickets
            .iter()
            .enumerate()
            .map(|(index, ticket)| (ticket.id.trim().to_string(), index))
            .collect();

        let mut dependents = vec![Vec::new(); tickets.len()];
        for (index, ticket) in tickets.iter().enumerate() {
            for dependency in &ticket.blocked_by {
                if let Some(&blocker) = index_of.get(dependency.trim()) {
                    dependents[blocker].push(index);
                }
            }
        }

        Scheduler {
            completed: vec![false; tickets.len()],
            failed: vec![false; tickets.len()],
            tickets,
            dependents,
            index_of,
        }
    }

    /// The next ticket to run, or `None` when nothing is executable.
    ///
    /// The frontier is every unsettled ticket whose blockers have all
    /// completed. The greatest remaining downstream chain wins; equal chains
    /// fall back to the lowest emitted index, which is why this only replaces a
    /// best when the chain is strictly greater.
    pub fn next(&self) -> Option<usize> {
        let mut best: Option<(usize, usize)> = None;
        for index in 0..self.tickets.len() {
            if self.settled(index) || !self.is_executable(index) {
                continue;
            }
            let chain = self.chain(index);
            match best {
                Some((best_chain, _)) if best_chain >= chain => {}
                _ => best = Some((chain, index)),
            }
        }
        best.map(|(_, index)| index)
    }

    /// Mark the ticket at `index` completed: it is durable and its dependents
    /// may now become executable.
    pub fn complete(&mut self, index: usize) {
        self.completed[index] = true;
    }

    /// Mark the ticket at `index` failed. A failed ticket is never scheduled
    /// again, and none of its dependents can ever become executable, so the
    /// frontier simply passes to the other branches.
    pub fn fail(&mut self, index: usize) {
        self.failed[index] = true;
    }

    /// How many tickets are still undecided: the work the queue still owes.
    ///
    /// Every one of them is blocked by a failure once [`Scheduler::next`]
    /// returns `None`.
    pub fn remaining(&self) -> usize {
        (0..self.tickets.len())
            .filter(|&index| !self.settled(index))
            .count()
    }

    /// How many tickets completed.
    pub fn completed_count(&self) -> usize {
        self.completed.iter().filter(|done| **done).count()
    }

    /// How many tickets failed.
    pub fn failed_count(&self) -> usize {
        self.failed.iter().filter(|failed| **failed).count()
    }

    /// The id of the failed ticket that blocks the ticket at `index`: the
    /// nearest failed ancestor, counted in `blocked_by` edges.
    ///
    /// When the frontier is empty every undecided ticket is blocked by some
    /// failure, and the edges are acyclic, so this walk reaches one -- unless
    /// the set names a dependency it does not hold, which the validator forbids
    /// but the scheduler does not rely on.
    pub fn failed_ancestor(&self, index: usize) -> Option<&str> {
        let mut queue: VecDeque<usize> = VecDeque::new();
        let mut seen = vec![false; self.tickets.len()];
        seen[index] = true;
        queue.push_back(index);
        while let Some(current) = queue.pop_front() {
            for dependency in &self.tickets[current].blocked_by {
                let Some(&blocker) = self.index_of.get(dependency.trim()) else {
                    continue;
                };
                if self.failed[blocker] {
                    return Some(self.tickets[blocker].id.trim());
                }
                if !seen[blocker] {
                    seen[blocker] = true;
                    queue.push_back(blocker);
                }
            }
        }
        None
    }

    /// Whether the ticket at `index` is already decided, so it must never be
    /// scheduled again.
    fn settled(&self, index: usize) -> bool {
        self.completed[index] || self.failed[index]
    }

    /// Whether every blocker this ticket names has completed.
    fn is_executable(&self, index: usize) -> bool {
        self.tickets[index].blocked_by.iter().all(|dependency| {
            match self.index_of.get(dependency.trim()) {
                Some(&blocker) => self.completed[blocker],
                // The validator guarantees every dependency exists; a missing
                // one is treated as unresolved so nothing runs out of order.
                None => false,
            }
        })
    }

    /// The ticket's remaining downstream chain: one plus the largest chain among
    /// its still-undecided direct dependents.
    fn chain(&self, index: usize) -> usize {
        let mut best = 1;
        for &dependent in &self.dependents[index] {
            if !self.settled(dependent) {
                best = best.max(1 + self.chain(dependent));
            }
        }
        best
    }
}

/// Why [`run`] returned.
#[derive(Debug)]
pub enum Outcome {
    /// Every ticket completed: the pipeline's completion step.
    Resolved,
    /// The queue drained without completing every ticket. The remaining tickets
    /// are blocked, failed, or both, and the report names how each one turned
    /// out.
    Incomplete,
    /// The queue stopped before it could drain. `ticket` is the ticket that was
    /// in flight -- empty when none had started -- and `reason` is the one-line
    /// explanation. Only a failed status write stops the queue this way.
    Stopped { ticket: String, reason: String },
}

/// What one queue invocation produced: how it ended, and the end-of-run report
/// markdown it generated for exactly that invocation.
///
/// The report travels back with the outcome instead of only landing on disk
/// because the caller composes the next phase from it, and the file write is
/// deliberately best-effort: a run whose report file could not be written still
/// has the markdown this invocation generated, and the caller must never fall
/// back to reading a `queue-report.md` an earlier invocation left behind.
#[derive(Debug)]
pub struct QueueResult {
    /// Why the queue returned.
    pub outcome: Outcome,
    /// The report markdown handed to [`Lifecycle::report`]: the same bytes the
    /// report file would hold, whether or not that write succeeded.
    pub report: String,
}

/// The narrow seam the queue drives: one code-agent attempt per ticket, its
/// verification gate, and the two files the queue writes.
///
/// Production wires the real session, the real shell, the real status file and
/// the real report. A unit test implements it with scripted outcomes, so the
/// queue's lifecycle is deterministic without a model, a shell or a live
/// filesystem.
trait Lifecycle {
    /// Hand one ticket to the code agent and report how the turn ended.
    ///
    /// The optional accumulator is forwarded into the production code-agent
    /// session, which adds its already-measured model attempts and executed
    /// commands to it. The queue never measures the code-agent work itself, and
    /// a lifecycle that has nothing to contribute simply ignores it.
    fn work(
        &mut self,
        ticket: &Ticket,
        handover: Option<&str>,
        contributions: Option<&mut QueueContributions>,
    ) -> code::Attempt;

    /// Run the ticket's verification gate. `Err` carries the gate's own output.
    fn verify(&mut self, ticket: &Ticket) -> Result<(), String>;

    /// Collect the repository working-tree diff for the retry handover: staged,
    /// unstaged and non-ignored untracked changes, with the run's own `.ejaj/`
    /// artifacts excluded. `Err` carries the collection failure.
    fn work_tree_diff(&mut self) -> Result<String, String>;

    /// Persist the whole status snapshot.
    fn persist(&mut self, snapshot: &QueueSnapshot) -> io::Result<()>;

    /// Write the end-of-run report and return the path it landed at. A failure
    /// is reported to the console and changes nothing else.
    fn report(&mut self, markdown: &str) -> io::Result<PathBuf>;
}

/// The production [`Lifecycle`]: a fresh code-agent session per ticket, the
/// ticket's gate through [`run_command`], and the snapshot and report on disk.
struct Production<'a> {
    config: &'a Config,
    scope: &'a str,
    /// The project root the run is anchored to; the retry handover's diff is
    /// read from this repository, never from queue state.
    root: &'a Path,
    /// The session directory of the invocation whose queue this is, when one
    /// was supplied. Every fresh ticket attempt -- first, retry, and later
    /// queue rounds alike -- forwards it to Bash dispatch.
    session_dir: Option<&'a Path>,
    status_path: &'a Path,
    report_path: &'a Path,
}

impl Production<'_> {
    /// The invocation context every production ticket attempt carries.
    ///
    /// The root is always explicit, independently of the optional session
    /// directory: a caller that supplied no session still keeps the root it
    /// selected, and a supplied session passes through unchanged.
    fn invocation(&self) -> InvocationContext<'_> {
        InvocationContext {
            root: Some(self.root),
            session_dir: self.session_dir,
        }
    }
}

impl Lifecycle for Production<'_> {
    fn work(
        &mut self,
        ticket: &Ticket,
        handover: Option<&str>,
        contributions: Option<&mut QueueContributions>,
    ) -> code::Attempt {
        let session = code::Session::new(self.config, self.scope);
        session.run_ticket_attempt_in(
            &ticket.to_markdown(),
            handover,
            contributions,
            self.invocation(),
        )
    }

    fn verify(&mut self, ticket: &Ticket) -> Result<(), String> {
        run_command_in(self.root, &ticket.verification_gate).map(|_| ())
    }

    fn work_tree_diff(&mut self) -> Result<String, String> {
        crate::script::host_diff(self.root)
    }

    fn persist(&mut self, snapshot: &QueueSnapshot) -> io::Result<()> {
        snapshot.write(self.status_path)
    }

    fn report(&mut self, markdown: &str) -> io::Result<PathBuf> {
        fs::write(self.report_path, markdown)?;
        Ok(self.report_path.to_path_buf())
    }
}

/// Run the queue over `set`, recording each ticket's status at `status_path` and
/// the end-of-run report at `report_path`.
///
/// The runner prints only its own progress lines and never exits the process.
/// It writes the full snapshot before any code-agent session starts and again
/// after every transition. A write failure is a queue-stop condition: the
/// attempted transition is not durable, so the queue stops with a persistence
/// reason and starts neither the agent nor the gate. The report is written on
/// every path out of the queue and cannot itself stop the queue; the markdown it
/// holds comes back either way, so a failed report write never costs the caller
/// the content of the run.
///
/// This compatibility entry point uses the process working directory as the
/// project root for the retry handover's diff. Callers that know their root use
/// [`run_in`].
pub fn run(
    config: &Config,
    scope: &str,
    set: &TicketSet,
    status_path: &Path,
    report_path: &Path,
) -> QueueResult {
    run_in(config, scope, Path::new("."), set, status_path, report_path)
}

/// The same queue run anchored to an explicit project root.
pub fn run_in(
    config: &Config,
    scope: &str,
    root: &Path,
    set: &TicketSet,
    status_path: &Path,
    report_path: &Path,
) -> QueueResult {
    run_with_prior_in(config, scope, root, &[], set, status_path, report_path)
}

/// Run the queue over `set` while preserving `prior_completed` tickets from
/// earlier rounds in both the status snapshot (`status_path`) and the
/// cumulative end-of-run report (`report_path`).
///
/// This compatibility entry point uses the process working directory as the
/// project root; [`run_with_prior_in`] takes the root explicitly.
pub fn run_with_prior(
    config: &Config,
    scope: &str,
    prior_completed: &[Ticket],
    set: &TicketSet,
    status_path: &Path,
    report_path: &Path,
) -> QueueResult {
    run_with_prior_in(
        config,
        scope,
        Path::new("."),
        prior_completed,
        set,
        status_path,
        report_path,
    )
}

/// The same prior-completed run anchored to an explicit project root.
pub fn run_with_prior_in(
    config: &Config,
    scope: &str,
    root: &Path,
    prior_completed: &[Ticket],
    set: &TicketSet,
    status_path: &Path,
    report_path: &Path,
) -> QueueResult {
    let mut lifecycle = Production {
        config,
        scope,
        root,
        session_dir: None,
        status_path,
        report_path,
    };
    run_queue_with_prior(&mut lifecycle, prior_completed, set)
}

/// The queue's accumulator-aware invocation path: the same run as
/// [`run_with_prior`], with every already-measured contribution -- the model
/// attempts and executed commands from the production ticket work, and each
/// verification gate -- recorded into the caller's ephemeral per-round
/// accumulator.
///
/// The public entry points keep their exact signatures and behavior by passing
/// no accumulator; only a later queue-accounting caller uses this path. Nothing
/// here reports the accumulator: the elapsed lines and the queue diagnostics are
/// unchanged, and the sums are the caller's to render.
#[allow(dead_code)]
pub(crate) fn run_with_prior_contributions(
    config: &Config,
    scope: &str,
    prior_completed: &[Ticket],
    set: &TicketSet,
    status_path: &Path,
    report_path: &Path,
    contributions: &mut QueueContributions,
) -> QueueResult {
    run_with_prior_contributions_in(
        config,
        scope,
        Path::new("."),
        prior_completed,
        set,
        QueueInvocation {
            status_path,
            report_path,
            contributions,
        },
    )
}

/// The per-invocation bookkeeping a queue round writes: its status snapshot
/// path, its report path, and the caller's ephemeral contribution accumulator.
pub(crate) struct QueueInvocation<'a> {
    pub(crate) status_path: &'a Path,
    pub(crate) report_path: &'a Path,
    pub(crate) contributions: &'a mut QueueContributions,
}

/// The accumulator-aware run anchored to an explicit project root. The retry
/// handover reads the project root's working-tree diff through the existing
/// read-only `host_diff` path.
pub(crate) fn run_with_prior_contributions_in(
    config: &Config,
    scope: &str,
    root: &Path,
    prior_completed: &[Ticket],
    set: &TicketSet,
    invocation: QueueInvocation<'_>,
) -> QueueResult {
    run_with_prior_contributions_in_session(
        config,
        scope,
        root,
        None,
        prior_completed,
        set,
        invocation,
    )
}

/// The accumulator-aware run with the invocation's session directory carried
/// into every fresh code-agent attempt. The directory is never retained: it is
/// supplied per queue invocation, so a later ask with a different directory
/// writes there instead.
pub(crate) fn run_with_prior_contributions_in_session(
    config: &Config,
    scope: &str,
    root: &Path,
    session_dir: Option<&Path>,
    prior_completed: &[Ticket],
    set: &TicketSet,
    invocation: QueueInvocation<'_>,
) -> QueueResult {
    let mut lifecycle = Production {
        config,
        scope,
        root,
        session_dir,
        status_path: invocation.status_path,
        report_path: invocation.report_path,
    };
    run_queue_with_prior_contributions(
        &mut lifecycle,
        prior_completed,
        set,
        Some(invocation.contributions),
    )
}

/// How a ticket turned out, in the report's own vocabulary.
#[derive(Debug, Clone)]
enum Ending {
    /// The agent declared it, and the gate passed.
    Declared,
    /// The agent never declared it, but the gate passed.
    GateAccepted,
    /// The agent never declared it, and the gate failed too.
    RetryExhausted,
    /// The agent declared it, and the gate failed.
    GateFailed,
    /// The loop broke off; the label names why.
    AgentFailure(FailureKind),
    /// The queue never reached it: the named failure blocks it.
    Blocked(String),
    /// The queue stopped before it was reached.
    NeverStarted,
}

impl Ending {
    /// Whether the ticket completed.
    fn completed(&self) -> bool {
        matches!(self, Ending::Declared | Ending::GateAccepted)
    }

    /// The ticket's status in the report. The snapshot keeps its own four
    /// values; only the report names a blocked ticket as such.
    fn status(&self) -> &'static str {
        match self {
            Ending::Declared | Ending::GateAccepted => "completed",
            Ending::GateFailed | Ending::RetryExhausted | Ending::AgentFailure(_) => "failed",
            Ending::Blocked(_) => "blocked",
            Ending::NeverStarted => "pending",
        }
    }

    /// One phrase naming how the ticket turned out. The console and the report
    /// both render through here, so the two cannot disagree.
    fn phrase(&self) -> String {
        match self {
            Ending::Declared => "declared".to_string(),
            Ending::GateAccepted => "gate accepted without declaration".to_string(),
            Ending::RetryExhausted => "retry exhausted without task_complete".to_string(),
            Ending::GateFailed => "gate failed".to_string(),
            Ending::AgentFailure(kind) => format!("agent failure: {}", kind.label()),
            Ending::Blocked(blocker) => format!("blocked by {blocker}"),
            Ending::NeverStarted => "never started".to_string(),
        }
    }
}

/// The queue's end-of-run report: how every ticket turned out, in emitted order.
struct Report<'a> {
    set: &'a TicketSet,
    endings: &'a [Ending],
}

impl Report<'_> {
    /// The counts the header states: completed, failed, and the rest.
    fn counts(&self) -> (usize, usize, usize) {
        let completed = self.endings.iter().filter(|e| e.completed()).count();
        let failed = self
            .endings
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    Ending::GateFailed | Ending::RetryExhausted | Ending::AgentFailure(_)
                )
            })
            .count();
        (completed, failed, self.endings.len() - completed - failed)
    }

    /// What the undecided tickets are called: `blocked` once the queue drained
    /// and failed around them, `never started` when a status write stopped the
    /// queue before they were reached.
    fn undecided(&self) -> &'static str {
        if self
            .endings
            .iter()
            .any(|e| matches!(e, Ending::NeverStarted))
        {
            "never started"
        } else {
            "blocked"
        }
    }

    /// The one-line summary both the console and the header state.
    fn summary(&self) -> String {
        let (completed, failed, undecided) = self.counts();
        format!(
            "{completed} completed, {failed} failed, {undecided} {}",
            self.undecided()
        )
    }

    /// The report document: a header, then one section per ticket. It ends with
    /// exactly one newline, like every other artifact the pipeline writes.
    fn markdown(&self) -> String {
        let (completed, failed, undecided) = self.counts();
        let tickets = self.endings.len();
        let noun = if tickets == 1 { "ticket" } else { "tickets" };
        let mut text = format!(
            "# Ticket queue report\n\n{tickets} {noun}: {completed} completed, {failed} failed, \
             {undecided} {}.\n",
            self.undecided()
        );
        for (ticket, ending) in self.set.tickets.iter().zip(self.endings) {
            text.push_str(&format!(
                "\n## {}\n\n- Title: {}\n- Status: {}\n- Ended: {}\n- Gate: `{}`\n",
                ticket.id.trim(),
                ticket.title.trim(),
                ending.status(),
                ending.phrase(),
                ticket.verification_gate.trim(),
            ));
        }
        text
    }
}

/// Whether a first-attempt failure earns the one retry.
///
/// A step-limit or context-guard stop is a bounded agent-loop stop: a fresh
/// attempt can still finish the ticket. API, transport and malformed-response
/// failures are fatal and never retry.
fn retries_after_first_stop(kind: FailureKind) -> bool {
    matches!(kind, FailureKind::StepLimit | FailureKind::ContextGuard)
}

/// One ticket's code-agent attempts: the first call and at most one fresh retry.
///
/// The first verification gate runs only after the first attempt declares
/// completion. A passing gate resolves the ticket; a failed gate is captured
/// and the ticket is retried once. Every eligible first stop -- a turn end, a
/// step limit, a context guard, or a declaration whose gate failed -- earns the
/// same single retry. After the second attempt the gate runs exactly once and
/// is authoritative, whatever the second attempt declared or how it stopped.
fn work_with_retry(
    lifecycle: &mut impl Lifecycle,
    ticket: &Ticket,
    id: &str,
    mut contributions: Option<&mut QueueContributions>,
) -> Ending {
    let first = lifecycle.work(ticket, None, contributions.as_deref_mut());
    match first.outcome {
        code::Outcome::Complete => {
            match invoke_gate(id, contributions.as_deref_mut(), || {
                lifecycle.verify(ticket)
            }) {
                Ok(()) => Ending::Declared,
                Err(output) => {
                    report_gate_output(&output);
                    let _ = crate::status::shared().stderr_log(&format!(
                        "[WARN] ticket {id}: the first verification gate failed after `task_complete`; retrying once with a fresh handover"
                    ));
                    retry_once(
                        lifecycle,
                        ticket,
                        id,
                        first.narration.as_deref(),
                        "task_complete, but the first verification gate failed",
                        Some(&output),
                        contributions,
                    )
                }
            }
        }
        code::Outcome::TurnEnd => {
            let _ = crate::status::shared().stderr_log(&format!(
                "[WARN] ticket {id}: the code agent ended its turn without `task_complete`; retrying once with a fresh handover"
            ));
            retry_once(
                lifecycle,
                ticket,
                id,
                first.narration.as_deref(),
                "turn end without task_complete",
                None,
                contributions,
            )
        }
        code::Outcome::Failed(kind) if retries_after_first_stop(kind) => {
            let _ = crate::status::shared().stderr_log(&format!(
                "[WARN] ticket {id}: the code agent stopped ({}); retrying once with a fresh handover",
                kind.label()
            ));
            retry_once(
                lifecycle,
                ticket,
                id,
                first.narration.as_deref(),
                &format!("agent loop stopped: {}", kind.label()),
                None,
                contributions,
            )
        }
        code::Outcome::Failed(kind) => Ending::AgentFailure(kind),
    }
}

/// Run the one fresh retry and then the authoritative final gate.
///
/// The second attempt keeps the unchanged ticket kickoff and receives the
/// rendered handover as one separate user message. The final gate runs once
/// whatever that attempt did; a passing gate resolves the ticket, and a failing
/// gate leaves it failed while its own output is reported.
fn retry_once(
    lifecycle: &mut impl Lifecycle,
    ticket: &Ticket,
    id: &str,
    narration: Option<&str>,
    stop_reason: &str,
    first_gate_output: Option<&str>,
    mut contributions: Option<&mut QueueContributions>,
) -> Ending {
    let diff = lifecycle.work_tree_diff();
    let handover = render_retry_handover(
        narration,
        stop_reason,
        first_gate_output,
        diff.as_deref().map_err(String::as_str),
    );
    let second = lifecycle.work(ticket, Some(&handover), contributions.as_deref_mut());
    match invoke_gate(id, contributions, || lifecycle.verify(ticket)) {
        Ok(()) => match second.outcome {
            code::Outcome::Complete => Ending::Declared,
            _ => Ending::GateAccepted,
        },
        Err(output) => {
            report_gate_output(&output);
            if second.outcome == code::Outcome::Complete {
                Ending::GateFailed
            } else {
                Ending::RetryExhausted
            }
        }
    }
}

/// Print a gate's own output, exactly the diagnostic the queue always showed.
fn report_gate_output(output: &str) {
    let _ = crate::status::shared().stderr_log(output);
}

/// Render the explicit retry handover. Absent narration and an unchanged
/// working tree are named rather than omitted, and a diff collection failure
/// carries its own error instead of pretending the tree was empty.
fn render_retry_handover(
    narration: Option<&str>,
    stop_reason: &str,
    first_gate_output: Option<&str>,
    diff: Result<&str, &str>,
) -> String {
    let narration = match narration.map(str::trim) {
        Some(text) if !text.is_empty() => text,
        _ => "(empty narration)",
    };
    let gate = match first_gate_output.map(str::trim) {
        Some(text) if !text.is_empty() => text,
        Some(_) => "(empty verification gate output)",
        None => "(the first attempt did not reach a failed verification gate)",
    };
    let diff = match diff {
        Ok(text) if !text.trim().is_empty() => text.to_string(),
        Ok(_) => "(empty diff)".to_string(),
        Err(error) => format!("(diff collection failed: {error})"),
    };
    format!(
        "[RETRY HANDOVER]\n\
         This is a fresh code-agent attempt for the same ticket. The ticket kickoff above is unchanged.\n\n\
         Previous attempt stop reason: {stop_reason}\n\n\
         Last assistant narration:\n{narration}\n\n\
         First verification gate output:\n{gate}\n\n\
         Repository working-tree diff (staged, unstaged, and non-ignored untracked changes; .ejaj/ excluded):\n{diff}\n"
    )
}

/// Print the end-of-run summary, write the report, and return the queue result:
/// the outcome, and the report markdown generated for this invocation.
///
/// The report write is best-effort: it lands on disk when it can, and a failure
/// is a warning that changes nothing -- the status snapshot, written earlier, is
/// the record of what the queue did. The markdown is returned whether or not
/// that write succeeded, so the caller composes the next phase from the content
/// this invocation generated rather than from whatever file is on disk.
fn finish(
    lifecycle: &mut impl Lifecycle,
    prior_completed: &[Ticket],
    set: &TicketSet,
    endings: &[Ending],
    outcome: Outcome,
) -> QueueResult {
    let mut combined_tickets = Vec::with_capacity(prior_completed.len() + set.tickets.len());
    combined_tickets.extend_from_slice(prior_completed);
    combined_tickets.extend_from_slice(&set.tickets);
    let combined_set = TicketSet {
        tickets: combined_tickets,
    };

    let mut combined_endings = vec![Ending::Declared; prior_completed.len()];
    combined_endings.extend_from_slice(endings);

    let report = Report {
        set: &combined_set,
        endings: &combined_endings,
    };
    match &outcome {
        Outcome::Resolved => {
            let _ = crate::status::shared().stdout_log(&format!(
                "[INFO] all {} tickets resolved",
                combined_set.tickets.len()
            ));
        }
        _ => {
            let _ = crate::status::shared()
                .stdout_log(&format!("[INFO] queue incomplete: {}", report.summary()));
            for (ticket, ending) in set.tickets.iter().zip(endings) {
                if !ending.completed() {
                    let _ = crate::status::shared().stderr_log(&format!(
                        "[WARN] {}: {}",
                        ticket.id.trim(),
                        ending.phrase()
                    ));
                }
            }
        }
    }
    let markdown = report.markdown();
    match lifecycle.report(&markdown) {
        Ok(path) => {
            let _ = crate::status::shared().stdout_log(&format!(
                "[INFO] queue report written to {}",
                path.display()
            ));
        }
        Err(error) => {
            let _ = crate::status::shared()
                .stderr_log(&format!("[WARN] could not write the queue report: {error}"));
        }
    }
    QueueResult {
        outcome,
        report: markdown,
    }
}

/// Invoke one ticket's verification gate, measure exactly that invocation,
/// report it under the ticket-scoped elapsed label, and record its duration when
/// the queue's accumulator is supplied.
///
/// The measurement is [`crate::timing::measured`]'s, so the interval begins
/// immediately before the gate is entered and ends immediately after it
/// returns: the elapsed-line write and the accumulator update are both outside
/// it. Reporting and recording never inspect or rewrite the gate result, so a
/// success and an error reach the caller exactly as the gate produced them.
fn invoke_gate(
    ticket_id: &str,
    contributions: Option<&mut QueueContributions>,
    verify: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    let (result, elapsed) = crate::timing::measured(verify);
    let label = format!("ticket {ticket_id} verification gate");
    let _ = crate::status::shared().elapsed_log(&label, elapsed);
    if let Some(contributions) = contributions {
        contributions.record_verification_gate(elapsed);
    }
    result
}

/// The queue itself, over an injected [`Lifecycle`]. [`run`] supplies the
/// production one; the lifecycle tests supply a scripted one.
#[cfg(test)]
fn run_queue(lifecycle: &mut impl Lifecycle, set: &TicketSet) -> QueueResult {
    run_queue_with_prior(lifecycle, &[], set)
}

/// Run the queue over `set` with `prior_completed` tickets pre-recorded as
/// completed in the status snapshot and cumulative report.
fn run_queue_with_prior(
    lifecycle: &mut impl Lifecycle,
    prior_completed: &[Ticket],
    set: &TicketSet,
) -> QueueResult {
    run_queue_with_prior_contributions(lifecycle, prior_completed, set, None)
}

/// Run the queue over `set`, forwarding an optional ephemeral accumulator into
/// the production ticket work and recording every verification-gate duration
/// into it.
///
/// Every other behavior is the queue's own: the same scheduling, persistence,
/// diagnostics, retries and report. The accumulator only adds already-measured
/// durations and never changes what the queue prints or how it ends.
fn run_queue_with_prior_contributions(
    lifecycle: &mut impl Lifecycle,
    prior_completed: &[Ticket],
    set: &TicketSet,
    mut contributions: Option<&mut QueueContributions>,
) -> QueueResult {
    let mut scheduler = Scheduler::new(set);
    let mut snapshot = QueueSnapshot::new(
        prior_completed
            .iter()
            .chain(set.tickets.iter())
            .map(|ticket| ticket.id.trim()),
    );
    for ticket in prior_completed {
        snapshot.set(ticket.id.trim(), TicketStatus::Completed);
    }
    // Every ticket in `set` starts out unreached, and each is overwritten as it
    // is decided: what is left when the queue stops was never started.
    let mut endings = vec![Ending::NeverStarted; set.tickets.len()];
    if let Err(error) = lifecycle.persist(&snapshot) {
        return finish(
            lifecycle,
            prior_completed,
            set,
            &endings,
            stopped_on_persistence(String::new(), &error),
        );
    }

    while scheduler.remaining() > 0 {
        // Nothing is executable: the rest of the set hangs off a failure.
        let Some(index) = scheduler.next() else {
            break;
        };

        let ticket = &set.tickets[index];
        let id = ticket.id.trim().to_string();
        let _ = crate::status::shared().stdout_log(&format!(
            "[INFO] ticket {}: {}",
            id,
            ticket.title.trim()
        ));

        // In progress before the session starts: the snapshot names the ticket
        // the queue is about to work, and a failed write here starts nothing.
        snapshot.set(&id, TicketStatus::InProgress);
        if let Err(error) = lifecycle.persist(&snapshot) {
            return finish(
                lifecycle,
                prior_completed,
                set,
                &endings,
                stopped_on_persistence(id, &error),
            );
        }

        // The first gate follows only a declaration; an eligible first stop
        // earns one fresh retry; the final gate is authoritative after the
        // second attempt. The ticket's work and every gate invocation are
        // measured into the caller's accumulator exactly once each.
        let ending = work_with_retry(lifecycle, ticket, &id, contributions.as_deref_mut());

        // The ending is recorded before the status write, so the report stays
        // truthful even when that write is what fails.
        let completed = ending.completed();
        endings[index] = ending;
        snapshot.set(
            &id,
            if completed {
                TicketStatus::Completed
            } else {
                TicketStatus::Failed
            },
        );
        if let Err(error) = lifecycle.persist(&snapshot) {
            return finish(
                lifecycle,
                prior_completed,
                set,
                &endings,
                stopped_on_persistence(id, &error),
            );
        }
        if completed {
            let _ = crate::status::shared().stdout_log(&format!("[INFO] ticket {id} resolved"));
            scheduler.complete(index);
        } else {
            // A failure is local: this branch dies, the rest keeps going.
            scheduler.fail(index);
        }
    }

    // Everything the queue never reached is blocked, and the report names the
    // failure that stopped it.
    for (index, ending) in endings.iter_mut().enumerate() {
        if matches!(ending, Ending::NeverStarted) {
            *ending = Ending::Blocked(match scheduler.failed_ancestor(index) {
                Some(blocker) => format!("{blocker} (failed)"),
                None => "an unresolved dependency".to_string(),
            });
        }
    }

    // `Resolved` means every ticket completed, not merely that nothing is left
    // to run: a set in which every ticket failed is still incomplete.
    let outcome = if scheduler.completed_count() == set.tickets.len() {
        Outcome::Resolved
    } else {
        Outcome::Incomplete
    };
    finish(lifecycle, prior_completed, set, &endings, outcome)
}

/// Every status-write stop reason starts with this phrase, so the queue-stop
/// reason distinguishes a persistence failure from an agent or gate failure.
const PERSISTENCE_PREFIX: &str = "could not persist the queue status";

/// The queue-stop outcome for a failed status write.
fn stopped_on_persistence(ticket: String, error: &io::Error) -> Outcome {
    Outcome::Stopped {
        ticket,
        reason: format!("{PERSISTENCE_PREFIX}: {error}"),
    }
}

/// The four states a ticket can be in while the queue runs.
///
/// The wire form of every variant is its lowercase name (`pending`, `in_progress`, `completed`,
/// `failed`), so a written snapshot stays human-readable and stable. This is runtime state, not
/// part of a [`Ticket`]: a ticket document is immutable, and the queue owns its own record of how
/// far each ticket has got.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TicketStatus {
    /// The ticket has not started: the queue has not reached it yet.
    Pending,
    /// The code agent is working the ticket.
    InProgress,
    /// The ticket's verification gate exited 0.
    Completed,
    /// The ticket stopped the queue: a failed gate or an abandoned turn.
    Failed,
}

/// The whole queue's status, keyed by ticket id, in one serializable object.
///
/// `#[serde(transparent)]` makes the wire shape the inner map: the JSON root is a direct object
/// keyed by ticket id, not an object wrapping a `tickets` field. A [`BTreeMap`] keeps a written
/// snapshot stable across runs, so re-persisting unchanged state cannot churn the file.
///
/// The type is deliberately free of orchestration: it knows nothing about scheduling, resumption
/// or atomic replacement. It only records the state and writes it to a supplied path with ordinary
/// filesystem I/O. [`run`] persists it through the queue's lifecycle.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct QueueSnapshot {
    /// One status per ticket id. Every id the snapshot was opened with is present.
    statuses: BTreeMap<String, TicketStatus>,
}

impl QueueSnapshot {
    /// Open a snapshot with every id in `ids` present as [`TicketStatus::Pending`].
    ///
    /// Every supplied ticket is covered: a repeated id simply maps to the same key, and an empty
    /// iterator yields an empty snapshot. The emitted order of `ids` is not preserved -- the map's
    /// own key order is the snapshot's order -- so the caller can pass the queue's order directly.
    pub fn new<I, S>(ids: I) -> QueueSnapshot
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let statuses = ids
            .into_iter()
            .map(|id| (id.into(), TicketStatus::Pending))
            .collect();
        QueueSnapshot { statuses }
    }

    /// Record `status` for `id`, replacing any previous value.
    ///
    /// An id that the snapshot does not already cover is inserted; the queue only ever names ids
    /// from the set it opened with, so normal use cannot silently invent a ticket.
    pub fn set(&mut self, id: &str, status: TicketStatus) {
        self.statuses.insert(id.to_string(), status);
    }

    /// The recorded status for `id`, or `None` when the snapshot does not cover it.
    pub fn get(&self, id: &str) -> Option<TicketStatus> {
        self.statuses.get(id).copied()
    }

    /// The number of ticket ids the snapshot covers.
    pub fn len(&self) -> usize {
        self.statuses.len()
    }

    /// Whether the snapshot covers no ticket ids.
    pub fn is_empty(&self) -> bool {
        self.statuses.is_empty()
    }

    /// Serialize the whole snapshot as a direct JSON object keyed by ticket id.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// Serialize the whole snapshot and write it to `path`, replacing any file already there.
    ///
    /// This is ordinary filesystem I/O: the parent directory must already exist, no temporary file
    /// is used and no rename is performed. A serialization failure is reported as an
    /// [`io::Error`] so the caller has one error type to handle.
    pub fn write(&self, path: &Path) -> io::Result<()> {
        let json = self.to_json().map_err(io::Error::other)?;
        fs::write(path, json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::process::{Command, Stdio};

    /// A ticket set built from `(id, blocked_by)` pairs; every other field is
    /// irrelevant to the scheduler.
    pub(super) fn set(edges: &[(&str, &[&str])]) -> TicketSet {
        TicketSet {
            tickets: edges
                .iter()
                .map(|(id, blocked_by)| Ticket {
                    id: id.to_string(),
                    acceptance_criteria: Vec::new(),
                    blocked_by: blocked_by.iter().map(|id| id.to_string()).collect(),
                    blocks: Vec::new(),
                    title: id.to_string(),
                    objective: "objective".to_string(),
                    prerequisites_assumed: "none".to_string(),
                    out_of_scope: Vec::new(),
                    scope_of_changes: Vec::new(),
                    test_expectations: Vec::new(),
                    verification_gate: "cargo check".to_string(),
                })
                .collect(),
        }
    }

    /// Drain the scheduler the way the queue does, returning the ids in the
    /// order they were scheduled.
    fn drain(set: &TicketSet) -> Vec<String> {
        let mut scheduler = Scheduler::new(set);
        let mut order = Vec::new();
        while scheduler.remaining() > 0 {
            let Some(index) = scheduler.next() else {
                break;
            };
            order.push(set.tickets[index].id.clone());
            scheduler.complete(index);
        }
        order
    }

    #[test]
    fn a_linear_chain_runs_in_dependency_order() {
        let set = set(&[
            ("1-first", &[]),
            ("2-second", &["1-first"]),
            ("3-third", &["2-second"]),
        ]);
        assert_eq!(drain(&set), ["1-first", "2-second", "3-third"]);
    }

    #[test]
    fn no_ticket_runs_before_its_blockers() {
        let set = set(&[
            ("1-first", &[]),
            ("2-second", &["1-first", "3-third"]),
            ("3-third", &["1-first"]),
        ]);
        let mut scheduler = Scheduler::new(&set);
        // The only executable ticket is the root; the two dependents wait.
        assert_eq!(scheduler.next(), Some(0));
        scheduler.complete(0);
        // `3-third` is now executable and `2-second` is still blocked by it, so
        // the join waits for its second blocker.
        assert_eq!(scheduler.next(), Some(2));
        scheduler.complete(2);
        assert_eq!(scheduler.next(), Some(1));
    }

    /// A diamond runs its root first, then a branch, then the other branch,
    /// then the join: the join is never executable before both branches.
    #[test]
    fn a_diamond_runs_the_root_then_the_branches_then_the_join() {
        let set = set(&[
            ("1-root", &[]),
            ("2-branch", &["1-root"]),
            ("3-branch", &["1-root"]),
            ("4-join", &["2-branch", "3-branch"]),
        ]);
        assert_eq!(drain(&set), ["1-root", "2-branch", "3-branch", "4-join"]);
    }

    /// When the frontier holds branches of different depth, the longer
    /// remaining downstream chain goes first.
    #[test]
    fn the_longer_remaining_chain_wins_the_frontier() {
        let set = set(&[
            ("1-root", &[]),
            ("2-long", &["1-root"]),
            ("3-short", &["1-root"]),
            ("4-deep", &["2-long"]),
        ]);
        let mut scheduler = Scheduler::new(&set);
        assert_eq!(scheduler.next(), Some(0));
        scheduler.complete(0);
        // `2-long`'s chain is two (itself plus `4-deep`); `3-short`'s is one.
        assert_eq!(scheduler.next(), Some(1));
    }

    #[test]
    fn a_deeper_later_ticket_wins_a_frontier_tie() {
        let set = set(&[
            ("1-shallow", &[]),
            ("2-deep", &[]),
            ("3-dependent", &["2-deep"]),
        ]);
        let scheduler = Scheduler::new(&set);
        // The later, deeper ticket outranks the earlier, shallower one.
        assert_eq!(scheduler.next(), Some(1));
    }

    #[test]
    fn completion_decreases_the_remaining_count_by_one() {
        let set = set(&[("1-a", &[]), ("2-b", &["1-a"]), ("3-c", &["2-b"])]);
        let mut scheduler = Scheduler::new(&set);
        assert_eq!(scheduler.remaining(), 3);
        scheduler.complete(0);
        assert_eq!(scheduler.remaining(), 2);
        scheduler.complete(1);
        assert_eq!(scheduler.remaining(), 1);
        scheduler.complete(2);
        assert_eq!(scheduler.remaining(), 0);
    }

    #[test]
    fn nothing_is_executable_when_every_remaining_ticket_is_blocked() {
        // A cycle is refused by the validator before the queue runs; the
        // scheduler itself simply reports that nothing can start.
        let set = set(&[("1-a", &["2-b"]), ("2-b", &["1-a"])]);
        let scheduler = Scheduler::new(&set);
        assert_eq!(scheduler.next(), None);
    }

    #[test]
    fn a_later_completion_does_not_reopen_a_settled_ticket() {
        let set = set(&[("1-root", &[]), ("2-dependent", &["1-root"])]);
        let mut scheduler = Scheduler::new(&set);
        scheduler.complete(1);
        // The root is still executable even though its dependent completed.
        assert_eq!(scheduler.next(), Some(0));
    }

    #[test]
    fn a_failed_ticket_is_never_scheduled_again() {
        let set = set(&[("1-first", &[]), ("2-second", &["1-first"])]);
        let mut scheduler = Scheduler::new(&set);
        assert_eq!(scheduler.next(), Some(0));
        scheduler.fail(0);
        // The failed ticket is decided, and its dependent can never run.
        assert_eq!(scheduler.next(), None);
        assert_eq!(scheduler.remaining(), 1);
        assert_eq!(scheduler.failed_count(), 1);
        assert_eq!(scheduler.completed_count(), 0);
    }

    #[test]
    fn a_failure_does_not_stop_an_independent_ticket() {
        let set = set(&[
            ("1-first", &[]),
            ("2-dependent", &["1-first"]),
            ("3-independent", &[]),
        ]);
        let mut scheduler = Scheduler::new(&set);
        assert_eq!(scheduler.next(), Some(0));
        scheduler.fail(0);
        // The dependent is stuck behind the failure; the other branch is not.
        assert_eq!(scheduler.next(), Some(2));
        scheduler.complete(2);
        assert_eq!(scheduler.next(), None);
        assert_eq!(scheduler.remaining(), 1);
        assert_eq!(scheduler.completed_count(), 1);
    }

    #[test]
    fn the_nearest_failed_ancestor_names_the_blocker() {
        let set = set(&[
            ("1-first", &[]),
            ("2-second", &["1-first"]),
            ("3-third", &["2-second"]),
        ]);
        let mut scheduler = Scheduler::new(&set);
        scheduler.fail(0);
        assert_eq!(scheduler.failed_ancestor(1), Some("1-first"));
        // The walk passes the undecided ticket and reaches the failure behind it.
        assert_eq!(scheduler.failed_ancestor(2), Some("1-first"));
        assert_eq!(scheduler.failed_ancestor(0), None);
    }

    #[test]
    fn failing_decreases_the_remaining_count_by_one() {
        let set = set(&[("1-a", &[]), ("2-b", &["1-a"])]);
        let mut scheduler = Scheduler::new(&set);
        assert_eq!(scheduler.remaining(), 2);
        scheduler.fail(0);
        assert_eq!(scheduler.remaining(), 1);
        scheduler.fail(1);
        assert_eq!(scheduler.remaining(), 0);
        assert_eq!(scheduler.failed_count(), 2);
    }

    /// The environment variable a child copy of the verification-gate forwarding
    /// regression sets on itself. The child runs the queue for real, letting its
    /// existing reporting path write to the process stderr the parent captures.
    const GATE_FORWARDING_CHILD: &str = "EJAJ_GATE_FORWARDING_CHILD";

    /// Whether this process is the child copy of the forwarding regression.
    fn is_forwarding_child() -> bool {
        std::env::var_os(GATE_FORWARDING_CHILD).is_some()
    }

    /// A test lifecycle whose only scripted behavior is the two external
    /// boundaries: the code-agent attempt and the verification gate. The status
    /// snapshot and report are written to real files in a temporary directory.
    struct Forwarding {
        work: VecDeque<code::Attempt>,
        verify: VecDeque<Result<(), String>>,
        status_path: PathBuf,
        report_path: PathBuf,
    }

    impl Lifecycle for Forwarding {
        fn work(
            &mut self,
            _ticket: &Ticket,
            _handover: Option<&str>,
            _contributions: Option<&mut QueueContributions>,
        ) -> code::Attempt {
            self.work
                .pop_front()
                .expect("the script has a code-agent outcome")
        }

        fn verify(&mut self, _ticket: &Ticket) -> Result<(), String> {
            self.verify
                .pop_front()
                .expect("the script has a verification-gate result")
        }

        fn work_tree_diff(&mut self) -> Result<String, String> {
            Ok(String::new())
        }

        fn persist(&mut self, snapshot: &QueueSnapshot) -> io::Result<()> {
            snapshot.write(&self.status_path)
        }

        fn report(&mut self, markdown: &str) -> io::Result<PathBuf> {
            fs::write(&self.report_path, markdown)?;
            Ok(self.report_path.clone())
        }
    }

    /// Run the queue with a first code-agent declaration whose verification gate
    /// returns `output` unchanged and a retry whose final gate passes. Only the
    /// agent and the gate are scripted; persistence and reporting are real.
    fn forwarding_run(output: &str) {
        let dir = std::env::temp_dir().join(format!("ejaj-gate-forwarding-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("the forwarding test directory is creatable");
        let mut lifecycle = Forwarding {
            work: VecDeque::from([
                code::Attempt::from(code::Outcome::Complete),
                code::Attempt::from(code::Outcome::Complete),
            ]),
            verify: VecDeque::from([Err(output.to_string()), Ok(())]),
            status_path: dir.join("queue.json"),
            report_path: dir.join("queue-report.md"),
        };

        let result = run_queue(&mut lifecycle, &set(&[("1-first", &[])]));

        assert!(
            matches!(result.outcome, Outcome::Resolved),
            "the retry's passing gate resolves the ticket: {:?}",
            result.outcome
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// Run this test's name in a child copy of the test binary and return the
    /// child's stderr: the caller-visible bytes the queue's forwarding path
    /// wrote. Stdout is discarded so the captured transcript holds only the
    /// diagnostics.
    fn forwarded_stderr(name: &str) -> String {
        let exe = std::env::current_exe().expect("the test binary path is known");
        let filter = format!("orchestrator::tests::{name}");
        let path = std::env::temp_dir().join(format!(
            "ejaj-gate-forwarding-{}-{name}.stderr",
            std::process::id()
        ));
        let sink = fs::File::create(&path).expect("the stderr capture file is created");
        let output = Command::new(exe)
            .args([
                "--exact",
                filter.as_str(),
                "--nocapture",
                "--test-threads=1",
            ])
            .env(GATE_FORWARDING_CHILD, "1")
            .stdout(Stdio::null())
            .stderr(Stdio::from(sink))
            .output()
            .expect("the child test process starts and is waited for");
        let transcript = fs::read_to_string(&path).unwrap_or_default();
        let _ = fs::remove_file(&path);

        assert!(
            output.status.success(),
            "the child forwarding run failed ({}):\n{transcript}",
            output.status
        );
        transcript
    }

    /// Regression: a verification gate's error text reaches the caller through
    /// the queue's existing forwarding path byte for byte. Nothing introduces a
    /// diagnostic prefix or rewrites the external text on its way out: the
    /// caller-visible event is exactly the external bytes plus the one
    /// coordinator-owned terminator, and the queue's own follow-up starts only
    /// after it.
    #[test]
    fn when_verification_gate_output_is_forwarded_given_external_error_text_then_preserves_exact_bytes()
     {
        const NAME: &str = "when_verification_gate_output_is_forwarded_given_external_error_text_then_preserves_exact_bytes";
        const EXTERNAL_ERROR_TEXT: &str =
            "verification gate: [ERROR] check failed\nsecond line stays verbatim\n";

        if is_forwarding_child() {
            forwarding_run(EXTERNAL_ERROR_TEXT);
            return;
        }

        let stderr = forwarded_stderr(NAME);
        // The forwarded event is the external text, internal newlines and all,
        // followed by exactly the newline the coordinator adds to every event it
        // writes. Nothing is prepended, no line is rewritten, and the terminator
        // is not doubled.
        let forwarded_event = format!("{EXTERNAL_ERROR_TEXT}\n");
        let subsequent = stderr
            .strip_prefix(forwarded_event.as_str())
            .unwrap_or_else(|| {
                panic!(
                    "the caller-visible stderr must open with exactly the forwarded event                      {forwarded_event:?} and no prefix of its own: {stderr:?}"
                )
            });
        // The queue's own follow-up is the next event, so the forwarded event
        // ends precisely at the coordinator's terminator.
        assert!(
            subsequent.starts_with("[WARN] ticket 1-first: the first verification gate failed"),
            "the queue's follow-up must begin right after the forwarded event: {subsequent:?}"
        );
    }
}

#[cfg(test)]
mod snapshot_tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// The fixture ids every test opens a snapshot with.
    fn ids() -> [&'static str; 3] {
        ["1-first", "2-second", "3-third"]
    }

    /// A unique path under the system temp directory; nothing is created here.
    fn unique(label: &str) -> PathBuf {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "ejaj-queue-status-{}-{n}-{label}",
            std::process::id()
        ))
    }

    /// Parse a snapshot's JSON into an object so a test can inspect the root shape.
    fn object(snapshot: &QueueSnapshot) -> serde_json::Map<String, serde_json::Value> {
        let json: serde_json::Value =
            serde_json::from_str(&snapshot.to_json().expect("the snapshot serializes"))
                .expect("the snapshot is valid JSON");
        json.as_object()
            .expect("the JSON root is an object")
            .clone()
    }

    #[test]
    fn every_status_serializes_to_its_lowercase_name() {
        assert_eq!(
            serde_json::to_string(&TicketStatus::Pending).unwrap(),
            "\"pending\""
        );
        assert_eq!(
            serde_json::to_string(&TicketStatus::InProgress).unwrap(),
            "\"in_progress\""
        );
        assert_eq!(
            serde_json::to_string(&TicketStatus::Completed).unwrap(),
            "\"completed\""
        );
        assert_eq!(
            serde_json::to_string(&TicketStatus::Failed).unwrap(),
            "\"failed\""
        );
    }

    #[test]
    fn every_status_round_trips_through_the_snapshot() {
        let statuses = [
            ("1-first", TicketStatus::Pending),
            ("2-second", TicketStatus::InProgress),
            ("3-third", TicketStatus::Completed),
            ("4-fourth", TicketStatus::Failed),
        ];
        let mut snapshot = QueueSnapshot::new(statuses.map(|(id, _)| id));
        for (id, status) in statuses {
            snapshot.set(id, status);
            assert_eq!(snapshot.get(id), Some(status));
        }

        let reparsed: QueueSnapshot =
            serde_json::from_str(&snapshot.to_json().unwrap()).expect("the snapshot round-trips");
        assert_eq!(reparsed, snapshot);
    }

    #[test]
    fn initialization_covers_every_ticket_as_pending() {
        let snapshot = QueueSnapshot::new(ids());
        assert_eq!(snapshot.len(), ids().len());
        for id in ids() {
            assert_eq!(snapshot.get(id), Some(TicketStatus::Pending), "{id}");
        }
    }

    #[test]
    fn the_json_root_is_a_direct_object_keyed_by_ticket_id() {
        let snapshot = QueueSnapshot::new(ids());
        let object = object(&snapshot);

        // Direct: one key per id, and no wrapping envelope field.
        assert_eq!(object.len(), ids().len());
        assert!(!object.contains_key("tickets"), "no envelope field");
        assert!(!object.contains_key("statuses"), "no envelope field");

        for id in ids() {
            assert_eq!(
                object.get(id).and_then(serde_json::Value::as_str),
                Some("pending"),
                "{id}"
            );
        }
    }

    #[test]
    fn an_update_preserves_every_id() {
        let mut snapshot = QueueSnapshot::new(ids());
        snapshot.set("2-second", TicketStatus::InProgress);
        snapshot.set("3-third", TicketStatus::Failed);

        let object = object(&snapshot);
        assert_eq!(object.len(), ids().len(), "every id survives the updates");
        assert_eq!(object["1-first"], "pending");
        assert_eq!(object["2-second"], "in_progress");
        assert_eq!(object["3-third"], "failed");
    }

    #[test]
    fn a_written_snapshot_holds_the_full_state() {
        let dir = unique("write");
        fs::create_dir_all(&dir).expect("the temp directory is creatable");
        let path = dir.join("queue.json");

        let mut snapshot = QueueSnapshot::new(ids());
        snapshot.set("1-first", TicketStatus::Completed);
        snapshot.write(&path).expect("the snapshot writes");

        let written = fs::read_to_string(&path).expect("the written file is readable");
        let json: serde_json::Value = serde_json::from_str(&written).expect("the file is JSON");
        let object = json.as_object().expect("the root is an object");
        assert_eq!(object.len(), ids().len(), "the whole snapshot is written");
        assert_eq!(object["1-first"], "completed");
        assert_eq!(object["2-second"], "pending");
        assert_eq!(object["3-third"], "pending");

        fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[test]
    fn a_write_into_a_missing_directory_fails() {
        // The parent directory is deliberately never created, so the write fails in the
        // filesystem and the error kind is the OS's own.
        let dir = unique("missing");
        let path = dir.join("queue.json");

        let error = QueueSnapshot::new(ids())
            .write(&path)
            .expect_err("a write with no parent directory must fail");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use std::collections::VecDeque;

    /// A scripted [`Lifecycle`]: the code-agent and gate outcomes are queued in
    /// advance, and one chosen persistence attempt fails. The snapshots it
    /// accepted, the reports it was asked to write, and the tickets it was asked
    /// to work are kept for assertions.
    struct Script {
        work: VecDeque<code::Attempt>,
        verify: VecDeque<Result<(), String>>,
        /// The successful snapshots, in order.
        snapshots: Vec<QueueSnapshot>,
        /// Every write attempt, so `fail_write` can name one by ordinal.
        attempts: usize,
        fail_write: Option<usize>,
        /// The reports it was asked to write, in order.
        reports: Vec<String>,
        fail_report: bool,
        worked: Vec<String>,
        verified: Vec<String>,
        /// The handover each work call received, in order.
        handovers: Vec<Option<String>>,
        /// The repository diff handed to the retry handover.
        diff: Result<String, String>,
        diff_calls: usize,
    }

    impl Script {
        fn new(work: Vec<code::Outcome>, verify: Vec<Result<(), String>>) -> Script {
            Script::with_attempts(work.into_iter().map(code::Attempt::from).collect(), verify)
        }

        fn with_attempts(work: Vec<code::Attempt>, verify: Vec<Result<(), String>>) -> Script {
            Script {
                work: work.into(),
                verify: verify.into(),
                snapshots: Vec::new(),
                attempts: 0,
                fail_write: None,
                reports: Vec::new(),
                fail_report: false,
                worked: Vec::new(),
                verified: Vec::new(),
                handovers: Vec::new(),
                diff: Ok(String::new()),
                diff_calls: 0,
            }
        }

        /// Serve the given working-tree diff to the retry handover.
        fn with_diff(mut self, diff: Result<String, String>) -> Script {
            self.diff = diff;
            self
        }

        /// Fail the `ordinal`th write attempt (0-based) instead of accepting it.
        fn failing_at(mut self, ordinal: usize) -> Script {
            self.fail_write = Some(ordinal);
            self
        }

        /// Refuse to write the report.
        fn refusing_the_report(mut self) -> Script {
            self.fail_report = true;
            self
        }

        /// The status of `id` after `write` successful snapshots.
        fn status_after(&self, id: &str, write: usize) -> Option<TicketStatus> {
            self.snapshots
                .get(write)
                .and_then(|snapshot| snapshot.get(id))
        }

        /// The status of `id` in the last accepted snapshot.
        fn last(&self, id: &str) -> Option<TicketStatus> {
            self.snapshots.last().and_then(|snapshot| snapshot.get(id))
        }

        /// The report the queue asked for last.
        fn report(&self) -> &str {
            self.reports.last().expect("the queue writes one report")
        }
    }

    impl Lifecycle for Script {
        fn work(
            &mut self,
            ticket: &Ticket,
            handover: Option<&str>,
            _contributions: Option<&mut QueueContributions>,
        ) -> code::Attempt {
            self.worked.push(ticket.id.clone());
            self.handovers.push(handover.map(str::to_string));
            self.work
                .pop_front()
                .expect("the script has a code-agent outcome")
        }

        fn verify(&mut self, ticket: &Ticket) -> Result<(), String> {
            self.verified.push(ticket.id.clone());
            self.verify
                .pop_front()
                .expect("the script has a verification-gate result")
        }

        fn work_tree_diff(&mut self) -> Result<String, String> {
            self.diff_calls += 1;
            self.diff.clone()
        }

        fn persist(&mut self, snapshot: &QueueSnapshot) -> io::Result<()> {
            let ordinal = self.attempts;
            self.attempts += 1;
            if self.fail_write == Some(ordinal) {
                return Err(io::Error::other("scripted status write"));
            }
            self.snapshots.push(snapshot.clone());
            Ok(())
        }

        fn report(&mut self, markdown: &str) -> io::Result<PathBuf> {
            if self.fail_report {
                return Err(io::Error::other("scripted report write"));
            }
            self.reports.push(markdown.to_string());
            Ok(PathBuf::from("queue-report.md"))
        }
    }

    /// One ticket.
    fn one() -> TicketSet {
        super::tests::set(&[("1-first", &[])])
    }

    /// A chain of two, the second blocked by the first.
    fn chain() -> TicketSet {
        super::tests::set(&[("1-first", &[]), ("2-second", &["1-first"])])
    }

    /// A chain plus a ticket that hangs off neither: the independent branch must
    /// keep running when the first one fails.
    fn fork() -> TicketSet {
        super::tests::set(&[
            ("1-first", &[]),
            ("2-dependent", &["1-first"]),
            ("3-independent", &[]),
        ])
    }

    /// The ticket and reason of a queue that stopped on a status write.
    fn stopped(result: &QueueResult) -> (&str, &str) {
        match &result.outcome {
            Outcome::Stopped { ticket, reason } => (ticket.as_str(), reason.as_str()),
            other => panic!("expected a stopped queue, got {other:?}"),
        }
    }

    /// Assert the queue drained without completing every ticket.
    fn incomplete(result: &QueueResult) {
        assert!(
            matches!(result.outcome, Outcome::Incomplete),
            "{:?}",
            result.outcome
        );
    }

    /// Assert the queue completed every ticket.
    fn resolved(result: &QueueResult) {
        assert!(
            matches!(result.outcome, Outcome::Resolved),
            "{:?}",
            result.outcome
        );
    }

    #[test]
    fn success_records_pending_then_in_progress_then_completed() {
        let mut lifecycle = Script::new(vec![code::Outcome::Complete], vec![Ok(())]);
        let result = run_queue(&mut lifecycle, &one());

        assert!(matches!(result.outcome, Outcome::Resolved));
        assert_eq!(
            lifecycle.status_after("1-first", 0),
            Some(TicketStatus::Pending)
        );
        assert_eq!(
            lifecycle.status_after("1-first", 1),
            Some(TicketStatus::InProgress)
        );
        assert_eq!(
            lifecycle.status_after("1-first", 2),
            Some(TicketStatus::Completed)
        );
        assert_eq!(lifecycle.worked, ["1-first"]);
        assert_eq!(lifecycle.verified, ["1-first"]);
    }

    #[test]
    fn resolution_follows_a_persisted_completion() {
        let mut lifecycle = Script::new(
            vec![code::Outcome::Complete, code::Outcome::Complete],
            vec![Ok(()), Ok(())],
        );
        let result = run_queue(&mut lifecycle, &chain());

        assert!(matches!(result.outcome, Outcome::Resolved));
        assert_eq!(lifecycle.worked, ["1-first", "2-second"]);
        // The dependent starts only after the blocker's completion is on disk.
        assert_eq!(
            lifecycle.status_after("1-first", 2),
            Some(TicketStatus::Completed)
        );
        assert_eq!(
            lifecycle.status_after("2-second", 3),
            Some(TicketStatus::InProgress)
        );
        assert_eq!(lifecycle.last("2-second"), Some(TicketStatus::Completed));
    }

    #[test]
    fn a_turn_end_is_retried_once_and_the_declaration_then_counts() {
        let mut lifecycle = Script::new(
            vec![code::Outcome::TurnEnd, code::Outcome::Complete],
            vec![Ok(())],
        );
        let result = run_queue(&mut lifecycle, &one());

        resolved(&result);
        // The retry replays the same ticket; neither attempt writes a snapshot.
        assert_eq!(lifecycle.worked, ["1-first", "1-first"]);
        assert_eq!(lifecycle.verified, ["1-first"]);
        assert_eq!(lifecycle.snapshots.len(), 3);
        assert_eq!(lifecycle.last("1-first"), Some(TicketStatus::Completed));
    }

    #[test]
    fn a_turn_end_that_survives_the_retry_is_accepted_when_the_gate_passes() {
        let mut lifecycle = Script::new(
            vec![code::Outcome::TurnEnd, code::Outcome::TurnEnd],
            vec![Ok(())],
        );
        let result = run_queue(&mut lifecycle, &one());

        resolved(&result);
        assert_eq!(lifecycle.worked, ["1-first", "1-first"]);
        assert_eq!(lifecycle.verified, ["1-first"]);
        assert_eq!(lifecycle.last("1-first"), Some(TicketStatus::Completed));
        assert!(
            lifecycle
                .report()
                .contains("- Ended: gate accepted without declaration"),
            "{}",
            lifecycle.report()
        );
    }

    #[test]
    fn a_turn_end_that_survives_the_retry_fails_the_ticket_and_blocks_its_dependents() {
        let mut lifecycle = Script::new(
            vec![code::Outcome::TurnEnd, code::Outcome::TurnEnd],
            vec![Err("gate output".to_string())],
        );
        let result = run_queue(&mut lifecycle, &chain());

        incomplete(&result);
        assert_eq!(lifecycle.worked, ["1-first", "1-first"]);
        assert_eq!(lifecycle.verified, ["1-first"]);
        assert_eq!(
            lifecycle.status_after("1-first", 2),
            Some(TicketStatus::Failed)
        );
        assert_eq!(lifecycle.last("2-second"), Some(TicketStatus::Pending));
        let report = lifecycle.report();
        assert!(
            report.contains("- Ended: retry exhausted without task_complete"),
            "{report}"
        );
        assert!(
            report.contains("- Ended: blocked by 1-first (failed)"),
            "{report}"
        );
    }

    #[test]
    fn an_ineligible_agent_failure_fails_the_active_ticket_without_a_retry() {
        let mut lifecycle = Script::new(vec![code::Outcome::Failed(FailureKind::Api)], vec![]);
        let result = run_queue(&mut lifecycle, &chain());

        incomplete(&result);
        // An API failure is fatal: neither a retry nor a gate is worth running.
        assert_eq!(lifecycle.worked, ["1-first"]);
        assert!(lifecycle.verified.is_empty());
        assert_eq!(lifecycle.handovers, [None]);
        assert_eq!(lifecycle.diff_calls, 0);
        assert_eq!(lifecycle.last("1-first"), Some(TicketStatus::Failed));
        assert!(
            lifecycle
                .report()
                .contains("- Ended: agent failure: API error"),
            "{}",
            lifecycle.report()
        );
    }

    #[test]
    fn an_eligible_agent_stop_is_retried_once_and_the_final_gate_decides() {
        let mut lifecycle = Script::new(
            vec![
                code::Outcome::Failed(FailureKind::StepLimit),
                code::Outcome::TurnEnd,
            ],
            vec![Ok(())],
        );
        let result = run_queue(&mut lifecycle, &one());

        resolved(&result);
        assert_eq!(lifecycle.worked, ["1-first", "1-first"]);
        assert_eq!(lifecycle.verified, ["1-first"]);
        assert_eq!(lifecycle.diff_calls, 1);
        assert!(
            lifecycle
                .report()
                .contains("- Ended: gate accepted without declaration"),
            "{}",
            lifecycle.report()
        );
    }

    #[test]
    fn a_declared_ticket_whose_first_gate_fails_is_retried_once() {
        let mut lifecycle = Script::new(
            vec![code::Outcome::Complete, code::Outcome::Complete],
            vec![Err("first gate output".to_string()), Ok(())],
        );
        let result = run_queue(&mut lifecycle, &one());

        resolved(&result);
        assert_eq!(lifecycle.worked, ["1-first", "1-first"]);
        assert_eq!(lifecycle.verified, ["1-first", "1-first"]);
        assert_eq!(lifecycle.last("1-first"), Some(TicketStatus::Completed));
        let handover = lifecycle.handovers[1]
            .as_deref()
            .expect("the retry carries a handover");
        assert!(handover.contains("first gate output"), "{handover}");
        assert!(
            lifecycle.report().contains("- Ended: declared"),
            "{}",
            lifecycle.report()
        );
    }

    #[test]
    fn a_declared_ticket_whose_final_gate_fails_fails_the_ticket() {
        let mut lifecycle = Script::new(
            vec![code::Outcome::Complete, code::Outcome::Complete],
            vec![
                Err("first gate output".to_string()),
                Err("final gate output".to_string()),
            ],
        );
        let result = run_queue(&mut lifecycle, &chain());

        incomplete(&result);
        assert!(
            lifecycle
                .snapshots
                .iter()
                .all(|snapshot| snapshot.get("1-first") != Some(TicketStatus::Completed)),
            "a failed final gate must never record a completion"
        );
        assert_eq!(lifecycle.last("1-first"), Some(TicketStatus::Failed));
        assert_eq!(lifecycle.last("2-second"), Some(TicketStatus::Pending));
        assert_eq!(lifecycle.worked, ["1-first", "1-first"]);
        assert_eq!(lifecycle.verified, ["1-first", "1-first"]);
        assert!(
            lifecycle.report().contains("- Ended: gate failed"),
            "{}",
            lifecycle.report()
        );
    }

    #[test]
    fn when_retry_handover_given_no_narration_or_repository_changes_then_reports_empty_values() {
        let mut lifecycle = Script::new(
            vec![code::Outcome::TurnEnd, code::Outcome::TurnEnd],
            vec![Ok(())],
        );
        let result = run_queue(&mut lifecycle, &one());

        resolved(&result);
        let handover = lifecycle.handovers[1]
            .as_deref()
            .expect("the retry carries a handover");
        assert!(handover.contains("(empty narration)"), "{handover}");
        assert!(handover.contains("(empty diff)"), "{handover}");
        assert!(
            !handover.contains("fabricated"),
            "no diagnostic content is invented: {handover}"
        );
    }

    #[test]
    fn when_retry_handover_given_diff_collection_error_then_names_collection_failure() {
        let mut lifecycle = Script::new(
            vec![code::Outcome::TurnEnd, code::Outcome::TurnEnd],
            vec![Ok(())],
        )
        .with_diff(Err("scripted git failure".to_string()));
        let result = run_queue(&mut lifecycle, &one());

        resolved(&result);
        let handover = lifecycle.handovers[1]
            .as_deref()
            .expect("the retry carries a handover");
        assert!(handover.contains("diff collection failed"), "{handover}");
        assert!(handover.contains("scripted git failure"), "{handover}");
    }

    #[test]
    fn an_initial_pending_write_failure_starts_no_session() {
        let mut lifecycle = Script::new(vec![code::Outcome::Complete], vec![Ok(())]).failing_at(0);
        let result = run_queue(&mut lifecycle, &one());

        let (ticket, reason) = stopped(&result);
        assert!(ticket.is_empty(), "no ticket is active yet: {ticket}");
        assert!(reason.starts_with(PERSISTENCE_PREFIX), "{reason}");
        assert!(lifecycle.worked.is_empty());
        assert!(lifecycle.verified.is_empty());
        assert!(lifecycle.snapshots.is_empty());
    }

    #[test]
    fn an_in_progress_write_failure_starts_no_session() {
        let mut lifecycle = Script::new(vec![code::Outcome::Complete], vec![Ok(())]).failing_at(1);
        let result = run_queue(&mut lifecycle, &one());

        let (ticket, reason) = stopped(&result);
        assert_eq!(ticket, "1-first");
        assert!(reason.starts_with(PERSISTENCE_PREFIX), "{reason}");
        // The pending snapshot was accepted; the session itself never started.
        assert!(lifecycle.worked.is_empty());
        assert!(lifecycle.verified.is_empty());
        assert_eq!(lifecycle.snapshots.len(), 1);
    }

    #[test]
    fn a_failed_status_write_failure_stops_with_a_persistence_reason() {
        // 0: pending, 1: in-progress, 2: the failed transition. The retry writes
        // nothing, so the ordinals match a declared failure's.
        let mut lifecycle = Script::new(
            vec![code::Outcome::TurnEnd, code::Outcome::TurnEnd],
            vec![Err("gate output".to_string())],
        )
        .failing_at(2);
        let result = run_queue(&mut lifecycle, &one());

        let (ticket, reason) = stopped(&result);
        assert_eq!(ticket, "1-first");
        assert!(reason.starts_with(PERSISTENCE_PREFIX), "{reason}");
        assert!(!reason.contains("without completing"), "{reason}");
        // The ending is recorded before the write that fails, so the report
        // still names how the ticket ended.
        assert!(
            lifecycle
                .report()
                .contains("- Ended: retry exhausted without task_complete"),
            "{}",
            lifecycle.report()
        );
    }

    #[test]
    fn a_completed_write_failure_neither_resolves_nor_starts_later_work() {
        let mut lifecycle = Script::new(
            vec![code::Outcome::Complete, code::Outcome::Complete],
            vec![Ok(()), Ok(())],
        )
        .failing_at(2);
        let result = run_queue(&mut lifecycle, &chain());

        let (ticket, reason) = stopped(&result);
        assert_eq!(ticket, "1-first");
        assert!(reason.starts_with(PERSISTENCE_PREFIX), "{reason}");
        // The gate ran, but the completion was not durable, so the dependent
        // must not be scheduled.
        assert_eq!(lifecycle.worked, ["1-first"]);
        assert_eq!(lifecycle.verified, ["1-first"]);
        assert_eq!(
            lifecycle.last("1-first"),
            Some(TicketStatus::InProgress),
            "the completion was not accepted"
        );
    }

    #[test]
    fn a_completed_ticket_stays_completed_when_a_later_ticket_fails() {
        let mut lifecycle = Script::new(
            vec![
                code::Outcome::Complete,
                code::Outcome::Failed(FailureKind::Api),
            ],
            vec![Ok(())],
        );
        let result = run_queue(&mut lifecycle, &chain());

        incomplete(&result);
        assert_eq!(lifecycle.last("1-first"), Some(TicketStatus::Completed));
        assert_eq!(lifecycle.last("2-second"), Some(TicketStatus::Failed));
        assert!(
            lifecycle
                .report()
                .contains("1 completed, 1 failed, 0 blocked"),
            "{}",
            lifecycle.report()
        );
    }

    #[test]
    fn a_failed_ticket_does_not_stop_an_independent_one() {
        let mut lifecycle = Script::new(
            vec![
                code::Outcome::Failed(FailureKind::Api),
                code::Outcome::Complete,
            ],
            vec![Ok(())],
        );
        let result = run_queue(&mut lifecycle, &fork());

        incomplete(&result);
        // The failing branch dies; the branch that hangs off nothing runs.
        assert_eq!(lifecycle.worked, ["1-first", "3-independent"]);
        assert_eq!(lifecycle.last("1-first"), Some(TicketStatus::Failed));
        assert_eq!(
            lifecycle.last("3-independent"),
            Some(TicketStatus::Completed)
        );
        assert_eq!(lifecycle.last("2-dependent"), Some(TicketStatus::Pending));
        let report = lifecycle.report();
        assert!(
            report.contains("1 completed, 1 failed, 1 blocked"),
            "{report}"
        );
        assert!(
            report.contains("- Ended: blocked by 1-first (failed)"),
            "{report}"
        );
    }

    #[test]
    fn an_unrunnable_set_ends_incomplete_without_naming_a_culprit() {
        // A cycle is refused by the validator before the queue runs; if one
        // reaches the queue anyway, nothing is executable at all.
        let mut lifecycle = Script::new(vec![], vec![]);
        let result = run_queue(
            &mut lifecycle,
            &super::tests::set(&[("1-a", &["2-b"]), ("2-b", &["1-a"])]),
        );

        incomplete(&result);
        assert!(lifecycle.worked.is_empty());
        assert!(
            lifecycle
                .report()
                .contains("- Ended: blocked by an unresolved dependency"),
            "{}",
            lifecycle.report()
        );
    }

    #[test]
    fn the_report_records_a_completed_ticket_in_full() {
        let mut lifecycle = Script::new(vec![code::Outcome::Complete], vec![Ok(())]);
        let result = run_queue(&mut lifecycle, &one());

        resolved(&result);
        let report = lifecycle.report();
        assert!(report.starts_with("# Ticket queue report\n"), "{report}");
        assert!(
            report.contains("1 ticket: 1 completed, 0 failed, 0 blocked."),
            "{report}"
        );
        assert!(report.contains("\n## 1-first\n"), "{report}");
        assert!(report.contains("- Title: 1-first\n"), "{report}");
        assert!(report.contains("- Status: completed\n"), "{report}");
        assert!(report.contains("- Ended: declared\n"), "{report}");
        assert!(report.contains("- Gate: `cargo check`\n"), "{report}");
        assert!(
            report.ends_with('\n') && !report.ends_with("\n\n"),
            "{report}"
        );
    }

    #[test]
    fn a_report_write_failure_is_a_warning_and_changes_nothing() {
        let mut lifecycle =
            Script::new(vec![code::Outcome::Complete], vec![Ok(())]).refusing_the_report();
        let result = run_queue(&mut lifecycle, &one());

        // The run still resolves: the report is derived state, and the snapshot
        // written earlier is the record.
        resolved(&result);
        assert!(lifecycle.reports.is_empty());
        assert_eq!(lifecycle.last("1-first"), Some(TicketStatus::Completed));
    }

    #[test]
    fn a_stopped_queue_still_reports_the_tickets_it_never_started() {
        let mut lifecycle = Script::new(vec![code::Outcome::Complete], vec![Ok(())]).failing_at(0);
        let result = run_queue(&mut lifecycle, &one());

        let _ = stopped(&result);
        let report = lifecycle.report();
        assert!(
            report.contains("0 completed, 0 failed, 1 never started"),
            "{report}"
        );
        assert!(report.contains("- Ended: never started"), "{report}");
    }

    /// The result carries the very markdown the queue handed to the report
    /// write, so the caller composes the next phase from this invocation's
    /// content rather than from a file an earlier run may have left behind.
    #[test]
    fn the_result_carries_the_report_generated_for_this_invocation() {
        let mut lifecycle = Script::new(vec![code::Outcome::Complete], vec![Ok(())]);
        let result = run_queue(&mut lifecycle, &one());

        resolved(&result);
        assert_eq!(
            result.report,
            lifecycle.report(),
            "the result must carry the markdown the queue generated"
        );
        assert!(result.report.starts_with(
            "# Ticket queue report
"
        ));
        assert!(result.report.contains("1 completed, 0 failed, 0 blocked."));
        assert!(result.report.contains(
            "
## 1-first
"
        ));
    }

    /// The report write is best-effort: a failed file write changes neither the
    /// outcome nor the content the caller gets back.
    #[test]
    fn the_generated_report_survives_a_report_write_failure() {
        let mut lifecycle =
            Script::new(vec![code::Outcome::Complete], vec![Ok(())]).refusing_the_report();
        let result = run_queue(&mut lifecycle, &one());

        resolved(&result);
        assert!(lifecycle.reports.is_empty(), "the write did fail");
        assert!(
            result.report.starts_with(
                "# Ticket queue report
"
            ),
            "{}",
            result.report
        );
        assert!(
            result.report.contains("1 completed, 0 failed, 0 blocked."),
            "{}",
            result.report
        );
        assert!(
            result.report.contains("- Status: completed"),
            "{}",
            result.report
        );
    }

    /// A queue that never drained still hands its report back, so the caller
    /// can summarise a partial run without inventing an outcome of its own.
    #[test]
    fn a_stopped_queue_returns_its_report_too() {
        let mut lifecycle = Script::new(vec![code::Outcome::Complete], vec![Ok(())]).failing_at(0);
        let result = run_queue(&mut lifecycle, &one());

        let _ = stopped(&result);
        assert_eq!(result.report, lifecycle.report());
        assert!(
            result
                .report
                .contains("0 completed, 0 failed, 1 never started"),
            "{}",
            result.report
        );
    }

    /// Create a fresh temporary project root for one test, so no test can
    /// collide with another's fixture or with the process working directory.
    fn scratch_root(label: &str) -> PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "ejaj-orchestrator-test-{}-{id}-{label}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("the temporary root should be creatable");
        root
    }

    /// A production lifecycle anchored to an explicit root, with the scratch
    /// config and unused queue files this verification-focused test needs.
    fn production<'a>(
        config: &'a Config,
        root: &'a Path,
        session_dir: Option<&'a Path>,
    ) -> Production<'a> {
        Production {
            config,
            scope: "",
            root,
            session_dir,
            status_path: Path::new("unused-queue.json"),
            report_path: Path::new("unused-queue-report.md"),
        }
    }

    /// One ticket whose only relevant field is its verification gate.
    fn gate_ticket(gate: &str) -> Ticket {
        let mut ticket = super::tests::set(&[("1-gate", &[])])
            .tickets
            .into_iter()
            .next()
            .expect("the set holds one ticket");
        ticket.verification_gate = gate.to_string();
        ticket
    }

    fn verification_config() -> Config {
        Config::new("http://example.invalid", "test-model", None)
    }

    /// Core entry-point behavior and regression: a verification gate runs in the
    /// explicit project root even when that root is not the process working
    /// directory. The marker exists only under the root, the process working
    /// directory is never changed, and the fixture is removed.
    #[test]
    fn when_ticket_verification_gate_runs_given_explicit_project_root_then_executes_in_that_root() {
        let root = scratch_root("verification-explicit-root");
        let marker = format!("ejaj-root-marker-{}-verification", std::process::id());
        fs::write(root.join(&marker), b"present").expect("the root marker should be writable");
        assert!(
            !Path::new(&marker).exists(),
            "the relative marker must be absent from the process working directory"
        );
        let before = std::env::current_dir().expect("the working directory should be readable");
        let config = verification_config();
        let mut production = production(&config, &root, None);

        assert_eq!(
            production.verify(&gate_ticket(&format!("test -f {marker}"))),
            Ok(()),
            "the gate must find its relative marker under the explicit project root"
        );

        let after = std::env::current_dir().expect("the working directory should be readable");
        assert_eq!(
            before, after,
            "the test must not change the process working directory"
        );
        assert!(
            !Path::new(&marker).exists(),
            "the marker must never appear in the process working directory"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// The outcome a verification-boundary row expects the gate to preserve.
    #[derive(Clone, Copy, Debug)]
    enum GateOutcome {
        /// A zero exit maps to `Ok(())`.
        Success,
        /// A nonzero exit or spawn failure surfaces the helper's error.
        HelperError,
    }

    /// One row of the consolidated verification-boundary table: explicit
    /// inputs (the fixture root, an optional session directory, the gate
    /// command) and the outcome the root-aware helper must produce.
    struct VerificationCase {
        /// The row's name, used for the fixture directory and failure messages.
        name: &'static str,
        /// Whether the explicit project root exists when the gate runs.
        root_exists: bool,
        /// The session directory carried beside the root, when any.
        session_dir: Option<&'static str>,
        /// A fixture file to create under the root before the gate runs.
        fixture: Option<&'static str>,
        /// Builds the gate command from the fixture file name.
        gate: fn(&str) -> String,
        /// The helper outcome the gate must preserve.
        expected: GateOutcome,
        /// A file the gate must have written, with its exact content.
        artifact: Option<(&'static str, &'static str)>,
        /// The minimum command length the row exercises, for the maximum case.
        minimum_command_len: usize,
    }

    /// A gate that succeeds without touching the fixture.
    fn passing_gate(_fixture: &str) -> String {
        "true".to_string()
    }

    /// A gate that proves the exact command text reaches the shell.
    fn exact_payload_gate(_fixture: &str) -> String {
        "printf '%s' '  exact gate payload  ' > exact-gate-output".to_string()
    }

    /// A long gate whose final suffix writes an observable artifact. Every
    /// repeatable clause before it succeeds on its own, so a gate truncated at
    /// one of those clause boundaries would still exit zero; only the complete
    /// command reaches the suffix that creates the artifact the case asserts.
    fn complete_gate(_fixture: &str) -> String {
        let prefix = "printf '%s' '' >/dev/null && ".repeat(400);
        format!("{prefix}printf '%s' 'complete-gate-ran' > complete-gate-artifact")
    }

    /// A gate that writes both streams and exits nonzero.
    fn nonzero_gate(_fixture: &str) -> String {
        "echo GATE-OUT; echo GATE-ERR >&2; exit 7".to_string()
    }

    /// The consolidated boundary table. It covers the empty/no-session case,
    /// the minimum valid explicit root with its exact command contract, the
    /// maximum complete command, a nonzero gate, and an unavailable root.
    fn verification_cases() -> Vec<VerificationCase> {
        vec![
            VerificationCase {
                name: "verification-no-session-directory",
                root_exists: true,
                session_dir: None,
                fixture: None,
                gate: passing_gate,
                expected: GateOutcome::Success,
                artifact: None,
                minimum_command_len: 0,
            },
            VerificationCase {
                name: "verification-valid-explicit-root",
                root_exists: true,
                session_dir: Some("session"),
                fixture: None,
                gate: exact_payload_gate,
                expected: GateOutcome::Success,
                artifact: Some(("exact-gate-output", "  exact gate payload  ")),
                minimum_command_len: 0,
            },
            VerificationCase {
                name: "verification-complete-gate-command",
                root_exists: true,
                session_dir: None,
                fixture: None,
                gate: complete_gate,
                expected: GateOutcome::Success,
                artifact: Some(("complete-gate-artifact", "complete-gate-ran")),
                minimum_command_len: 8_001,
            },
            VerificationCase {
                name: "verification-nonzero-gate",
                root_exists: true,
                session_dir: None,
                fixture: None,
                gate: nonzero_gate,
                expected: GateOutcome::HelperError,
                artifact: None,
                minimum_command_len: 0,
            },
            VerificationCase {
                name: "verification-unavailable-root",
                root_exists: false,
                session_dir: None,
                fixture: None,
                gate: passing_gate,
                expected: GateOutcome::HelperError,
                artifact: None,
                minimum_command_len: 0,
            },
        ]
    }

    /// Edge-case table over the verification and ticket-work boundary: the
    /// explicit root survives every case, an optional session directory is
    /// forwarded unchanged, and the gate preserves exactly the root-aware
    /// helper's success or error outcome.
    #[test]
    fn when_ticket_verification_gate_receives_boundary_cases_given_rooted_commands_then_preserves_outcomes()
     {
        let config = verification_config();

        for case in verification_cases() {
            let root = scratch_root(case.name);
            if !case.root_exists {
                fs::remove_dir_all(&root).expect("the unavailable root fixture must be removable");
            }
            assert_eq!(
                root.exists(),
                case.root_exists,
                "{}: root fixture",
                case.name
            );

            let fixture = case.fixture.unwrap_or_default();
            if let Some(marker) = case.fixture {
                fs::write(root.join(marker), b"present")
                    .expect("the root fixture marker should be writable");
            }

            let session = case.session_dir.map(|name| root.join(".ejaj").join(name));
            let mut production = production(&config, &root, session.as_deref());

            let invocation = production.invocation();
            assert_eq!(
                invocation.root,
                Some(root.as_path()),
                "{}: the explicit root must survive every boundary",
                case.name
            );
            assert_eq!(
                invocation.session_dir,
                session.as_deref(),
                "{}: a supplied session directory must be forwarded unchanged",
                case.name
            );

            let command = (case.gate)(fixture);
            assert!(
                command.len() >= case.minimum_command_len,
                "{}: the fixture must be long enough to expose truncation",
                case.name
            );
            let expected = match case.expected {
                GateOutcome::Success => Ok(()),
                GateOutcome::HelperError => {
                    let expected = run_command_in(&root, &command).map(|_| ());
                    assert!(
                        expected.is_err(),
                        "{}: the row's helper must fail: {expected:?}",
                        case.name
                    );
                    expected
                }
            };

            assert_eq!(
                production.verify(&gate_ticket(&command)),
                expected,
                "{}: the gate must preserve the helper's outcome",
                case.name
            );

            if let Some((file, content)) = case.artifact {
                assert_eq!(
                    fs::read_to_string(root.join(file)).unwrap_or_else(|error| panic!(
                        "{}: the gate should write {file}: {error}",
                        case.name
                    )),
                    content,
                    "{}: the complete gate text must reach the shell unmodified",
                    case.name
                );
            }

            let _ = fs::remove_dir_all(&root);
        }
    }

    #[test]
    fn when_running_queue_given_prior_completed_tickets_then_preserves_cumulative_status_and_report()
     {
        let prior = super::tests::set(&[("1-initial", &[])]).tickets;
        let fix_set = super::tests::set(&[("r1-1-fix", &[])]);
        let mut lifecycle = Script::new(vec![code::Outcome::Complete], vec![Ok(())]);

        let result = run_queue_with_prior(&mut lifecycle, &prior, &fix_set);
        resolved(&result);

        assert_eq!(lifecycle.last("1-initial"), Some(TicketStatus::Completed));
        assert_eq!(lifecycle.last("r1-1-fix"), Some(TicketStatus::Completed));
        assert!(
            result
                .report
                .contains("2 tickets: 2 completed, 0 failed, 0 blocked.")
        );
        assert!(result.report.contains("## 1-initial"));
        assert!(result.report.contains("## r1-1-fix"));
    }
}
