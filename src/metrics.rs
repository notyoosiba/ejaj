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
use crate::orchestrator::QueueRoundMetrics;
use crate::tools::Phase;

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;
use std::time::Duration;

/// The version of the serialized metrics contract this crate writes.
pub const METRICS_SCHEMA_VERSION: u32 = 1;

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
pub fn persist_run_metrics(
    root: &Path,
    dir: &Path,
    metrics: &RunMetrics,
) -> io::Result<bool> {
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
    fn when_run_metrics_are_serialized_given_post_handoff_outcomes_then_matches_versioned_contract()
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
                runs: 2,
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
        assert_eq!(value["schema_version"], 1);
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
        assert_eq!(value["transition_gate"]["runs"], 2);
        assert_eq!(value["transition_gate"]["elapsed_ms"], 400);
        assert!(value["transition_gate"].get("command").is_none());
        assert!(value["transition_gate"].get("failures").is_none());
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
        assert_eq!(
            value["summary"]["rejected_candidates"][0],
            "markdown_fence"
        );
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
        for superseded_review in ["verdict", "convergence", "findings_count", "fix_tickets_emitted"] {
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
        assert_eq!(
            value["retry_reason"],
            "ended early (malformed response)"
        );

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
            ("mid", mid, &[("mid", "mid-model"), ("smoll", "smoll-model")]),
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
            assert_eq!(actual.as_slice(), expected, "{label}: only active roles and models");

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
        assert_eq!(payload["schema_version"], 1);
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
}
