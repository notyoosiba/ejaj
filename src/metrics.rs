//! The stable, versioned run-metrics contract and its directly testable
//! filesystem helpers.
//!
//! One run produces one RunMetrics document. It carries the phase and queue
//! measurements the pipeline already owns, the per-role provider usage rolled up
//! from the persisted trajectories, and the summary-rejection categories in
//! their wire vocabulary. The queue and timing structs are carried through from
//! their original modules rather than duplicated here, so a measurement has one
//! representation.
//!
//! The helpers in this module are deliberately small and caller-facing: a
//! duration is floored to whole milliseconds, trajectories are aggregated
//! deterministically in lexical filename order, pricing comes only from the
//! role that served the request, and persistence writes pretty JSON at the top
//! of a session directory only when an exact .ejaj path component is present.

use crate::api::TokenUsage;
use crate::config::Config;
use crate::orchestrator::{AttemptOutcomeTag, QueueRoundMetrics, TicketStatusTag};
use crate::tools::Phase;

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;
use std::time::Duration;

/// The version of the serialized metrics contract this crate writes.
pub const METRICS_SCHEMA_VERSION: u32 = 2;

/// The file name a persisted run-metrics document always has.
pub const METRICS_FILE_NAME: &str = "metrics.json";

/// The exact suffix every persisted trajectory file carries.
const TRAJECTORY_SUFFIX: &str = ".traj.json";

/// The closed set of phases, in routing order, used to resolve a role's
/// configured rates through the configuration's canonical routing.
const ROUTE_PHASES: [Phase; 7] = [
    Phase::Plan,
    Phase::Spec,
    Phase::Tickets,
    Phase::Triage,
    Phase::Code,
    Phase::Review,
    Phase::Summary,
];

/// The run-level terminal outcome, in the stable wire vocabulary.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalOutcome {
    /// Every post-handoff phase handed off and the run finished.
    #[default]
    Complete,
    /// A reached phase returned a recoverable retry.
    Retry,
    /// A handoff artifact could not be written.
    WriteFailed,
    /// The summary pass produced no usable summary.
    SummaryFailed,
    /// A Git operation failed.
    GitFailed,
}

/// The final outcome of one unattended pass, in the stable wire vocabulary.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PassOutcome {
    /// The pass produced its handoff.
    Handoff,
    /// The pass ended its own turn without a handoff.
    #[default]
    TurnEnd,
    /// The pass failed on an API error.
    FailedApi,
    /// The pass failed on a transport error.
    FailedTransport,
    /// The pass failed on a malformed response.
    FailedMalformed,
    /// The pass reached its step limit.
    FailedStepLimit,
    /// The pass reached its context guard.
    FailedContextGuard,
}

impl PassOutcome {
    /// Map one agent-loop failure kind into the stable wire pass outcome.
    pub(crate) fn from_failure_kind(kind: crate::loops::FailureKind) -> Self {
        match kind {
            crate::loops::FailureKind::Api => Self::FailedApi,
            crate::loops::FailureKind::Transport => Self::FailedTransport,
            crate::loops::FailureKind::Malformed => Self::FailedMalformed,
            crate::loops::FailureKind::StepLimit => Self::FailedStepLimit,
            crate::loops::FailureKind::ContextGuard => Self::FailedContextGuard,
        }
    }
}

/// The final outcome of the summary pass, in the stable wire vocabulary.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SummaryOutcome {
    /// A candidate was accepted as the canonical summary.
    Accepted,
    /// No present candidate satisfied the format before the cap.
    #[default]
    Invalid,
    /// The pass failed on an API error.
    FailedApi,
    /// The pass failed on a transport error.
    FailedTransport,
    /// The pass failed on a malformed response.
    FailedMalformed,
    /// The pass reached its step limit.
    FailedStepLimit,
    /// The pass reached its context guard.
    FailedContextGuard,
}

impl SummaryOutcome {
    /// Map one agent-loop failure kind into the stable wire summary outcome.
    pub(crate) fn from_failure_kind(kind: crate::loops::FailureKind) -> Self {
        match kind {
            crate::loops::FailureKind::Api => Self::FailedApi,
            crate::loops::FailureKind::Transport => Self::FailedTransport,
            crate::loops::FailureKind::Malformed => Self::FailedMalformed,
            crate::loops::FailureKind::StepLimit => Self::FailedStepLimit,
            crate::loops::FailureKind::ContextGuard => Self::FailedContextGuard,
        }
    }
}

/// One run's complete, versioned metrics document.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RunMetrics {
    /// The contract version this document was written with.
    pub schema_version: u32,
    /// The canonical configured mode that routed the run.
    pub mode: String,
    /// The configured model name of every active provider role, keyed by the
    /// canonical role label. Inactive roles are absent.
    pub provider_models: BTreeMap<String, String>,
    /// The run-level terminal outcome.
    pub terminal_outcome: TerminalOutcome,
    /// The phase or artifact that explains a non-complete terminal outcome.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal_detail: Option<String>,
    /// The whole measured run, in whole milliseconds.
    pub elapsed_ms: u64,
    /// The plan interview's sent turns and measured ask intervals.
    pub plan_interview: PlanInterviewMetrics,
    /// Run-level HTTP requests, summed over every role.
    pub http_requests: u64,
    /// Run-level HTTP retries, summed over every role.
    pub http_retries: u64,
    /// The spec pass, when it was reached.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spec_pass: Option<PassMetrics>,
    /// The tickets pass, when it was reached.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tickets_pass: Option<PassMetrics>,
    /// The triage pass, when it was reached.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub triage: Option<TriageMetrics>,
    /// The ordered, one-based queue-round records.
    pub queue_rounds: Vec<QueueRoundRecord>,
    /// The transition-gate aggregate.
    pub transition_gate: TransitionGateMetrics,
    /// The ordered, one-based review-round records.
    pub review_rounds: Vec<ReviewRoundMetrics>,
    /// The summary pass, when it was reached.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<SummaryMetrics>,
    /// The per-role provider usage rolled up from the trajectories.
    pub provider_roles: Vec<ProviderRoleMetrics>,
    /// The run-level estimated cost, when at least one used role was priced.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_cost_usd: Option<f64>,
}

impl RunMetrics {
    /// Seed the active provider-role models from the configuration routing.
    pub(crate) fn record_provider_models(&mut self, config: &Config) {
        self.provider_models = config
            .active_provider_models()
            .into_iter()
            .map(|(role, model)| (role.to_string(), model.to_string()))
            .collect();
    }

    /// Replace the per-role usage records and recompute the run-level totals
    /// from them with saturating addition.
    pub(crate) fn record_provider_roles(&mut self, roles: Vec<ProviderRoleMetrics>) {
        self.http_requests = roles
            .iter()
            .fold(0u64, |total, role| total.saturating_add(role.http_requests));
        self.http_retries = roles
            .iter()
            .fold(0u64, |total, role| total.saturating_add(role.http_retries));
        let mut total = 0.0f64;
        let mut priced = false;
        for role in &roles {
            if let Some(cost) = role.estimated_cost_usd {
                total += cost;
                priced = true;
            }
        }
        self.estimated_cost_usd = priced.then_some(total);
        self.provider_roles = roles;
    }
}

/// The plan interview's accumulated measurements across pre-handoff retries.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlanInterviewMetrics {
    /// The nonblank sent turns.
    pub turns: u64,
    /// The accumulated ask intervals, in whole milliseconds.
    pub elapsed_ms: u64,
}

/// One unattended pass's measurements and stable outcome.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PassMetrics {
    /// The pass's measured interval, in whole milliseconds.
    pub elapsed_ms: u64,
    /// The final pass outcome.
    pub outcome: PassOutcome,
    /// Whether a fresh-session retry ran after the first attempt failed.
    pub retried: bool,
    /// The first attempt's reason, present only when a retry ran.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_reason: Option<String>,
    /// Whether the final pass handed off successfully.
    pub succeeded: bool,
}

/// The triage pass's measurements, including the report-derived counts and the
/// queue authorization only a report can supply.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TriageMetrics {
    /// The pass measurements, flattened into the same record.
    #[serde(flatten)]
    pub pass: PassMetrics,
    /// One classification per draft ticket, when a report exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub draft_count: Option<u64>,
    /// Draft tickets ready for the agent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ready_count: Option<u64>,
    /// Draft tickets already done.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub done_count: Option<u64>,
    /// Draft tickets in every other (blocked) state.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked_count: Option<u64>,
    /// The exact queue-authorization answer, when a report exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queue_authorized: Option<bool>,
}

/// One review round's measurements and stable convergence outcome.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewRoundMetrics {
    /// The one-based review round.
    pub round: u64,
    /// The pass's measured interval, in whole milliseconds.
    pub elapsed_ms: u64,
    /// The review round's convergence outcome.
    pub outcome: ReviewRoundOutcome,
    /// Whether a fresh-session retry ran after the first attempt failed.
    pub retried: bool,
    /// The first attempt's reason, present only when a retry ran.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_reason: Option<String>,
    /// Whether the pass handed off a valid review report.
    pub succeeded: bool,
    /// Findings in the report, when the round produced one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub findings: Option<u64>,
    /// Fix tickets the report emitted, when the round produced one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fixes: Option<u64>,
}

/// The transition-gate aggregate across the run.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TransitionGateMetrics {
    /// The gate runs.
    pub runs: u64,
    /// The gate invocations that returned an error.
    pub failures: u64,
    /// The gate's accumulated measured interval, in whole milliseconds.
    pub elapsed_ms: u64,
}

/// The summary pass's measurements and stable outcome.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SummaryMetrics {
    /// The whole pass interval, in whole milliseconds.
    pub elapsed_ms: u64,
    /// The final summary outcome.
    pub outcome: SummaryOutcome,
    /// The non-absent candidates generated, including rejected ones.
    pub candidates_generated: u64,
    /// Whether a candidate was accepted.
    pub accepted: bool,
    /// Rejected-candidate categories, in candidate order.
    pub rejected_candidates: Vec<SummaryRejectionCategory>,
}
/// One provider role's aggregated trajectory usage.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ProviderRoleMetrics {
    /// The canonical trajectory role.
    pub role: String,
    /// The role's configured model name.
    pub model: String,
    /// The number of trajectory invocations.
    pub invocations: u64,
    /// The total tool steps.
    pub steps: u64,
    /// The total HTTP requests.
    pub http_requests: u64,
    /// The total HTTP retries.
    pub http_retries: u64,
    /// The token usage fields, flattened into the same record.
    #[serde(flatten)]
    pub usage: TokenUsage,
    /// The role's priced estimate, when its routing was priced.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_cost_usd: Option<f64>,
}

impl ProviderRoleMetrics {
    /// Fold one trajectory's usage into this role record with saturating
    /// addition for every counter and token field.
    fn add(&mut self, record: &TrajectoryRecord) {
        self.invocations = self.invocations.saturating_add(1);
        self.steps = self.steps.saturating_add(record.steps_taken);
        self.http_requests = self
            .http_requests
            .saturating_add(record.telemetry.provider_send_attempts);
        self.http_retries = self
            .http_retries
            .saturating_add(record.telemetry.transient_retries);
        self.usage.input_tokens = self
            .usage
            .input_tokens
            .saturating_add(record.telemetry.token_usage.input_tokens);
        self.usage.cached_input_tokens = self
            .usage
            .cached_input_tokens
            .saturating_add(record.telemetry.token_usage.cached_input_tokens);
        self.usage.output_tokens = self
            .usage
            .output_tokens
            .saturating_add(record.telemetry.token_usage.output_tokens);
        self.usage.reasoning_tokens = self
            .usage
            .reasoning_tokens
            .saturating_add(record.telemetry.token_usage.reasoning_tokens);
    }

    /// Price this role's usage from its configured routing only.
    fn price(&mut self, config: &Config) {
        let (input, cached, output) = role_rates(config, &self.role);
        self.estimated_cost_usd = self.usage.estimate_cost_usd(input, cached, output);
    }
}

/// One measured queue round, carrying the queue's own metrics under the stable
/// metrics key.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueueRoundRecord {
    /// The one-based round number.
    pub round: u64,
    /// The round's stable outcome label.
    pub outcome: QueueRoundOutcome,
    /// The queue round's existing measurements.
    pub metrics: QueueRoundMetrics,
}

impl QueueRoundRecord {
    /// Attach one queue round's number, outcome and measurements.
    pub(crate) fn new(
        round: u64,
        outcome: QueueRoundOutcome,
        metrics: QueueRoundMetrics,
    ) -> QueueRoundRecord {
        QueueRoundRecord {
            round,
            outcome,
            metrics,
        }
    }
}

/// The queue round's stable outcome vocabulary.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueRoundOutcome {
    /// Every ticket resolved.
    #[default]
    Resolved,
    /// The queue drained without resolving every ticket.
    Incomplete,
    /// The queue stopped on a ticket.
    Stopped,
}

/// The review round's stable outcome vocabulary.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewRoundOutcome {
    /// The review passed.
    #[default]
    Passed,
    /// Fix tickets were queued.
    FixesQueued,
    /// The pass ended its turn without a report.
    TurnEnd,
    /// The pass failed.
    Failed,
    /// The circuit breaker stopped the run.
    #[serde(rename = "circuit_broken")]
    CircuitBroken,
    /// The round cap stopped the run.
    RoundCapReached,
}
/// The summary-rejection categories, in wire vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SummaryRejectionCategory {
    /// An absent or empty final answer.
    Empty,
    /// A fenced code block around the answer.
    MarkdownFence,
    /// A generated ticket id reproduced in the answer.
    GeneratedTicketId,
    /// A disallowed Refs footer.
    RefsFooter,
    /// An empty body.
    EmptyBody,
    /// A disallowed footer.
    InvalidFooter,
    /// A missing blank line between the header and the body.
    MissingBlankLine,
    /// A line over the limit.
    LineOverLimit,
    /// An invalid header or preamble.
    InvalidHeaderOrPreamble,
}

/// Floor a duration to whole milliseconds and convert it without wrapping. A
/// duration beyond the u64 millisecond range saturates.
pub fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Aggregate every usable <dir>/*.traj.json file by canonical provider role.
///
/// Files are read in lexical filename order, and roles are returned in the
/// order their first usable file appears. A missing directory is an empty
/// result. An unreadable, malformed or otherwise unusable entry is skipped with
/// a warning and never aborts the aggregation.
pub fn collect_trajectory_usage(
    dir: &Path,
    config: &Config,
) -> Result<Vec<ProviderRoleMetrics>, io::Error> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };

    let mut files = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                warn_unusable(dir, &error.to_string());
                continue;
            }
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with(TRAJECTORY_SUFFIX) {
            files.push((name, entry.path()));
        }
    }
    files.sort_by(|left, right| left.0.cmp(&right.0));

    // Only the roles the configured routing actually serves may appear. The
    // configured model is the artifact's model, never the trajectory's wire
    // echo of it.
    let active = config.active_provider_models();
    let mut roles: Vec<ProviderRoleMetrics> = Vec::new();
    for (name, path) in files {
        match fs::metadata(&path) {
            Ok(metadata) if metadata.is_file() => {}
            Ok(_) => {
                warn_unusable(&path, "not a regular file");
                continue;
            }
            Err(error) => {
                warn_unusable(&path, &error.to_string());
                continue;
            }
        }
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) => {
                warn_unusable(&path, &error.to_string());
                continue;
            }
        };
        let record: TrajectoryRecord = match serde_json::from_slice(&bytes) {
            Ok(record) => record,
            Err(error) => {
                warn_unusable(&path, &format!("{name}: {error}"));
                continue;
            }
        };
        let Some(role) = record.role.as_deref().and_then(canonical_role) else {
            warn_unusable(&path, "no canonical provider role");
            continue;
        };
        if record
            .model
            .as_deref()
            .map(str::trim)
            .filter(|model| !model.is_empty())
            .is_none()
        {
            warn_unusable(&path, "no provider model");
            continue;
        }
        let Some((_, configured_model)) = active.iter().find(|(active, _)| *active == role) else {
            warn_unusable(&path, "the role is not active in the configured mode");
            continue;
        };

        let index = match roles.iter().position(|existing| existing.role == role) {
            Some(index) => index,
            None => {
                roles.push(ProviderRoleMetrics {
                    role: role.to_string(),
                    model: (*configured_model).to_string(),
                    ..ProviderRoleMetrics::default()
                });
                roles.len() - 1
            }
        };
        roles[index].add(&record);
    }

    for metrics in &mut roles {
        metrics.price(config);
    }
    Ok(roles)
}

/// Resolve one canonical role's configured rates through the configuration's
/// own phase routing, so pricing can never borrow another role's rates.
///
/// A role that no phase currently routes (a stale trajectory from another
/// mode) has no configured rates and therefore stays unpriced.
fn role_rates(config: &Config, role: &str) -> (Option<f64>, Option<f64>, Option<f64>) {
    if !config.has_provider_role(role) {
        return (None, None, None);
    }
    let phase = ROUTE_PHASES
        .iter()
        .copied()
        .find(|phase| config.provider_role(*phase) == role);
    match phase {
        Some(phase) => (
            config.input_price(phase),
            config.cached_input_price(phase),
            config.output_price(phase),
        ),
        None => (None, None, None),
    }
}

/// One trajectory file's fields that contribute to the metrics rollup. Every
/// field defaults, so telemetry an older trajectory omitted simply adds zero.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct TrajectoryRecord {
    role: Option<String>,
    model: Option<String>,
    steps_taken: u64,
    telemetry: TrajectoryTelemetry,
}

/// The trajectory telemetry counters and usage that the rollup consumes.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct TrajectoryTelemetry {
    provider_send_attempts: u64,
    transient_retries: u64,
    token_usage: TrajectoryUsage,
}

/// A permissive copy of the provider token counters, defaulted field by field.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct TrajectoryUsage {
    input_tokens: u64,
    cached_input_tokens: u64,
    output_tokens: u64,
    reasoning_tokens: u64,
}

impl From<TrajectoryUsage> for TokenUsage {
    fn from(usage: TrajectoryUsage) -> Self {
        TokenUsage {
            input_tokens: usage.input_tokens,
            cached_input_tokens: usage.cached_input_tokens,
            output_tokens: usage.output_tokens,
            reasoning_tokens: usage.reasoning_tokens,
        }
    }
}

/// The canonical provider-role name a trajectory carries, or None when the
/// value is not one of the three roles the configuration routes.
fn canonical_role(raw: &str) -> Option<&'static str> {
    match raw.trim() {
        "mid" => Some("mid"),
        "giga" => Some("giga"),
        "smoll" => Some("smoll"),
        _ => None,
    }
}

/// Report one unusable trajectory entry without letting the diagnostic itself
/// fail the best-effort aggregation.
fn warn_unusable(path: &Path, reason: &str) {
    let _ = crate::status::shared().stderr_log(&format!(
        "[WARN] skipping unusable trajectory {}: {reason}",
        path.display()
    ));
}

/// Persist metrics as pretty JSON at the top of dir, but only when the
/// resolved directory lies beneath an exact .ejaj path component.
///
/// Returns whether a file was written. A lookalike such as .ejaj-extra is an
/// ordinary destination and is left alone.
pub fn persist_run_metrics(root: &Path, dir: &Path, metrics: &RunMetrics) -> io::Result<bool> {
    let destination = if dir.is_absolute() {
        dir.to_path_buf()
    } else {
        root.join(dir)
    };
    if crate::tools::ejaj_ancestor(&destination).is_none() {
        return Ok(false);
    }
    fs::create_dir_all(&destination)?;
    let payload = serde_json::to_vec_pretty(metrics)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    fs::write(destination.join(METRICS_FILE_NAME), payload)?;
    Ok(true)
}

/// Render a deterministic offline Markdown scorecard over the direct child
/// session directories of one corpus.
///
/// Only direct child directories are considered, in lexical order. A session
/// without a metrics document is ignored; a present document that cannot be
/// read, is not a regular file, does not decode, or does not carry the current
/// schema version is counted once as skipped so every usable sibling still
/// contributes. A not-found corpus root renders the same valid zero-run report
/// as an empty one, while any other corpus-root read error is returned.
pub fn render_corpus_report(dir: &Path) -> io::Result<String> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(CorpusAccumulator::default().render());
        }
        Err(error) => return Err(error),
    };

    let mut sessions = Vec::new();
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            sessions.push((entry.file_name(), entry.path()));
        }
    }
    sessions.sort_by(|left, right| left.0.cmp(&right.0));

    let mut accumulator = CorpusAccumulator::default();
    for (_, session) in sessions {
        match read_session_metrics(&session) {
            SessionMetrics::Usable(metrics) => accumulator.add(&metrics),
            SessionMetrics::Absent => {}
            SessionMetrics::Unusable => {
                accumulator.skipped_files = accumulator.skipped_files.saturating_add(1);
            }
        }
    }
    Ok(accumulator.render())
}

/// One direct child session directory's metrics-document disposition.
enum SessionMetrics {
    /// The session has no metrics document, so it is neither analyzed nor
    /// skipped.
    Absent,
    /// The session's present metrics document cannot be analyzed.
    Unusable,
    /// The session's present metrics document is a usable version-2 run.
    Usable(Box<RunMetrics>),
}

/// Read one session's metrics document and classify it without ever aborting
/// the surrounding corpus scan.
fn read_session_metrics(session: &Path) -> SessionMetrics {
    let path = session.join(METRICS_FILE_NAME);
    let presence = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return SessionMetrics::Absent,
        Err(_) => return SessionMetrics::Unusable,
    };
    let metadata = if presence.file_type().is_symlink() {
        match fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(_) => return SessionMetrics::Unusable,
        }
    } else {
        presence
    };
    if !metadata.is_file() {
        return SessionMetrics::Unusable;
    }
    let Ok(bytes) = fs::read(&path) else {
        return SessionMetrics::Unusable;
    };
    let metrics: RunMetrics = match serde_json::from_slice(&bytes) {
        Ok(metrics) => metrics,
        Err(_) => return SessionMetrics::Unusable,
    };
    if metrics.schema_version != METRICS_SCHEMA_VERSION {
        return SessionMetrics::Unusable;
    }
    SessionMetrics::Usable(Box::new(metrics))
}

/// One mode's analyzed-run totals.
#[derive(Default)]
struct ModeAccumulator {
    runs: u64,
    completed: u64,
}

/// One provider role's corpus-wide counters, tokens and priced total.
#[derive(Default)]
struct RoleAccumulator {
    invocations: u64,
    steps: u64,
    http_requests: u64,
    http_retries: u64,
    usage: TokenUsage,
    estimated_cost_usd: f64,
    priced: bool,
}

/// One phase's fresh-session retry attempts and their successful outcomes.
#[derive(Default)]
struct FreshRetryAccumulator {
    reached: u64,
    retried: u64,
    succeeded: u64,
    succeeded_after_retry: u64,
}

impl FreshRetryAccumulator {
    /// Fold one reached pass's retry and final success outcome.
    fn add_pass(&mut self, retried: bool, succeeded: bool) {
        self.reached = self.reached.saturating_add(1);
        if retried {
            self.retried = self.retried.saturating_add(1);
            if succeeded {
                self.succeeded_after_retry = self.succeeded_after_retry.saturating_add(1);
            }
        }
        if succeeded {
            self.succeeded = self.succeeded.saturating_add(1);
        }
    }
}

/// Corpus-wide queue loop and tool-friction totals.
#[derive(Default)]
struct LoopFrictionAccumulator {
    elapsed_ms: u64,
    bash_calls: u64,
    bash_failures: u64,
    bash_timeouts: u64,
    bash_output_spills: u64,
    script_calls: u64,
    script_failures: u64,
    script_timeouts: u64,
    rejected_control_calls: u64,
    rejected_tool_calls: u64,
    tier1_compactions: u64,
    tier2_summaries: u64,
    tier2_summary_failures: u64,
    overflow_recoveries: u64,
    truncation_recoveries: u64,
    repeated_turn_warnings: u64,
    repeated_turn_aborts: u64,
}

impl LoopFrictionAccumulator {
    /// Fold one ticket's loop telemetry into the corpus totals, saturating
    /// every counter.
    fn add(&mut self, telemetry: &crate::loops::LoopTelemetry) {
        self.elapsed_ms = self.elapsed_ms.saturating_add(telemetry.elapsed_ms);
        self.bash_calls = self.bash_calls.saturating_add(telemetry.bash_calls);
        self.bash_failures = self.bash_failures.saturating_add(telemetry.bash_failures);
        self.bash_timeouts = self.bash_timeouts.saturating_add(telemetry.bash_timeouts);
        self.bash_output_spills = self
            .bash_output_spills
            .saturating_add(telemetry.bash_output_spills);
        self.script_calls = self
            .script_calls
            .saturating_add(telemetry.execute_script_calls);
        self.script_failures = self
            .script_failures
            .saturating_add(telemetry.execute_script_failures);
        self.script_timeouts = self
            .script_timeouts
            .saturating_add(telemetry.execute_script_timeouts);
        self.rejected_control_calls = self
            .rejected_control_calls
            .saturating_add(telemetry.rejected_control_tool_calls);
        self.rejected_tool_calls = self
            .rejected_tool_calls
            .saturating_add(telemetry.rejected_other_tool_calls);
        self.tier1_compactions = self
            .tier1_compactions
            .saturating_add(telemetry.tier1_compactions);
        self.tier2_summaries = self
            .tier2_summaries
            .saturating_add(telemetry.tier2_summaries_accepted);
        self.tier2_summary_failures = self
            .tier2_summary_failures
            .saturating_add(telemetry.tier2_summary_failures);
        self.overflow_recoveries = self
            .overflow_recoveries
            .saturating_add(telemetry.overflow_compaction_recoveries);
        self.truncation_recoveries = self
            .truncation_recoveries
            .saturating_add(telemetry.truncation_recoveries);
        self.repeated_turn_warnings = self
            .repeated_turn_warnings
            .saturating_add(telemetry.repeated_tool_warnings);
        self.repeated_turn_aborts = self
            .repeated_turn_aborts
            .saturating_add(telemetry.repeated_tool_aborts);
    }
}

/// The private corpus aggregation state. Only the rendered Markdown leaves
/// this module, so every accumulator stays an implementation detail.
#[derive(Default)]
struct CorpusAccumulator {
    analyzed_runs: u64,
    skipped_files: usize,
    modes: BTreeMap<String, ModeAccumulator>,
    elapsed_total_ms: u64,
    completed_runs: u64,
    completed_elapsed_ms: u64,
    priced_runs: u64,
    estimated_cost_usd: f64,
    priced_completed_tickets: u64,
    terminal_outcomes: BTreeMap<String, u64>,
    provider_roles: BTreeMap<String, RoleAccumulator>,
    ticket_records: u64,
    attempts: u64,
    first_pass_completions: u64,
    retried_tickets: u64,
    rescued_tickets: u64,
    first_attempt_failures: u64,
    first_attempt_failure_categories: BTreeMap<String, u64>,
    escalations: u64,
    rescued_escalations: u64,
    triage_runs: u64,
    triage_succeeded: u64,
    triage_retried: u64,
    triage_outcomes: BTreeMap<String, u64>,
    drafts: u64,
    ready: u64,
    done: u64,
    blocked: u64,
    queue_authorized: u64,
    queue_unauthorized: u64,
    spec_retries: FreshRetryAccumulator,
    tickets_retries: FreshRetryAccumulator,
    triage_retries: FreshRetryAccumulator,
    review_retries: BTreeMap<u64, FreshRetryAccumulator>,
    gate_runs: u64,
    gate_failures: u64,
    gate_elapsed_ms: u64,
    review_rounds: u64,
    review_stalls: u64,
    review_outcomes: BTreeMap<String, u64>,
    summary_runs: u64,
    summary_accepted: u64,
    summary_pass_at_1: u64,
    summary_with_candidates: u64,
    summary_outcomes: BTreeMap<String, u64>,
    rejection_categories: BTreeMap<String, u64>,
    friction: LoopFrictionAccumulator,
}

impl CorpusAccumulator {
    /// Fold one analyzed version-2 run into every corpus dimension with
    /// saturating integer addition.
    fn add(&mut self, metrics: &RunMetrics) {
        self.analyzed_runs = self.analyzed_runs.saturating_add(1);
        self.elapsed_total_ms = self.elapsed_total_ms.saturating_add(metrics.elapsed_ms);
        let completed = metrics.terminal_outcome == TerminalOutcome::Complete;
        if completed {
            self.completed_runs = self.completed_runs.saturating_add(1);
            self.completed_elapsed_ms =
                self.completed_elapsed_ms.saturating_add(metrics.elapsed_ms);
        }
        let mode = self.modes.entry(metrics.mode.clone()).or_default();
        mode.runs = mode.runs.saturating_add(1);
        if completed {
            mode.completed = mode.completed.saturating_add(1);
        }
        bump(&mut self.terminal_outcomes, terminal_label(metrics));
        if let Some(cost) = metrics.estimated_cost_usd {
            self.priced_runs = self.priced_runs.saturating_add(1);
            self.estimated_cost_usd += cost;
            let completed_tickets = u64::try_from(
                metrics
                    .queue_rounds
                    .iter()
                    .flat_map(|round| round.metrics.tickets.iter())
                    .filter(|ticket| ticket.final_status == TicketStatusTag::Completed)
                    .count(),
            )
            .unwrap_or(u64::MAX);
            self.priced_completed_tickets = self
                .priced_completed_tickets
                .saturating_add(completed_tickets);
        }
        for role in &metrics.provider_roles {
            self.add_role(role);
        }
        self.add_queue(metrics);
        self.add_triage(metrics);
        self.add_fresh_retries(metrics);
        self.add_transition_gate(metrics);
        self.add_review(metrics);
        self.add_summary(metrics);
    }

    /// Fold one provider role's counters, tokens and priced estimate.
    fn add_role(&mut self, role: &ProviderRoleMetrics) {
        let entry = self.provider_roles.entry(role.role.clone()).or_default();
        entry.invocations = entry.invocations.saturating_add(role.invocations);
        entry.steps = entry.steps.saturating_add(role.steps);
        entry.http_requests = entry.http_requests.saturating_add(role.http_requests);
        entry.http_retries = entry.http_retries.saturating_add(role.http_retries);
        entry.usage = entry.usage.saturating_add(role.usage);
        if let Some(cost) = role.estimated_cost_usd {
            entry.estimated_cost_usd += cost;
            entry.priced = true;
        }
    }

    /// Fold every ticket record of every queue round into the funnel and loop
    /// friction totals.
    fn add_queue(&mut self, metrics: &RunMetrics) {
        for round in &metrics.queue_rounds {
            for ticket in &round.metrics.tickets {
                self.ticket_records = self.ticket_records.saturating_add(1);
                self.attempts = self.attempts.saturating_add(ticket.attempt_count);
                let completed = ticket.final_status == TicketStatusTag::Completed;
                if completed && ticket.attempt_count == 1 {
                    self.first_pass_completions = self.first_pass_completions.saturating_add(1);
                }
                if ticket.attempt_count > 1 {
                    self.retried_tickets = self.retried_tickets.saturating_add(1);
                    if completed && ticket.attempt_count == 2 {
                        self.rescued_tickets = self.rescued_tickets.saturating_add(1);
                    }
                }
                let failed_first_attempt = ticket.attempt_count > 0
                    && (ticket.attempt_count > 1
                        || ticket.first_attempt_outcome != Some(AttemptOutcomeTag::Complete));
                if failed_first_attempt {
                    self.first_attempt_failures = self.first_attempt_failures.saturating_add(1);
                    let category = ticket
                        .first_attempt_outcome
                        .map(attempt_outcome_label)
                        .unwrap_or("unknown");
                    bump(
                        &mut self.first_attempt_failure_categories,
                        category.to_string(),
                    );
                }
                if ticket.retry_escalated {
                    self.escalations = self.escalations.saturating_add(1);
                    if completed {
                        self.rescued_escalations = self.rescued_escalations.saturating_add(1);
                    }
                }
                self.friction.add(&ticket.telemetry);
            }
        }
    }

    /// Fold the triage pass's outcomes and report-derived counts.
    fn add_triage(&mut self, metrics: &RunMetrics) {
        let Some(triage) = &metrics.triage else {
            return;
        };
        self.triage_runs = self.triage_runs.saturating_add(1);
        if triage.pass.succeeded {
            self.triage_succeeded = self.triage_succeeded.saturating_add(1);
        }
        if triage.pass.retried {
            self.triage_retried = self.triage_retried.saturating_add(1);
        }
        bump(
            &mut self.triage_outcomes,
            pass_outcome_label(triage.pass.outcome).to_string(),
        );
        self.drafts = self.drafts.saturating_add(triage.draft_count.unwrap_or(0));
        self.ready = self.ready.saturating_add(triage.ready_count.unwrap_or(0));
        self.done = self.done.saturating_add(triage.done_count.unwrap_or(0));
        self.blocked = self
            .blocked
            .saturating_add(triage.blocked_count.unwrap_or(0));
        match triage.queue_authorized {
            Some(true) => self.queue_authorized = self.queue_authorized.saturating_add(1),
            Some(false) => {
                self.queue_unauthorized = self.queue_unauthorized.saturating_add(1);
            }
            None => {}
        }
    }

    /// Fold each reached phase's fresh-session retry outcome.
    fn add_fresh_retries(&mut self, metrics: &RunMetrics) {
        if let Some(pass) = &metrics.spec_pass {
            self.spec_retries.add_pass(pass.retried, pass.succeeded);
        }
        if let Some(pass) = &metrics.tickets_pass {
            self.tickets_retries.add_pass(pass.retried, pass.succeeded);
        }
        if let Some(triage) = &metrics.triage {
            self.triage_retries
                .add_pass(triage.pass.retried, triage.pass.succeeded);
        }
        for round in &metrics.review_rounds {
            self.review_retries
                .entry(round.round)
                .or_default()
                .add_pass(round.retried, round.succeeded);
        }
    }

    /// Fold the transition-gate aggregate.
    fn add_transition_gate(&mut self, metrics: &RunMetrics) {
        self.gate_runs = self.gate_runs.saturating_add(metrics.transition_gate.runs);
        self.gate_failures = self
            .gate_failures
            .saturating_add(metrics.transition_gate.failures);
        self.gate_elapsed_ms = self
            .gate_elapsed_ms
            .saturating_add(metrics.transition_gate.elapsed_ms);
    }

    /// Fold every review round's convergence outcome.
    fn add_review(&mut self, metrics: &RunMetrics) {
        let rounds = u64::try_from(metrics.review_rounds.len()).unwrap_or(u64::MAX);
        self.review_rounds = self.review_rounds.saturating_add(rounds);
        for round in &metrics.review_rounds {
            if matches!(
                round.outcome,
                ReviewRoundOutcome::CircuitBroken | ReviewRoundOutcome::RoundCapReached
            ) {
                self.review_stalls = self.review_stalls.saturating_add(1);
            }
            bump(
                &mut self.review_outcomes,
                review_outcome_label(round.outcome).to_string(),
            );
        }
    }

    /// Fold the summary pass's outcome and rejected-candidate categories.
    fn add_summary(&mut self, metrics: &RunMetrics) {
        let Some(summary) = &metrics.summary else {
            return;
        };
        self.summary_runs = self.summary_runs.saturating_add(1);
        if summary.accepted {
            self.summary_accepted = self.summary_accepted.saturating_add(1);
        }
        if summary.candidates_generated > 0 {
            self.summary_with_candidates = self.summary_with_candidates.saturating_add(1);
        }
        if summary.accepted && summary.candidates_generated == 1 {
            self.summary_pass_at_1 = self.summary_pass_at_1.saturating_add(1);
        }
        bump(
            &mut self.summary_outcomes,
            summary_outcome_label(summary.outcome).to_string(),
        );
        for category in &summary.rejected_candidates {
            bump(
                &mut self.rejection_categories,
                rejection_label(*category).to_string(),
            );
        }
    }

    /// Render the complete scorecard with fixed section order and exactly one
    /// final newline.
    fn render(&self) -> String {
        let mut lines = Vec::new();
        lines.push(format!(
            "{} runs analyzed, {} metrics files skipped",
            self.analyzed_runs, self.skipped_files
        ));
        lines.push(String::new());
        lines.push("# ejaj Execution Metrics".to_string());
        self.render_modes(&mut lines);
        self.render_elapsed_and_cost(&mut lines);
        self.render_terminal_outcomes(&mut lines);
        self.render_provider_roles(&mut lines);
        self.render_queue_funnel(&mut lines);
        self.render_triage(&mut lines);
        self.render_transition_gate(&mut lines);
        self.render_review_and_summary(&mut lines);
        self.render_friction(&mut lines);
        let mut report = lines.join("\n");
        report.push('\n');
        report
    }

    /// Render per-mode run totals and completion rates in lexical mode order.
    fn render_modes(&self, lines: &mut Vec<String>) {
        push_heading(lines, "## Mode Summary");
        lines.push("| Mode | Runs | Completed | Completion Rate |".to_string());
        lines.push("| --- | --- | --- | --- |".to_string());
        for (mode, stats) in &self.modes {
            lines.push(format!(
                "| {mode} | {} | {} | {} |",
                stats.runs,
                stats.completed,
                completion_rate(stats.completed, stats.runs)
            ));
        }
    }

    /// Render elapsed totals, means and priced-run cost figures.
    fn render_elapsed_and_cost(&self, lines: &mut Vec<String>) {
        push_heading(lines, "## Elapsed & Cost");
        lines.push(format!("- Total elapsed: {} ms", self.elapsed_total_ms));
        lines.push(format!(
            "- Mean elapsed per analyzed run: {}",
            mean_ms(self.elapsed_total_ms, self.analyzed_runs)
        ));
        lines.push(format!(
            "- Mean elapsed per completed run: {}",
            mean_ms(self.completed_elapsed_ms, self.completed_runs)
        ));
        lines.push(format!("- Priced runs: {}", self.priced_runs));
        lines.push(format!(
            "- Estimated cost: {}",
            cost_total(self.estimated_cost_usd, self.priced_runs > 0)
        ));
        lines.push(format!(
            "- Mean cost per completed ticket: {}",
            mean_cost(self.estimated_cost_usd, self.priced_completed_tickets)
        ));
    }

    /// Render terminal-outcome labels in lexical order.
    fn render_terminal_outcomes(&self, lines: &mut Vec<String>) {
        push_heading(lines, "## Terminal Outcomes");
        lines.push("| Outcome | Runs |".to_string());
        lines.push("| --- | --- |".to_string());
        for (outcome, runs) in &self.terminal_outcomes {
            lines.push(format!("| {outcome} | {runs} |"));
        }
    }

    /// Render provider-role counters, tokens, cache hit rates and cost totals
    /// in lexical role order.
    fn render_provider_roles(&self, lines: &mut Vec<String>) {
        push_heading(lines, "## Provider Roles");
        lines.push(
            "| Role | Invocations | Steps | HTTP Requests | HTTP Retries | Input Tokens | Cached Input Tokens | Output Tokens | Reasoning Tokens | Cache Hit Rate | Estimated Cost |"
                .to_string(),
        );
        lines.push(
            "| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |".to_string(),
        );
        for (role, stats) in &self.provider_roles {
            lines.push(format!(
                "| {role} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
                stats.invocations,
                stats.steps,
                stats.http_requests,
                stats.http_retries,
                stats.usage.input_tokens,
                stats.usage.cached_input_tokens,
                stats.usage.output_tokens,
                stats.usage.reasoning_tokens,
                cache_hit_rate(&stats.usage),
                cost_total(stats.estimated_cost_usd, stats.priced)
            ));
        }
    }

    /// Render the queue ticket, attempt, rescue, failure and escalation funnel.
    fn render_queue_funnel(&self, lines: &mut Vec<String>) {
        push_heading(lines, "## Queue Funnel");
        for line in [
            format!("- Ticket records: {}", self.ticket_records),
            format!("- Attempts: {}", self.attempts),
            format!("- First-pass completions: {}", self.first_pass_completions),
            format!("- Retried tickets: {}", self.retried_tickets),
            format!("- Rescued tickets: {}", self.rescued_tickets),
            format!("- First-attempt failures: {}", self.first_attempt_failures),
            format!("- Escalations: {}", self.escalations),
            format!("- Rescued escalations: {}", self.rescued_escalations),
        ] {
            lines.push(line);
        }
        lines.push(String::new());
        lines.push("| First Attempt Outcome | Failures |".to_string());
        lines.push("| --- | --- |".to_string());
        for (outcome, failures) in &self.first_attempt_failure_categories {
            lines.push(format!("| {outcome} | {failures} |"));
        }
    }

    /// Render the triage outcomes and fresh-session retry outcomes.
    fn render_triage(&self, lines: &mut Vec<String>) {
        push_heading(lines, "## Triage & Fresh-Session Retries");
        for line in [
            format!("- Triage runs: {}", self.triage_runs),
            format!("- Triage succeeded: {}", self.triage_succeeded),
            format!("- Triage retried: {}", self.triage_retried),
            format!("- Drafts: {}", self.drafts),
            format!("- Ready: {}", self.ready),
            format!("- Done: {}", self.done),
            format!("- Blocked: {}", self.blocked),
            format!("- Queue authorized: {}", self.queue_authorized),
            format!("- Queue unauthorized: {}", self.queue_unauthorized),
        ] {
            lines.push(line);
        }
        lines.push(String::new());
        lines.push("| Triage Outcome | Runs |".to_string());
        lines.push("| --- | --- |".to_string());
        for (outcome, runs) in &self.triage_outcomes {
            lines.push(format!("| {outcome} | {runs} |"));
        }
        lines.push(String::new());
        lines.push("| Phase | Reached | Retried | Succeeded | Succeeded After Retry |".to_string());
        lines.push("| --- | --- | --- | --- | --- |".to_string());
        for (phase, stats) in [
            ("spec", &self.spec_retries),
            ("tickets", &self.tickets_retries),
            ("triage", &self.triage_retries),
        ] {
            lines.push(fresh_retry_row(phase, stats));
        }
        for (round, stats) in &self.review_retries {
            lines.push(fresh_retry_row(&format!("review {round}"), stats));
        }
    }

    /// Render the transition-gate run, failure and elapsed totals.
    fn render_transition_gate(&self, lines: &mut Vec<String>) {
        push_heading(lines, "## Transition Gate");
        for line in [
            format!("- Gate runs: {}", self.gate_runs),
            format!("- Gate failures: {}", self.gate_failures),
            format!("- Gate elapsed: {} ms", self.gate_elapsed_ms),
        ] {
            lines.push(line);
        }
    }

    /// Render review convergence and stall outcomes plus summary outcomes and
    /// rejection categories.
    fn render_review_and_summary(&self, lines: &mut Vec<String>) {
        push_heading(lines, "## Review & Summary");
        lines.push(format!("- Review rounds: {}", self.review_rounds));
        lines.push(format!("- Review stalls: {}", self.review_stalls));
        lines.push(String::new());
        lines.push("| Review Outcome | Rounds |".to_string());
        lines.push("| --- | --- |".to_string());
        for (outcome, rounds) in &self.review_outcomes {
            lines.push(format!("| {outcome} | {rounds} |"));
        }
        lines.push(format!("- Summary runs: {}", self.summary_runs));
        lines.push(format!("- Summaries accepted: {}", self.summary_accepted));
        lines.push(format!(
            "- Summary pass@1: {}/{} ({})",
            self.summary_pass_at_1,
            self.summary_with_candidates,
            completion_rate(self.summary_pass_at_1, self.summary_with_candidates)
        ));
        lines.push(String::new());
        lines.push("| Summary Outcome | Runs |".to_string());
        lines.push("| --- | --- |".to_string());
        for (outcome, runs) in &self.summary_outcomes {
            lines.push(format!("| {outcome} | {runs} |"));
        }
        lines.push(String::new());
        lines.push("| Summary Rejection | Candidates |".to_string());
        lines.push("| --- | --- |".to_string());
        for (category, candidates) in &self.rejection_categories {
            lines.push(format!("| {category} | {candidates} |"));
        }
    }

    /// Render the queue loop and tool-friction totals.
    fn render_friction(&self, lines: &mut Vec<String>) {
        let friction = &self.friction;
        push_heading(lines, "## Queue Loop & Tool Friction");
        for line in [
            format!("- Bash calls: {}", friction.bash_calls),
            format!("- Bash failures: {}", friction.bash_failures),
            format!("- Bash timeouts: {}", friction.bash_timeouts),
            format!("- Bash output spills: {}", friction.bash_output_spills),
            format!("- Script calls: {}", friction.script_calls),
            format!("- Script failures: {}", friction.script_failures),
            format!("- Script timeouts: {}", friction.script_timeouts),
            format!(
                "- Rejected control calls: {}",
                friction.rejected_control_calls
            ),
            format!("- Rejected tool calls: {}", friction.rejected_tool_calls),
            format!("- Tier 1 compactions: {}", friction.tier1_compactions),
            format!("- Tier 2 summaries: {}", friction.tier2_summaries),
            format!(
                "- Tier 2 summary failures: {}",
                friction.tier2_summary_failures
            ),
            format!("- Overflow recoveries: {}", friction.overflow_recoveries),
            format!(
                "- Truncation recoveries: {}",
                friction.truncation_recoveries
            ),
            format!(
                "- Repeated-turn warnings: {}",
                friction.repeated_turn_warnings
            ),
            format!("- Repeated-turn aborts: {}", friction.repeated_turn_aborts),
            format!("- Loop elapsed: {} ms", friction.elapsed_ms),
        ] {
            lines.push(line);
        }
    }
}

/// Append one blank-separated level-two heading.
fn push_heading(lines: &mut Vec<String>, heading: &str) {
    lines.push(String::new());
    lines.push(heading.to_string());
    lines.push(String::new());
}

/// One fresh-session table row in the declared column order.
fn fresh_retry_row(phase: &str, stats: &FreshRetryAccumulator) -> String {
    format!(
        "| {phase} | {} | {} | {} | {} |",
        stats.reached, stats.retried, stats.succeeded, stats.succeeded_after_retry
    )
}

/// Saturating increment of one map-backed counter.
fn bump(map: &mut BTreeMap<String, u64>, key: String) {
    let slot = map.entry(key).or_insert(0);
    *slot = slot.saturating_add(1);
}

/// The stable terminal-outcome label, including the explanatory detail when
/// the run carries one.
fn terminal_label(metrics: &RunMetrics) -> String {
    let (base, fallback) = match metrics.terminal_outcome {
        TerminalOutcome::Complete => return "complete".to_string(),
        TerminalOutcome::Retry => ("retry", "unspecified"),
        TerminalOutcome::WriteFailed => ("write_failed", "unspecified"),
        TerminalOutcome::SummaryFailed => ("summary_failed", "summary"),
        TerminalOutcome::GitFailed => ("git_failed", "unspecified"),
    };
    let detail = metrics
        .terminal_detail
        .as_deref()
        .map(str::trim)
        .filter(|detail| !detail.is_empty())
        .map(|detail| detail.replace(['\n', '\r'], " ").replace('|', "/"));
    format!("{base}: {}", detail.as_deref().unwrap_or(fallback))
}

/// One attempt outcome's stable wire label.
fn attempt_outcome_label(outcome: AttemptOutcomeTag) -> &'static str {
    match outcome {
        AttemptOutcomeTag::Complete => "complete",
        AttemptOutcomeTag::TurnEnd => "turn_end",
        AttemptOutcomeTag::FailedApi => "failed_api",
        AttemptOutcomeTag::FailedTransport => "failed_transport",
        AttemptOutcomeTag::FailedMalformed => "failed_malformed",
        AttemptOutcomeTag::FailedStepLimit => "failed_step_limit",
        AttemptOutcomeTag::FailedContextGuard => "failed_context_guard",
    }
}

/// One triage pass outcome's stable wire label.
fn pass_outcome_label(outcome: PassOutcome) -> &'static str {
    match outcome {
        PassOutcome::Handoff => "handoff",
        PassOutcome::TurnEnd => "turn_end",
        PassOutcome::FailedApi => "failed_api",
        PassOutcome::FailedTransport => "failed_transport",
        PassOutcome::FailedMalformed => "failed_malformed",
        PassOutcome::FailedStepLimit => "failed_step_limit",
        PassOutcome::FailedContextGuard => "failed_context_guard",
    }
}

/// One review round's stable outcome label.
fn review_outcome_label(outcome: ReviewRoundOutcome) -> &'static str {
    match outcome {
        ReviewRoundOutcome::Passed => "passed",
        ReviewRoundOutcome::FixesQueued => "fixes_queued",
        ReviewRoundOutcome::TurnEnd => "turn_end",
        ReviewRoundOutcome::Failed => "failed",
        ReviewRoundOutcome::CircuitBroken => "circuit_broken",
        ReviewRoundOutcome::RoundCapReached => "round_cap_reached",
    }
}

/// One summary pass's stable outcome label.
fn summary_outcome_label(outcome: SummaryOutcome) -> &'static str {
    match outcome {
        SummaryOutcome::Accepted => "accepted",
        SummaryOutcome::Invalid => "invalid",
        SummaryOutcome::FailedApi => "failed_api",
        SummaryOutcome::FailedTransport => "failed_transport",
        SummaryOutcome::FailedMalformed => "failed_malformed",
        SummaryOutcome::FailedStepLimit => "failed_step_limit",
        SummaryOutcome::FailedContextGuard => "failed_context_guard",
    }
}

/// One summary-rejection category's stable wire label.
fn rejection_label(category: SummaryRejectionCategory) -> &'static str {
    match category {
        SummaryRejectionCategory::Empty => "empty",
        SummaryRejectionCategory::MarkdownFence => "markdown_fence",
        SummaryRejectionCategory::GeneratedTicketId => "generated_ticket_id",
        SummaryRejectionCategory::RefsFooter => "refs_footer",
        SummaryRejectionCategory::EmptyBody => "empty_body",
        SummaryRejectionCategory::InvalidFooter => "invalid_footer",
        SummaryRejectionCategory::MissingBlankLine => "missing_blank_line",
        SummaryRejectionCategory::LineOverLimit => "line_over_limit",
        SummaryRejectionCategory::InvalidHeaderOrPreamble => "invalid_header_or_preamble",
    }
}

/// A completion rate with one decimal place, or an em dash when the
/// denominator is zero.
fn completion_rate(completed: u64, runs: u64) -> String {
    if runs == 0 {
        "—".to_string()
    } else {
        format!("{:.1}%", completed as f64 * 100.0 / runs as f64)
    }
}

/// A mean whole-millisecond duration with one decimal place, or an em dash
/// when undefined. The quotient is carried in integer tenths so an upper-limit
/// total keeps every digit instead of rounding through f64.
fn mean_ms(total: u64, count: u64) -> String {
    if count == 0 {
        return "—".to_string();
    }
    let tenths = (u128::from(total) * 10 + u128::from(count) / 2) / u128::from(count);
    format!("{}.{} ms", tenths / 10, tenths % 10)
}

/// A cache hit rate with one decimal place, or an em dash when the role
/// reported no input tokens.
fn cache_hit_rate(usage: &TokenUsage) -> String {
    if usage.input_tokens == 0 {
        "—".to_string()
    } else {
        format!(
            "{:.1}%",
            usage.cached_input_tokens as f64 * 100.0 / usage.input_tokens as f64
        )
    }
}

/// A six-decimal USD total, or an em dash when no run was priced.
fn cost_total(total: f64, priced: bool) -> String {
    if priced {
        format!("${total:.6}")
    } else {
        "—".to_string()
    }
}

/// A six-decimal mean cost over completed tickets, or an em dash when there
/// is no completed ticket to divide by.
fn mean_cost(total: f64, completed_tickets: u64) -> String {
    if completed_tickets == 0 {
        "—".to_string()
    } else {
        format!("${:.6}", total / completed_tickets as f64)
    }
}

/// Classify one summary canonicalizer rejection reason into its stable
/// category.
pub fn classify_summary_rejection(reason: &str) -> SummaryRejectionCategory {
    if reason.contains("answer is empty") {
        return SummaryRejectionCategory::Empty;
    }
    if reason.contains("Markdown fence") {
        return SummaryRejectionCategory::MarkdownFence;
    }
    if reason.contains("generated ticket id") {
        return SummaryRejectionCategory::GeneratedTicketId;
    }
    if reason.contains("Refs:") {
        return SummaryRejectionCategory::RefsFooter;
    }
    if reason.contains("no body") || reason.contains("non-empty body") {
        return SummaryRejectionCategory::EmptyBody;
    }
    // The invalid-footer reason names the surrounding blank line too, so the
    // footer check has to come before the missing-blank-line check.
    if reason.contains("footer") {
        return SummaryRejectionCategory::InvalidFooter;
    }
    if reason.contains("blank line") {
        return SummaryRejectionCategory::MissingBlankLine;
    }
    if reason.contains("body line is")
        || reason.contains("characters; the limit")
        || reason.contains("characters long")
        || (reason.contains("the description is") && reason.contains("characters"))
    {
        return SummaryRejectionCategory::LineOverLimit;
    }
    SummaryRejectionCategory::InvalidHeaderOrPreamble
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loops::LoopTelemetry;
    use crate::orchestrator::{AttemptOutcomeTag, TicketExecutionMetrics, TicketStatusTag};
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A fresh scratch directory for one test, unique to the process and case.
    fn scratch_dir(label: &str) -> PathBuf {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "ejaj-metrics-test-{}-{id}-{label}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("the scratch directory should be creatable");
        dir
    }

    /// The fixed scorecard section order every rendered report preserves.
    const REPORT_SECTIONS: [&str; 10] = [
        "# ejaj Execution Metrics",
        "## Mode Summary",
        "## Elapsed & Cost",
        "## Terminal Outcomes",
        "## Provider Roles",
        "## Queue Funnel",
        "## Triage & Fresh-Session Retries",
        "## Transition Gate",
        "## Review & Summary",
        "## Queue Loop & Tool Friction",
    ];

    /// Assert every fixed section appears once and in the fixed order.
    fn assert_section_order(report: &str) {
        let mut previous = None;
        for heading in REPORT_SECTIONS {
            let index = report
                .find(heading)
                .unwrap_or_else(|| panic!("the report must contain the {heading} section"));
            if let Some(previous) = previous {
                assert!(
                    index > previous,
                    "{heading} must follow the previous section in the fixed order"
                );
            }
            previous = Some(index);
        }
    }

    /// The report text from one heading up to the next level-two heading.
    fn report_section<'a>(report: &'a str, heading: &str) -> &'a str {
        let start = report
            .find(heading)
            .unwrap_or_else(|| panic!("the report must contain {heading}"));
        let content = start + heading.len();
        let end = report[content..]
            .find("\n## ")
            .map(|offset| content + offset + 1)
            .unwrap_or(report.len());
        &report[start..end]
    }

    /// Whether the report contains exactly this whole line.
    fn has_line(report: &str, expected: &str) -> bool {
        report.lines().any(|line| line == expected)
    }

    /// Persist one session's run metrics as real JSON under the corpus root.
    fn write_session_metrics(corpus: &Path, session: &str, metrics: &RunMetrics) {
        let dir = corpus.join(session);
        fs::create_dir_all(&dir).expect("the session directory should be creatable");
        fs::write(
            dir.join(METRICS_FILE_NAME),
            serde_json::to_vec_pretty(metrics).expect("the run metrics should serialize"),
        )
        .expect("the run metrics should persist");
    }

    /// A minimal analyzed run with only the fields a given test needs changed.
    fn base_session(mode: &str, outcome: TerminalOutcome, elapsed_ms: u64) -> RunMetrics {
        RunMetrics {
            schema_version: METRICS_SCHEMA_VERSION,
            mode: mode.to_string(),
            terminal_outcome: outcome,
            elapsed_ms,
            ..RunMetrics::default()
        }
    }

    /// One provider-role usage record with explicit counters and tokens.
    fn provider_role(
        role: &str,
        invocations: u64,
        steps: u64,
        http_requests: u64,
        http_retries: u64,
        tokens: [u64; 4],
        cost: Option<f64>,
    ) -> ProviderRoleMetrics {
        ProviderRoleMetrics {
            role: role.to_string(),
            model: format!("{role}-model"),
            invocations,
            steps,
            http_requests,
            http_retries,
            usage: TokenUsage {
                input_tokens: tokens[0],
                cached_input_tokens: tokens[1],
                output_tokens: tokens[2],
                reasoning_tokens: tokens[3],
            },
            estimated_cost_usd: cost,
        }
    }

    /// One ticket execution record with its attempt and ending tags.
    fn ticket_record(
        id: &str,
        attempts: u64,
        status: TicketStatusTag,
        first_outcome: Option<AttemptOutcomeTag>,
        escalated: bool,
    ) -> TicketExecutionMetrics {
        TicketExecutionMetrics {
            ticket_id: id.to_string(),
            attempt_count: attempts,
            first_attempt_outcome: first_outcome,
            retry_escalated: escalated,
            final_status: status,
            ..TicketExecutionMetrics::default()
        }
    }

    /// One queue round carrying the given ticket records.
    fn queue_round_record(round: u64, tickets: Vec<TicketExecutionMetrics>) -> QueueRoundRecord {
        QueueRoundRecord {
            round,
            outcome: QueueRoundOutcome::Resolved,
            metrics: QueueRoundMetrics {
                tickets,
                ..QueueRoundMetrics::default()
            },
        }
    }

    /// One review round with its convergence outcome and retry flags.
    fn review_round_record(
        round: u64,
        outcome: ReviewRoundOutcome,
        retried: bool,
        succeeded: bool,
    ) -> ReviewRoundMetrics {
        ReviewRoundMetrics {
            round,
            outcome,
            retried,
            succeeded,
            ..ReviewRoundMetrics::default()
        }
    }

    /// The first multi-session run: a completed mid run with triage, a rescued
    /// escalation, review convergence, summary rejection and tool friction.
    fn first_corpus_session() -> RunMetrics {
        let mut first_ticket = ticket_record(
            "t1",
            1,
            TicketStatusTag::Completed,
            Some(AttemptOutcomeTag::Complete),
            false,
        );
        first_ticket.telemetry = LoopTelemetry {
            elapsed_ms: 100,
            bash_calls: 2,
            bash_failures: 1,
            bash_output_spills: 1,
            execute_script_calls: 3,
            execute_script_failures: 1,
            execute_script_timeouts: 1,
            rejected_control_tool_calls: 1,
            tier1_compactions: 1,
            tier2_summaries_accepted: 1,
            overflow_compaction_recoveries: 1,
            repeated_tool_warnings: 2,
            ..LoopTelemetry::default()
        };
        let mut second_ticket = ticket_record(
            "t2",
            2,
            TicketStatusTag::Completed,
            Some(AttemptOutcomeTag::TurnEnd),
            true,
        );
        second_ticket.telemetry = LoopTelemetry {
            elapsed_ms: 200,
            bash_calls: 1,
            execute_script_calls: 2,
            rejected_other_tool_calls: 1,
            tier1_compactions: 2,
            tier2_summary_failures: 1,
            truncation_recoveries: 1,
            repeated_tool_warnings: 1,
            repeated_tool_aborts: 1,
            ..LoopTelemetry::default()
        };
        let third_ticket = ticket_record(
            "t3",
            2,
            TicketStatusTag::Failed,
            Some(AttemptOutcomeTag::FailedApi),
            false,
        );

        let mut metrics = base_session("mid", TerminalOutcome::Complete, 1_000);
        metrics.estimated_cost_usd = Some(2.0);
        metrics.provider_roles = vec![
            provider_role("mid", 2, 4, 5, 1, [100, 50, 20, 5], Some(1.5)),
            provider_role("smoll", 1, 1, 1, 0, [50, 0, 10, 0], Some(0.5)),
        ];
        metrics.triage = Some(TriageMetrics {
            pass: PassMetrics {
                elapsed_ms: 50,
                outcome: PassOutcome::Handoff,
                retried: false,
                retry_reason: None,
                succeeded: true,
            },
            draft_count: Some(3),
            ready_count: Some(2),
            done_count: Some(1),
            blocked_count: Some(0),
            queue_authorized: Some(true),
        });
        metrics.queue_rounds = vec![queue_round_record(
            1,
            vec![first_ticket, second_ticket, third_ticket],
        )];
        metrics.transition_gate = TransitionGateMetrics {
            runs: 2,
            failures: 1,
            elapsed_ms: 300,
        };
        metrics.review_rounds = vec![
            review_round_record(1, ReviewRoundOutcome::FixesQueued, true, true),
            review_round_record(2, ReviewRoundOutcome::CircuitBroken, false, false),
        ];
        metrics.summary = Some(SummaryMetrics {
            elapsed_ms: 90,
            outcome: SummaryOutcome::Accepted,
            candidates_generated: 2,
            accepted: true,
            rejected_candidates: vec![SummaryRejectionCategory::MarkdownFence],
        });
        metrics.spec_pass = Some(PassMetrics {
            elapsed_ms: 10,
            outcome: PassOutcome::Handoff,
            retried: true,
            retry_reason: Some("ended its turn".to_string()),
            succeeded: true,
        });
        metrics.tickets_pass = Some(PassMetrics {
            elapsed_ms: 20,
            outcome: PassOutcome::Handoff,
            retried: false,
            retry_reason: None,
            succeeded: true,
        });
        metrics
    }

    /// The second multi-session run: an unpriced mid run that ended in retry,
    /// so its cost never enters the priced-run denominators.
    fn second_corpus_session() -> RunMetrics {
        let mut metrics = base_session("mid", TerminalOutcome::Retry, 500);
        metrics.terminal_detail = Some("review".to_string());
        metrics.provider_roles = vec![provider_role("mid", 1, 2, 2, 1, [100, 50, 0, 0], None)];
        metrics.queue_rounds = vec![queue_round_record(
            1,
            vec![ticket_record(
                "t4",
                1,
                TicketStatusTag::Failed,
                Some(AttemptOutcomeTag::FailedMalformed),
                false,
            )],
        )];
        metrics.transition_gate = TransitionGateMetrics {
            runs: 1,
            failures: 0,
            elapsed_ms: 100,
        };
        metrics.review_rounds = vec![review_round_record(
            1,
            ReviewRoundOutcome::Passed,
            false,
            true,
        )];
        metrics
    }

    /// The third multi-session run: a completed, priced hybrid run whose only
    /// role is giga, used to prove lexical mode and role ordering.
    fn hybrid_corpus_session() -> RunMetrics {
        let mut metrics = base_session("hybrid", TerminalOutcome::Complete, 250);
        metrics.estimated_cost_usd = Some(1.0);
        metrics.provider_roles = vec![provider_role(
            "giga",
            1,
            3,
            2,
            0,
            [1_000, 250, 100, 0],
            Some(1.0),
        )];
        metrics
    }

    /// One trajectory JSON value in the production shape, with telemetry
    /// counters that deliberately differ from the step count so a reader can
    /// tell which source each aggregate came from.
    fn trajectory(
        role: &str,
        steps_taken: u64,
        send_attempts: u64,
        transient_retries: u64,
        usage: [u64; 4],
    ) -> serde_json::Value {
        serde_json::json!({
            "phase": "code",
            "role": role,
            "model": format!("{role}-model"),
            "steps_taken": steps_taken,
            "exit_status": "complete",
            "messages": [],
            "telemetry": {
                "provider_send_attempts": send_attempts,
                "transient_retries": transient_retries,
                "bash_calls": 999,
                "execute_script_calls": 999,
                "token_usage": {
                    "input_tokens": usage[0],
                    "cached_input_tokens": usage[1],
                    "output_tokens": usage[2],
                    "reasoning_tokens": usage[3]
                }
            }
        })
    }

    /// Write one serialized trajectory entry under dir.
    fn write_trajectory(dir: &Path, name: &str, record: &serde_json::Value) {
        fs::write(dir.join(name), serde_json::to_vec_pretty(record).unwrap())
            .expect("the trajectory should be writable");
    }

    /// A mid-mode configuration with no rates: enough for the pure aggregation
    /// tests, which do not price anything.
    fn unpriced_config() -> Config {
        Config::new("http://mid.test/v1/chat/completions", "mid-model", None)
    }

    /// A hybrid configuration where every role is active, still with no rates.
    fn multi_role_config() -> Config {
        Config::new("http://mid.test/v1/chat/completions", "mid-model", None)
            .with_mode(crate::llm::Mode::Hybrid)
            .with_giga("http://giga.test/v1/responses", "giga-model", None)
            .with_smoll("http://smoll.test/v1/messages", "smoll-model", "sk-smoll")
    }

    /// Contract and invariant: a fully populated post-handoff document
    /// serializes every Section 4 field under its stable versioned name and
    /// shape, and no superseded name survives beside it.
    #[test]
    fn when_run_metrics_are_serialized_given_schema_two_transition_failures_then_persists_all_gate_counters()
     {
        let metrics = RunMetrics {
            schema_version: METRICS_SCHEMA_VERSION,
            mode: "hybrid".to_string(),
            provider_models: BTreeMap::from([
                ("giga".to_string(), "giga-model".to_string()),
                ("mid".to_string(), "mid-model".to_string()),
                ("smoll".to_string(), "smoll-model".to_string()),
            ]),
            terminal_outcome: TerminalOutcome::Retry,
            terminal_detail: Some("review".to_string()),
            elapsed_ms: 12_345,
            plan_interview: PlanInterviewMetrics {
                turns: 3,
                elapsed_ms: 900,
            },
            spec_pass: Some(PassMetrics {
                elapsed_ms: 1_000,
                outcome: PassOutcome::Handoff,
                retried: false,
                retry_reason: None,
                succeeded: true,
            }),
            tickets_pass: Some(PassMetrics {
                elapsed_ms: 1_100,
                outcome: PassOutcome::Handoff,
                retried: true,
                retry_reason: Some("ended early (malformed response)".to_string()),
                succeeded: true,
            }),
            triage: Some(TriageMetrics {
                pass: PassMetrics {
                    elapsed_ms: 1_200,
                    outcome: PassOutcome::Handoff,
                    retried: false,
                    retry_reason: None,
                    succeeded: true,
                },
                draft_count: Some(3),
                ready_count: Some(2),
                done_count: Some(1),
                blocked_count: Some(0),
                queue_authorized: Some(true),
            }),
            queue_rounds: vec![QueueRoundRecord {
                round: 1,
                outcome: QueueRoundOutcome::Resolved,
                metrics: QueueRoundMetrics::default(),
            }],
            transition_gate: TransitionGateMetrics {
                runs: 3,
                failures: 2,
                elapsed_ms: 400,
            },
            review_rounds: vec![ReviewRoundMetrics {
                round: 1,
                elapsed_ms: 1_500,
                outcome: ReviewRoundOutcome::FixesQueued,
                retried: true,
                retry_reason: Some("ended its turn".to_string()),
                succeeded: true,
                findings: Some(2),
                fixes: Some(1),
            }],
            summary: Some(SummaryMetrics {
                elapsed_ms: 800,
                outcome: SummaryOutcome::Accepted,
                candidates_generated: 2,
                accepted: true,
                rejected_candidates: vec![SummaryRejectionCategory::MarkdownFence],
            }),
            provider_roles: vec![ProviderRoleMetrics {
                role: "mid".to_string(),
                model: "mid-model".to_string(),
                invocations: 4,
                steps: 9,
                http_requests: 7,
                http_retries: 2,
                usage: TokenUsage {
                    input_tokens: 100,
                    cached_input_tokens: 20,
                    output_tokens: 30,
                    reasoning_tokens: 5,
                },
                estimated_cost_usd: Some(0.5),
            }],
            http_requests: 7,
            http_retries: 2,
            estimated_cost_usd: Some(0.5),
        };

        let value = serde_json::to_value(&metrics).unwrap();
        assert_eq!(value["schema_version"], 2);
        assert_eq!(value["mode"], "hybrid");
        assert_eq!(value["provider_models"]["mid"], "mid-model");
        assert_eq!(value["provider_models"]["giga"], "giga-model");
        assert_eq!(value["provider_models"]["smoll"], "smoll-model");
        assert_eq!(value["terminal_outcome"], "retry");
        assert_eq!(value["terminal_detail"], "review");
        assert_eq!(value["elapsed_ms"], 12_345);
        assert_eq!(value["plan_interview"]["turns"], 3);
        assert_eq!(value["plan_interview"]["elapsed_ms"], 900);
        assert_eq!(value["spec_pass"]["elapsed_ms"], 1_000);
        assert_eq!(value["spec_pass"]["outcome"], "handoff");
        assert_eq!(value["spec_pass"]["retried"], false);
        assert_eq!(value["spec_pass"]["succeeded"], true);
        assert!(value["spec_pass"].get("retry_reason").is_none());
        assert_eq!(value["tickets_pass"]["retried"], true);
        assert_eq!(
            value["tickets_pass"]["retry_reason"],
            "ended early (malformed response)"
        );
        assert_eq!(value["triage"]["draft_count"], 3);
        assert_eq!(value["triage"]["ready_count"], 2);
        assert_eq!(value["triage"]["done_count"], 1);
        assert_eq!(value["triage"]["blocked_count"], 0);
        assert_eq!(value["triage"]["queue_authorized"], true);
        assert_eq!(
            value["queue_rounds"][0]["metrics"]["tickets"]
                .as_array()
                .map(Vec::len),
            Some(0)
        );
        assert_eq!(value["queue_rounds"][0]["round"], 1);
        assert_eq!(value["queue_rounds"][0]["outcome"], "resolved");
        assert_eq!(value["transition_gate"]["runs"], 3);
        assert_eq!(value["transition_gate"]["failures"], 2);
        assert_eq!(value["transition_gate"]["elapsed_ms"], 400);
        assert!(value["transition_gate"].get("command").is_none());
        assert!(value["transition_gate"].get("total_elapsed_ms").is_none());
        assert_eq!(value["review_rounds"][0]["round"], 1);
        assert_eq!(value["review_rounds"][0]["elapsed_ms"], 1_500);
        assert_eq!(value["review_rounds"][0]["outcome"], "fixes_queued");
        assert_eq!(value["review_rounds"][0]["retried"], true);
        assert_eq!(value["review_rounds"][0]["retry_reason"], "ended its turn");
        assert_eq!(value["review_rounds"][0]["succeeded"], true);
        assert_eq!(value["review_rounds"][0]["findings"], 2);
        assert_eq!(value["review_rounds"][0]["fixes"], 1);
        assert_eq!(value["summary"]["outcome"], "accepted");
        assert_eq!(value["summary"]["candidates_generated"], 2);
        assert_eq!(value["summary"]["accepted"], true);
        assert_eq!(value["summary"]["rejected_candidates"][0], "markdown_fence");
        assert_eq!(value["provider_roles"][0]["role"], "mid");
        assert_eq!(value["provider_roles"][0]["model"], "mid-model");
        assert_eq!(value["provider_roles"][0]["invocations"], 4);
        assert_eq!(value["provider_roles"][0]["steps"], 9);
        assert_eq!(value["provider_roles"][0]["http_requests"], 7);
        assert_eq!(value["provider_roles"][0]["http_retries"], 2);
        assert_eq!(value["provider_roles"][0]["input_tokens"], 100);
        assert_eq!(value["provider_roles"][0]["cached_input_tokens"], 20);
        assert_eq!(value["provider_roles"][0]["output_tokens"], 30);
        assert_eq!(value["provider_roles"][0]["reasoning_tokens"], 5);
        assert_eq!(value["provider_roles"][0]["estimated_cost_usd"], 0.5);
        assert_eq!(value["http_requests"], 7);
        assert_eq!(value["http_retries"], 2);
        assert_eq!(value["estimated_cost_usd"], 0.5);
        for superseded in [
            "spec",
            "role_usage",
            "total_elapsed_ms",
            "total_estimated_cost_usd",
            "review_stalled_breaker_tripped",
            "mid_model",
            "giga_model",
            "smoll_model",
            "total_http_requests",
            "total_http_retries",
        ] {
            assert!(
                value.get(superseded).is_none(),
                "the superseded {superseded} name is not part of the contract"
            );
        }
        for superseded_review in [
            "verdict",
            "convergence",
            "findings_count",
            "fix_tickets_emitted",
        ] {
            assert!(
                value["review_rounds"][0].get(superseded_review).is_none(),
                "the superseded review field {superseded_review} is not part of the contract"
            );
        }

        let empty_optional = RunMetrics {
            schema_version: METRICS_SCHEMA_VERSION,
            mode: "mid".to_string(),
            terminal_outcome: TerminalOutcome::Complete,
            ..RunMetrics::default()
        };
        let empty_value = serde_json::to_value(&empty_optional).unwrap();
        for omitted in [
            "terminal_detail",
            "spec_pass",
            "tickets_pass",
            "triage",
            "summary",
            "estimated_cost_usd",
        ] {
            assert!(
                empty_value.get(omitted).is_none(),
                "an unattempted or absent optional field {omitted} must be omitted"
            );
        }
    }

    /// Contract: every pass outcome carries one fixed wire label, its handoff
    /// answer and -- only when a retry actually ran -- the first attempt's
    /// human-readable reason.
    #[test]
    fn when_pass_outcomes_vary_given_handoff_turn_end_and_failures_then_serializes_stable_labels() {
        let cases = [
            (PassOutcome::Handoff, "handoff", true),
            (PassOutcome::TurnEnd, "turn_end", false),
            (PassOutcome::FailedApi, "failed_api", false),
            (PassOutcome::FailedTransport, "failed_transport", false),
            (PassOutcome::FailedMalformed, "failed_malformed", false),
            (PassOutcome::FailedStepLimit, "failed_step_limit", false),
            (
                PassOutcome::FailedContextGuard,
                "failed_context_guard",
                false,
            ),
        ];
        for (outcome, wire, succeeded) in cases {
            let record = PassMetrics {
                elapsed_ms: 7,
                outcome,
                retried: true,
                retry_reason: Some("ended its turn".to_string()),
                succeeded,
            };
            let value = serde_json::to_value(&record).unwrap();
            assert_eq!(value["outcome"], wire, "the stable {wire} label");
            assert_eq!(value["succeeded"], succeeded);
            assert_eq!(value["retried"], true);
            assert_eq!(value["retry_reason"], "ended its turn");
            assert_eq!(value["elapsed_ms"], 7);
        }

        let failed_retry = PassMetrics {
            elapsed_ms: 9,
            outcome: PassOutcome::FailedMalformed,
            retried: true,
            retry_reason: Some("ended early (malformed response)".to_string()),
            succeeded: false,
        };
        let value = serde_json::to_value(&failed_retry).unwrap();
        assert_eq!(value["outcome"], "failed_malformed");
        assert_eq!(value["retry_reason"], "ended early (malformed response)");

        let not_retried = PassMetrics {
            elapsed_ms: 9,
            outcome: PassOutcome::FailedApi,
            retried: false,
            retry_reason: None,
            succeeded: false,
        };
        let value = serde_json::to_value(&not_retried).unwrap();
        assert_eq!(value["outcome"], "failed_api");
        assert!(
            value.get("retry_reason").is_none(),
            "a pass that never retried has no first-attempt reason"
        );
    }

    /// Edge case: a triage pass that reached no report still records its own
    /// outcome and timing, but omits every report-derived count and the queue
    /// authorization it could not have answered.
    #[test]
    fn when_triage_ends_without_report_given_no_handoff_then_omits_report_counts_and_authorization()
    {
        let triage = TriageMetrics {
            pass: PassMetrics {
                elapsed_ms: 250,
                outcome: PassOutcome::TurnEnd,
                retried: true,
                retry_reason: Some("ended its turn".to_string()),
                succeeded: false,
            },
            ..TriageMetrics::default()
        };
        let value = serde_json::to_value(&triage).unwrap();
        assert_eq!(value["outcome"], "turn_end");
        assert_eq!(value["elapsed_ms"], 250);
        assert_eq!(value["succeeded"], false);
        assert_eq!(value["retried"], true);
        assert_eq!(value["retry_reason"], "ended its turn");
        for omitted in [
            "draft_count",
            "ready_count",
            "done_count",
            "blocked_count",
            "queue_authorized",
        ] {
            assert!(
                value.get(omitted).is_none(),
                "a report-absent triage record omits {omitted}"
            );
        }
    }

    /// Contract: each configured mode advertises only the roles its routing
    /// serves, exactly once, with the configured model rather than the
    /// trajectory's wire echo.
    #[test]
    fn when_provider_roles_are_recorded_given_each_configured_mode_then_includes_only_active_roles_and_models()
     {
        type RoleExpectations<'a> = (&'a str, Config, &'a [(&'a str, &'a str)]);

        let root = scratch_dir("provider-roles-modes");
        let dir = root.join("trajectories");
        fs::create_dir_all(&dir).unwrap();
        write_trajectory(
            &dir,
            "a-mid.traj.json",
            &trajectory("mid", 1, 1, 0, [1, 0, 0, 0]),
        );
        write_trajectory(
            &dir,
            "b-giga.traj.json",
            &trajectory("giga", 1, 1, 0, [1, 0, 0, 0]),
        );
        write_trajectory(
            &dir,
            "c-smoll.traj.json",
            &trajectory("smoll", 1, 1, 0, [1, 0, 0, 0]),
        );

        let mid = unpriced_config().with_smoll(
            "http://smoll.test/v1/messages",
            "smoll-model",
            "sk-smoll",
        );
        let cases: Vec<RoleExpectations<'_>> = vec![
            (
                "mid",
                mid,
                &[("mid", "mid-model"), ("smoll", "smoll-model")],
            ),
            (
                "hybrid",
                multi_role_config(),
                &[
                    ("mid", "mid-model"),
                    ("giga", "giga-model"),
                    ("smoll", "smoll-model"),
                ],
            ),
        ];
        for (label, config, expected) in cases {
            let roles = collect_trajectory_usage(&dir, &config).unwrap();
            let actual: Vec<(&str, &str)> = roles
                .iter()
                .map(|role| (role.role.as_str(), role.model.as_str()))
                .collect();
            assert_eq!(
                actual.as_slice(),
                expected,
                "{label}: only active roles and models"
            );

            let mut metrics = RunMetrics::default();
            metrics.record_provider_models(&config);
            metrics.record_provider_roles(roles);
            let value = serde_json::to_value(&metrics).unwrap();
            let models = value["provider_models"].as_object().unwrap();
            assert_eq!(
                models.len(),
                expected.len(),
                "{label}: one entry per active role"
            );
            for (role, model) in expected {
                assert_eq!(value["provider_models"][*role], *model, "{label}: {role}");
            }
            assert_eq!(
                value["provider_roles"].as_array().map(Vec::len),
                Some(expected.len()),
                "{label}: one role record per active role"
            );
        }
        let _ = fs::remove_dir_all(&root);
    }

    /// Edge case: a trajectory without a canonical role or without a model is
    /// unusable and is skipped, while its valid sibling still aggregates.
    #[test]
    fn when_trajectory_entries_are_incomplete_given_missing_role_or_model_then_skips_unusable_file()
    {
        let root = scratch_dir("incomplete-trajectories");
        let dir = root.join("trajectories");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("a-no-role.traj.json"),
            serde_json::to_vec(&serde_json::json!({"model": "wire-model", "steps_taken": 9}))
                .unwrap(),
        )
        .unwrap();
        fs::write(
            dir.join("b-no-model.traj.json"),
            serde_json::to_vec(&serde_json::json!({"role": "mid", "steps_taken": 8})).unwrap(),
        )
        .unwrap();
        fs::write(
            dir.join("c-blank-model.traj.json"),
            serde_json::to_vec(
                &serde_json::json!({"role": "mid", "model": "   ", "steps_taken": 7}),
            )
            .unwrap(),
        )
        .unwrap();
        write_trajectory(
            &dir,
            "d-valid.traj.json",
            &trajectory("mid", 3, 2, 1, [10, 0, 5, 0]),
        );

        let roles = collect_trajectory_usage(&dir, &unpriced_config()).unwrap();
        assert_eq!(roles.len(), 1, "only the usable trajectory contributes");
        assert_eq!(roles[0].role, "mid");
        assert_eq!(
            roles[0].model, "mid-model",
            "the configured model is recorded"
        );
        assert_eq!(roles[0].invocations, 1);
        assert_eq!(roles[0].steps, 3);
        assert_eq!(roles[0].http_requests, 2);
        let _ = fs::remove_dir_all(&root);
    }

    /// Core entry-point behavior: valid trajectory files are aggregated by
    /// role in lexical filename order, and each role record reports its own
    /// invocations, steps, send attempts, retries and token counters.
    #[test]
    fn when_trajectory_files_are_collected_given_lexically_ordered_valid_roles_then_aggregates_invocations_steps_requests_retries_and_tokens()
     {
        let root = scratch_dir("collect-ordered");
        let dir = root.join("trajectories");
        fs::create_dir_all(&dir).unwrap();
        write_trajectory(
            &dir,
            "a-code.traj.json",
            &trajectory("mid", 2, 3, 1, [100, 20, 30, 5]),
        );
        write_trajectory(
            &dir,
            "b-plan.traj.json",
            &trajectory("giga", 4, 5, 0, [200, 40, 60, 10]),
        );
        write_trajectory(
            &dir,
            "c-code-1.traj.json",
            &trajectory("mid", 5, 7, 2, [300, 60, 90, 15]),
        );
        fs::write(dir.join("notes.txt"), "not a trajectory").unwrap();

        let roles = collect_trajectory_usage(&dir, &multi_role_config())
            .expect("a readable trajectory directory should aggregate");

        assert_eq!(roles.len(), 2, "one record per canonical role");
        assert_eq!(
            roles[0].role, "mid",
            "the first lexical file names the first role"
        );
        assert_eq!(roles[0].invocations, 2);
        assert_eq!(roles[0].steps, 7);
        assert_eq!(roles[0].http_requests, 10);
        assert_eq!(roles[0].http_retries, 3);
        assert_eq!(
            roles[0].usage,
            TokenUsage {
                input_tokens: 400,
                cached_input_tokens: 80,
                output_tokens: 120,
                reasoning_tokens: 20,
            }
        );
        assert_eq!(roles[0].estimated_cost_usd, None);

        assert_eq!(roles[1].role, "giga");
        assert_eq!(roles[1].invocations, 1);
        assert_eq!(roles[1].steps, 4);
        assert_eq!(roles[1].http_requests, 5);
        assert_eq!(roles[1].http_retries, 0);
        assert_eq!(
            roles[1].usage,
            TokenUsage {
                input_tokens: 200,
                cached_input_tokens: 40,
                output_tokens: 60,
                reasoning_tokens: 10,
            }
        );

        let _ = fs::remove_dir_all(&root);
    }

    /// Edge-case table: a missing directory, an empty directory and a mixed
    /// directory of malformed and unusable entries all stay best-effort, and
    /// only the usable invocation survives.
    #[test]
    fn when_trajectory_inputs_vary_given_missing_empty_malformed_and_unusable_files_then_keeps_only_usable_invocations()
     {
        let root = scratch_dir("collect-edge");
        let missing = root.join("missing");
        let empty = root.join("empty");
        fs::create_dir_all(empty.join("trajectories")).unwrap();
        let mixed = root.join("mixed");
        let mixed_dir = mixed.join("trajectories");
        fs::create_dir_all(&mixed_dir).unwrap();
        fs::write(mixed_dir.join("a-empty.traj.json"), "").unwrap();
        fs::write(mixed_dir.join("b-malformed.traj.json"), "{ not json").unwrap();
        fs::write(
            mixed_dir.join("c-no-role.traj.json"),
            serde_json::to_vec(&serde_json::json!({"steps_taken": 9})).unwrap(),
        )
        .unwrap();
        write_trajectory(
            &mixed_dir,
            "d-unknown-role.traj.json",
            &trajectory("hybrid", 1, 1, 1, [1, 1, 1, 1]),
        );
        fs::create_dir_all(mixed_dir.join("e-directory.traj.json")).unwrap();
        write_trajectory(
            &mixed_dir,
            "f-valid.traj.json",
            &trajectory("smoll", 3, 4, 1, [50, 5, 6, 7]),
        );
        fs::write(mixed_dir.join("g-notes.txt"), "not a trajectory").unwrap();

        let empty_dir = empty.join("trajectories");
        let cases = [
            ("missing", missing.as_path(), 0u64, None),
            ("empty", empty_dir.as_path(), 0u64, None),
            ("mixed", mixed_dir.as_path(), 1u64, Some("smoll")),
        ];

        for (name, dir, expected_invocations, expected_role) in cases {
            let roles = collect_trajectory_usage(dir, &multi_role_config())
                .unwrap_or_else(|error| panic!("{name} must stay best-effort: {error}"));
            assert_eq!(
                roles.len(),
                usize::from(expected_role.is_some()),
                "{name}: only a usable trajectory contributes a role"
            );
            if let Some(role) = expected_role {
                assert_eq!(roles[0].role, role, "{name}: the usable role survives");
                assert_eq!(roles[0].invocations, expected_invocations, "{name}");
                assert_eq!(roles[0].steps, 3, "{name}: the valid step count");
                assert_eq!(roles[0].http_requests, 4, "{name}: send attempts only");
                assert_eq!(roles[0].http_retries, 1, "{name}: transient retries only");
                assert_eq!(
                    roles[0].usage,
                    TokenUsage {
                        input_tokens: 50,
                        cached_input_tokens: 5,
                        output_tokens: 6,
                        reasoning_tokens: 7,
                    },
                    "{name}: the valid token counters"
                );
            } else {
                assert_eq!(expected_invocations, 0, "{name}: no usable invocation");
            }
        }

        let _ = fs::remove_dir_all(&root);
    }

    /// Contract: every counter and every token field saturates independently at
    /// u64::MAX instead of wrapping.
    #[test]
    fn when_usage_sums_overflow_given_maximum_counters_and_tokens_then_saturates_every_total() {
        let root = scratch_dir("collect-saturation");
        let dir = root.join("trajectories");
        fs::create_dir_all(&dir).unwrap();
        write_trajectory(
            &dir,
            "a-first.traj.json",
            &trajectory(
                "mid",
                u64::MAX - 1,
                u64::MAX - 1,
                u64::MAX - 1,
                [u64::MAX - 1, u64::MAX - 1, u64::MAX - 1, u64::MAX - 1],
            ),
        );
        write_trajectory(
            &dir,
            "b-second.traj.json",
            &trajectory("mid", 7, 7, 7, [7, 7, 7, 7]),
        );

        let roles = collect_trajectory_usage(&dir, &unpriced_config()).unwrap();

        assert_eq!(roles.len(), 1);
        assert_eq!(
            roles[0].invocations, 2,
            "invocations count files, not steps"
        );
        assert_eq!(roles[0].steps, u64::MAX);
        assert_eq!(roles[0].http_requests, u64::MAX);
        assert_eq!(roles[0].http_retries, u64::MAX);
        assert_eq!(
            roles[0].usage,
            TokenUsage {
                input_tokens: u64::MAX,
                cached_input_tokens: u64::MAX,
                output_tokens: u64::MAX,
                reasoning_tokens: u64::MAX,
            }
        );

        let mut metrics = RunMetrics::default();
        metrics.record_provider_roles(roles);
        assert_eq!(metrics.http_requests, u64::MAX);
        assert_eq!(metrics.http_retries, u64::MAX);
        assert_eq!(metrics.provider_roles.len(), 1);

        let _ = fs::remove_dir_all(&root);
    }

    /// A hybrid configuration whose three roles carry deliberately different
    /// rates; Smoll carries none at all.
    fn priced_config() -> Config {
        let values: Vec<(&str, &str)> = vec![
            ("AGENT_MODE", "hybrid"),
            ("LLM_MID_URL", "http://mid.test/v1/chat/completions"),
            ("LLM_MID_MODEL", "mid-model"),
            ("LLM_MID_API_KEY", "sk-mid"),
            ("LLM_MID_INPUT_PRICE_PER_MILLION_USD", "1.0"),
            ("LLM_MID_OUTPUT_PRICE_PER_MILLION_USD", "5.0"),
            ("LLM_GIGA_URL", "http://giga.test/v1/responses"),
            ("LLM_GIGA_MODEL", "giga-model"),
            ("LLM_GIGA_API_KEY", "sk-giga"),
            ("LLM_GIGA_INPUT_PRICE_PER_MILLION_USD", "10.0"),
            ("LLM_GIGA_CACHED_INPUT_PRICE_PER_MILLION_USD", "2.0"),
            ("LLM_GIGA_OUTPUT_PRICE_PER_MILLION_USD", "100.0"),
            ("LLM_SMOLL_URL", "http://smoll.test/v1/messages"),
            ("LLM_SMOLL_MODEL", "smoll-model"),
            ("LLM_SMOLL_API_KEY", "sk-smoll"),
        ];
        let environment = |name: &str| {
            values
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_string())
        };
        Config::from_environment(&environment).expect("the priced fixture should configure")
    }

    /// Contract: each role is priced from its own rates, a missing cached rate
    /// falls back to that same role's input rate, and an unpriced role's
    /// estimate is omitted both from the record and from the root total.
    #[test]
    fn when_role_prices_vary_given_cached_rate_absent_or_role_unpriced_then_uses_local_fallback_and_omits_unpriced_costs()
     {
        let root = scratch_dir("collect-prices");
        let dir = root.join("trajectories");
        fs::create_dir_all(&dir).unwrap();
        write_trajectory(
            &dir,
            "a-mid.traj.json",
            &trajectory("mid", 2, 3, 1, [1_000_000, 500_000, 0, 0]),
        );
        write_trajectory(
            &dir,
            "b-giga.traj.json",
            &trajectory("giga", 4, 5, 2, [2_000_000, 1_000_000, 100_000, 0]),
        );
        write_trajectory(
            &dir,
            "c-smoll.traj.json",
            &trajectory("smoll", 1, 1, 0, [1_000_000, 0, 0, 0]),
        );

        let roles = collect_trajectory_usage(&dir, &priced_config())
            .expect("the priced trajectory directory should aggregate");
        assert_eq!(roles.len(), 3);
        assert_eq!(
            roles[0].estimated_cost_usd,
            Some(1.0),
            "mid's absent cached rate falls back to mid's own input rate"
        );
        assert_eq!(
            roles[1].estimated_cost_usd,
            Some(22.0),
            "giga prices uncached input, cached input and output at giga rates"
        );
        assert_eq!(
            roles[2].estimated_cost_usd, None,
            "an unpriced role has no estimate"
        );

        let smoll = serde_json::to_value(&roles[2]).unwrap();
        assert!(
            smoll.get("estimated_cost_usd").is_none(),
            "an unpriced role omits the estimate from the wire contract"
        );

        let unpriced_smoll = roles[2].clone();
        let mut metrics = RunMetrics::default();
        metrics.record_provider_roles(roles);
        assert_eq!(metrics.http_requests, 9);
        assert_eq!(metrics.http_retries, 3);
        assert_eq!(
            metrics.estimated_cost_usd,
            Some(23.0),
            "the root cost sums only the available role estimates"
        );

        let mut unpriced_only = RunMetrics::default();
        unpriced_only.record_provider_roles(vec![unpriced_smoll]);
        assert_eq!(
            unpriced_only.estimated_cost_usd, None,
            "a run whose only invoked role is unpriced omits the root estimate"
        );
        let unpriced_wire = serde_json::to_value(&unpriced_only).unwrap();
        assert!(
            unpriced_wire.get("estimated_cost_usd").is_none(),
            "the unpriced root estimate is omitted from the wire contract"
        );

        let _ = fs::remove_dir_all(&root);
    }

    /// Edge-case table: a document is written at the top of the session
    /// directory only when the resolved path has an exact .ejaj component.
    #[test]
    fn when_metrics_are_persisted_given_exact_ejaj_and_lookalike_paths_then_writes_only_for_exact_component()
     {
        let root = scratch_dir("persist");
        let metrics = RunMetrics {
            schema_version: METRICS_SCHEMA_VERSION,
            mode: "hybrid".to_string(),
            provider_models: BTreeMap::from([
                ("mid".to_string(), "mid-model".to_string()),
                ("giga".to_string(), "giga-model".to_string()),
            ]),
            terminal_outcome: TerminalOutcome::Complete,
            elapsed_ms: 1234,
            plan_interview: PlanInterviewMetrics {
                turns: 3,
                elapsed_ms: 900,
            },
            ..RunMetrics::default()
        };

        let relative_exact = PathBuf::from(".ejaj/run-1");
        let relative_exact_dir = root.join(&relative_exact);
        assert!(
            persist_run_metrics(&root, &relative_exact, &metrics).unwrap(),
            "a relative path with an exact .ejaj component is eligible"
        );
        let relative_file = relative_exact_dir.join(METRICS_FILE_NAME);
        assert!(
            relative_file.is_file(),
            "the document lands at the top of the session directory"
        );
        let payload: serde_json::Value =
            serde_json::from_slice(&fs::read(&relative_file).unwrap()).unwrap();
        assert_eq!(payload["schema_version"], 2);
        assert_eq!(payload["mode"], "hybrid");
        assert_eq!(payload["provider_models"]["mid"], "mid-model");
        assert_eq!(payload["provider_models"]["giga"], "giga-model");
        assert_eq!(payload["terminal_outcome"], "complete");
        assert_eq!(payload["elapsed_ms"], 1234);
        assert_eq!(payload["plan_interview"]["turns"], 3);
        assert!(
            payload["provider_models"].get("smoll").is_none(),
            "an inactive role is not advertised"
        );
        assert!(
            payload.get("estimated_cost_usd").is_none(),
            "an unpriced run omits the root estimate"
        );

        let absolute_exact = root.join("sessions").join(".ejaj").join("run-2");
        assert!(
            persist_run_metrics(&root, &absolute_exact, &metrics).unwrap(),
            "an absolute path with a nested exact .ejaj component is eligible"
        );
        assert!(absolute_exact.join(METRICS_FILE_NAME).is_file());

        let lookalikes = [
            ("suffix", PathBuf::from(".ejaj-extra/run-3")),
            ("prefix", PathBuf::from("not.ejaj/run-4")),
            ("plain", PathBuf::from("plain/run-5")),
        ];
        for (name, relative) in lookalikes {
            let dir = root.join(&relative);
            assert!(
                !persist_run_metrics(&root, &relative, &metrics).unwrap(),
                "{name}: a lookalike component is not eligible"
            );
            assert!(
                !dir.join(METRICS_FILE_NAME).exists(),
                "{name}: an ineligible destination is never written"
            );
        }

        let _ = fs::remove_dir_all(&root);
    }

    /// Contract: every known canonicalizer rejection maps to one fixed
    /// category, and an unknown reason deterministically takes the fallback.
    #[test]
    fn when_summary_rejection_reasons_vary_given_known_and_unknown_rejections_then_returns_stable_categories()
     {
        let cases = [
            (
                "the answer is empty: a summary is a header line, one blank line and a non-empty body",
                SummaryRejectionCategory::Empty,
                "empty",
            ),
            (
                "the answer carries a Markdown fence; a summary is plain text with no fence",
                SummaryRejectionCategory::MarkdownFence,
                "markdown_fence",
            ),
            (
                "the summary reproduces a generated ticket id; a summary refers to ticket work by title and status",
                SummaryRejectionCategory::GeneratedTicketId,
                "generated_ticket_id",
            ),
            (
                "a Refs: footer is not accepted: a footer line is one of Fixes #N",
                SummaryRejectionCategory::RefsFooter,
                "refs_footer",
            ),
            (
                "the footer line Fixes 12 is not in a final block separated from the body by one blank line",
                SummaryRejectionCategory::InvalidFooter,
                "invalid_footer",
            ),
            (
                "the header must be followed by exactly one blank line and then the body",
                SummaryRejectionCategory::MissingBlankLine,
                "missing_blank_line",
            ),
            (
                "a body line is 73 characters; every body line must be at most 72 characters",
                SummaryRejectionCategory::LineOverLimit,
                "line_over_limit",
            ),
            (
                "the summary has no body: the header must be followed by a blank line and then a non-empty body",
                SummaryRejectionCategory::EmptyBody,
                "empty_body",
            ),
            (
                "the header must start with one of the types feat, fix, docs; found nope: thing",
                SummaryRejectionCategory::InvalidHeaderOrPreamble,
                "invalid_header_or_preamble",
            ),
        ];

        for (reason, expected, wire) in cases {
            assert_eq!(
                classify_summary_rejection(reason),
                expected,
                "reason should classify as {wire}: {reason}"
            );
            assert_eq!(
                serde_json::to_string(&expected).unwrap(),
                format!("\"{wire}\""),
                "{wire} is the stable wire name"
            );
        }

        for reason in [
            "an unforeseen rejection reason",
            "another reason the classifier has never seen",
        ] {
            assert_eq!(
                classify_summary_rejection(reason),
                SummaryRejectionCategory::InvalidHeaderOrPreamble,
                "an unknown reason takes the deterministic fallback"
            );
        }
    }

    /// Core entry-point behavior: a missing corpus and an empty corpus both
    /// render a valid zero-run scorecard with no skipped metrics files.
    #[test]
    fn when_metrics_are_reported_given_missing_or_empty_corpus_then_shows_zero_runs() {
        let root = scratch_dir("corpus-empty");
        let missing = root.join("missing");
        let empty = root.join("empty");
        fs::create_dir_all(&empty).unwrap();
        fs::write(empty.join("notes.txt"), "a non-directory child is ignored").unwrap();

        let cases = [("missing", missing.as_path()), ("empty", empty.as_path())];
        for (name, dir) in cases {
            let report = render_corpus_report(dir)
                .unwrap_or_else(|error| panic!("{name}: every valid corpus renders: {error}"));
            assert_eq!(
                report.lines().next(),
                Some("0 runs analyzed, 0 metrics files skipped"),
                "{name}: the first physical line reports zero runs and zero skipped files: {report}"
            );
            assert_section_order(&report);
            assert!(
                report.ends_with('\n'),
                "{name}: the report ends with a newline"
            );
            assert!(
                !report.ends_with("\n\n"),
                "{name}: the report ends with exactly one newline"
            );
        }

        let _ = fs::remove_dir_all(&root);
    }

    /// Edge-case table: a malformed, unsupported-schema or non-regular metrics
    /// file is counted once and skipped, a missing metrics file is not counted,
    /// and an adjacent valid session still contributes to the report.
    #[test]
    fn when_metrics_are_reported_given_malformed_or_unsupported_sessions_then_skips_them_and_continues()
     {
        #[derive(Clone, Copy)]
        enum Subject {
            Missing,
            Malformed,
            UnsupportedSchema,
            NonRegular,
            Valid,
        }

        let root = scratch_dir("corpus-mixed");
        let cases = [
            ("missing", Subject::Missing, 1u64, 0usize),
            ("malformed", Subject::Malformed, 1, 1),
            ("unsupported", Subject::UnsupportedSchema, 1, 1),
            ("non-regular", Subject::NonRegular, 1, 1),
            ("valid", Subject::Valid, 2, 0),
        ];

        for (name, subject, expected_runs, expected_skipped) in cases {
            let corpus = root.join(name);
            write_session_metrics(
                &corpus,
                "a-valid",
                &base_session("mid", TerminalOutcome::Complete, 100),
            );
            let subject_dir = corpus.join("b-subject");
            match subject {
                Subject::Missing => {
                    fs::create_dir_all(&subject_dir).unwrap();
                }
                Subject::Malformed => {
                    fs::create_dir_all(&subject_dir).unwrap();
                    fs::write(subject_dir.join(METRICS_FILE_NAME), "{ not json").unwrap();
                }
                Subject::UnsupportedSchema => {
                    let mut metrics = base_session("mid", TerminalOutcome::Complete, 999);
                    metrics.schema_version = METRICS_SCHEMA_VERSION - 1;
                    write_session_metrics(&corpus, "b-subject", &metrics);
                }
                Subject::NonRegular => {
                    fs::create_dir_all(subject_dir.join(METRICS_FILE_NAME)).unwrap();
                }
                Subject::Valid => {
                    write_session_metrics(
                        &corpus,
                        "b-subject",
                        &base_session("mid", TerminalOutcome::Complete, 100),
                    );
                }
            }

            let report = render_corpus_report(&corpus)
                .unwrap_or_else(|error| panic!("{name}: unusable sessions never abort: {error}"));
            let expected_first_line =
                format!("{expected_runs} runs analyzed, {expected_skipped} metrics files skipped");
            assert_eq!(
                report.lines().next(),
                Some(expected_first_line.as_str()),
                "{name}: expected the first physical line to be {expected_first_line:?}: {report}"
            );
            let expected_elapsed = format!("- Total elapsed: {} ms", expected_runs * 100);
            assert!(
                has_line(&report, &expected_elapsed),
                "{name}: only usable sessions contribute elapsed time: {report}"
            );
            assert!(
                report_section(&report, "## Mode Summary").contains("| mid |"),
                "{name}: the valid session still contributes a mode row"
            );
        }

        let _ = fs::remove_dir_all(&root);
    }

    /// Core entry-point behavior: several real persisted sessions aggregate
    /// with independently calculated totals, fixed section order, lexical map
    /// ordering and exactly one final newline.
    #[test]
    fn when_metrics_are_reported_given_multiple_sessions_then_aggregates_deterministically() {
        let root = scratch_dir("corpus-multi");
        let corpus = root.join("corpus");
        write_session_metrics(&corpus, "a-first", &first_corpus_session());
        write_session_metrics(&corpus, "b-second", &second_corpus_session());
        write_session_metrics(&corpus, "c-hybrid", &hybrid_corpus_session());

        let report = render_corpus_report(&corpus).expect("the corpus should render");

        assert_eq!(
            report.lines().next(),
            Some("3 runs analyzed, 0 metrics files skipped"),
            "the first physical line reports the analyzed and skipped counts: {report}"
        );
        assert_section_order(&report);

        let modes = report_section(&report, "## Mode Summary");
        assert!(modes.contains("| hybrid | 1 | 1 | 100.0% |"), "{modes}");
        assert!(modes.contains("| mid | 2 | 1 | 50.0% |"), "{modes}");
        assert!(
            modes.find("| hybrid |").unwrap() < modes.find("| mid |").unwrap(),
            "mode rows are lexically ordered: {modes}"
        );

        let cost = report_section(&report, "## Elapsed & Cost");
        for line in [
            "- Total elapsed: 1750 ms",
            "- Mean elapsed per analyzed run: 583.3 ms",
            "- Mean elapsed per completed run: 625.0 ms",
            "- Priced runs: 2",
            "- Estimated cost: $3.000000",
            "- Mean cost per completed ticket: $1.500000",
        ] {
            assert!(has_line(cost, line), "expected {line:?} in: {cost}");
        }

        let outcomes = report_section(&report, "## Terminal Outcomes");
        assert!(outcomes.contains("| complete | 2 |"), "{outcomes}");
        assert!(outcomes.contains("| retry: review | 1 |"), "{outcomes}");
        assert!(
            outcomes.find("| complete |").unwrap() < outcomes.find("| retry: review |").unwrap(),
            "terminal outcomes are lexically ordered: {outcomes}"
        );

        let roles = report_section(&report, "## Provider Roles");
        for row in [
            "| giga | 1 | 3 | 2 | 0 | 1000 | 250 | 100 | 0 | 25.0% | $1.000000 |",
            "| mid | 3 | 6 | 7 | 2 | 200 | 100 | 20 | 5 | 50.0% | $1.500000 |",
            "| smoll | 1 | 1 | 1 | 0 | 50 | 0 | 10 | 0 | 0.0% | $0.500000 |",
        ] {
            assert!(roles.contains(row), "expected role row {row:?} in: {roles}");
        }
        assert!(
            roles.find("| giga |").unwrap() < roles.find("| mid |").unwrap()
                && roles.find("| mid |").unwrap() < roles.find("| smoll |").unwrap(),
            "provider-role rows are lexically ordered: {roles}"
        );

        let funnel = report_section(&report, "## Queue Funnel");
        for line in [
            "- Ticket records: 4",
            "- Attempts: 6",
            "- First-pass completions: 1",
            "- Retried tickets: 2",
            "- Rescued tickets: 1",
            "- First-attempt failures: 3",
            "- Escalations: 1",
            "- Rescued escalations: 1",
        ] {
            assert!(has_line(funnel, line), "expected {line:?} in: {funnel}");
        }

        let triage = report_section(&report, "## Triage & Fresh-Session Retries");
        for line in [
            "- Triage runs: 1",
            "- Triage succeeded: 1",
            "- Triage retried: 0",
            "- Drafts: 3",
            "- Ready: 2",
            "- Done: 1",
            "- Blocked: 0",
            "- Queue authorized: 1",
            "- Queue unauthorized: 0",
        ] {
            assert!(has_line(triage, line), "expected {line:?} in: {triage}");
        }
        for row in [
            "| spec | 1 | 1 | 1 | 1 |",
            "| tickets | 1 | 0 | 1 | 0 |",
            "| triage | 1 | 0 | 1 | 0 |",
            "| review 1 | 2 | 1 | 2 | 1 |",
            "| review 2 | 1 | 0 | 0 | 0 |",
        ] {
            assert!(has_line(triage, row), "expected row {row:?} in: {triage}");
        }
        assert!(
            triage.find("| spec |").unwrap() < triage.find("| tickets |").unwrap()
                && triage.find("| tickets |").unwrap() < triage.find("| triage |").unwrap()
                && triage.find("| triage |").unwrap() < triage.find("| review 1 |").unwrap(),
            "fresh-session retry rows use the declared order: {triage}"
        );

        let gate = report_section(&report, "## Transition Gate");
        for line in [
            "- Gate runs: 3",
            "- Gate failures: 1",
            "- Gate elapsed: 400 ms",
        ] {
            assert!(has_line(gate, line), "expected {line:?} in: {gate}");
        }

        let review_summary = report_section(&report, "## Review & Summary");
        for line in [
            "- Review rounds: 3",
            "- Review stalls: 1",
            "- Summary runs: 1",
            "- Summaries accepted: 1",
            "| circuit_broken | 1 |",
            "| fixes_queued | 1 |",
            "| passed | 1 |",
            "| accepted | 1 |",
            "| markdown_fence | 1 |",
        ] {
            assert!(
                has_line(review_summary, line),
                "expected {line:?} in: {review_summary}"
            );
        }
        assert!(
            review_summary.find("| circuit_broken |").unwrap()
                < review_summary.find("| fixes_queued |").unwrap()
                && review_summary.find("| fixes_queued |").unwrap()
                    < review_summary.find("| passed |").unwrap(),
            "review outcome rows are lexically ordered: {review_summary}"
        );

        let friction = report_section(&report, "## Queue Loop & Tool Friction");
        for line in [
            "- Bash calls: 3",
            "- Bash failures: 1",
            "- Bash timeouts: 0",
            "- Bash output spills: 1",
            "- Script calls: 5",
            "- Script failures: 1",
            "- Script timeouts: 1",
            "- Rejected control calls: 1",
            "- Rejected tool calls: 1",
            "- Tier 1 compactions: 3",
            "- Tier 2 summaries: 1",
            "- Tier 2 summary failures: 1",
            "- Overflow recoveries: 1",
            "- Truncation recoveries: 1",
            "- Repeated-turn warnings: 3",
            "- Repeated-turn aborts: 1",
            "- Loop elapsed: 300 ms",
        ] {
            assert!(has_line(friction, line), "expected {line:?} in: {friction}");
        }

        assert!(report.ends_with('\n'), "the report ends with a newline");
        assert!(
            !report.ends_with("\n\n"),
            "the report ends with exactly one newline"
        );
        for volatile in ["/tmp", "ejaj-metrics-test", "timestamp", "sk-"] {
            assert!(
                !report.contains(volatile),
                "the report must not leak volatile data such as {volatile}"
            );
        }

        let _ = fs::remove_dir_all(&root);
    }

    /// Core entry-point behavior and edge-case table: every queue record counts
    /// once even when its ticket id repeats across rounds, and the funnel uses
    /// the exact attempt, rescue, failure and escalation rules.
    #[test]
    fn when_metrics_are_reported_given_ticket_attempts_and_outcomes_then_applies_exact_funnel_rules()
     {
        let root = scratch_dir("corpus-funnel-rules");
        let corpus = root.join("corpus");
        let mut metrics = base_session("mid", TerminalOutcome::Complete, 100);
        metrics.queue_rounds = vec![
            queue_round_record(
                1,
                vec![
                    ticket_record(
                        "r-a",
                        1,
                        TicketStatusTag::Completed,
                        Some(AttemptOutcomeTag::Complete),
                        false,
                    ),
                    ticket_record(
                        "r-b",
                        2,
                        TicketStatusTag::Completed,
                        Some(AttemptOutcomeTag::TurnEnd),
                        false,
                    ),
                    ticket_record(
                        "r-c",
                        3,
                        TicketStatusTag::Completed,
                        Some(AttemptOutcomeTag::TurnEnd),
                        false,
                    ),
                    ticket_record(
                        "r-d",
                        1,
                        TicketStatusTag::Failed,
                        Some(AttemptOutcomeTag::FailedApi),
                        false,
                    ),
                ],
            ),
            queue_round_record(
                2,
                vec![
                    ticket_record("r-a", 1, TicketStatusTag::Failed, None, false),
                    ticket_record("r-f", 0, TicketStatusTag::Completed, None, false),
                    ticket_record("r-g", 1, TicketStatusTag::Failed, None, false),
                    ticket_record("r-h", 2, TicketStatusTag::Failed, None, false),
                    ticket_record(
                        "r-i",
                        2,
                        TicketStatusTag::Completed,
                        Some(AttemptOutcomeTag::Complete),
                        true,
                    ),
                    ticket_record(
                        "r-j",
                        3,
                        TicketStatusTag::Failed,
                        Some(AttemptOutcomeTag::FailedMalformed),
                        true,
                    ),
                    ticket_record(
                        "r-k",
                        3,
                        TicketStatusTag::Completed,
                        Some(AttemptOutcomeTag::FailedStepLimit),
                        true,
                    ),
                ],
            ),
        ];
        write_session_metrics(&corpus, "a-run", &metrics);

        let report = render_corpus_report(&corpus).expect("the corpus should render");
        let funnel = report_section(&report, "## Queue Funnel");
        for line in [
            "- Ticket records: 11",
            "- Attempts: 19",
            "- First-pass completions: 1",
            "- Retried tickets: 6",
            "- Rescued tickets: 2",
            "- First-attempt failures: 9",
            "- Escalations: 3",
            "- Rescued escalations: 2",
            "| complete | 1 |",
            "| failed_api | 1 |",
            "| failed_malformed | 1 |",
            "| failed_step_limit | 1 |",
            "| turn_end | 2 |",
            "| unknown | 3 |",
        ] {
            assert!(has_line(funnel, line), "expected {line:?} in: {funnel}");
        }
        let categories = [
            "complete",
            "failed_api",
            "failed_malformed",
            "failed_step_limit",
            "turn_end",
            "unknown",
        ];
        let mut previous = None;
        for category in categories {
            let needle = format!("| {category} |");
            let index = funnel
                .find(&needle)
                .unwrap_or_else(|| panic!("missing category {category}: {funnel}"));
            if let Some(previous) = previous {
                assert!(
                    index > previous,
                    "categories are lexically ordered: {funnel}"
                );
            }
            previous = Some(index);
        }

        let _ = fs::remove_dir_all(&root);
    }

    /// Core entry-point behavior: fresh-session retry rows cover spec, tickets,
    /// triage and every observed review round, with reached, retried, succeeded
    /// and post-retry success counts from real persisted run metrics.
    #[test]
    fn when_metrics_are_reported_given_fresh_session_retries_then_reports_each_reached_pass_and_outcome()
     {
        let root = scratch_dir("corpus-fresh-retries");
        let corpus = root.join("corpus");

        let mut first = base_session("mid", TerminalOutcome::Complete, 10);
        first.spec_pass = Some(PassMetrics {
            retried: true,
            succeeded: true,
            ..PassMetrics::default()
        });
        first.tickets_pass = Some(PassMetrics {
            retried: false,
            succeeded: true,
            ..PassMetrics::default()
        });
        first.triage = Some(TriageMetrics {
            pass: PassMetrics {
                retried: true,
                succeeded: false,
                ..PassMetrics::default()
            },
            ..TriageMetrics::default()
        });
        first.review_rounds = vec![
            review_round_record(1, ReviewRoundOutcome::Passed, true, true),
            review_round_record(2, ReviewRoundOutcome::FixesQueued, false, true),
        ];
        write_session_metrics(&corpus, "a-first", &first);

        let mut second = base_session("mid", TerminalOutcome::Complete, 10);
        second.tickets_pass = Some(PassMetrics {
            retried: true,
            succeeded: false,
            ..PassMetrics::default()
        });
        second.review_rounds = vec![
            review_round_record(1, ReviewRoundOutcome::TurnEnd, true, false),
            review_round_record(3, ReviewRoundOutcome::Passed, false, true),
        ];
        write_session_metrics(&corpus, "b-second", &second);

        let report = render_corpus_report(&corpus).expect("the corpus should render");
        let section = report_section(&report, "## Triage & Fresh-Session Retries");
        for row in [
            "| spec | 1 | 1 | 1 | 1 |",
            "| tickets | 2 | 1 | 1 | 0 |",
            "| triage | 1 | 1 | 0 | 0 |",
            "| review 1 | 2 | 2 | 1 | 1 |",
            "| review 2 | 1 | 0 | 1 | 0 |",
            "| review 3 | 1 | 0 | 1 | 0 |",
        ] {
            assert!(has_line(section, row), "expected row {row:?} in: {section}");
        }
        let order = [
            "| spec |",
            "| tickets |",
            "| triage |",
            "| review 1 |",
            "| review 2 |",
            "| review 3 |",
        ];
        let mut previous = None;
        for needle in order {
            let index = section
                .find(needle)
                .unwrap_or_else(|| panic!("missing row {needle}: {section}"));
            if let Some(previous) = previous {
                assert!(index > previous, "rows use the declared order: {section}");
            }
            previous = Some(index);
        }

        let _ = fs::remove_dir_all(&root);
    }

    /// Core entry-point behavior: triage pass outcomes aggregate by their
    /// stable persisted labels alongside the existing triage totals and
    /// fresh-session retry rows.
    #[test]
    fn when_metrics_are_reported_given_triage_pass_outcomes_then_aggregates_stable_labels() {
        let root = scratch_dir("corpus-triage-outcomes");
        let corpus = root.join("corpus");
        let cases = [
            ("a-handoff", PassOutcome::Handoff, false, true),
            ("b-handoff", PassOutcome::Handoff, false, true),
            ("c-turn-end", PassOutcome::TurnEnd, true, false),
            ("d-api", PassOutcome::FailedApi, false, false),
            ("e-transport", PassOutcome::FailedTransport, false, false),
            ("f-malformed", PassOutcome::FailedMalformed, false, false),
            ("g-step-limit", PassOutcome::FailedStepLimit, false, false),
            (
                "h-context-guard",
                PassOutcome::FailedContextGuard,
                false,
                false,
            ),
        ];
        for (name, outcome, retried, succeeded) in cases {
            let mut metrics = base_session("mid", TerminalOutcome::Complete, 10);
            metrics.triage = Some(TriageMetrics {
                pass: PassMetrics {
                    outcome,
                    retried,
                    succeeded,
                    ..PassMetrics::default()
                },
                ..TriageMetrics::default()
            });
            write_session_metrics(&corpus, name, &metrics);
        }

        let report = render_corpus_report(&corpus).expect("the corpus should render");
        let section = report_section(&report, "## Triage & Fresh-Session Retries");
        for line in [
            "- Triage runs: 8",
            "- Triage succeeded: 2",
            "- Triage retried: 1",
            "| failed_api | 1 |",
            "| failed_context_guard | 1 |",
            "| failed_malformed | 1 |",
            "| failed_step_limit | 1 |",
            "| failed_transport | 1 |",
            "| handoff | 2 |",
            "| turn_end | 1 |",
            "| triage | 8 | 1 | 2 | 0 |",
        ] {
            assert!(has_line(section, line), "expected {line:?} in: {section}");
        }
        let order = [
            "| failed_api |",
            "| failed_context_guard |",
            "| failed_malformed |",
            "| failed_step_limit |",
            "| failed_transport |",
            "| handoff |",
            "| turn_end |",
        ];
        let mut previous = None;
        for needle in order {
            let index = section
                .find(needle)
                .unwrap_or_else(|| panic!("missing triage outcome {needle}: {section}"));
            if let Some(previous) = previous {
                assert!(
                    index > previous,
                    "triage outcome rows are lexically ordered: {section}"
                );
            }
            previous = Some(index);
        }

        let _ = fs::remove_dir_all(&root);
    }

    /// Core entry-point behavior: a circuit-broken or round-capped review round
    /// is a stalled trip, while ordinary convergence outcomes are not.
    #[test]
    fn when_metrics_are_reported_given_review_round_caps_then_counts_stalls() {
        let root = scratch_dir("corpus-review-stalls");
        let corpus = root.join("corpus");
        let mut metrics = base_session("mid", TerminalOutcome::Retry, 10);
        metrics.review_rounds = vec![
            review_round_record(1, ReviewRoundOutcome::Passed, false, true),
            review_round_record(2, ReviewRoundOutcome::FixesQueued, false, true),
            review_round_record(3, ReviewRoundOutcome::CircuitBroken, false, false),
            review_round_record(4, ReviewRoundOutcome::RoundCapReached, false, false),
        ];
        write_session_metrics(&corpus, "a-run", &metrics);

        let report = render_corpus_report(&corpus).expect("the corpus should render");
        let section = report_section(&report, "## Review & Summary");
        for line in [
            "- Review rounds: 4",
            "- Review stalls: 2",
            "| circuit_broken | 1 |",
            "| fixes_queued | 1 |",
            "| passed | 1 |",
            "| round_cap_reached | 1 |",
        ] {
            assert!(has_line(section, line), "expected {line:?} in: {section}");
        }

        let _ = fs::remove_dir_all(&root);
    }

    /// Core entry-point behavior: summary pass@1 counts only accepted summaries
    /// with exactly one generated candidate, over summaries that generated any
    /// candidate at all.
    #[test]
    fn when_metrics_are_reported_given_summary_candidate_outcomes_then_reports_pass_at_one() {
        let root = scratch_dir("corpus-summary-pass-one");
        let corpus = root.join("corpus");
        let cases = [
            ("a-one-accepted", true, 1),
            ("b-two-accepted", true, 2),
            ("c-one-rejected", false, 1),
            ("d-zero-rejected", false, 0),
        ];
        for (name, accepted, candidates) in cases {
            let mut metrics = base_session("mid", TerminalOutcome::Complete, 10);
            metrics.summary = Some(SummaryMetrics {
                outcome: if accepted {
                    SummaryOutcome::Accepted
                } else {
                    SummaryOutcome::Invalid
                },
                candidates_generated: candidates,
                accepted,
                ..SummaryMetrics::default()
            });
            write_session_metrics(&corpus, name, &metrics);
        }

        let report = render_corpus_report(&corpus).expect("the corpus should render");
        let section = report_section(&report, "## Review & Summary");
        for line in [
            "- Summary runs: 4",
            "- Summaries accepted: 2",
            "- Summary pass@1: 1/3 (33.3%)",
        ] {
            assert!(has_line(section, line), "expected {line:?} in: {section}");
        }

        let _ = fs::remove_dir_all(&root);
    }

    /// Core entry-point behavior and edge-case table: terminal outcomes retain
    /// persisted detail, and absent or blank detail falls back to the stable
    /// label for that outcome.
    #[test]
    fn when_metrics_are_reported_given_terminal_details_then_uses_stable_labels_and_fallbacks() {
        let root = scratch_dir("corpus-terminal-labels");
        let corpus = root.join("corpus");
        let cases = [
            (
                "a-complete",
                TerminalOutcome::Complete,
                Some("ignored detail"),
            ),
            ("b-retry-detail", TerminalOutcome::Retry, Some("review")),
            ("c-retry-blank", TerminalOutcome::Retry, Some("   ")),
            ("d-write-detail", TerminalOutcome::WriteFailed, Some("spec")),
            ("e-write-absent", TerminalOutcome::WriteFailed, None),
            (
                "f-summary-detail",
                TerminalOutcome::SummaryFailed,
                Some("summary"),
            ),
            ("g-summary-absent", TerminalOutcome::SummaryFailed, None),
            ("h-git-detail", TerminalOutcome::GitFailed, Some("commit")),
            ("i-git-absent", TerminalOutcome::GitFailed, None),
        ];
        for (name, outcome, detail) in cases {
            let mut metrics = base_session("mid", outcome, 1);
            metrics.terminal_detail = detail.map(str::to_string);
            write_session_metrics(&corpus, name, &metrics);
        }

        let report = render_corpus_report(&corpus).expect("the corpus should render");
        let outcomes = report_section(&report, "## Terminal Outcomes");
        for row in [
            "| complete | 1 |",
            "| git_failed: commit | 1 |",
            "| git_failed: unspecified | 1 |",
            "| retry: review | 1 |",
            "| retry: unspecified | 1 |",
            "| summary_failed: summary | 2 |",
            "| write_failed: spec | 1 |",
            "| write_failed: unspecified | 1 |",
        ] {
            assert!(
                outcomes.contains(row),
                "expected row {row:?} in: {outcomes}"
            );
        }

        let _ = fs::remove_dir_all(&root);
    }

    /// Boundary: upper-limit elapsed totals and denominators render exact
    /// one-decimal means instead of a lossy floating-point approximation.
    #[test]
    fn when_metrics_are_reported_given_upper_boundary_elapsed_totals_then_renders_accurate_means() {
        let root = scratch_dir("corpus-upper-elapsed");
        let cases = [
            (
                "single",
                vec![(u64::MAX, TerminalOutcome::Complete)],
                "18446744073709551615.0 ms",
                "18446744073709551615.0 ms",
            ),
            (
                "pair",
                vec![
                    (u64::MAX - 1, TerminalOutcome::Complete),
                    (1, TerminalOutcome::Complete),
                ],
                "9223372036854775807.5 ms",
                "9223372036854775807.5 ms",
            ),
            (
                "mixed",
                vec![
                    (u64::MAX - 1, TerminalOutcome::Complete),
                    (1, TerminalOutcome::Complete),
                    (0, TerminalOutcome::Retry),
                ],
                "6148914691236517205.0 ms",
                "9223372036854775807.5 ms",
            ),
            (
                "quartet",
                vec![
                    (u64::MAX, TerminalOutcome::Complete),
                    (0, TerminalOutcome::Complete),
                    (0, TerminalOutcome::Complete),
                    (0, TerminalOutcome::Complete),
                ],
                "4611686018427387903.8 ms",
                "4611686018427387903.8 ms",
            ),
        ];

        for (name, sessions, analyzed_mean, completed_mean) in cases {
            let corpus = root.join(name);
            for (index, (elapsed_ms, outcome)) in sessions.into_iter().enumerate() {
                write_session_metrics(
                    &corpus,
                    &format!("session-{index}"),
                    &base_session("mid", outcome, elapsed_ms),
                );
            }

            let report = render_corpus_report(&corpus)
                .unwrap_or_else(|error| panic!("{name}: the corpus should render: {error}"));
            let cost = report_section(&report, "## Elapsed & Cost");
            for line in [
                "- Total elapsed: 18446744073709551615 ms",
                &format!("- Mean elapsed per analyzed run: {analyzed_mean}"),
                &format!("- Mean elapsed per completed run: {completed_mean}"),
            ] {
                assert!(has_line(cost, line), "{name}: expected {line:?} in: {cost}");
            }
        }

        let _ = fs::remove_dir_all(&root);
    }

    /// Contract and invariant: saturating counters preserve u64::MAX totals,
    /// and every undefined derived value renders as an em dash.
    #[test]
    fn when_metrics_are_reported_given_saturated_counters_and_empty_denominators_then_preserves_totals_and_shows_em_dashes()
     {
        let root = scratch_dir("corpus-saturated");
        let corpus = root.join("corpus");

        let mut heavy = base_session("mid", TerminalOutcome::Retry, u64::MAX);
        heavy.terminal_detail = Some("queue".to_string());
        heavy.provider_roles = vec![provider_role(
            "mid",
            u64::MAX,
            u64::MAX,
            u64::MAX,
            u64::MAX,
            [0, 0, 0, 0],
            None,
        )];
        heavy.queue_rounds = vec![queue_round_record(
            1,
            vec![ticket_record(
                "heavy",
                u64::MAX,
                TicketStatusTag::Failed,
                Some(AttemptOutcomeTag::FailedApi),
                false,
            )],
        )];
        heavy.transition_gate = TransitionGateMetrics {
            runs: u64::MAX,
            failures: u64::MAX,
            elapsed_ms: u64::MAX,
        };
        heavy.review_rounds = vec![review_round_record(
            1,
            ReviewRoundOutcome::CircuitBroken,
            false,
            false,
        )];
        write_session_metrics(&corpus, "a-heavy", &heavy);

        let mut extra = base_session("mid", TerminalOutcome::Retry, 5);
        extra.terminal_detail = Some("queue".to_string());
        extra.provider_roles = vec![provider_role("mid", 3, 4, 5, 6, [0, 0, 0, 0], None)];
        extra.queue_rounds = vec![queue_round_record(
            1,
            vec![ticket_record(
                "extra",
                10,
                TicketStatusTag::Failed,
                None,
                false,
            )],
        )];
        extra.transition_gate = TransitionGateMetrics {
            runs: 7,
            failures: 8,
            elapsed_ms: 9,
        };
        extra.review_rounds = vec![review_round_record(
            1,
            ReviewRoundOutcome::Passed,
            false,
            true,
        )];
        write_session_metrics(&corpus, "b-extra", &extra);

        let report = render_corpus_report(&corpus).expect("the saturated corpus should render");

        assert!(has_line(
            &report,
            "2 runs analyzed, 0 metrics files skipped"
        ));
        assert!(has_line(&report, "| mid | 2 | 0 | 0.0% |"));
        for line in [
            "- Total elapsed: 18446744073709551615 ms",
            "- Mean elapsed per analyzed run: 9223372036854775807.5 ms",
            "- Mean elapsed per completed run: —",
            "- Priced runs: 0",
            "- Estimated cost: —",
            "- Mean cost per completed ticket: —",
            "- Attempts: 18446744073709551615",
            "- Retried tickets: 2",
            "- First-attempt failures: 2",
            "- Gate runs: 18446744073709551615",
            "- Gate failures: 18446744073709551615",
            "- Gate elapsed: 18446744073709551615 ms",
            "| mid | 18446744073709551615 | 18446744073709551615 | 18446744073709551615 | 18446744073709551615 | 0 | 0 | 0 | 0 | — | — |",
        ] {
            assert!(has_line(&report, line), "expected {line:?} in: {report}");
        }

        let _ = fs::remove_dir_all(&root);
    }

    /// Boundary: a denied corpus root is an error, while a denied session file
    /// is skipped and the readable session is retained. The assertion is only
    /// meaningful on hosts where permissions are reliably enforced.
    #[cfg(unix)]
    #[test]
    fn when_metrics_are_reported_given_denied_corpus_and_session_reads_then_returns_or_skips_as_specified()
     {
        use std::os::unix::fs::PermissionsExt;

        let root = scratch_dir("corpus-permissions");
        let corpus = root.join("corpus");
        write_session_metrics(
            &corpus,
            "b-readable",
            &base_session("mid", TerminalOutcome::Complete, 100),
        );
        let denied_session = corpus.join("a-denied");
        fs::create_dir_all(&denied_session).unwrap();
        fs::write(
            denied_session.join(METRICS_FILE_NAME),
            serde_json::to_vec_pretty(&base_session("hybrid", TerminalOutcome::Complete, 999))
                .unwrap(),
        )
        .unwrap();

        let denied_file = denied_session.join(METRICS_FILE_NAME);
        fs::set_permissions(&denied_file, fs::Permissions::from_mode(0o000)).unwrap();
        let enforced = fs::read(&denied_file).is_err();
        if !enforced {
            fs::set_permissions(&denied_file, fs::Permissions::from_mode(0o644)).unwrap();
            let _ = fs::remove_dir_all(&root);
            return;
        }

        let report = render_corpus_report(&corpus)
            .expect("a denied session file is skipped rather than aborting");
        assert!(
            has_line(&report, "1 runs analyzed, 1 metrics files skipped"),
            "the denied session is skipped and the readable one retained: {report}"
        );
        assert!(
            has_line(&report, "- Total elapsed: 100 ms"),
            "the readable session still contributes: {report}"
        );
        assert!(
            !report.contains("999 ms"),
            "the denied session never contributes: {report}"
        );

        fs::set_permissions(&corpus, fs::Permissions::from_mode(0o000)).unwrap();
        let denied_root = render_corpus_report(&corpus);
        fs::set_permissions(&corpus, fs::Permissions::from_mode(0o755)).unwrap();

        fs::set_permissions(&denied_file, fs::Permissions::from_mode(0o644)).unwrap();
        let _ = fs::remove_dir_all(&root);

        assert!(
            denied_root.is_err(),
            "a corpus-directory read error other than not-found is returned"
        );
    }
}
