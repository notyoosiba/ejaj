//! Run-metrics finalization across the pipeline's post-handoff lifecycle.
//!
//! Every test drives the production pipeline over a real disposable Git
//! repository and the scripted external transport. Metrics live under real
//! directories, aggregating real trajectory files, so eligibility, terminal
//! mapping and best-effort failure behavior are observed through the public
//! artifact boundary.

use super::common;
use ejaj::Config;
use ejaj::api::{CallError, Outgoing, RawResponse, TokenUsage, Transport};
use ejaj::metrics::{
    PassMetrics, PassOutcome, QueueRoundOutcome, RunMetrics, SummaryOutcome,
    SummaryRejectionCategory, TerminalOutcome,
};
use ejaj::pipeline::{Pipeline, Step};
use ejaj::specification::Specification;
use ejaj::tickets::TicketSet;
use ejaj::timing::QueueTimingBreakdown;
use ejaj::triage::TriageState;

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// The scripted pipeline configuration over one external transport.
fn config_for(transport: Arc<dyn Transport>) -> Config {
    Config::new(
        common::test_url(),
        common::test_model(),
        Some(common::TEST_API_KEY.to_string()),
    )
    .with_smoll(
        common::TEST_SMOLL_URL,
        common::TEST_SMOLL_MODEL,
        common::TEST_SMOLL_API_KEY,
    )
    .with_transport(transport)
}

/// Run one production pipeline anchored at the repository and artifact
/// directory the caller prepared.
fn run_pipeline(
    repository: &common::TempRepo,
    run_dir: &Path,
    transport: Arc<dyn Transport>,
) -> Step {
    let config = config_for(transport);
    let mut pipeline = Pipeline::new(&config, "scripted metrics scope", repository.path());
    pipeline.ask(&common::harmless_plan_request(), run_dir)
}

/// Read the metrics document the run persisted at the run directory's top
/// level.
fn read_metrics(run_dir: &Path) -> RunMetrics {
    let bytes = fs::read(run_dir.join("metrics.json")).unwrap_or_else(|error| {
        panic!(
            "the run must persist top-level metrics at {}: {error}",
            run_dir.join("metrics.json").display()
        )
    });
    serde_json::from_slice(&bytes).expect("the persisted metrics document is valid JSON")
}

/// One trajectory JSON value in the production shape. Telemetry counters
/// deliberately differ from the step count so a reader can tell which source
/// each aggregate came from.
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
            "bash_calls": 0,
            "execute_script_calls": 0,
            "token_usage": {
                "input_tokens": usage[0],
                "cached_input_tokens": usage[1],
                "output_tokens": usage[2],
                "reasoning_tokens": usage[3]
            }
        }
    })
}

/// A transport that stops the plan interview before any handoff, either at a
/// turn end or with a provider refusal. Every other request is answered with a
/// plain assistant turn as well, so a leaked downstream request is harmless.
struct PlanStopTransport {
    refusal: bool,
}

impl Transport for PlanStopTransport {
    fn send(&self, _outgoing: &Outgoing) -> Result<RawResponse, CallError> {
        if self.refusal {
            return Err(CallError::Api {
                status: 400,
                message: "the endpoint refused the plan request".to_string(),
            });
        }
        Ok(RawResponse {
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

/// Core entry-point behavior: a completed run under an exact .ejaj ancestor
/// persists its whole-run document at the run directory's top level, beside
/// the other handoffs, with the complete terminal label.
#[test]
fn when_run_completes_given_exact_ejaj_directory_then_writes_top_level_complete_metrics() {
    let repository = common::TempRepo::new();
    let run_dir = repository.path().join(".ejaj/20260101-120000");
    fs::create_dir_all(&run_dir).expect("the run directory is creatable");
    repository.commit_baseline();

    let transport = Arc::new(
        common::ScriptedTransport::new()
            .with_bash_command("printf 'run output\n' > run-output.txt"),
    );
    let step = run_pipeline(&repository, &run_dir, transport);
    assert!(
        matches!(step, Step::Complete),
        "the run completes: {step:?}"
    );

    let metrics = read_metrics(&run_dir);
    assert_eq!(
        metrics.terminal_outcome,
        TerminalOutcome::Complete,
        "the successful run's terminal outcome is the complete label"
    );
    assert_eq!(metrics.mode, "mid", "the configured mode label is recorded");
    assert_eq!(
        metrics.provider_models.get("mid").map(String::as_str),
        Some(common::TEST_MODEL),
        "the configured mid-provider model is recorded"
    );
    assert_eq!(
        metrics.provider_models.get("giga"),
        None,
        "a mid-mode configuration has no giga model"
    );
    assert_eq!(
        metrics.provider_models.get("smoll").map(String::as_str),
        Some(common::TEST_SMOLL_MODEL),
        "the configured Smoll-provider model is recorded"
    );
    assert!(
        run_dir.join("metrics.json").is_file(),
        "metrics.json sits at the run directory's top level"
    );
    assert!(
        run_dir.join("plan.md").is_file(),
        "the same top level holds the plan handoff beside metrics.json"
    );
}

/// Core entry-point behavior: every post-handoff terminal result attempts
/// persistence with the matching terminal outcome, and the primary Step and
/// its diagnostic are returned unchanged.
#[test]
fn when_post_handoff_results_vary_given_retry_write_summary_and_git_failures_then_persists_matching_outcomes_without_replacing_step()
 {
    // Retry: the spec pass ends its turn on both the initial attempt and its
    // fresh-session retry, so the run goes back to the plan.
    let repository = common::TempRepo::new();
    let run_dir = repository.path().join(".ejaj/retry-run");
    fs::create_dir_all(&run_dir).expect("the run directory is creatable");
    repository.commit_baseline();
    let transport = Arc::new(common::ScriptedTransport::new().with_spec(
        common::HandoffScript::Turns(&[common::HandoffReply::TurnEnd]),
    ));
    let step = run_pipeline(&repository, &run_dir, transport);
    assert!(
        matches!(step, Step::Retry),
        "the spec stop retries: {step:?}"
    );
    let retry_metrics = read_metrics(&run_dir);
    assert_eq!(
        retry_metrics.terminal_outcome,
        TerminalOutcome::Retry,
        "a recoverable downstream stop persists the retry outcome"
    );
    assert_eq!(retry_metrics.terminal_detail.as_deref(), Some("spec"));
    let retry_raw = read_metrics_value(&run_dir);
    assert_eq!(retry_raw["spec_pass"]["outcome"], "turn_end");
    assert_eq!(retry_raw["spec_pass"]["retried"], true);
    assert_eq!(retry_raw["spec_pass"]["retry_reason"], "ended its turn");
    assert_eq!(retry_raw["spec_pass"]["succeeded"], false);
    for omitted in ["tickets_pass", "triage", "summary", "estimated_cost_usd"] {
        assert!(
            retry_raw.get(omitted).is_none(),
            "an unreached or unpriced optional field {omitted} must be omitted"
        );
    }

    // Write failure: the plan artifact path is a real directory, so the
    // post-handoff plan write fails after the validated handoff.
    let repository = common::TempRepo::new();
    let run_dir = repository.path().join(".ejaj/write-run");
    fs::create_dir_all(run_dir.join("plan.md")).expect("the obstruction is creatable");
    repository.commit_baseline();
    let transport = Arc::new(common::ScriptedTransport::new());
    let step = run_pipeline(&repository, &run_dir, transport);
    match &step {
        Step::WriteFailed { artifact, error } => {
            assert_eq!(*artifact, "plan", "the failed artifact is the plan handoff");
            assert_eq!(
                error.kind(),
                ErrorKind::IsADirectory,
                "the original artifact-write diagnostic survives: {error}"
            );
        }
        other => panic!("a post-handoff plan write failure stays a WriteFailed: {other:?}"),
    }
    assert_eq!(
        read_metrics(&run_dir).terminal_outcome,
        TerminalOutcome::WriteFailed,
        "a post-handoff write failure persists the write_failed outcome"
    );

    // Summary failure: the endpoint refuses the summary request.
    let repository = common::TempRepo::new();
    let run_dir = repository.path().join(".ejaj/summary-run");
    fs::create_dir_all(&run_dir).expect("the run directory is creatable");
    repository.commit_baseline();
    let transport =
        Arc::new(common::ScriptedTransport::new().with_summary(common::SummaryScript::ApiError));
    let step = run_pipeline(&repository, &run_dir, transport);
    match &step {
        Step::SummaryFailed { reason } => assert!(
            reason.contains("API error"),
            "the original summary diagnostic survives: {reason}"
        ),
        other => panic!("a refused summary stays a SummaryFailed: {other:?}"),
    }
    let summary_failed_metrics = read_metrics(&run_dir);
    assert_eq!(
        summary_failed_metrics.terminal_outcome,
        TerminalOutcome::SummaryFailed,
        "a summary failure persists the summary_failed outcome"
    );
    assert_eq!(
        summary_failed_metrics
            .summary
            .as_ref()
            .expect("the reached summary failure is recorded")
            .outcome,
        SummaryOutcome::FailedApi
    );
    assert_eq!(
        read_metrics_value(&run_dir)["summary"]["outcome"].as_str(),
        Some("failed_api")
    );

    // Git failure: a resolved run with no visible ordinary change cannot
    // commit, so the default-branch finalizer stops at the commit stage.
    let repository = common::TempRepo::new();
    let run_dir = repository.path().join(".ejaj/git-run");
    fs::create_dir_all(&run_dir).expect("the run directory is creatable");
    repository.commit_baseline();
    let transport = Arc::new(common::ScriptedTransport::new());
    let step = run_pipeline(&repository, &run_dir, transport);
    match &step {
        Step::GitFailed { stage, reason } => {
            assert_eq!(*stage, "commit", "the failed Git stage is named: {reason}");
            assert!(
                !reason.is_empty(),
                "the original Git diagnostic survives the metrics attempt"
            );
        }
        other => panic!("a refused empty commit stays a GitFailed: {other:?}"),
    }
    assert_eq!(
        read_metrics(&run_dir).terminal_outcome,
        TerminalOutcome::GitFailed,
        "a Git failure persists the git_failed outcome"
    );
}

/// Edge-case table: only an exact .ejaj path component qualifies. A real run
/// under .ejaj-extra writes no metrics file and keeps its terminal Step.
#[test]
fn when_run_directory_varies_given_exact_ejaj_and_lookalike_ancestors_then_persists_only_exact_match()
 {
    let exact_repository = common::TempRepo::new();
    let exact_run = exact_repository.path().join(".ejaj/exact-run");
    fs::create_dir_all(&exact_run).expect("the exact run directory is creatable");
    exact_repository.commit_baseline();
    let transport = Arc::new(common::ScriptedTransport::new().with_spec(
        common::HandoffScript::Turns(&[common::HandoffReply::TurnEnd]),
    ));
    let exact_step = run_pipeline(&exact_repository, &exact_run, transport);
    assert!(
        matches!(exact_step, Step::Retry),
        "the exact run keeps its terminal step: {exact_step:?}"
    );
    assert!(
        exact_run.join("metrics.json").is_file(),
        "an exact .ejaj ancestor is eligible for persistence"
    );

    let lookalike_repository = common::TempRepo::new();
    let lookalike_run = lookalike_repository.path().join(".ejaj-extra/run");
    fs::create_dir_all(&lookalike_run).expect("the lookalike run directory is creatable");
    lookalike_repository.commit_baseline();
    let transport = Arc::new(common::ScriptedTransport::new().with_spec(
        common::HandoffScript::Turns(&[common::HandoffReply::TurnEnd]),
    ));
    let lookalike_step = run_pipeline(&lookalike_repository, &lookalike_run, transport);
    assert!(
        matches!(lookalike_step, Step::Retry),
        "a lookalike path does not change the pipeline outcome: {lookalike_step:?}"
    );
    assert!(
        !lookalike_run.join("metrics.json").exists(),
        "a .ejaj-extra ancestor is not eligible for persistence"
    );
    assert!(
        lookalike_run.join("plan.md").is_file(),
        "the non-qualifying run still completed its ordinary handoff writes"
    );
}

/// Edge-case table: each pre-handoff exit creates neither the run directory
/// nor a metrics document.
#[test]
fn when_run_is_pre_handoff_given_preflight_turn_end_or_plan_failure_then_creates_no_run_metrics() {
    // Preflight failure: an ordinary uncommitted change gates the run before
    // the plan agent is asked anything.
    let repository = common::TempRepo::new();
    repository.commit_baseline();
    repository.write("dirty.txt", "uncommitted change\n");
    let run_dir = repository.path().join(".ejaj/preflight-run");
    let transport = Arc::new(common::ScriptedTransport::new());
    let step = run_pipeline(&repository, &run_dir, transport);
    assert!(
        matches!(
            step,
            Step::GitFailed {
                stage: "preflight",
                ..
            }
        ),
        "a dirty tree fails preflight: {step:?}"
    );
    assert!(
        !run_dir.exists(),
        "a preflight failure creates no run directory"
    );

    // A plan turn end and a provider refusal both stop before the handoff.
    for (label, refusal) in [("turn end", false), ("plan failure", true)] {
        let repository = common::TempRepo::new();
        repository.commit_baseline();
        let run_dir = repository.path().join(".ejaj/pre-handoff-run");
        let transport = Arc::new(PlanStopTransport { refusal });
        let step = run_pipeline(&repository, &run_dir, transport);
        assert!(
            matches!(step, Step::Retry),
            "{label}: a pre-handoff stop retries: {step:?}"
        );
        assert!(
            !run_dir.exists(),
            "{label}: no run directory is created before a handoff"
        );
        assert!(
            !run_dir.join("metrics.json").exists(),
            "{label}: no metrics document is created before a handoff"
        );
    }
}

/// Regression: a metrics persistence failure stays diagnostic-only, so the
/// original post-handoff terminal Step is returned unchanged.
#[test]
fn when_metrics_writes_fail_given_post_handoff_terminal_step_then_preserves_original_result() {
    let repository = common::TempRepo::new();
    let run_dir = repository.path().join(".ejaj/unwritable-metrics");
    fs::create_dir_all(&run_dir).expect("the run directory is creatable");
    // A file where trajectory collection expects a directory and a directory
    // where metrics.json must be written: both best-effort paths fail.
    fs::write(run_dir.join("trajectories"), "not a directory\n")
        .expect("the trajectory obstruction is writable");
    fs::create_dir_all(run_dir.join("metrics.json"))
        .expect("the metrics obstruction is creatable as a directory");
    repository.commit_baseline();

    let transport = Arc::new(common::ScriptedTransport::new().with_spec(
        common::HandoffScript::Turns(&[common::HandoffReply::TurnEnd]),
    ));
    let step = run_pipeline(&repository, &run_dir, transport);

    assert!(
        matches!(step, Step::Retry),
        "collection and persistence failures leave the terminal Step unchanged: {step:?}"
    );
    assert!(
        run_dir.join("metrics.json").is_dir(),
        "the failed metrics write leaves the obstruction in place"
    );
    assert!(
        run_dir.join("plan.md").is_file(),
        "the ordinary handoff artifacts survive the metrics failure"
    );
}

/// Contract and invariant: trajectory usage is aggregated into per-role
/// records and the root request and retry totals saturate across roles.
#[test]
fn when_run_finishes_given_trajectory_usage_then_totals_requests_and_retries_saturate() {
    let repository = common::TempRepo::new();
    let run_dir = repository.path().join(".ejaj/trajectory-run");
    let trajectories = run_dir.join("trajectories");
    fs::create_dir_all(&trajectories).expect("the trajectory directory is creatable");
    fs::write(
        trajectories.join("a-smoll.traj.json"),
        serde_json::to_vec(&trajectory(
            "smoll",
            2,
            u64::MAX - 5,
            u64::MAX - 7,
            [10, 20, 30, 40],
        ))
        .expect("the first trajectory is JSON"),
    )
    .expect("the first trajectory is writable");
    fs::write(
        trajectories.join("b-smoll.traj.json"),
        serde_json::to_vec(&trajectory("smoll", 3, 10, 10, [1, 2, 3, 4]))
            .expect("the second trajectory is JSON"),
    )
    .expect("the second trajectory is writable");
    repository.commit_baseline();

    let transport = Arc::new(common::ScriptedTransport::new().with_spec(
        common::HandoffScript::Turns(&[common::HandoffReply::TurnEnd]),
    ));
    let step = run_pipeline(&repository, &run_dir, transport);
    assert!(matches!(step, Step::Retry), "the run finishes: {step:?}");

    let metrics = read_metrics(&run_dir);
    let smoll = metrics
        .provider_roles
        .iter()
        .find(|role| role.role == "smoll")
        .expect("the usable smoll trajectories contribute one role record");
    assert_eq!(smoll.invocations, 2, "both smoll invocations are counted");
    assert_eq!(smoll.steps, 5, "the role's steps sum");
    assert_eq!(
        smoll.http_requests,
        u64::MAX,
        "send attempts sum with saturation"
    );
    assert_eq!(
        smoll.http_retries,
        u64::MAX,
        "transient retries sum with saturation"
    );
    assert_eq!(
        smoll.usage,
        TokenUsage {
            input_tokens: 11,
            cached_input_tokens: 22,
            output_tokens: 33,
            reasoning_tokens: 44,
        },
        "the existing token counters sum across invocations"
    );
    assert_eq!(
        metrics.http_requests,
        u64::MAX,
        "the root request total saturates the role records"
    );
    assert_eq!(
        metrics.http_retries,
        u64::MAX,
        "the root retry total saturates the role records"
    );
}

// -- Unattended pass records and triage authorization ----------------------

/// Run one scripted production pipeline under an exact .ejaj run directory and
/// return its terminal step, the metrics it persisted, and the transport that
/// recorded every request.
fn metrics_run(
    transport: common::ScriptedTransport,
) -> (Step, RunMetrics, Arc<common::ScriptedTransport>) {
    let (_repository, _run_dir, step, metrics, transport) =
        metrics_run_keeping_repository(transport);
    (step, metrics, transport)
}

/// The same complete scripted run as [metrics_run], keeping the disposable
/// repository and resolved run directory alive so a test can inspect every
/// artifact and the serialized JSON together.
fn metrics_run_keeping_repository(
    transport: common::ScriptedTransport,
) -> (
    common::TempRepo,
    PathBuf,
    Step,
    RunMetrics,
    Arc<common::ScriptedTransport>,
) {
    let repository = common::TempRepo::new();
    // A finished run commits the working tree; the artifacts under .ejaj are
    // ignored, so the scripted code command leaves the one visible change a
    // completed run records.
    let command = format!(
        "printf 'changed by the run\\n' > '{}'",
        repository.path().join("tracked-change.txt").display()
    );
    let transport = Arc::new(transport.with_bash_command(command));
    let run_dir = repository.path().join(".ejaj/pass-metrics");
    fs::create_dir_all(&run_dir).expect("the run directory is creatable");
    repository.commit_baseline();
    let step = run_pipeline(&repository, &run_dir, transport.clone());
    let metrics = read_metrics(&run_dir);
    (repository, run_dir, step, metrics, transport)
}

/// Read the persisted metrics document as the raw JSON a consumer observes,
/// including the keys serialization deliberately omits.
fn read_metrics_value(run_dir: &Path) -> serde_json::Value {
    let bytes =
        fs::read(run_dir.join("metrics.json")).expect("the run persisted top-level metrics.json");
    serde_json::from_slice(&bytes).expect("the persisted metrics document is valid JSON")
}

/// The three unattended passes whose records the run metrics carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PassKind {
    Spec,
    Tickets,
    Triage,
}

const TURN_END_THEN_HANDOFF: &[common::HandoffReply] =
    &[common::HandoffReply::TurnEnd, common::HandoffReply::Handoff];
const MALFORMED_THEN_HANDOFF: &[common::HandoffReply] = &[
    common::HandoffReply::Malformed,
    common::HandoffReply::Handoff,
];
const TURN_END: &[common::HandoffReply] = &[common::HandoffReply::TurnEnd];
const API_ERROR: &[common::HandoffReply] = &[common::HandoffReply::ApiError];

/// Answer the chosen unattended pass with the scripted replies, leaving every
/// other pass on its normal handoff.
fn transport_for(
    pass: PassKind,
    replies: &'static [common::HandoffReply],
) -> common::ScriptedTransport {
    let transport = common::ScriptedTransport::new();
    match pass {
        PassKind::Spec => transport.with_spec(common::HandoffScript::Turns(replies)),
        PassKind::Tickets => transport.with_tickets_handoff(common::HandoffScript::Turns(replies)),
        PassKind::Triage => transport.with_triage(common::TriageScript::Turns(replies)),
    }
}

/// The pass record one pass kind persisted, or a loud failure when the pass was
/// not reached.
fn pass_metrics(metrics: &RunMetrics, pass: PassKind) -> &PassMetrics {
    match pass {
        PassKind::Spec => metrics.spec_pass.as_ref().expect("the spec pass record"),
        PassKind::Tickets => metrics
            .tickets_pass
            .as_ref()
            .expect("the tickets pass record"),
        PassKind::Triage => {
            &metrics
                .triage
                .as_ref()
                .expect("the triage pass record")
                .pass
        }
    }
}

/// Assert one pass record against independently known literal expectations.
fn assert_pass_record(
    record: &PassMetrics,
    total_elapsed_ms: u64,
    outcome: PassOutcome,
    retried: bool,
    reason: Option<&str>,
    succeeded: bool,
) {
    assert_eq!(record.outcome, outcome, "unexpected pass outcome");
    assert_eq!(
        record.retried, retried,
        "unexpected fresh-session retry state"
    );
    assert_eq!(
        record.retry_reason.as_deref(),
        reason,
        "unexpected first-attempt failure reason"
    );
    assert_eq!(
        record.succeeded, succeeded,
        "unexpected final pass outcome"
    );
    assert!(
        record.elapsed_ms <= total_elapsed_ms,
        "the pass interval {} ms is outside the whole run's {} ms",
        record.elapsed_ms,
        total_elapsed_ms
    );
}

/// The marker only the chosen pass's own requests carry.
fn pass_marker(pass: PassKind) -> &'static str {
    match pass {
        PassKind::Spec => common::SPEC_MARKER,
        PassKind::Tickets => common::TICKETS_MARKER,
        PassKind::Triage => common::TRIAGE_MARKER,
    }
}

/// The three-ticket fixture decoded into the typed set.
fn multi_ticket_set() -> TicketSet {
    let value: serde_json::Value = serde_json::from_str(common::multi_tickets_json())
        .expect("the multi-ticket fixture is JSON");
    serde_json::from_value(value["tickets"].clone()).expect("the fixture carries a ticket set")
}

/// Core entry-point behavior: each of the three unattended passes records its
/// first attempt's reason and the retry that rescued it, and every reached
/// pass records elapsed whole milliseconds and a successful handoff.
#[test]
fn when_unattended_passes_retry_given_eligible_first_failures_then_records_first_reason_and_final_handoff()
 {
    for (pass, replies, expected_reason) in [
        (PassKind::Spec, TURN_END_THEN_HANDOFF, "ended its turn"),
        (
            PassKind::Tickets,
            MALFORMED_THEN_HANDOFF,
            "ended early (malformed response)",
        ),
        (PassKind::Triage, TURN_END_THEN_HANDOFF, "ended its turn"),
    ] {
        let (step, metrics, transport) = metrics_run(transport_for(pass, replies));
        assert!(
            matches!(step, Step::Complete),
            "{pass:?}: the fresh-session retry completes the run: {step:?}"
        );

        for recorded in [PassKind::Spec, PassKind::Tickets, PassKind::Triage] {
            let record = pass_metrics(&metrics, recorded);
            if recorded == pass {
                assert_pass_record(
                    record,
                    metrics.elapsed_ms,
                    PassOutcome::Handoff,
                    true,
                    Some(expected_reason),
                    true,
                );
            } else {
                assert_pass_record(
                    record,
                    metrics.elapsed_ms,
                    PassOutcome::Handoff,
                    false,
                    None,
                    true,
                );
            }
        }

        let requests = transport
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|body| body.to_string().contains(pass_marker(pass)))
            .count();
        assert_eq!(
            requests, 2,
            "{pass:?}: exactly one fresh-session retry is made"
        );
    }
}

/// Core entry-point behavior: a summary pass whose first candidates the format
/// rejects and whose correction the format accepts records the candidate count,
/// the ordered rejection categories and the acceptance in the persisted run
/// artifact, so the convergence story is observable from the JSON alone.
#[test]
fn when_summary_correction_finishes_given_rejected_then_accepted_candidate_then_writes_ordered_summary_metrics()
 {
    const FENCED: &str =
        "\u{60}\u{60}\u{60}\ntest: fenced candidate\n\nA body line.\n\u{60}\u{60}\u{60}\n";
    const REFS: &str = "feat: add the thing\n\nA body line.\n\nRefs: some other reference\n";
    let transport =
        common::ScriptedTransport::new().with_summary(common::SummaryScript::Turns(vec![
            common::SummaryReply::Answer(FENCED.to_string()),
            common::SummaryReply::Answer(REFS.to_string()),
            common::SummaryReply::Answer(common::SCRIPTED_SUMMARY.to_string()),
        ]));
    let (step, metrics, _) = metrics_run(transport);
    assert!(
        matches!(step, Step::Complete),
        "the accepted correction completes the run: {step:?}"
    );

    let summary = metrics
        .summary
        .as_ref()
        .expect("the reached summary pass records its convergence metrics");
    assert!(
        summary.accepted,
        "the corrected third candidate is accepted"
    );
    assert_eq!(
        summary.candidates_generated, 3,
        "two rejected candidates and the accepted one are all generated candidates"
    );
    assert_eq!(
        summary.rejected_candidates,
        vec![
            SummaryRejectionCategory::MarkdownFence,
            SummaryRejectionCategory::RefsFooter,
        ],
        "the rejection categories stay in candidate order"
    );
    assert!(
        summary.elapsed_ms <= metrics.elapsed_ms,
        "the summary interval {} ms is outside the whole run's {} ms",
        summary.elapsed_ms,
        metrics.elapsed_ms
    );
}

/// Contract and invariant: the triage counts partition the draft set and the
/// recorded authorization is the existing predicate over those classifications.
#[test]
fn when_triage_report_varies_given_draft_ready_done_and_blocked_states_then_records_partition_and_authorization()
 {
    for (triage, retried, ready, done, blocked, authorized) in [
        (common::TriageScript::AllReady, false, 3, 0, 0, true),
        (common::TriageScript::DoneAt(0), false, 2, 1, 0, true),
        (
            common::TriageScript::DoneAndBlocked(TriageState::NeedsInfo),
            false,
            1,
            1,
            1,
            false,
        ),
    ] {
        let transport = common::ScriptedTransport::new()
            .with_tickets(multi_ticket_set())
            .with_triage(triage);
        let (step, metrics, _) = metrics_run(transport);
        let triage = metrics.triage.as_ref().expect("the triage pass record");
        assert_eq!(
            triage.draft_count,
            Some(3),
            "the draft count is every ticket"
        );
        assert_eq!(triage.ready_count, Some(ready), "the ready count");
        assert_eq!(triage.done_count, Some(done), "the done count");
        assert_eq!(triage.blocked_count, Some(blocked), "the blocked count");
        assert_eq!(
            triage.ready_count.unwrap() + triage.done_count.unwrap() + triage.blocked_count.unwrap(),
            triage.draft_count.unwrap(),
            "the classifications partition the draft set"
        );
        assert_eq!(
            triage.queue_authorized,
            Some(authorized),
            "authorization matches the classifications"
        );
        assert!(
            triage.pass.succeeded,
            "every valid report is a successful triage handoff"
        );
        assert_eq!(
            triage.pass.retried, retried,
            "the scripted retry state is recorded"
        );
        if authorized {
            assert!(
                matches!(step, Step::Complete | Step::Retry),
                "an authorized report reaches the queue: {step:?}"
            );
        } else {
            assert!(
                matches!(step, Step::Retry),
                "a blocked report returns to the plan: {step:?}"
            );
        }
    }
}

/// Core entry-point behavior: a valid triage report that blocks the queue is
/// still a successful handoff, and no queue round is fabricated for it.
#[test]
fn when_triage_report_blocks_queue_given_valid_handoff_then_records_successful_pass_without_queue_round()
 {
    let transport = common::ScriptedTransport::new()
        .with_triage(common::TriageScript::Blocked(TriageState::NeedsInfo));
    let (step, metrics, transport) = metrics_run(transport);
    assert!(
        matches!(step, Step::Retry),
        "a blocked report returns to the plan: {step:?}"
    );

    let triage = metrics.triage.as_ref().expect("the triage pass record");
    assert!(
        triage.pass.succeeded,
        "a valid blocked report is a successful triage handoff"
    );
    assert!(
        triage.queue_authorized == Some(false),
        "the blocked classifications do not authorize the queue"
    );
    assert_eq!(
        triage.draft_count,
        Some(1),
        "the one draft ticket is counted"
    );
    assert_eq!(triage.ready_count, Some(0), "no ticket is ready");
    assert_eq!(triage.done_count, Some(0), "no ticket is done");
    assert_eq!(triage.blocked_count, Some(1), "the one ticket is blocked");
    assert!(
        metrics.queue_rounds.is_empty(),
        "a blocked report fabricates no queue round"
    );
    assert_eq!(
        metrics.terminal_outcome,
        TerminalOutcome::Retry,
        "the incomplete run persists its retry outcome"
    );
    assert!(
        transport
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|body| !body.to_string().contains(common::CODE_MARKER)),
        "a blocked report sends no code-agent request"
    );
}

/// Edge-case table: each pass records its stable final outcome, retry state and
/// first-attempt reason for turn ends and the eligible failure kinds, and a
/// non-eligible failure records no first-attempt reason because no retry ran.
#[test]
fn when_pass_endings_vary_given_turn_end_and_failure_kinds_then_records_stable_outcomes() {
    for pass in [PassKind::Spec, PassKind::Tickets, PassKind::Triage] {
        for (replies, reason) in [
            (TURN_END_THEN_HANDOFF, "ended its turn"),
            (MALFORMED_THEN_HANDOFF, "ended early (malformed response)"),
        ] {
            let (step, metrics, _) = metrics_run(transport_for(pass, replies));
            assert!(
                matches!(step, Step::Complete),
                "{pass:?}: the retry hands the pass over: {step:?}"
            );
            assert_pass_record(
                pass_metrics(&metrics, pass),
                metrics.elapsed_ms,
                PassOutcome::Handoff,
                true,
                Some(reason),
                true,
            );
        }

        let (step, metrics, _) = metrics_run(transport_for(pass, TURN_END));
        assert!(
            matches!(step, Step::Retry),
            "{pass:?}: two turn ends leave the run retryable: {step:?}"
        );
        assert_pass_record(
            pass_metrics(&metrics, pass),
            metrics.elapsed_ms,
            PassOutcome::TurnEnd,
            true,
            Some("ended its turn"),
            false,
        );

        let (step, metrics, _) = metrics_run(transport_for(pass, API_ERROR));
        assert!(
            matches!(step, Step::Retry),
            "{pass:?}: a refusal leaves the run retryable: {step:?}"
        );
        assert_pass_record(
            pass_metrics(&metrics, pass),
            metrics.elapsed_ms,
            PassOutcome::FailedApi,
            false,
            None,
            false,
        );
    }
}

/// The queue report the scripted complete run must keep writing, byte for
/// byte, beside the new metrics artifact.
const EXPECTED_QUEUE_REPORT: &str = r#"# Ticket queue report

1 ticket: 1 completed, 0 failed, 0 blocked.

## 1-verify-command-output

- Title: Verify command outputs exactly 'ejaj-e2e-ok'
- Status: completed
- Ended: declared
- Gate: `if [ "$(echo 'ejaj-e2e-ok')" != 'ejaj-e2e-ok' ]; then exit 1; fi`
"#;

/// The review report the scripted complete run must keep writing, byte for
/// byte, beside the new metrics artifact.
const EXPECTED_REVIEW_REPORT: &str = r#"# Review report

## Round 1

**Verdict:** Passed (no fix tickets)

### Pre-Flight Audit Scorecard

- **Keeper coverage:** Core entry-point behavior verified.
- **Sediment & coupling:** Zero internal mocks; no private state introspection.
- **Inferred assumptions:** Echo command exits 0 on standard shells.
- **Boundary matrix:** All 5 boundary dimensions evaluated for command output.
- **Code & scope:** Ticket scope and verification gate honoured.

### Findings

None.

### Fix Tickets

None.
"#;

/// Core entry-point behavior: one run that reaches every phase, resolves its
/// queue, passes review, writes an accepted summary and records the commit
/// persists the complete version-1 contract at the run directory's top level.
#[test]
fn when_complete_run_finishes_given_all_phases_and_provider_trajectories_then_writes_versioned_convergence_contract()
 {
    let (_repository, run_dir, step, metrics, _transport) =
        metrics_run_keeping_repository(common::ScriptedTransport::new());
    assert!(
        matches!(step, Step::Complete),
        "every phase completed and the commit was recorded: {step:?}"
    );
    assert_eq!(metrics.terminal_outcome, TerminalOutcome::Complete);
    assert!(
        run_dir.join("metrics.json").is_file(),
        "metrics.json sits at the resolved run directory's top level"
    );
    assert!(
        run_dir.join("plan.md").is_file(),
        "the metrics artifact sits beside the existing top-level handoffs"
    );
    assert!(
        !run_dir.join("tickets").join("metrics.json").exists(),
        "metrics.json is never written under tickets/"
    );

    let raw = read_metrics_value(&run_dir);
    assert_eq!(
        raw["schema_version"].as_u64(),
        Some(1),
        "the versioned contract"
    );
    assert_eq!(
        raw["mode"].as_str(),
        Some("mid"),
        "the canonical configured mode"
    );
    assert_eq!(
        raw["provider_models"]["mid"].as_str(),
        Some(common::TEST_MODEL)
    );
    assert_eq!(
        raw["provider_models"]["smoll"].as_str(),
        Some(common::TEST_SMOLL_MODEL)
    );
    assert!(
        raw["provider_models"].get("giga").is_none(),
        "the inactive giga model is omitted"
    );
    assert_eq!(raw["terminal_outcome"].as_str(), Some("complete"));
    assert!(
        raw.get("terminal_detail").is_none(),
        "a completed run omits terminal_detail"
    );

    let total = raw["elapsed_ms"]
        .as_u64()
        .expect("the run records its whole elapsed time");
    assert_eq!(raw["plan_interview"]["turns"].as_u64(), Some(1));
    assert!(
        raw["plan_interview"]["elapsed_ms"].as_u64().unwrap() <= total,
        "the plan interval is part of the whole run"
    );

    assert_eq!(raw["spec_pass"]["outcome"].as_str(), Some("handoff"));
    assert_eq!(raw["spec_pass"]["succeeded"].as_bool(), Some(true));
    assert_eq!(raw["spec_pass"]["retried"].as_bool(), Some(false));
    assert!(raw["spec_pass"].get("retry_reason").is_none());
    assert_eq!(raw["tickets_pass"]["outcome"].as_str(), Some("handoff"));
    assert_eq!(raw["tickets_pass"]["succeeded"].as_bool(), Some(true));
    assert_eq!(raw["triage"]["outcome"].as_str(), Some("handoff"));
    assert_eq!(raw["triage"]["succeeded"].as_bool(), Some(true));
    assert_eq!(raw["triage"]["draft_count"].as_u64(), Some(1));
    assert_eq!(raw["triage"]["ready_count"].as_u64(), Some(1));
    assert_eq!(raw["triage"]["done_count"].as_u64(), Some(0));
    assert_eq!(raw["triage"]["blocked_count"].as_u64(), Some(0));
    assert_eq!(raw["triage"]["queue_authorized"].as_bool(), Some(true));

    let queue = raw["queue_rounds"]
        .as_array()
        .expect("the reached queue round is recorded");
    assert_eq!(queue.len(), 1);
    assert_eq!(queue[0]["round"].as_u64(), Some(1));
    assert_eq!(queue[0]["outcome"].as_str(), Some("resolved"));
    let tickets = queue[0]["metrics"]["tickets"]
        .as_array()
        .expect("the queue round records its ticket");
    assert_eq!(tickets.len(), 1);
    let ticket = &tickets[0];
    assert_eq!(
        ticket["ticket_id"].as_str(),
        Some("1-verify-command-output")
    );
    assert_eq!(ticket["initial_blocker_count"].as_u64(), Some(0));
    assert_eq!(ticket["downstream_chain_length"].as_u64(), Some(1));
    assert_eq!(ticket["attempt_count"].as_u64(), Some(1));
    assert_eq!(ticket["first_attempt_role"].as_str(), Some("mid"));
    assert_eq!(ticket["first_attempt_outcome"].as_str(), Some("complete"));
    assert!(ticket["retry_role"].is_null());
    assert_eq!(ticket["retry_escalated"].as_bool(), Some(false));
    assert_eq!(ticket["verification_gate_count"].as_u64(), Some(1));
    assert_eq!(ticket["final_status"].as_str(), Some("completed"));
    assert_eq!(ticket["ending"].as_str(), Some("declared"));
    assert!(ticket["failed_blocker"].is_null());

    assert_eq!(raw["transition_gate"]["runs"].as_u64(), Some(0));
    assert_eq!(raw["transition_gate"]["elapsed_ms"].as_u64(), Some(0));

    let reviews = raw["review_rounds"]
        .as_array()
        .expect("the reached review round is recorded");
    assert_eq!(reviews.len(), 1);
    assert_eq!(reviews[0]["round"].as_u64(), Some(1));
    assert_eq!(reviews[0]["outcome"].as_str(), Some("passed"));
    assert_eq!(reviews[0]["retried"].as_bool(), Some(false));
    assert_eq!(reviews[0]["succeeded"].as_bool(), Some(true));
    assert_eq!(reviews[0]["findings"].as_u64(), Some(0));
    assert_eq!(reviews[0]["fixes"].as_u64(), Some(0));
    assert!(raw.get("review_stalled_breaker_tripped").is_none());

    let summary = &raw["summary"];
    assert_eq!(summary["outcome"].as_str(), Some("accepted"));
    assert_eq!(summary["accepted"].as_bool(), Some(true));
    assert_eq!(summary["candidates_generated"].as_u64(), Some(1));
    assert_eq!(
        summary["rejected_candidates"].as_array().map(Vec::len),
        Some(0)
    );

    let roles = raw["provider_roles"]
        .as_array()
        .expect("the active provider roles are recorded");
    assert_eq!(roles.len(), 2, "only the active mid and smoll roles run");
    assert_eq!(roles[0]["role"].as_str(), Some("mid"));
    assert_eq!(roles[1]["role"].as_str(), Some("smoll"));
    assert_eq!(roles[0]["model"].as_str(), Some(common::TEST_MODEL));
    assert_eq!(roles[1]["model"].as_str(), Some(common::TEST_SMOLL_MODEL));
    assert_eq!(roles[0]["invocations"].as_u64(), Some(6));
    assert_eq!(roles[1]["invocations"].as_u64(), Some(1));
    assert_eq!(raw["http_requests"].as_u64(), Some(8));
    assert_eq!(raw["http_retries"].as_u64(), Some(0));
}

/// Edge case: usable provider usage whose roles the configuration leaves
/// unpriced serializes every usage record while omitting both the role-local
/// and the run-total cost fields entirely.
#[test]
fn when_inactive_role_trajectory_is_present_given_mid_mode_then_omits_inactive_role_and_costs() {
    let repository = common::TempRepo::new();
    let run_dir = repository.path().join(".ejaj/unpriced-metrics");
    let trajectories = run_dir.join("trajectories");
    fs::create_dir_all(&trajectories).expect("the trajectory directory is creatable");
    let seeded = serde_json::json!({
        "phase": "code",
        "role": "giga",
        "model": "giga-test-model",
        "steps_taken": 5,
        "exit_status": "complete",
        "messages": [],
        "telemetry": {
            "provider_send_attempts": 10,
            "transient_retries": 3,
            "bash_calls": 0,
            "execute_script_calls": 0,
            "token_usage": {
                "input_tokens": 11,
                "cached_input_tokens": 22,
                "output_tokens": 33,
                "reasoning_tokens": 44
            }
        }
    });
    fs::write(trajectories.join("a-giga.traj.json"), seeded.to_string())
        .expect("the seeded trajectory is writable");
    let command = format!(
        "printf 'changed by the run\\n' > '{}'",
        repository.path().join("tracked-change.txt").display()
    );
    let transport = Arc::new(common::ScriptedTransport::new().with_bash_command(command));
    repository.commit_baseline();
    let step = run_pipeline(&repository, &run_dir, transport);
    assert!(
        matches!(step, Step::Complete),
        "the scripted run still completes: {step:?}"
    );

    let raw = read_metrics_value(&run_dir);
    let roles = raw["provider_roles"]
        .as_array()
        .expect("the reachable roles are recorded");
    assert_eq!(
        roles.len(),
        2,
        "only configured mid and smoll roles are emitted"
    );
    assert!(
        roles
            .iter()
            .all(|role| role["role"].as_str() != Some("giga")),
        "an inactive giga trajectory is skipped"
    );
    for role in roles {
        assert!(
            role.get("estimated_cost_usd").is_none(),
            "an unpriced role omits its estimate: {role}"
        );
        assert!(role["invocations"].as_u64().unwrap() >= 1);
    }
    assert!(
        raw.get("estimated_cost_usd").is_none(),
        "a run with no priced role omits the total estimate"
    );
}

/// Contract and invariant: adding the metrics artifact leaves every existing
/// handoff at its path with its content, keeps the public summary seam intact,
/// and never lets the new file leak into the ticket directory.
#[test]
fn when_metrics_artifact_is_written_given_existing_handoffs_then_preserves_artifact_paths_contents_and_public_behavior()
 {
    let (_repository, run_dir, step, metrics, _transport) =
        metrics_run_keeping_repository(common::ScriptedTransport::new());
    assert!(
        matches!(step, Step::Complete),
        "the run completion is unchanged"
    );
    assert_eq!(metrics.terminal_outcome, TerminalOutcome::Complete);

    let read = |name: &str| {
        fs::read_to_string(run_dir.join(name))
            .unwrap_or_else(|error| panic!("{name} must remain readable: {error}"))
    };

    assert_eq!(
        read("plan.md"),
        common::harmless_plan_fixture().to_markdown(),
        "plan.md keeps its exact rendering at its top-level path"
    );
    let expected_spec = Specification::from_markdown(common::harmless_spec_markdown())
        .expect("the scripted specification parses")
        .to_markdown();
    assert_eq!(read("spec.md"), expected_spec);

    let fixture_value: serde_json::Value =
        serde_json::from_str(common::harmless_tickets_json()).expect("the ticket fixture is JSON");
    let fixture_set: TicketSet = serde_json::from_value(fixture_value["tickets"].clone())
        .expect("the ticket fixture carries a ticket set");
    assert_eq!(
        read("tickets/1-verify-command-output.md"),
        fixture_set.tickets[0].to_markdown()
    );
    let mut ticket_files: Vec<String> = fs::read_dir(run_dir.join("tickets"))
        .expect("the tickets directory is readable")
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    ticket_files.sort();
    assert_eq!(
        ticket_files,
        vec!["1-verify-command-output.md", "queue-status.json"],
        "the ticket directory keeps exactly its existing contents"
    );

    assert_eq!(read("queue-report.md"), EXPECTED_QUEUE_REPORT);
    assert_eq!(read("review.md"), EXPECTED_REVIEW_REPORT);
    assert_eq!(read("change-summary.md"), common::SCRIPTED_SUMMARY);

    let triage = read("ticket-triage.md");
    assert!(
        triage.starts_with(
            "# Ticket triage report\n\n## 1-verify-command-output: Verify command outputs exactly 'ejaj-e2e-ok'\n\n"
        ),
        "the triage report keeps its header and ticket block: {triage}"
    );
    assert!(triage.contains("- **Category:** enhancement\n- **State:** ready-for-agent"));
    assert!(
        triage.contains(
            "- **Notes:**\n  - Audited `1-verify-command-output` against the repository."
        )
    );
    assert!(triage.contains(
        "**Target behavior:** TRIAGE-ONLY-BRIEF: implement Verify command outputs exactly 'ejaj-e2e-ok'."
    ));
    assert!(triage.ends_with('\n') && !triage.ends_with("\n\n"));

    assert!(run_dir.join("metrics.json").is_file());
    assert!(!run_dir.join("tickets").join("metrics.json").exists());

    let public_config = config_for(Arc::new(common::ScriptedTransport::new()));
    let outcome = ejaj::loops::summary::summarize(
        &public_config,
        "scripted metrics scope",
        "resolved",
        "a queue report",
        "a review report",
        &[],
    );
    match outcome {
        ejaj::loops::summary::Outcome::Summary(text) => {
            assert_eq!(
                text,
                common::SCRIPTED_SUMMARY,
                "the public summary seam is unchanged"
            );
        }
        other => panic!("the public summary seam still accepts a canonical answer: {other:?}"),
    }
}

// -- Queue rounds, transition gates and review convergence -----------------

/// Run one scripted production pipeline over a disposable repository whose root
/// already carries the given committed files, so a standard test manifest can
/// select the transition-gate command.
fn metrics_run_with_root_files(
    transport: common::ScriptedTransport,
    files: &[(&str, &str)],
) -> (Step, RunMetrics) {
    let repository = common::TempRepo::new();
    for (path, contents) in files {
        repository.write(path, contents);
    }
    let command = format!(
        "printf 'changed by the run\\n' > '{}'",
        repository.path().join("tracked-change.txt").display()
    );
    let transport = Arc::new(transport.with_bash_command(command));
    let run_dir = repository.path().join(".ejaj/queue-and-gate-metrics");
    fs::create_dir_all(&run_dir).expect("the run directory is creatable");
    repository.commit_baseline();
    let step = run_pipeline(&repository, &run_dir, transport);
    let metrics = read_metrics(&run_dir);
    (step, metrics)
}

/// A package.json whose test script runs the given shell command.
fn package_json_with_test(script: &str) -> String {
    serde_json::json!({ "scripts": { "test": script } }).to_string()
}

/// A real external suite command that counts its own invocations in the
/// repository and fails when the given shell condition holds, so the same
/// command serves both a repair ticket's own gate and the transition gate.
fn counted_gate_script(failing_condition: &str) -> String {
    format!(
        "count=0\n\
         if [ -f gate-count.txt ]; then count=$(cat gate-count.txt); fi\n\
         count=$((count + 1))\n\
         echo \"$count\" > gate-count.txt\n\
         if {failing_condition}; then\n\
           echo \"suite gate failed on invocation $count\"\n\
           exit 1\n\
         fi\n\
         echo \"suite gate passed on invocation $count\"\n"
    )
}

/// Core entry-point behavior: an executed queue round and a precompleted round
/// are both recorded as round one with their own outcome, the executed round's
/// ticket records, and the queue's own timing rather than a second measurement.
#[test]
fn when_queue_rounds_run_given_execution_and_precompleted_sets_then_records_one_based_outcomes_and_copied_timing()
 {
    let (step, metrics, _) = metrics_run(common::ScriptedTransport::new());
    assert!(
        matches!(step, Step::Complete),
        "the executed run completes: {step:?}"
    );
    assert_eq!(
        metrics.queue_rounds.len(),
        1,
        "one executed queue round is recorded"
    );
    let executed = &metrics.queue_rounds[0];
    assert_eq!(executed.round, 1, "the executed round is one-based");
    assert_eq!(
        executed.outcome,
        QueueRoundOutcome::Resolved,
        "the resolved execution keeps its outcome"
    );
    assert_eq!(
        executed.metrics.tickets.len(),
        1,
        "the executed round carries its ticket execution record"
    );
    assert_eq!(
        executed.metrics.tickets[0].ticket_id, "1-verify-command-output",
        "the ticket record names the ticket the round executed"
    );
    let timing = executed.metrics.timing;
    assert!(
        timing.total_elapsed_ms > 0,
        "the executed round carries the queue's measured total"
    );
    assert!(
        timing.total_elapsed_ms <= metrics.elapsed_ms,
        "the copied round interval {} ms stays inside the {} ms run",
        timing.total_elapsed_ms,
        metrics.elapsed_ms
    );
    let attributed = timing.model_attempt_ms
        + timing.executed_tool_ms
        + timing.verification_gate_ms
        + timing.residual_overhead_ms;
    assert!(
        attributed <= timing.total_elapsed_ms,
        "the copied attribution {attributed} ms never exceeds the round's {} ms",
        timing.total_elapsed_ms
    );
    assert!(
        timing.model_attempt_ms > 0 && timing.executed_tool_ms > 0,
        "the executed round carries its measured model and tool contributions: {timing:?}"
    );

    let (step, metrics, _) =
        metrics_run(common::ScriptedTransport::new().with_triage(common::TriageScript::AllDone));
    assert!(
        !matches!(step, Step::WriteFailed { .. }),
        "the precompleted run reaches its ordinary terminal step: {step:?}"
    );
    assert_eq!(
        metrics.queue_rounds.len(),
        1,
        "the precompleted set still records its actual round"
    );
    let precompleted = &metrics.queue_rounds[0];
    assert_eq!(
        precompleted.round, 1,
        "the precompleted round is one-based as well"
    );
    assert_eq!(
        precompleted.outcome,
        QueueRoundOutcome::Resolved,
        "an already complete set resolves"
    );
    assert!(
        precompleted.metrics.tickets.is_empty(),
        "a precompleted round ran no code attempt and records no ticket"
    );
    assert_eq!(
        precompleted.metrics.timing,
        QueueTimingBreakdown::default(),
        "a precompleted round copies the queue's existing unmeasured timing"
    );
}

/// Edge-case table and contract: a missing transition-gate manifest contributes
/// no invocation and no time, while passing, repair-producing and terminating
/// invocations are each counted with the whole milliseconds they actually ran.
#[test]
fn when_transition_gate_is_absent_or_invoked_given_pass_repair_and_termination_then_counts_only_actual_durations()
 {
    let (step, metrics, _) = metrics_run(common::ScriptedTransport::new());
    assert!(
        matches!(step, Step::Complete),
        "the gate-absent run completes: {step:?}"
    );
    assert_eq!(
        metrics.transition_gate,
        ejaj::metrics::TransitionGateMetrics::default(),
        "a missing manifest contributes neither an invocation nor elapsed time"
    );

    let passing = package_json_with_test("exit 0");
    let (step, metrics) = metrics_run_with_root_files(
        common::ScriptedTransport::new(),
        &[("package.json", &passing)],
    );
    assert!(
        matches!(step, Step::Complete),
        "the passing gate completes: {step:?}"
    );
    assert_eq!(
        metrics.transition_gate.runs, 1,
        "one passing invocation is counted"
    );
    assert!(
        metrics.transition_gate.elapsed_ms > 0,
        "the passing invocation's actual whole milliseconds are accumulated"
    );

    let repair = package_json_with_test("sh gate-script.sh");
    let repair_suite = counted_gate_script("[ \"$count\" -eq 1 ]");
    let (step, metrics) = metrics_run_with_root_files(
        common::ScriptedTransport::new(),
        &[("package.json", &repair), ("gate-script.sh", &repair_suite)],
    );
    assert!(
        matches!(step, Step::Complete),
        "the repaired run completes after the repairing queue round: {step:?}"
    );
    assert_eq!(
        metrics.transition_gate.runs, 2,
        "the failing and the repaired invocations are both counted"
    );
    assert!(
        metrics.transition_gate.elapsed_ms > 0,
        "both repair-path invocations contribute their actual durations"
    );
    assert_eq!(
        metrics.queue_rounds.len(),
        2,
        "the repair ticket's queue round is recorded beside the original"
    );
    assert_eq!(
        metrics
            .queue_rounds
            .iter()
            .map(|round| round.round)
            .collect::<Vec<_>>(),
        vec![1, 2],
        "both queue rounds remain one-based"
    );

    let terminating = package_json_with_test("sh gate-script.sh");
    let terminating_suite = counted_gate_script("[ $((count % 2)) -eq 1 ]");
    let (step, metrics) = metrics_run_with_root_files(
        common::ScriptedTransport::new(),
        &[
            ("package.json", &terminating),
            ("gate-script.sh", &terminating_suite),
        ],
    );
    assert!(
        matches!(step, Step::Retry),
        "the terminating gate returns to the plan: {step:?}"
    );
    assert_eq!(
        metrics.transition_gate.runs, 3,
        "every terminating invocation is counted"
    );
    assert!(
        metrics.transition_gate.elapsed_ms > 0,
        "the terminating invocations contribute their actual durations"
    );
    assert_eq!(
        metrics.queue_rounds.len(),
        3,
        "the three queue rounds before termination are retained"
    );
}

/// Core review behavior: a retry-eligible turn end, an API failure, and valid
/// reports each record their own outcome, handoff, report counts and
/// convergence in the persisted run metrics.
#[test]
fn when_review_rounds_end_given_retry_failure_and_valid_reports_then_records_outcome_handoff_counts_and_convergence()
 {
    let (_, retry, _) =
        metrics_run(common::ScriptedTransport::new().with_review(common::ReviewScript::TurnEnd));
    let value = serde_json::to_value(&retry).expect("metrics serialize");
    let round = &value["review_rounds"][0];
    assert_eq!(round["round"], 1, "the first review pass is one-based");
    assert_eq!(
        round["outcome"], "turn_end",
        "a turn end is its own outcome"
    );
    assert_eq!(
        round["retried"], true,
        "the retry-eligible turn end is retried in a fresh session"
    );
    assert_eq!(
        round["retry_reason"], "ended its turn",
        "the first failure reason survives the retry"
    );
    assert_eq!(round["succeeded"], false, "a turn end is not handed off");
    assert!(
        round.get("findings").is_none(),
        "no report means no finding count"
    );
    assert!(
        round.get("fixes").is_none(),
        "no report means no fix count"
    );

    let (_, failed, _) =
        metrics_run(common::ScriptedTransport::new().with_review(common::ReviewScript::ApiError));
    let value = serde_json::to_value(&failed).expect("metrics serialize");
    let round = &value["review_rounds"][0];
    assert_eq!(round["outcome"], "failed", "an API failure records failure");
    assert_eq!(
        round["retried"], false,
        "an API failure is not retried in a fresh session"
    );
    assert_eq!(
        round["succeeded"], false,
        "a failed pass is not handed off"
    );
    assert!(
        round.get("findings").is_none(),
        "no report means no finding count"
    );
    assert!(
        round.get("fixes").is_none(),
        "no report means no fix count"
    );

    let (_, fixed, _) = metrics_run(
        common::ScriptedTransport::new().with_review(common::ReviewScript::FixOnceThenPass),
    );
    let value = serde_json::to_value(&fixed).expect("metrics serialize");
    let first = &value["review_rounds"][0];
    assert_eq!(first["round"], 1);
    assert_eq!(first["outcome"], "fixes_queued");
    assert_eq!(
        first["succeeded"], true,
        "a valid fix report is handed off"
    );
    assert_eq!(
        first["findings"], 1,
        "the report's finding is counted"
    );
    assert_eq!(
        first["fixes"], 1,
        "the report's fix is counted"
    );
    let second = &value["review_rounds"][1];
    assert_eq!(second["round"], 2, "the converging pass is round two");
    assert_eq!(second["outcome"], "passed");
    assert_eq!(second["succeeded"], true);
    assert_eq!(second["findings"], 0);
    assert_eq!(second["fixes"], 0);
}

/// Boundaries: the existing review round cap and circuit breaker are unchanged,
/// every capped or circuit-broken round retains its report counts and handoff
/// status, and the final round records its terminal convergence outcome.
#[test]
fn when_review_round_limits_are_reached_given_stalled_fixes_or_round_cap_then_preserves_existing_bounds_and_records_report_counts()
 {
    let (step, metrics, _) = metrics_run(
        common::ScriptedTransport::new().with_review(common::ReviewScript::AlwaysFixes),
    );
    assert!(
        matches!(step, Step::Retry),
        "the capped run returns to the plan: {step:?}"
    );
    let value = serde_json::to_value(&metrics).expect("metrics serialize");
    let rounds = value["review_rounds"]
        .as_array()
        .expect("review rounds array");
    assert_eq!(
        rounds.len(),
        10,
        "the existing ten review-round cap is unchanged"
    );
    assert_eq!(
        value["queue_rounds"].as_array().map(Vec::len),
        Some(10),
        "the existing nine fix-queue rounds plus the initial round are unchanged"
    );
    for (index, round) in rounds.iter().enumerate() {
        assert_eq!(
            round["round"],
            index + 1,
            "each capped pass stays one-based"
        );
        assert_eq!(round["succeeded"], true);
        assert_eq!(
            round["findings"], 1,
            "every capped report keeps its finding count"
        );
        assert_eq!(
            round["fixes"], 1,
            "every capped report keeps its fix count"
        );
        if index + 1 < 10 {
            assert_eq!(round["outcome"], "fixes_queued");
        }
    }
    assert_eq!(
        rounds.last().expect("a final capped round")["outcome"],
        "round_cap_reached",
        "the final pass records the round cap rather than convergence"
    );

    // Circuit breaker: round 1 reports f1-edge-case, round 2 reports f2-edge-case,
    // and round 3 re-opens f1-edge-case, stopping the loop at round 3.
    struct ReopeningReviewTransport {
        base: common::ScriptedTransport,
        review_round: std::sync::atomic::AtomicUsize,
    }
    impl Transport for ReopeningReviewTransport {
        fn send(&self, outgoing: &Outgoing) -> Result<RawResponse, CallError> {
            let serialized = outgoing.body.to_string();
            if serialized.contains(common::REVIEW_MARKER) {
                let round = self
                    .review_round
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                    + 1;
                let args = if round == 3 {
                    let mut parsed: serde_json::Value =
                        serde_json::from_str(&common::fixing_review_json(1)).unwrap();
                    parsed["review"]["fixes"][0]["id"] =
                        serde_json::Value::String("r3-1-verify-edge-case".to_string());
                    parsed.to_string()
                } else {
                    common::fixing_review_json(round)
                };
                return Ok(RawResponse {
                    status: 200,
                    body: serde_json::json!({
                        "choices": [{
                            "message": {
                                "role": "assistant",
                                "content": null,
                                "tool_calls": [{
                                    "id": format!("call_review_{round}"),
                                    "type": "function",
                                    "function": {
                                        "name": "finish_review",
                                        "arguments": args,
                                    }
                                }]
                            }
                        }]
                    })
                    .to_string(),
                });
            }
            self.base.send(outgoing)
        }
    }

    let repository = common::TempRepo::new();
    let run_dir = repository.path().join(".ejaj/circuit-breaker-metrics");
    fs::create_dir_all(&run_dir).expect("the run directory is creatable");
    repository.commit_baseline();
    let command = format!(
        "printf 'changed by the run\\n' > '{}'",
        repository.path().join("tracked-change.txt").display()
    );
    let transport = Arc::new(ReopeningReviewTransport {
        base: common::ScriptedTransport::new().with_bash_command(command),
        review_round: std::sync::atomic::AtomicUsize::new(0),
    });
    let step = run_pipeline(&repository, &run_dir, transport);
    assert!(
        matches!(step, Step::Retry),
        "the circuit-broken run returns to the plan: {step:?}"
    );
    let broken = read_metrics_value(&run_dir);
    let broken_rounds = broken["review_rounds"]
        .as_array()
        .expect("circuit-broken review rounds");
    assert_eq!(broken_rounds.len(), 3);
    assert_eq!(broken_rounds[0]["outcome"], "fixes_queued");
    assert_eq!(broken_rounds[1]["outcome"], "fixes_queued");
    assert_eq!(broken_rounds[2]["outcome"], "circuit_broken");
    assert_eq!(broken_rounds[2]["succeeded"], true);
    assert_eq!(broken_rounds[2]["findings"], 1);
    assert_eq!(broken_rounds[2]["fixes"], 1);
}

/// Regression: a post-handoff write failure after a completed queue round and
/// review pass keeps every partial convergence measurement and preserves the
/// primary write-failure step rather than replacing it.
#[test]
fn when_queue_or_review_write_fails_given_completed_measurements_then_persists_partial_convergence_without_replacing_step()
 {
    let repository = common::TempRepo::new();
    let command = format!(
        "printf 'changed by the run\\n' > '{}'",
        repository.path().join("tracked-change.txt").display()
    );
    let transport = Arc::new(
        common::ScriptedTransport::new()
            .with_review(common::ReviewScript::Pass)
            .with_bash_command(command),
    );
    let run_dir = repository.path().join(".ejaj/write-failure-metrics");
    repository.write("package.json", &package_json_with_test("exit 0"));
    fs::create_dir_all(run_dir.join("review.md")).expect("the blocking directory is creatable");
    repository.commit_baseline();
    let step = run_pipeline(&repository, &run_dir, transport);
    let metrics = read_metrics(&run_dir);
    assert!(
        matches!(step, Step::WriteFailed { .. }),
        "the primary write failure step is preserved: {step:?}"
    );
    assert!(
        format!("{step:?}").contains("review"),
        "the preserved step names the review artifact: {step:?}"
    );
    let value = serde_json::to_value(&metrics).expect("metrics serialize");
    assert_eq!(
        value["queue_rounds"].as_array().map(Vec::len),
        Some(1),
        "the completed queue round survives the later write failure"
    );
    assert_eq!(value["queue_rounds"][0]["round"], 1);
    assert_eq!(value["queue_rounds"][0]["outcome"], "resolved");
    let rounds = value["review_rounds"]
        .as_array()
        .expect("review rounds array");
    assert_eq!(
        rounds.len(),
        1,
        "the completed review pass survives the write failure"
    );
    assert_eq!(rounds[0]["round"], 1);
    assert_eq!(rounds[0]["outcome"], "passed");
    assert_eq!(rounds[0]["succeeded"], true);
    assert_eq!(rounds[0]["findings"], 0);
    assert_eq!(rounds[0]["fixes"], 0);
    assert_eq!(
        value["transition_gate"]["runs"], 1,
        "the completed transition-gate invocation survives the review write failure"
    );
    assert!(
        value["transition_gate"]["elapsed_ms"]
            .as_u64()
            .unwrap()
            > 0
    );
}

/// Zero-usage boundary: configured token prices still produce an explicit zero
/// estimate when real scripted provider trajectories contain no usage.
#[test]
fn when_provider_usage_is_zero_given_configured_role_prices_then_records_a_zero_cost_estimate() {
    let repository = common::TempRepo::new();
    let run_dir = repository.path().join(".ejaj/zero-priced-usage");
    fs::create_dir_all(&run_dir).expect("the run directory is creatable");
    let command = format!(
        "printf 'changed by the run\n' > '{}'",
        repository.path().join("tracked-change.txt").display()
    );
    let transport = Arc::new(common::ScriptedTransport::new().with_bash_command(command));
    let values: Vec<(String, String)> = vec![
        ("LLM_MID_URL".to_string(), common::test_url()),
        ("LLM_MID_MODEL".to_string(), common::test_model()),
        (
            "LLM_MID_API_KEY".to_string(),
            common::TEST_API_KEY.to_string(),
        ),
        (
            "LLM_MID_INPUT_PRICE_PER_MILLION_USD".to_string(),
            "2.0".to_string(),
        ),
        (
            "LLM_SMOLL_URL".to_string(),
            common::TEST_SMOLL_URL.to_string(),
        ),
        (
            "LLM_SMOLL_MODEL".to_string(),
            common::TEST_SMOLL_MODEL.to_string(),
        ),
        (
            "LLM_SMOLL_API_KEY".to_string(),
            common::TEST_SMOLL_API_KEY.to_string(),
        ),
        (
            "LLM_SMOLL_OUTPUT_PRICE_PER_MILLION_USD".to_string(),
            "3.0".to_string(),
        ),
    ];
    let environment = |name: &str| {
        values
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
    };
    let config = Config::from_environment(&environment)
        .expect("the priced provider roles are configured")
        .with_transport(transport);
    repository.commit_baseline();
    let mut pipeline = Pipeline::new(&config, "scripted metrics scope", repository.path());

    let step = pipeline.ask(&common::harmless_plan_request(), &run_dir);
    assert!(
        matches!(step, Step::Complete),
        "the priced run completes: {step:?}"
    );
    let metrics = read_metrics(&run_dir);
    assert_eq!(
        metrics.provider_roles.len(),
        2,
        "both configured roles were invoked"
    );
    for role in &metrics.provider_roles {
        assert_eq!(role.usage, TokenUsage::default(), "scripted usage is zero");
        assert_eq!(
            role.estimated_cost_usd,
            Some(0.0),
            "configured prices estimate zero cost"
        );
    }
    assert_eq!(metrics.estimated_cost_usd, Some(0.0));
}

/// Public seam: callers measure whole milliseconds, aggregate a real
/// trajectory directory, persist only under an exact `.ejaj` component, and
/// classify a canonicalizer rejection, all against known literal values.
#[test]
fn when_callers_use_the_run_metrics_seams_given_known_inputs_then_observe_the_stable_contract() {
    assert_eq!(
        ejaj::metrics::duration_ms(Duration::from_micros(1_999)),
        1,
        "a duration floors to whole milliseconds"
    );
    assert_eq!(
        ejaj::metrics::duration_ms(Duration::from_millis(u64::MAX)),
        u64::MAX,
        "a duration beyond the u64 millisecond range saturates"
    );

    assert_eq!(
        ejaj::metrics::classify_summary_rejection(
            "the answer is empty: a summary is a header line, one blank line and a non-empty body"
        ),
        SummaryRejectionCategory::Empty,
        "an empty canonical answer is classified as empty"
    );
    assert_eq!(
        ejaj::metrics::classify_summary_rejection(
            "body line 2 is 121 characters; the limit is 120"
        ),
        SummaryRejectionCategory::LineOverLimit,
        "an over-limit body line is classified separately"
    );

    let repository = common::TempRepo::new();
    let session = repository.path().join(".ejaj/public-seams");
    let trajectories = session.join("trajectories");
    fs::create_dir_all(&trajectories).expect("the trajectory directory is creatable");
    fs::write(
        trajectories.join("a-mid.traj.json"),
        serde_json::to_vec(&trajectory("mid", 3, 2, 1, [10, 4, 5, 6]))
            .expect("the trajectory serializes"),
    )
    .expect("the trajectory is writable");

    let config = config_for(Arc::new(common::ScriptedTransport::new()));
    let roles = ejaj::metrics::collect_trajectory_usage(&trajectories, &config)
        .expect("the readable trajectory aggregates");
    assert_eq!(roles.len(), 1, "one configured role owns the trajectory");
    assert_eq!(roles[0].role, "mid");
    assert_eq!(
        roles[0].model,
        common::TEST_MODEL,
        "the configured model names the role, not the trajectory's stale copy"
    );
    assert_eq!(roles[0].invocations, 1);
    assert_eq!(roles[0].steps, 3);
    assert_eq!(roles[0].http_requests, 2);
    assert_eq!(roles[0].http_retries, 1);
    assert_eq!(
        roles[0].usage,
        TokenUsage {
            input_tokens: 10,
            cached_input_tokens: 4,
            output_tokens: 5,
            reasoning_tokens: 6,
        }
    );

    let metrics = RunMetrics {
        schema_version: 1,
        elapsed_ms: 2_500,
        ..RunMetrics::default()
    };
    assert!(
        ejaj::metrics::persist_run_metrics(
            repository.path(),
            Path::new(".ejaj/public-seams"),
            &metrics,
        )
        .expect("persisting under the exact session component succeeds"),
        "the artifact is written below the .ejaj boundary"
    );
    let raw: serde_json::Value = serde_json::from_slice(
        &fs::read(session.join("metrics.json")).expect("the persisted metrics are readable"),
    )
    .expect("the persisted metrics are JSON");
    assert_eq!(raw["schema_version"], 1);
    assert_eq!(raw["elapsed_ms"], 2_500);
    assert!(
        !ejaj::metrics::persist_run_metrics(
            repository.path(),
            Path::new(".ejaj-extra/nope"),
            &metrics,
        )
        .expect("a lookalike destination stays a no-op"),
        "a destination outside .ejaj is never written"
    );
}
