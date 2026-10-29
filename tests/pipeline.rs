#![allow(clippy::cognitive_complexity, clippy::manual_let_else)]

//! The end-to-end pipeline: one `Pipeline::ask` must walk a plan agent through
//! `finish_plan`, write the plan, run the spec pass, write the specification,
//! run the tickets pass, write one ticket file per ticket, and then drive the
//! ticket queue -- all through the production types, anchored to a disposable
//! repository that begins clean and is left unchanged.
//!
//! Every test comes in two forms: the default replays a committed fixture with
//! no network, and the `live_` twin (ignored) runs against a real model --
//! recording the fixture when `EJAJ_LLM_RECORD=1` is set. See `tests/common/mod.rs`.

mod common;

#[path = "support/retry_tests.rs"]
mod retry_tests;

use ejaj::Config;
use ejaj::api::{CallError, Outgoing, RawResponse, Transport};
use ejaj::loops::plan::{Outcome as PlanOutcome, Session};
use ejaj::pipeline::{Pipeline, Step};
use ejaj::plan_document::Plan;
use ejaj::specification::Specification;
use ejaj::tickets::TicketSet;
use ejaj::triage::TriageState;

use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A scratch directory outside the repository, named per run so a parallel or
/// abandoned run never collides with another. The per-process ordinal keeps two
/// tests that start within one clock tick -- the clock resolution is coarser
/// than its nanosecond field suggests -- from sharing a directory.
fn scratch_dir() -> PathBuf {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the clock is after the Unix epoch")
        .as_nanos();
    let ordinal = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "ejaj-pipeline-{}-{}-{}",
        std::process::id(),
        nanos,
        ordinal
    ))
}

/// The one ticket id the scripted pipeline produces, trimmed the way the
/// queue names it in its elapsed and report lines.
const GATE_TICKET_ID: &str = "1-verify-command-output";

/// The ids of the tickets produced by the scripted pipeline.
fn recorded_ticket_ids() -> Vec<String> {
    vec![GATE_TICKET_ID.to_string()]
}

/// The ticket files the pipeline wrote, in name order.
fn ticket_files(dir: &Path) -> Vec<PathBuf> {
    let entries = fs::read_dir(dir.join("tickets"))
        .unwrap_or_else(|e| common::failed("the ticket directory", &e.to_string()));
    let mut files: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
        .collect();
    files.sort();
    files
}

/// The whole run: one `Pipeline::ask` over the harmless plan, then the `Step`,
/// every artifact, the ticket files, and the repository's before/after status.
/// Nothing here re-implements orchestration or writes an artifact.
fn run_the_pipeline(config: &Config) {
    let dir = scratch_dir();
    // The run's artifacts land inside its own disposable repository, so a
    // successful run has the changes it made to commit.
    let repository = common::TempRepo::at(dir.clone());
    repository.commit_baseline();
    let before = repository.status();

    let mut pipeline = Pipeline::new(config, common::scope(), repository.path());
    let completed = match pipeline.ask(&common::harmless_plan_request(), &dir) {
        // The queue resolved every ticket.
        Step::Complete => true,
        // A stopped queue is a recoverable state, not a failure: the ticket
        // bodies, their gates and the code agent's steps all come from the
        // recorded model, so either terminal step is acceptable.
        Step::Retry => false,
        Step::WriteFailed { artifact, error } => common::failed(
            "the pipeline run",
            &format!("could not write the {artifact} artifact: {error}"),
        ),
        Step::SummaryFailed { reason } => common::failed(
            "the summary phase",
            &format!("the run could not be summarised: {reason}"),
        ),
        // The one staged Git failure: the preflight of the disposable
        // repository, the staging of its changes and the commit that follows
        // are all stages this run must pass.
        Step::GitFailed { stage, reason } => {
            panic!("the disposable repository must pass the {stage} stage: {reason}")
        }
    };

    // The artifact is the harness re-render of the typed plan the boundary
    // exchanged, so it must parse, carry the mandatory content and render back
    // to the same bytes.
    let plan_text = fs::read_to_string(dir.join("plan.md"))
        .unwrap_or_else(|e| common::failed("the plan artifact", &e.to_string()));
    let plan = match Plan::from_markdown(&plan_text) {
        Ok(plan) => plan,
        Err(e) => common::failed(
            "the plan artifact",
            &format!("`plan.md` does not parse: {e}"),
        ),
    };
    assert!(!plan.goal.trim().is_empty(), "the plan had no goal");
    assert!(!plan.decisions.is_empty(), "the plan carried no decisions");
    assert!(
        !plan.constraints.is_empty(),
        "the plan carried no constraints"
    );
    assert_eq!(
        plan.to_markdown(),
        plan_text,
        "`plan.md` is not the typed plan re-rendered"
    );
    // The mid recording is accepted only when the model copied the settled
    // plan it was handed; a mismatch is a hard failure, so the staged recording
    // is discarded rather than committed.
    if plan != common::harmless_plan_fixture() {
        common::failed(
            "the mid plan copy",
            "the model did not copy the settled plan verbatim",
        );
    }

    // The specification artifact must be a complete document that ends with
    // exactly one newline.
    let spec_text = fs::read_to_string(dir.join("spec.md"))
        .unwrap_or_else(|e| common::failed("the spec artifact", &e.to_string()));
    let specification = match Specification::from_markdown(&spec_text) {
        Ok(specification) => specification,
        Err(e) => common::failed(
            "the spec artifact",
            &format!("`spec.md` does not parse: {e}"),
        ),
    };
    assert!(
        !specification.feature_name.trim().is_empty(),
        "the specification had no title"
    );
    assert!(
        !specification.user_stories.is_empty(),
        "the specification had no user stories"
    );
    assert!(
        spec_text.ends_with('\n'),
        "`spec.md` does not end with a newline"
    );
    assert!(
        !spec_text.ends_with("\n\n"),
        "`spec.md` ends with a blank line"
    );

    // One markdown file per id in the recorded ticket set, in emitted order,
    // each holding a non-empty document that ends with exactly one newline.
    let mut expected: Vec<String> = recorded_ticket_ids()
        .iter()
        .map(|id| format!("{id}.md"))
        .collect();
    let files = ticket_files(&dir);
    assert!(!files.is_empty(), "the pipeline wrote no ticket files");
    let mut names: Vec<String> = files
        .iter()
        .map(|path| {
            path.file_name()
                .expect("a ticket file has a name")
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    // The fixture is the recorded set only while replaying; a live re-record
    // writes it after the run, so the identity check waits for a readable one.
    if !expected.is_empty() {
        expected.sort();
        names.sort();
        assert_eq!(
            names, expected,
            "the ticket directory does not hold one file per recorded id"
        );
    }
    for path in &files {
        let text = fs::read_to_string(path)
            .unwrap_or_else(|e| common::failed("a ticket artifact", &e.to_string()));
        assert!(!text.is_empty(), "{} is empty", path.display());
        assert!(
            text.ends_with('\n') && !text.ends_with("\n\n"),
            "{} does not end with exactly one newline",
            path.display()
        );
    }

    // The triage report sits in the run directory beside the plan and the
    // specification, rendered from the validated typed report.
    let triage_text = fs::read_to_string(dir.join("ticket-triage.md"))
        .unwrap_or_else(|e| common::failed("the triage artifact", &e.to_string()));
    assert!(
        triage_text.starts_with("# Ticket triage report\n"),
        "the triage report has no heading: {triage_text}"
    );
    assert!(
        triage_text.ends_with('\n') && !triage_text.ends_with("\n\n"),
        "the triage report does not end with exactly one newline"
    );

    // The queue's status snapshot lives beside the ticket files: a direct JSON
    // object keyed by ticket id, its keys exactly the ticket artifacts, and
    // every value one of the four allowed statuses.
    let status_text = fs::read_to_string(dir.join("tickets/queue-status.json"))
        .unwrap_or_else(|e| common::failed("the queue status artifact", &e.to_string()));
    let snapshot: serde_json::Value = serde_json::from_str(&status_text)
        .unwrap_or_else(|e| common::failed("the queue status artifact", &e.to_string()));
    let statuses = snapshot.as_object().unwrap_or_else(|| {
        common::failed(
            "the queue status artifact",
            "the JSON root is not an object",
        )
    });

    let mut status_ids: Vec<&str> = statuses.keys().map(String::as_str).collect();
    let mut artifact_ids: Vec<&str> = files
        .iter()
        .filter_map(|path| path.file_stem().and_then(|stem| stem.to_str()))
        .collect();
    status_ids.sort();
    artifact_ids.sort();
    assert_eq!(
        status_ids, artifact_ids,
        "the queue status keys are not the ticket artifact ids"
    );

    for (id, value) in statuses {
        let status = value.as_str().unwrap_or_else(|| {
            common::failed(
                "the queue status artifact",
                &format!("{id} is not a string"),
            )
        });
        assert!(
            ["pending", "in_progress", "completed", "failed"].contains(&status),
            "`{id}` has status `{status}`, which is not one of the four"
        );
    }

    if completed {
        for (id, value) in statuses {
            assert_eq!(
                value.as_str(),
                Some("completed"),
                "a completed queue left `{id}` unresolved"
            );
        }
    }

    // The end-of-run report sits beside the plan and the specification, names
    // every ticket the queue ran, and ends like every other artifact.
    let report_text = fs::read_to_string(dir.join("queue-report.md"))
        .unwrap_or_else(|e| common::failed("the queue report artifact", &e.to_string()));
    assert!(!report_text.is_empty(), "the queue report is empty");
    assert!(
        report_text.starts_with("# Ticket queue report\n"),
        "the queue report has no heading: {report_text}"
    );
    assert!(
        report_text.ends_with('\n') && !report_text.ends_with("\n\n"),
        "the queue report does not end with exactly one newline"
    );
    for id in artifact_ids.iter() {
        assert!(
            report_text.contains(&format!("## {id}")),
            "the queue report does not name `{id}`: {report_text}"
        );
    }

    // The review report sits beside the queue report, holding the rendered
    // scorecard and verdict for each completed review round.
    let review_text = fs::read_to_string(dir.join("review.md"))
        .unwrap_or_else(|e| common::failed("the review artifact", &e.to_string()));
    assert!(
        review_text.starts_with("# Review report\n\n## Round 1\n"),
        "the review report has no heading: {review_text}"
    );
    assert!(
        review_text.ends_with('\n') && !review_text.ends_with("\n\n"),
        "the review report does not end with exactly one newline"
    );

    // The change summary sits in the same run directory as the plan, the
    // specification, the tickets and the queue report: the scripted summary
    // pass answered with the canonical text, and the artifact holds exactly
    // that -- one terminal LF, no extra blank line.
    let summary_text = fs::read_to_string(dir.join("change-summary.md"))
        .unwrap_or_else(|e| common::failed("the change summary artifact", &e.to_string()));
    assert_eq!(
        summary_text,
        common::SCRIPTED_SUMMARY,
        "the change summary artifact is not the canonical summary the pass produced"
    );
    assert!(
        summary_text.ends_with('\n') && !summary_text.ends_with("\n\n"),
        "the change summary does not end with exactly one newline: {summary_text:?}"
    );
    assert!(
        !summary_text.contains("\r"),
        "the change summary carries a carriage return: {summary_text:?}"
    );

    // The run must not change its own repository. The scratch artifacts live
    // outside it, and the harmless plan touches no file inside it.
    let after = repository.status();
    assert_eq!(
        before, after,
        "the pipeline run changed the disposable repository's status"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn when_running_given_recorded_pipeline_then_preserves_phase_behavior() {
    let config = common::scripted_pipeline_config();
    run_the_pipeline(&config);
}

// ---------------------------------------------------------------------------
// The summary phase after the queue
//
// These tests drive the same scripted pipeline from `tests/common`, with two
// knobs: which way the queue ends, and how the summary pass answers. Every one
// of them asserts the request the production code actually sent, so the report
// content, the queue outcome and the routing are checked as the pass saw them.
// ---------------------------------------------------------------------------

/// The three ways a scripted queue can end, each with what it needs from the run
/// directory beforehand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueueOutcome {
    /// The ticket's gate passes: every ticket completes.
    Resolved,
    /// The ticket's gate fails: the queue drains without completing it.
    Incomplete,
    /// The status snapshot cannot be written: the queue stops before the ticket
    /// starts.
    Stopped,
    /// The ticket's gate fails after a measurable pause: the queue drains
    /// incomplete with a real, measurable gate interval.
    SlowIncomplete,
}

impl QueueOutcome {
    /// The gate this outcome needs.
    fn gate(self) -> common::GateScript {
        match self {
            QueueOutcome::Incomplete => common::GateScript::Failing,
            QueueOutcome::SlowIncomplete => common::GateScript::SlowFailing,
            QueueOutcome::Resolved | QueueOutcome::Stopped => common::GateScript::Passing,
        }
    }

    /// Plant whatever the queue needs to end this way, before the pipeline runs.
    fn prepare(self, dir: &Path) {
        if self == QueueOutcome::Stopped {
            // A directory where the status snapshot must go: the queue's first
            // persistence attempt fails, so it stops before any ticket starts.
            fs::create_dir_all(dir.join("tickets/queue-status.json"))
                .expect("the status path is plantable as a directory");
        }
    }
}

/// One scripted pipeline run over a fresh scratch directory, prepared by
/// `prepare` before the pipeline touches it -- which is how a test plants a
/// stale artifact or a path the queue cannot write.
fn scripted_run_prepared(
    outcome: QueueOutcome,
    summary: common::SummaryScript,
    prepare: impl FnOnce(&Path),
) -> (Step, PathBuf, Arc<common::ScriptedTransport>) {
    let (config, transport) = common::scripted_pipeline_with(outcome.gate(), summary);
    let dir = scratch_dir();
    fs::create_dir_all(&dir).expect("the scratch directory is creatable");
    // The pipeline's root is the run's own disposable repository, and the
    // artifacts land inside it, so a successful run commits the changes it
    // made. Whatever the test plants first is baselined, so the preflight
    // still sees a clean tree.
    let repository = common::TempRepo::at(dir.clone());
    outcome.prepare(&dir);
    prepare(&dir);
    repository.commit_baseline();

    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), &dir);
    // The repository and the artifacts are the same directory, so the handle
    // must outlive this helper: the caller reads the run's files and removes
    // the directory itself.
    repository.keep();
    (step, dir, transport)
}

/// One scripted pipeline run with nothing planted beforehand.
fn scripted_run(
    outcome: QueueOutcome,
    summary: common::SummaryScript,
) -> (Step, PathBuf, Arc<common::ScriptedTransport>) {
    scripted_run_prepared(outcome, summary, |_| {})
}

/// The last request the run made, as text: on a run that reached the summary
/// phase this is the summary request, which is the one the pass was handed.
fn last_request(transport: &common::ScriptedTransport) -> String {
    transport
        .requests
        .lock()
        .unwrap()
        .last()
        .expect("the run made at least one request")
        .to_string()
}

/// The reason a fatal summary failure carries.
fn summary_reason(step: &Step) -> &str {
    match step {
        Step::SummaryFailed { reason } => reason.as_str(),
        other => panic!("expected a fatal summary failure, got {other:?}"),
    }
}

/// The change summary artifact, read back or reported loudly.
fn summary_artifact(dir: &Path) -> String {
    fs::read_to_string(dir.join("change-summary.md"))
        .unwrap_or_else(|e| common::failed("the change summary artifact", &e.to_string()))
}

/// The resolved run: exactly one summary request after the queue's five, sent to
/// the mid provider, carrying this invocation's report and outcome, with the canonical
/// artifact in the same run directory as the other handoffs.
#[test]
fn the_pipeline_summarises_the_resolved_queue_once_after_the_report() {
    let (step, dir, transport) =
        scripted_run(QueueOutcome::Resolved, common::SummaryScript::Summary);

    assert!(
        matches!(step, Step::Complete),
        "a resolved, summarised run completes: {step:?}"
    );

    // Six queue-driven steps (the three handoffs, the triage pass, and the
    // ticket's two code steps), one review pass, then one summary request.
    let urls = transport.urls_called.lock().unwrap().clone();
    assert_eq!(
        urls.len(),
        8,
        "the pipeline must run the review pass and one summary request after the queue: {urls:?}"
    );
    // The seven non-summary steps run on the mid provider; Summary always
    // runs on the dedicated Smoll Anthropic Messages provider.
    for url in &urls[..7] {
        assert_eq!(
            url,
            &common::test_url(),
            "mid mode runs every non-summary step on the mid provider"
        );
    }
    assert_eq!(
        urls[7],
        common::TEST_SMOLL_URL,
        "the summary pass must run on the Smoll provider"
    );
    let body = last_request(&transport);
    assert!(
        body.contains("Conventional-Commits-style header"),
        "the summary prompt must be sent: {body}"
    );
    assert!(
        body.contains("## 1-verify-command-output"),
        "the summary request must carry the report this run generated: {body}"
    );
    assert!(
        body.contains("- Status: completed"),
        "the report's ticket section must reach the pass: {body}"
    );
    assert!(
        body.contains("Resolved"),
        "the queue outcome must reach the pass: {body}"
    );

    // The report the request carried is the one in the run directory, and the
    // summary artifact is the canonical text the pass produced.
    let report = fs::read_to_string(dir.join("queue-report.md"))
        .unwrap_or_else(|e| common::failed("the queue report artifact", &e.to_string()));
    assert!(report.contains("## 1-verify-command-output"), "{report}");
    assert_eq!(summary_artifact(&dir), common::SCRIPTED_SUMMARY);

    let _ = fs::remove_dir_all(&dir);
}

/// The summary request carries the report this invocation generated, never the
/// `queue-report.md` an earlier invocation left behind.
#[test]
fn the_summary_request_carries_the_generated_report_not_a_stale_one() {
    let (step, dir, transport) = scripted_run_prepared(
        QueueOutcome::Resolved,
        common::SummaryScript::Summary,
        |dir| {
            fs::write(
                dir.join("queue-report.md"),
                "# Ticket queue report\n\n## 9-stale\n\n- Status: completed\n",
            )
            .expect("the stale report is plantable");
        },
    );
    assert!(matches!(step, Step::Complete), "{step:?}");

    let body = last_request(&transport);
    assert!(
        body.contains("## 1-verify-command-output"),
        "the pass must see this run's report: {body}"
    );
    assert!(
        !body.contains("9-stale"),
        "the pass must never see a previous invocation's report: {body}"
    );

    // The fresh report replaced the stale file as well.
    let report = fs::read_to_string(dir.join("queue-report.md"))
        .unwrap_or_else(|e| common::failed("the queue report artifact", &e.to_string()));
    assert!(report.contains("## 1-verify-command-output"), "{report}");
    assert!(!report.contains("9-stale"), "{report}");

    let _ = fs::remove_dir_all(&dir);
}

/// The queue report file write is best-effort: when it fails the queue still
/// resolves, and the summary is composed from the markdown generated in memory.
#[test]
fn a_queue_report_write_failure_still_summarises_the_generated_report() {
    let (step, dir, transport) = scripted_run_prepared(
        QueueOutcome::Resolved,
        common::SummaryScript::Summary,
        |dir| {
            // A directory where the report file must go: the queue's own write
            // cannot succeed. The sentinel file keeps the directory visible to
            // Git, so the default-branch stash restores it instead of cleaning
            // an empty untracked directory away.
            fs::create_dir_all(dir.join("queue-report.md"))
                .expect("the report path is plantable as a directory");
            fs::write(
                dir.join("queue-report.md/sentinel"),
                "keep the directory
",
            )
            .expect("the sentinel is writable");
        },
    );

    assert!(
        matches!(step, Step::Complete),
        "a failed report write is a warning and changes nothing: {step:?}"
    );
    assert!(
        dir.join("queue-report.md").is_dir(),
        "the report write must really have failed"
    );

    let body = last_request(&transport);
    assert!(
        body.contains("## 1-verify-command-output"),
        "the generated report must reach the pass even when the file write failed: {body}"
    );
    assert!(
        body.contains("- Status: completed"),
        "the generated report's ticket section must reach the pass: {body}"
    );
    assert_eq!(summary_artifact(&dir), common::SCRIPTED_SUMMARY);

    let _ = fs::remove_dir_all(&dir);
}

/// An incomplete queue is summarised too, and keeps its recoverable step: the
/// summary succeeded, so the human is back at `plan>>`.
#[test]
fn the_incomplete_queue_is_summarised_and_stays_recoverable() {
    let (step, dir, transport) =
        scripted_run(QueueOutcome::Incomplete, common::SummaryScript::Summary);

    assert!(
        matches!(step, Step::Retry),
        "a summarised incomplete queue keeps its recoverable step: {step:?}"
    );
    assert_eq!(
        transport.urls_called.lock().unwrap().len(),
        9,
        "the failing first gate earns a second attempt before the summary"
    );

    let body = last_request(&transport);
    assert!(
        body.contains("## 1-verify-command-output"),
        "the pass must see the report: {body}"
    );
    assert!(
        body.contains("- Status: failed"),
        "the report must name the failed ticket: {body}"
    );
    assert!(
        body.contains("Incomplete"),
        "the outcome must reach the pass: {body}"
    );
    assert_eq!(summary_artifact(&dir), common::SCRIPTED_SUMMARY);

    let _ = fs::remove_dir_all(&dir);
}

/// A stopped queue is summarised with its stopped ticket and reason, and keeps
/// its recoverable step.
#[test]
fn the_stopped_queue_is_summarised_and_stays_recoverable() {
    let (step, dir, transport) =
        scripted_run(QueueOutcome::Stopped, common::SummaryScript::Summary);

    assert!(
        matches!(step, Step::Retry),
        "a summarised stopped queue keeps its recoverable step: {step:?}"
    );
    // The queue stopped before the code agent, and the summary still ran.
    assert_eq!(
        transport.urls_called.lock().unwrap().len(),
        5,
        "three handoff steps, the triage pass and the summary pass"
    );

    let body = last_request(&transport);
    assert!(
        body.contains("Stopped"),
        "the outcome must reach the pass: {body}"
    );
    assert!(
        body.contains("could not persist the queue status"),
        "the stop reason must reach the pass: {body}"
    );
    assert!(
        body.contains("- Status: pending"),
        "the report must name the ticket that never started: {body}"
    );
    assert_eq!(summary_artifact(&dir), common::SCRIPTED_SUMMARY);

    let _ = fs::remove_dir_all(&dir);
}

/// A summary pass that fails on the API is fatal, whatever the queue made of the
/// tickets: no step claims success, no step is recoverable, and no summary
/// artifact is left behind.
#[test]
fn a_summary_api_failure_is_fatal_for_every_queue_outcome() {
    for outcome in [
        QueueOutcome::Resolved,
        QueueOutcome::Incomplete,
        QueueOutcome::Stopped,
    ] {
        let (step, dir, transport) = scripted_run(outcome, common::SummaryScript::ApiError);

        let reason = summary_reason(&step);
        assert!(
            reason.contains("API error"),
            "{outcome:?}: the reason must name the failure: {reason}"
        );
        assert_eq!(
            summary_requests(&transport).len(),
            1,
            "{outcome:?}: a failed initial call buys no correction request"
        );
        assert!(
            !dir.join("change-summary.md").exists(),
            "{outcome:?}: a failed summary leaves no artifact"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}

/// A summary pass that never stops calling tools hits its step limit, which is
/// fatal rather than a retry.
#[test]
fn a_summary_step_limit_is_fatal() {
    let (step, dir, _transport) = scripted_run(
        QueueOutcome::Resolved,
        common::SummaryScript::EndlessToolCalls,
    );

    let reason = summary_reason(&step);
    assert!(
        reason.contains("step limit"),
        "the reason must name the step limit: {reason}"
    );
    assert!(!dir.join("change-summary.md").exists());

    let _ = fs::remove_dir_all(&dir);
}

/// A summary pass whose transport fails is fatal too, after the loop's own
/// bounded retries.
#[test]
fn a_summary_transport_failure_is_fatal() {
    let (step, dir, _transport) = scripted_run(
        QueueOutcome::Resolved,
        common::SummaryScript::TransportError,
    );

    let reason = summary_reason(&step);
    assert!(
        reason.contains("transport error"),
        "the reason must name the transport failure: {reason}"
    );
    assert!(!dir.join("change-summary.md").exists());

    let _ = fs::remove_dir_all(&dir);
}

/// A summary pass that ends without a usable answer is fatal: a native turn
/// with no content blocks is an unusable response, and a whitespace-only answer
/// is an empty summary.
#[test]
fn a_summary_without_a_final_answer_is_fatal() {
    for (summary, expected) in [
        (common::SummaryScript::NoContent, "malformed response"),
        (common::SummaryScript::Blank, "empty"),
    ] {
        let (step, dir, _transport) = scripted_run(QueueOutcome::Resolved, summary);

        let reason = summary_reason(&step);
        assert!(
            reason.contains(expected),
            "the reason must say the answer was unusable: {reason}"
        );
        assert!(!dir.join("change-summary.md").exists());
        let _ = fs::remove_dir_all(&dir);
    }
}

/// A summary answer the format refuses is a generation failure carrying the
/// validator's own reason, never a summary the run claims to have written.
#[test]
fn a_malformed_summary_answer_is_fatal() {
    for (answer, expected) in [
        (
            "Here is the summary:\n\nfeat: add the thing\n\nBody line.\n",
            "preamble",
        ),
        ("feat: add the thing\n", "no body"),
        ("feat: Add the thing\n\nBody line.\n", "lowercase"),
    ] {
        let (step, dir, _transport) = scripted_run(
            QueueOutcome::Resolved,
            common::SummaryScript::Answer(answer),
        );

        let reason = summary_reason(&step);
        assert!(
            reason.contains(expected),
            "{answer:?}: the reason must name what was wrong: {reason}"
        );
        assert!(!dir.join("change-summary.md").exists());
        let _ = fs::remove_dir_all(&dir);
    }
}

/// A summary the pipeline cannot store is a fatal write failure naming the
/// `change-summary` artifact, and the queue's own report is unaffected.
#[test]
fn a_summary_artifact_write_failure_names_the_change_summary() {
    let (step, dir, _transport) = scripted_run_prepared(
        QueueOutcome::Resolved,
        common::SummaryScript::Summary,
        |dir| {
            // A directory where the summary file must go: the write cannot
            // succeed.
            fs::create_dir_all(dir.join("change-summary.md"))
                .expect("the summary path is plantable as a directory");
        },
    );

    match &step {
        Step::WriteFailed { artifact, .. } => assert_eq!(*artifact, "change-summary"),
        other => panic!("expected a fatal write failure, got {other:?}"),
    }
    assert!(
        dir.join("queue-report.md").is_file(),
        "the queue's own report still landed"
    );

    let _ = fs::remove_dir_all(&dir);
}

/// The existing fix-once-then-pass flow, unchanged by the wider bound: one
/// accepted fix queue runs, the second review passes, and the run completes.
#[test]
fn when_reviewing_given_one_fix_then_completes_after_second_review() {
    let (config, transport) = common::scripted_pipeline_with_review(
        common::ReviewScript::FixOnceThenPass,
        common::SummaryScript::Summary,
    );
    let dir = scratch_dir();
    let repository = common::TempRepo::at(dir.clone());
    repository.commit_baseline();

    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), &dir);
    assert!(
        matches!(step, Step::Complete),
        "expected Step::Complete, got {step:?}"
    );

    // 6 initial steps (3 handoffs + triage + 2 code) + 1 review + 2 fix-ticket
    // code steps + 1 review + 1 summary = 11 requests.
    assert_eq!(transport.urls_called.lock().unwrap().len(), 11);

    // Both initial and fix tickets exist on disk.
    assert!(dir.join("tickets/1-verify-command-output.md").is_file());
    assert!(dir.join("tickets/r1-1-verify-edge-case.md").is_file());

    // Cumulative queue status has both tickets completed.
    let status_text = fs::read_to_string(dir.join("tickets/queue-status.json")).unwrap();
    let status_json: serde_json::Value = serde_json::from_str(&status_text).unwrap();
    assert_eq!(status_json["1-verify-command-output"], "completed");
    assert_eq!(status_json["r1-1-verify-edge-case"], "completed");

    // Cumulative queue report names both tickets.
    let report_text = fs::read_to_string(dir.join("queue-report.md")).unwrap();
    assert!(report_text.contains("2 tickets: 2 completed, 0 failed, 0 blocked."));
    assert!(report_text.contains("## 1-verify-command-output"));
    assert!(report_text.contains("## r1-1-verify-edge-case"));

    // Review artifact holds both Round 1 (Needs fixes) and Round 2 (Passed).
    let review_text = fs::read_to_string(dir.join("review.md")).unwrap();
    assert!(
        review_text.contains("## Round 1\n\n**Verdict:** Needs fixes (1 finding, 1 fix ticket)")
    );
    assert!(review_text.contains("## Round 2\n\n**Verdict:** Passed (no fix tickets)"));

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn when_review_exceeds_max_rounds_given_persistent_findings_then_summarises_and_returns_retry() {
    let (config, transport) = common::scripted_pipeline_with_review(
        common::ReviewScript::AlwaysFixes,
        common::SummaryScript::Summary,
    );
    let dir = scratch_dir();
    let repository = common::TempRepo::at(dir.join("repository"));

    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), &dir);
    assert!(
        matches!(step, Step::Retry),
        "expected Step::Retry at round cap, got {step:?}"
    );

    // The three permitted fix queues ran; the fourth review's fix ticket was
    // reported but never queued.
    assert!(dir.join("tickets/r1-1-verify-edge-case.md").is_file());
    assert!(dir.join("tickets/r2-1-verify-edge-case.md").is_file());
    assert!(dir.join("tickets/r3-1-verify-edge-case.md").is_file());
    assert!(!dir.join("tickets/r4-1-verify-edge-case.md").exists());

    let review_text = fs::read_to_string(dir.join("review.md")).unwrap();
    for round in 1..=4 {
        assert!(
            review_text.contains(&format!("## Round {round}")),
            "review round {round} must be rendered: {review_text}"
        );
    }
    assert!(!review_text.contains("## Round 5"), "{review_text}");

    // The summary still ran and noted the unresolved review findings.
    let summary_req = last_request(&transport);
    assert!(
        summary_req.contains("maximum of 4 review rounds"),
        "{summary_req}"
    );
    assert_eq!(summary_artifact(&dir), common::SCRIPTED_SUMMARY);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn when_review_ends_early_given_turn_end_or_api_error_then_summarises_and_returns_retry() {
    for review_script in [
        common::ReviewScript::TurnEnd,
        common::ReviewScript::ApiError,
    ] {
        let (config, transport) =
            common::scripted_pipeline_with_review(review_script, common::SummaryScript::Summary);
        let dir = scratch_dir();
        let repository = common::TempRepo::at(dir.join("repository"));

        let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
        let step = pipeline.ask(&common::harmless_plan_request(), &dir);
        assert!(
            matches!(step, Step::Retry),
            "expected Step::Retry, got {step:?}"
        );

        let summary_req = last_request(&transport);
        assert!(
            summary_req.contains("review pass did not complete"),
            "{summary_req}"
        );
        assert_eq!(summary_artifact(&dir), common::SCRIPTED_SUMMARY);

        let _ = fs::remove_dir_all(&dir);
    }
}

#[test]
fn when_writing_review_artifact_given_unwritable_path_then_returns_write_failed_for_review() {
    let (step, dir, _transport) = scripted_run_prepared(
        QueueOutcome::Resolved,
        common::SummaryScript::Summary,
        |dir| {
            fs::create_dir_all(dir.join("review.md"))
                .expect("the review path is plantable as a directory");
        },
    );

    match &step {
        Step::WriteFailed { artifact, .. } => assert_eq!(*artifact, "review"),
        other => panic!("expected WriteFailed for review, got {other:?}"),
    }

    let _ = fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// The bounded review loop: three fix queues and a fourth review
//
// One Pipeline::ask runs the original queue and then at most three
// review-generated fix queues, each followed by its own review: four reviews in
// all. Any passing review ends the loop immediately, a queue that stops or
// drains incomplete ends the run without another review, and only a resolved
// queue whose last review passed reaches the summary and the Git commit.
// ---------------------------------------------------------------------------

/// The phrase only the review kickoff carries, so the review turns can be told
/// from the queue's, the triage pass's and the summary's by their own prompts.
const REVIEW_KICKOFF: &str = "The ticket queue resolved every ticket in the current round.";

/// Every review request the run made, in order, as whole request text. The
/// review prompt alone tells the pass's own requests from the queue's, so the
/// count and the order are the production pass's own.
fn review_requests(transport: &common::ScriptedTransport) -> Vec<String> {
    transport
        .requests
        .lock()
        .unwrap()
        .iter()
        .map(|body| body.to_string())
        .filter(|body| body.contains(REVIEW_KICKOFF))
        .collect()
}

/// The decoded user messages of the review turns, in order: the specification,
/// the executed tickets, the cumulative queue report and the prior review
/// history each review round was handed.
fn review_contexts(transport: &common::ScriptedTransport) -> Vec<String> {
    user_contents(transport)
        .into_iter()
        .filter(|content| content.contains(REVIEW_KICKOFF))
        .collect()
}

/// The ticket documents the queue handed to the code agent, in the order the
/// queue first reached each ticket. One ticket is handed over once per
/// code-agent turn, so the documents are deduplicated by their own bytes.
fn executed_ticket_payloads(transport: &common::ScriptedTransport) -> Vec<String> {
    let mut payloads: Vec<String> = Vec::new();
    for payload in code_payloads(transport) {
        if !payloads.contains(&payload) {
            payloads.push(payload);
        }
    }
    payloads
}

/// The fix tickets the scripted review rounds emit, in round order: the three
/// fix sets the queue executes.
const FIX_TICKET_IDS: [&str; 3] = [
    "r1-1-verify-edge-case",
    "r2-1-verify-edge-case",
    "r3-1-verify-edge-case",
];

/// Every ticket the scripted three-fix-round run executes, in order: the
/// original ticket and then the three review-generated fix sets.
fn executed_ticket_ids() -> Vec<&'static str> {
    std::iter::once("1-verify-command-output")
        .chain(FIX_TICKET_IDS)
        .collect()
}

/// One scripted review run against a fresh disposable repository whose baseline
/// is committed first, so a run that completes has its own changes to commit.
/// The prepare closure plants whatever a case needs before the preflight reads
/// the tree.
fn review_run(
    review: common::ReviewScript,
    prepare: impl FnOnce(&Path),
) -> (Step, common::TempRepo, Arc<common::ScriptedTransport>) {
    review_run_with_gate(common::GateScript::Passing, review, prepare)
}

/// The same run with a chosen original-queue gate, so a case can decide how the
/// original queue ends independently of how the review would answer.
fn review_run_with_gate(
    gate: common::GateScript,
    review: common::ReviewScript,
    prepare: impl FnOnce(&Path),
) -> (Step, common::TempRepo, Arc<common::ScriptedTransport>) {
    let (config, transport) = common::scripted_pipeline_with_gate_and_review(
        gate,
        review,
        common::SummaryScript::Summary,
    );
    // Whatever a case plants is committed as part of the baseline, so the
    // preflight still reads a clean tree and the run has its own changes to
    // commit.
    let repository = common::TempRepo::new();
    prepare(repository.path());
    repository.commit_baseline();

    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), repository.path());
    (step, repository, transport)
}

/// The same repeated review/fix run with a chosen code-agent script, so every
/// queue round can carry a real uninstrumented retry backoff.
fn review_run_with_code(
    review: common::ReviewScript,
    code: common::CodeScript,
) -> (Step, common::TempRepo, Arc<common::ScriptedTransport>) {
    let transport = Arc::new(
        common::ScriptedTransport::new()
            .with_gate(common::GateScript::Passing)
            .with_review(review)
            .with_summary(common::SummaryScript::Summary)
            .with_code(code),
    );
    let config = Config::new(
        common::test_url(),
        common::test_model(),
        Some(common::TEST_API_KEY.to_string()),
    )
    .with_smoll(
        common::TEST_SMOLL_URL,
        common::TEST_SMOLL_MODEL,
        common::TEST_SMOLL_API_KEY,
    )
    .with_transport(transport.clone());

    let repository = common::TempRepo::new();
    repository.commit_baseline();
    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), repository.path());
    (step, repository, transport)
}

/// Read a run artifact back, or report it loudly.
fn artifact(dir: &Path, name: &str) -> String {
    fs::read_to_string(dir.join(name)).unwrap_or_else(|e| common::failed(name, &e.to_string()))
}

/// The ticket artifact for a ticket id, read back or reported loudly.
fn ticket_artifact(dir: &Path, id: &str) -> String {
    artifact(dir, &format!("tickets/{id}.md"))
}

// -- Core Entry-Point Behaviors / Maximum limit / Upper boundary ------------

#[test]
fn when_reviewing_given_three_fix_rounds_then_runs_fourth_verification_review() {
    let (step, repository, transport) =
        review_run(common::ReviewScript::FixThreeRoundsThenPass, |_| {});
    let dir = repository.path();

    assert!(
        matches!(step, Step::Complete),
        "the fourth review passes, so the run completes: {step:?}"
    );

    // The original ticket and then the three fix sets, each executed from its
    // own artifact, in round order.
    let payloads = executed_ticket_payloads(&transport);
    assert_eq!(
        payloads.len(),
        4,
        "the original queue and exactly three review-generated fix queues ran: {payloads:?}"
    );
    for (id, payload) in executed_ticket_ids().iter().zip(&payloads) {
        assert!(
            payload.ends_with(&ticket_artifact(dir, id)),
            "ticket {id} was not run from its own ticket artifact"
        );
    }
    assert!(
        !dir.join("tickets/r4-1-verify-edge-case.md").exists(),
        "the fourth review's fix ticket is never written: there is no fourth fix queue"
    );

    // Four ordered reviews, the fourth a pass, and no fifth.
    assert_eq!(
        review_contexts(&transport).len(),
        4,
        "exactly four reviews ran, and no fifth"
    );
    let review = artifact(dir, "review.md");
    let mut order = Vec::new();
    for round in 1..=4 {
        let heading = format!("## Round {round}");
        order.push(
            review
                .find(&heading)
                .unwrap_or_else(|| panic!("review.md has no {heading}: {review}")),
        );
    }
    assert!(
        order.windows(2).all(|pair| pair[0] < pair[1]),
        "the rounds must be in order: {review}"
    );
    assert!(!review.contains("## Round 5"), "{review}");
    for round in 1..=3 {
        assert!(
            review.contains(&format!(
                "## Round {round}\n\n**Verdict:** Needs fixes (1 finding, 1 fix ticket)"
            )),
            "{review}"
        );
        assert!(
            review.contains(&format!(
                "- `{}`: Verify command output handles empty boundary case",
                FIX_TICKET_IDS[round - 1]
            )),
            "round {round}'s fix ticket must render: {review}"
        );
    }
    assert!(
        review.contains("## Round 4\n\n**Verdict:** Passed (no fix tickets)"),
        "{review}"
    );

    // The summary phase ran once, and the real repository received the commit
    // of the whole run.
    assert_eq!(
        summary_requests(&transport).len(),
        1,
        "the summary phase runs exactly once"
    );
    assert_eq!(
        head_commits(&repository),
        2,
        "the baseline plus the run's one commit"
    );
    assert_eq!(head_message(&repository), common::SCRIPTED_SUMMARY);
    let committed = head_paths(&repository);
    for id in executed_ticket_ids() {
        assert!(
            committed
                .iter()
                .any(|path| path == &format!("tickets/{id}.md")),
            "the executed ticket artifact for {id} must be committed: {committed:?}"
        );
    }
    assert!(committed.iter().any(|path| path == "review.md"));
    assert_eq!(
        repository.status(),
        "",
        "a completed run leaves its repository clean"
    );
}

// -- Edge-Case Tables -------------------------------------------------------

#[test]
fn when_reviewing_given_pass_or_stopped_queue_then_skips_later_fix_rounds() {
    // An early clean review is a pass: no fix queue starts behind it.
    {
        let (step, repository, transport) = review_run(common::ReviewScript::Pass, |_| {});
        assert!(matches!(step, Step::Complete), "{step:?}");
        assert_eq!(
            review_contexts(&transport).len(),
            1,
            "one review, and no later round"
        );
        assert_eq!(
            executed_ticket_payloads(&transport).len(),
            1,
            "a pass starts no fix queue"
        );
        assert!(
            !repository
                .path()
                .join("tickets/r1-1-verify-edge-case.md")
                .exists()
        );
        assert_eq!(head_commits(&repository), 2);
    }

    // A queue that stops before it resolves never reaches a review, so no later
    // fix set can run behind it.
    {
        let (step, repository, transport) = review_run_with_gate(
            common::GateScript::Passing,
            common::ReviewScript::FixThreeRoundsThenPass,
            |dir| {
                fs::create_dir_all(dir.join("tickets/queue-status.json"))
                    .expect("the status path is plantable as a directory");
            },
        );
        assert!(
            matches!(step, Step::Retry),
            "a stopped queue stays retryable: {step:?}"
        );
        assert_eq!(
            review_contexts(&transport).len(),
            0,
            "a stopped queue reaches no review"
        );
        assert_eq!(executed_ticket_payloads(&transport).len(), 0);
        assert!(!repository.path().join("review.md").exists());
        assert!(repository.path().join("change-summary.md").is_file());
        assert_eq!(
            head_commits(&repository),
            1,
            "a stopped queue commits nothing"
        );
    }

    // A queue that drains incomplete ends the run just as firmly.
    {
        let (step, repository, transport) = review_run_with_gate(
            common::GateScript::Failing,
            common::ReviewScript::FixThreeRoundsThenPass,
            |_| {},
        );
        assert!(
            matches!(step, Step::Retry),
            "an incomplete queue stays retryable: {step:?}"
        );
        assert_eq!(
            review_contexts(&transport).len(),
            0,
            "an incomplete queue reaches no review"
        );
        assert_eq!(
            executed_ticket_payloads(&transport).len(),
            1,
            "only the original ticket ran"
        );
        assert!(
            !repository
                .path()
                .join("tickets/r1-1-verify-edge-case.md")
                .exists()
        );
        assert!(!repository.path().join("review.md").exists());
        assert_eq!(head_commits(&repository), 1);
    }

    // A failed fix queue ends the run too: one review, one fix set, and neither
    // another review nor a later fix set.
    {
        let (step, repository, transport) =
            review_run(common::ReviewScript::FixOnceWithFailingGate, |_| {});
        let dir = repository.path();
        assert!(
            matches!(step, Step::Retry),
            "a failed fix queue stays retryable: {step:?}"
        );
        assert_eq!(
            review_contexts(&transport).len(),
            1,
            "the failed fix queue earns no second review"
        );
        assert_eq!(
            executed_ticket_payloads(&transport).len(),
            2,
            "the original ticket and the one fix ticket ran"
        );
        assert!(dir.join("tickets/r1-1-verify-edge-case.md").is_file());
        assert!(!dir.join("tickets/r2-1-verify-edge-case.md").exists());
        let review = artifact(dir, "review.md");
        assert!(review.contains("## Round 1"), "{review}");
        assert!(!review.contains("## Round 2"), "{review}");
        let status: serde_json::Value =
            serde_json::from_str(&artifact(dir, "tickets/queue-status.json"))
                .expect("the queue status is JSON");
        assert_eq!(
            status["r1-1-verify-edge-case"], "failed",
            "the fix ticket failed its own gate"
        );
        assert!(dir.join("change-summary.md").is_file());
        assert_eq!(
            head_commits(&repository),
            1,
            "an incomplete fix queue commits nothing"
        );
    }
}

// -- Contract & Invariant Checks --------------------------------------------

#[test]
fn when_reviewing_given_executed_fixes_then_preserves_cumulative_context_and_queue_artifacts() {
    const STALE: &str = "# Ticket queue report\n\nSTALE-REPORT-FROM-AN-EARLIER-RUN\n";
    let (step, repository, transport) =
        review_run(common::ReviewScript::FixThreeRoundsThenPass, |dir| {
            fs::write(dir.join("queue-report.md"), STALE).expect("the stale report is plantable");
        });
    let dir = repository.path();
    assert!(matches!(step, Step::Complete), "{step:?}");

    let contexts = review_contexts(&transport);
    assert_eq!(contexts.len(), 4, "one review per queue round");

    // Review 4 is handed the original ticket, every executed fix ticket -- in
    // execution order -- and the prior review history.
    let fourth = &contexts[3];
    let executed_section = fourth
        .find("## Executed Tickets")
        .expect("the executed tickets are named");
    let report_section = fourth
        .find("## Queue Report")
        .expect("the cumulative report is handed over");
    let history_section = fourth
        .find("## Prior Review Rounds")
        .expect("the prior rounds are handed over");
    assert!(
        executed_section < report_section && report_section < history_section,
        "{fourth}"
    );
    let mut cursor = executed_section;
    for id in executed_ticket_ids() {
        let document = ticket_artifact(dir, id);
        let found = fourth.find(document.trim_end()).unwrap_or_else(|| {
            panic!("review 4 must carry the executed ticket document for {id}:\n{fourth}")
        });
        assert!(
            found >= cursor,
            "review 4 carries the executed tickets in order: {fourth}"
        );
        cursor = found;
    }
    for round in 1..=3 {
        assert!(
            fourth[history_section..].contains(&format!("## Round {round}")),
            "review 4 must carry round {round}'s history: {fourth}"
        );
    }
    assert!(
        !fourth[history_section..].contains("## Round 4"),
        "review 4's own report is not prior history"
    );

    // Every earlier review sees exactly the rounds that preceded it, and the
    // generated report -- never the stale report the run found on disk.
    for (index, context) in contexts.iter().enumerate() {
        let round = index + 1;
        let noun = if round == 1 { "ticket" } else { "tickets" };
        assert!(
            context.contains(&format!(
                "{round} {noun}: {round} completed, 0 failed, 0 blocked."
            )),
            "review {round} must carry its own cumulative report: {context}"
        );
        assert!(
            !context.contains("STALE-REPORT-FROM-AN-EARLIER-RUN"),
            "review {round} must not fall back to the stale report on disk"
        );
    }
    assert!(
        contexts[0].contains("this is review round 1"),
        "{}",
        contexts[0]
    );
    assert!(
        !contexts[0].contains("## Round 1"),
        "review 1 has no prior history: {}",
        contexts[0]
    );
    assert!(
        contexts[1].contains("## Round 1"),
        "review 2 must carry round 1's history"
    );

    // The queue's final artifacts name exactly the executed tickets.
    let report = artifact(dir, "queue-report.md");
    assert!(report.starts_with("# Ticket queue report\n"));
    assert!(
        report.contains("4 tickets: 4 completed, 0 failed, 0 blocked."),
        "{report}"
    );
    assert!(
        !report.contains("STALE-REPORT-FROM-AN-EARLIER-RUN"),
        "{report}"
    );
    assert!(!report.contains("r4-1-verify-edge-case"), "{report}");
    for id in executed_ticket_ids() {
        assert!(report.contains(&format!("## {id}\n")), "{report}");
    }
    assert!(
        fourth.contains(report.trim_end()),
        "review 4 must be handed the report this run generated"
    );

    let status: serde_json::Value =
        serde_json::from_str(&artifact(dir, "tickets/queue-status.json"))
            .expect("the queue status is JSON");
    let mut keys: Vec<&str> = status
        .as_object()
        .expect("the status root is an object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort();
    let mut expected = executed_ticket_ids();
    expected.sort();
    assert_eq!(
        keys, expected,
        "the status snapshot holds the executed tickets and nothing else"
    );
    for id in executed_ticket_ids() {
        assert_eq!(status[id], "completed", "ticket {id} must be completed");
    }
}

// -- Empty/Zero/Nil ---------------------------------------------------------

#[test]
fn when_reviewing_given_empty_fixes_then_passes_without_queue() {
    let (step, repository, transport) = review_run(common::ReviewScript::Pass, |_| {});
    let dir = repository.path();
    assert!(
        matches!(step, Step::Complete),
        "a valid no-fix report is a pass: {step:?}"
    );
    assert_eq!(
        review_contexts(&transport).len(),
        1,
        "a pass starts no further review"
    );
    assert_eq!(
        executed_ticket_payloads(&transport).len(),
        1,
        "an empty fixes list starts no queue"
    );
    assert_eq!(summary_requests(&transport).len(), 1);

    let review = artifact(dir, "review.md");
    assert!(
        review.contains("**Verdict:** Passed (no fix tickets)"),
        "{review}"
    );
    assert!(!review.contains("Needs fixes"), "{review}");
    assert!(!review.contains("## Round 2"), "{review}");

    let mut ticket_names: Vec<String> = fs::read_dir(dir.join("tickets"))
        .expect("the ticket directory exists")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".md"))
        .collect();
    ticket_names.sort();
    assert_eq!(
        ticket_names,
        vec!["1-verify-command-output.md".to_string()],
        "an empty fix set writes no fix artifact"
    );
    assert_eq!(head_commits(&repository), 2);
}

// -- Minimum valid boundary -------------------------------------------------

#[test]
fn when_reviewing_given_one_valid_fix_then_runs_next_queue() {
    let (step, repository, transport) = review_run(common::ReviewScript::FixOnceThenPass, |_| {});
    let dir = repository.path();
    assert!(matches!(step, Step::Complete), "{step:?}");

    // The one accepted fix is persisted as its own ticket artifact before the
    // queue runs it from exactly those bytes.
    let fix = "r1-1-verify-edge-case";
    let document = ticket_artifact(dir, fix);
    assert!(!document.trim().is_empty());
    let payloads = executed_ticket_payloads(&transport);
    assert_eq!(
        payloads.len(),
        2,
        "the original ticket and one fix ticket ran"
    );
    assert!(
        payloads[1].ends_with(&document),
        "the fix queue ran the persisted artifact itself"
    );

    // Its own verification gate ran and passed.
    let status: serde_json::Value =
        serde_json::from_str(&artifact(dir, "tickets/queue-status.json"))
            .expect("the queue status is JSON");
    assert_eq!(
        status[fix], "completed",
        "the fix ticket passed its verification gate"
    );
    assert_eq!(status["1-verify-command-output"], "completed");
    let report = artifact(dir, "queue-report.md");
    assert!(
        report.contains("2 tickets: 2 completed, 0 failed, 0 blocked."),
        "{report}"
    );
    assert!(report.contains(&format!("## {fix}\n")), "{report}");

    // The second review is handed the executed fix as prior context.
    let contexts = review_contexts(&transport);
    assert_eq!(contexts.len(), 2);
    assert!(
        contexts[1].contains(document.trim_end()),
        "the second review carries the executed fix"
    );
    assert!(
        contexts[1].contains("## Round 1"),
        "the second review carries the prior history"
    );
    assert_eq!(head_commits(&repository), 2);
}

// -- Malformed/Invalid payload ----------------------------------------------

#[test]
fn when_review_handoff_given_invalid_fix_set_then_rejects_without_scheduling() {
    // A payload the boundary refuses is a tool result, not a handoff: the
    // corrected turn in the same review round is the only report that reaches
    // the pipeline, and because it passes no fix queue is scheduled and no fix
    // artifact is written.
    let rejected = [
        (
            common::InvalidReview::FixesWithoutFindings,
            "hands over fix tickets without any",
        ),
        (common::InvalidReview::BlankAcceptanceCriteria, "has blank"),
        (common::InvalidReview::DuplicateFindingIds, "is duplicated"),
        (
            common::InvalidReview::CollidingTicketId,
            "duplicates a ticket id from an earlier round",
        ),
        (
            common::InvalidReview::InvalidDependency,
            "is not a ticket in this set",
        ),
    ];
    for (case, reason) in rejected {
        let (step, repository, transport) =
            review_run(common::ReviewScript::InvalidOnceThenPass(case), |_| {});
        let dir = repository.path();
        assert!(
            matches!(step, Step::Complete),
            "{case:?}: a corrected valid handoff proceeds: {step:?}"
        );

        let requests = review_requests(&transport);
        assert_eq!(
            requests.len(),
            2,
            "{case:?}: the rejected call buys exactly one correction in the same review round"
        );
        assert!(
            requests[1].contains(reason),
            "{case:?}: the correction must carry the rejection reason: {}",
            requests[1]
        );
        assert_eq!(
            executed_ticket_payloads(&transport).len(),
            1,
            "{case:?}: an invalid fix set schedules no queue"
        );
        assert!(
            !dir.join("tickets/r1-1-verify-edge-case.md").exists(),
            "{case:?}: an invalid fix set writes no fix artifact"
        );
        let review = artifact(dir, "review.md");
        assert!(
            review.contains("**Verdict:** Passed (no fix tickets)"),
            "{case:?}: {review}"
        );
        assert!(!review.contains("Needs fixes"), "{case:?}: {review}");
        assert_eq!(
            head_commits(&repository),
            2,
            "{case:?}: the corrected pass commits the run"
        );
    }

    // A pass that never hands over a valid report authorizes nothing at all.
    for case in [
        common::InvalidReview::CollidingTicketId,
        common::InvalidReview::InvalidDependency,
    ] {
        let (step, repository, transport) =
            review_run(common::ReviewScript::AlwaysInvalid(case), |_| {});
        let dir = repository.path();
        assert!(
            matches!(step, Step::Retry),
            "{case:?}: an unaccepted handoff stays retryable: {step:?}"
        );
        assert_eq!(
            executed_ticket_payloads(&transport).len(),
            1,
            "{case:?}: no fix queue is authorized"
        );
        assert!(
            !dir.join("tickets/r1-1-verify-edge-case.md").exists(),
            "{case:?}: no pending fix artifact exists"
        );
        assert!(
            !dir.join("review.md").exists(),
            "{case:?}: an unaccepted handoff writes no review artifact"
        );
        assert!(
            dir.join("change-summary.md").is_file(),
            "{case:?}: the run is still summarised"
        );
        assert_eq!(
            head_commits(&repository),
            1,
            "{case:?}: an unaccepted handoff commits nothing"
        );
    }
}

// -- Boundary unauthorized/Forbidden access ---------------------------------

#[test]
fn when_review_agent_given_read_only_phase_then_rejects_bash_and_writable_scripts() {
    use ejaj::script::{Capabilities, QuickJsEngine, ScriptEngine};
    use ejaj::tools::{Phase, Source, Tool};

    // The review pass is offered the read-only inspection tool and its handoff,
    // never bash, and its script capabilities expose no mutating binding.
    let names: Vec<&str> = Tool::definitions(Phase::Review)
        .into_iter()
        .map(|definition| definition.function.name)
        .collect();
    assert_eq!(names, ["execute_script", "finish_review"]);
    assert!(
        !Capabilities::for_phase(Phase::Review).writable,
        "the review pass must have read-only script capabilities"
    );
    assert!(
        Tool::parse("bash", r#"{"command":"ls"}"#, Phase::Review, Source::None).is_err(),
        "bash must not be available to the review pass"
    );

    // The real run advertises exactly that read-only pair on its review request.
    let (step, repository, transport) = review_run(common::ReviewScript::Pass, |_| {});
    assert!(matches!(step, Step::Complete), "{step:?}");
    let requests = review_requests(&transport);
    assert_eq!(requests.len(), 1, "the passing review is one request");
    let body: serde_json::Value =
        serde_json::from_str(&requests[0]).expect("the review request is JSON");
    let advertised: Vec<String> = body["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|tool| tool["function"]["name"].as_str().map(str::to_string))
        .collect();
    assert_eq!(advertised, ["execute_script", "finish_review"]);

    // And the review phase's own script engine exposes only the four read-only
    // bindings: a write or a replace is refused and the named file is untouched.
    let root = repository.path().join("read-only-check");
    fs::create_dir_all(&root).expect("the scratch root is creatable");
    fs::write(root.join("note.txt"), "original").expect("the note is writable");
    let engine = QuickJsEngine::new();
    let caps = Capabilities::for_phase(Phase::Review).with_root(&root);
    let keys = engine
        .run("return Object.keys(tools).sort();", &caps)
        .expect("the keys query succeeds");
    assert_eq!(keys, r#"["diff","grep","list","read"]"#);
    for code in [
        r#"tools.write("note.txt", "mutated");"#,
        r#"tools.replace("note.txt", "original", "mutated");"#,
    ] {
        let error = engine
            .run(code, &caps)
            .expect_err("a writable binding must be refused in the review phase");
        assert!(error.contains("not a function"), "{error}");
    }
    assert_eq!(
        fs::read_to_string(root.join("note.txt")).expect("the note is readable"),
        "original"
    );
}

// ---------------------------------------------------------------------------
// Triage: the gate between the ticket writes and the queue
//
// The triage pass runs once, after every original ticket file is written and
// before the queue is constructed or invoked. Its validated typed report is the
// only authority on whether the queue runs: every classification must be
// ready-for-agent, or the whole set is blocked and the pipeline returns to the
// plan REPL without running a single ticket.
// ---------------------------------------------------------------------------

/// The decoded user-message content of every request, in order, so an assertion
/// reads the bytes a phase was actually handed rather than the JSON envelope
/// that escaped them.
fn user_contents(transport: &common::ScriptedTransport) -> Vec<String> {
    transport
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter_map(|body| {
            body["messages"]
                .as_array()?
                .iter()
                .find(|message| message["role"] == "user")
                .and_then(|message| message["content"].as_str())
                .map(str::to_string)
        })
        .collect()
}

/// The decoded user messages of the code-agent turns: the ticket payloads the
/// queue actually handed over.
fn code_payloads(transport: &common::ScriptedTransport) -> Vec<String> {
    user_contents(transport)
        .into_iter()
        .filter(|content| content.contains(common::CODE_MARKER))
        .collect()
}

/// The position of the one triage request the run made.
fn triage_request_index(transport: &common::ScriptedTransport) -> usize {
    transport
        .requests
        .lock()
        .unwrap()
        .iter()
        .position(|body| body.to_string().contains(common::TRIAGE_MARKER))
        .expect("the run made a triage request")
}

/// One scripted one-ticket run over a fresh scratch directory, prepared before
/// the pipeline touches it.
fn triage_run_prepared(
    triage: common::TriageScript,
    prepare: impl FnOnce(&Path),
) -> (Step, PathBuf, Arc<common::ScriptedTransport>) {
    let (config, transport) = common::scripted_triage_pipeline(triage);
    let dir = scratch_dir();
    fs::create_dir_all(&dir).expect("the scratch directory is creatable");
    let repository = common::TempRepo::at(dir.clone());
    prepare(&dir);
    repository.commit_baseline();

    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), &dir);
    repository.keep();
    (step, dir, transport)
}

/// One scripted one-ticket run with nothing planted beforehand.
fn triage_run(triage: common::TriageScript) -> (Step, PathBuf, Arc<common::ScriptedTransport>) {
    triage_run_prepared(triage, |_| {})
}

/// One scripted run over the three-ticket fixture.
fn multi_triage_run(
    triage: common::TriageScript,
) -> (Step, PathBuf, Arc<common::ScriptedTransport>) {
    let (config, transport) = common::scripted_multi_triage_pipeline(triage);
    let dir = scratch_dir();
    let repository = common::TempRepo::at(dir.clone());
    repository.commit_baseline();

    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), &dir);
    repository.keep();
    (step, dir, transport)
}

/// The triage report artifact, read back or reported loudly.
fn triage_artifact(dir: &Path) -> String {
    fs::read_to_string(dir.join("ticket-triage.md"))
        .unwrap_or_else(|e| common::failed("the triage artifact", &e.to_string()))
}

/// Assert a blocked run produced no queue: no status snapshot, no report, no
/// review, no summary, and no code-agent request at all.
fn assert_no_queue(dir: &Path, transport: &common::ScriptedTransport) {
    for artifact in [
        "tickets/queue-status.json",
        "queue-report.md",
        "review.md",
        "change-summary.md",
    ] {
        assert!(
            !dir.join(artifact).exists(),
            "`{artifact}` must not exist on a blocked run"
        );
    }
    assert!(
        code_payloads(transport).is_empty(),
        "a blocked run must send no code-agent request"
    );
}

/// The name of the blocked-run test. The parent below spawns a copy of this
/// test binary that runs exactly this test, so the run's production transcript
/// can be captured at the process boundary without a mock or a private seam.
const BLOCKED_RUN_TEST: &str = "when_triaging_tickets_given_needs_info_then_blocks_entire_queue";

/// The environment variable the parent sets on that copy. A copy that sees it
/// performs the blocked run for real -- letting the pipeline's own output reach
/// the streams the parent captured -- and then returns.
const BLOCKED_RUN_CHILD: &str = "EJAJ_BLOCKED_TRIAGE_RUN_CHILD";

/// Run the blocked triage once in a child process and return everything that
/// run wrote to stdout and stderr, concatenated: the transcript the caller
/// reads. The child is this same test binary running exactly the blocked-run
/// test with BLOCKED_RUN_CHILD set, so the run goes through the production
/// output path and nothing here mocks a stream or inspects private state.
fn blocked_run_transcript() -> String {
    let exe = std::env::current_exe().expect("the test binary path is known");
    let output = Command::new(exe)
        .args([
            "--exact",
            BLOCKED_RUN_TEST,
            "--nocapture",
            "--test-threads=1",
        ])
        .env(BLOCKED_RUN_CHILD, "1")
        .output()
        .expect("the child test process starts and is waited for");
    assert!(
        output.status.success(),
        "the child blocked run failed ({}):\n--- stdout ---\n{}\n--- stderr ---\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

// -- Core Entry-Point Behaviors --------------------------------------------

#[test]
fn when_triaging_tickets_given_all_ready_then_writes_report_and_starts_queue() {
    let (step, dir, transport) = triage_run(common::TriageScript::AllReady);

    assert!(
        matches!(step, Step::Complete),
        "an all-ready triage must start the queue: {step:?}"
    );

    // The report sits beside the plan and the spec, rendered from the typed
    // result: the ticket's own id and title, its category and state, its notes
    // and the complete Agent Brief.
    let report = triage_artifact(&dir);
    assert!(report.starts_with("# Ticket triage report\n"), "{report}");
    assert!(
        report.contains("## 1-verify-command-output: Verify command outputs exactly 'ejaj-e2e-ok'"),
        "{report}"
    );
    assert!(report.contains("- **Category:** enhancement"), "{report}");
    assert!(report.contains("- **State:** ready-for-agent"), "{report}");
    assert!(report.contains("- **Notes:**"), "{report}");
    assert!(report.contains("### Agent Brief"), "{report}");
    assert!(report.contains("**Target behavior:**"), "{report}");
    assert!(report.contains("**Acceptance criteria:**"), "{report}");
    assert!(report.contains("**Out of scope:**"), "{report}");
    assert!(report.contains("**Blocked by:** None."), "{report}");
    assert!(
        report.ends_with('\n') && !report.ends_with("\n\n"),
        "the triage artifact must end with exactly one newline: {report:?}"
    );
    assert!(dir.join("plan.md").is_file(), "plan.md is missing");
    assert!(dir.join("spec.md").is_file(), "spec.md is missing");

    // Exactly one triage pass, after the three handoff writes and before the
    // queue's own requests, on the configured provider.
    let index = triage_request_index(&transport);
    assert_eq!(
        index, 3,
        "triage runs after the three handoff writes and before the queue"
    );
    assert_eq!(
        transport.urls_called.lock().unwrap()[index],
        common::test_url(),
        "mid mode runs the triage pass on its configured provider"
    );

    // The queue really ran the ticket.
    let status_text = fs::read_to_string(dir.join("tickets/queue-status.json"))
        .unwrap_or_else(|e| common::failed("the queue status artifact", &e.to_string()));
    let status: serde_json::Value =
        serde_json::from_str(&status_text).expect("the queue status is JSON");
    assert_eq!(status["1-verify-command-output"], "completed");
    assert!(dir.join("queue-report.md").is_file());

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn when_triaging_tickets_given_needs_info_then_blocks_entire_queue() {
    // The child copy: perform the blocked run for real and return, so the
    // pipeline's own output reaches the streams the parent below captured.
    if std::env::var_os(BLOCKED_RUN_CHILD).is_some() {
        let (step, dir, _transport) =
            triage_run(common::TriageScript::Blocked(TriageState::NeedsInfo));
        assert!(
            matches!(step, Step::Retry),
            "a needs-info triage must return to the plan REPL: {step:?}"
        );
        let _ = fs::remove_dir_all(&dir);
        return;
    }

    let (step, dir, transport) = triage_run(common::TriageScript::Blocked(TriageState::NeedsInfo));

    assert!(
        matches!(step, Step::Retry),
        "a needs-info triage must return to the plan REPL: {step:?}"
    );

    // The findings are readable: the blocked state and the blocker explanation.
    let report = triage_artifact(&dir);
    assert!(report.contains("- **State:** needs-info"), "{report}");
    assert!(report.contains("- **Blocker explanation:**"), "{report}");
    assert!(
        report.contains("needs a human decision"),
        "the report must carry the blocker explanation: {report}"
    );

    assert_no_queue(&dir, &transport);
    assert_eq!(
        transport.urls_called.lock().unwrap().len(),
        4,
        "three handoff steps and the one triage pass, and nothing else"
    );

    let _ = fs::remove_dir_all(&dir);

    // The caller, not just the report on disk: the same blocked run in a child
    // process, whose production transcript is captured at the process boundary.
    // The blocked ticket's finding and the incomplete, retryable, new-run
    // guidance must both be visible on the run's own output.
    let transcript = blocked_run_transcript();
    assert!(
        transcript
            .contains("## 1-verify-command-output: Verify command outputs exactly 'ejaj-e2e-ok'"),
        "the run must name the blocked draft ticket: {transcript}"
    );
    assert!(
        transcript.contains("- **State:** needs-info"),
        "the blocked state must reach the caller: {transcript}"
    );
    assert!(
        transcript.contains("- **Blocker explanation:**"),
        "the blocker explanation must reach the caller: {transcript}"
    );
    assert!(
        transcript.contains("needs a human decision"),
        "the blocker explanation the report carries must reach the caller: {transcript}"
    );
    assert!(
        transcript.contains("[WARN] Triage did not mark every draft ticket ready-for-agent"),
        "the blocked-run warning must reach the caller: {transcript}"
    );
    assert!(
        transcript.contains("incomplete and retryable"),
        "the caller must be told the run is incomplete and retryable: {transcript}"
    );
    assert!(
        transcript.contains("no subset of the draft set executes"),
        "the caller must be told no ready ticket is partially executed: {transcript}"
    );
    assert!(
        transcript.contains("no answer is waited for"),
        "the caller must be told the run does not wait for interactive input: {transcript}"
    );
    assert!(
        transcript.contains("call `finish_plan` again to start a new run"),
        "the caller must be told a new run is required after the blocker: {transcript}"
    );
}

// -- Edge-Case Tables ------------------------------------------------------

#[test]
fn when_triaging_tickets_given_each_nonready_state_then_blocks_entire_queue() {
    // The three non-agent-ready wire values are distinct, and each one blocks
    // the whole set independently: ready-for-human, needs-info and wontfix.
    for state in [
        TriageState::ReadyForHuman,
        TriageState::NeedsInfo,
        TriageState::Wontfix,
    ] {
        let (step, dir, transport) = triage_run(common::TriageScript::Blocked(state));

        assert!(
            matches!(step, Step::Retry),
            "{}: the whole set must be blocked: {step:?}",
            state.wire()
        );

        let report = triage_artifact(&dir);
        assert!(
            report.contains(&format!("- **State:** {}", state.wire())),
            "{}: {report}",
            state.wire()
        );
        assert!(
            report.contains("- **Category:** bug"),
            "{}: a non-ready classification still carries exactly one category: {report}",
            state.wire()
        );
        assert!(
            report.contains("- **Blocker explanation:**"),
            "{}: {report}",
            state.wire()
        );
        assert!(
            !report.contains("### Agent Brief"),
            "{}: a non-ready classification carries no Agent Brief: {report}",
            state.wire()
        );

        assert_no_queue(&dir, &transport);
        let _ = fs::remove_dir_all(&dir);
    }
}

#[test]
fn when_triaging_tickets_given_one_blocked_ticket_in_multiple_then_runs_no_partial_queue() {
    let (step, dir, transport) =
        multi_triage_run(common::TriageScript::OneBlocked(TriageState::NeedsInfo));

    assert!(
        matches!(step, Step::Retry),
        "one blocked classification in a three-ticket set must block all three: {step:?}"
    );

    // Every original ticket file stays the renderer output of the draft set.
    for id in [
        "1-verify-command-output",
        "2-verify-command-failure",
        "3-verify-command-empty",
    ] {
        assert!(
            dir.join("tickets").join(format!("{id}.md")).is_file(),
            "the draft ticket `{id}` must survive a blocked triage"
        );
    }

    // Every decision of the set is in the report, in draft order: two ready and
    // the one blocked.
    let report = triage_artifact(&dir);
    let first = report
        .find("## 1-verify-command-output: ")
        .expect("the first ticket renders");
    let second = report
        .find("## 2-verify-command-failure: ")
        .expect("the second ticket renders");
    let third = report
        .find("## 3-verify-command-empty: ")
        .expect("the third ticket renders");
    assert!(first < second && second < third, "{report}");
    assert_eq!(
        report.matches("- **State:** ready-for-agent").count(),
        2,
        "{report}"
    );
    assert_eq!(
        report.matches("- **State:** needs-info").count(),
        1,
        "{report}"
    );

    // No subset ran: no ticket, and no code-agent request.
    assert_no_queue(&dir, &transport);
    assert_eq!(transport.urls_called.lock().unwrap().len(), 4);

    let _ = fs::remove_dir_all(&dir);
}

// -- Contract & Invariant Checks -------------------------------------------

#[test]
fn when_triaging_tickets_given_all_ready_then_code_request_matches_original_ticket_artifact() {
    let (step, dir, transport) = triage_run(common::TriageScript::AllReady);
    assert!(matches!(step, Step::Complete), "{step:?}");

    let artifact = fs::read_to_string(dir.join("tickets/1-verify-command-output.md"))
        .unwrap_or_else(|e| common::failed("the ticket artifact", &e.to_string()));

    let payloads = code_payloads(&transport);
    assert!(
        !payloads.is_empty(),
        "the queue must hand the ticket to the code agent"
    );
    for payload in &payloads {
        assert!(
            payload.ends_with(&artifact),
            "the code-agent payload must end with exactly the ticket artifact bytes: {payload}"
        );
        assert!(
            !payload.contains(common::BRIEF_MARKER),
            "the triage Agent Brief must never reach the code agent: {payload}"
        );
        assert!(!payload.contains("### Agent Brief"), "{payload}");
        assert!(!payload.contains("# Ticket triage report"), "{payload}");
    }

    // The marker really is in the triage artifact, so the absence above is the
    // one-request contract and not an empty brief.
    let report = triage_artifact(&dir);
    assert!(report.contains(common::BRIEF_MARKER), "{report}");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn when_writing_triage_given_unwritable_artifact_path_then_returns_triage_write_failure() {
    let (step, dir, transport) = triage_run_prepared(common::TriageScript::AllReady, |dir| {
        // A directory where the report file must go: the write cannot succeed.
        fs::create_dir_all(dir.join("ticket-triage.md"))
            .expect("the triage path is plantable as a directory");
    });

    match &step {
        Step::WriteFailed { artifact, .. } => assert_eq!(*artifact, "triage"),
        other => panic!("expected WriteFailed for triage, got {other:?}"),
    }
    assert!(
        dir.join("ticket-triage.md").is_dir(),
        "the triage write must really have failed"
    );
    assert_no_queue(&dir, &transport);

    let _ = fs::remove_dir_all(&dir);
}

// -- Regression Tests ------------------------------------------------------

#[test]
fn when_triaging_tickets_given_all_ready_then_preserves_queue_review_and_summary_outcomes() {
    let (step, dir, transport) = triage_run(common::TriageScript::AllReady);
    assert!(
        matches!(step, Step::Complete),
        "an all-ready triage keeps the pre-existing completion: {step:?}"
    );

    let status_text = fs::read_to_string(dir.join("tickets/queue-status.json"))
        .unwrap_or_else(|e| common::failed("the queue status artifact", &e.to_string()));
    let status: serde_json::Value =
        serde_json::from_str(&status_text).expect("the queue status is JSON");
    assert_eq!(status["1-verify-command-output"], "completed");

    let report = fs::read_to_string(dir.join("queue-report.md"))
        .unwrap_or_else(|e| common::failed("the queue report artifact", &e.to_string()));
    assert!(
        report.contains("1 ticket: 1 completed, 0 failed, 0 blocked."),
        "{report}"
    );

    let review = fs::read_to_string(dir.join("review.md"))
        .unwrap_or_else(|e| common::failed("the review artifact", &e.to_string()));
    assert!(
        review.starts_with("# Review report\n\n## Round 1\n"),
        "the review pass must run unchanged: {review}"
    );

    assert_eq!(summary_artifact(&dir), common::SCRIPTED_SUMMARY);
    assert_eq!(
        transport.urls_called.lock().unwrap().len(),
        8,
        "plan, spec, tickets, triage, the ticket's two steps, the review pass and the summary"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn when_triaging_tickets_given_wontfix_then_does_not_claim_completion() {
    let (step, dir, transport) = triage_run(common::TriageScript::Blocked(TriageState::Wontfix));

    assert!(
        !matches!(step, Step::Complete),
        "a wontfix triage must never be reported as completion: {step:?}"
    );
    assert!(matches!(step, Step::Retry), "{step:?}");

    let report = triage_artifact(&dir);
    assert!(report.contains("- **State:** wontfix"), "{report}");
    assert!(report.contains("- **Blocker explanation:**"), "{report}");

    // The draft tickets are neither rewritten nor removed: the ticket files stay
    // the renderer output of the original tickets.
    let artifact = fs::read_to_string(dir.join("tickets/1-verify-command-output.md"))
        .unwrap_or_else(|e| common::failed("the ticket artifact", &e.to_string()));
    assert!(
        artifact.starts_with("---\nid: \"1-verify-command-output\"\n"),
        "{artifact}"
    );
    assert!(!dir.join("change-summary.md").exists());
    assert_no_queue(&dir, &transport);

    let _ = fs::remove_dir_all(&dir);
}

// -- Empty/Zero/Nil --------------------------------------------------------

#[test]
fn when_triaging_tickets_given_missing_or_invalid_handoff_then_returns_retry_without_queue() {
    for (case, script) in [
        (
            "a turn that ends without finish_triage",
            common::TriageScript::TurnEnd,
        ),
        ("a refused triage request", common::TriageScript::ApiError),
    ] {
        let (step, dir, transport) = triage_run(script);

        assert!(matches!(step, Step::Retry), "{case}: {step:?}");
        assert!(
            !dir.join("ticket-triage.md").exists(),
            "{case}: no validated report means no artifact"
        );
        assert_no_queue(&dir, &transport);
        assert_eq!(
            transport.urls_called.lock().unwrap().len(),
            4,
            "{case}: the three handoff steps and the one triage attempt"
        );

        let _ = fs::remove_dir_all(&dir);
    }
}

// -- Minimum valid boundary ------------------------------------------------

#[test]
fn when_triaging_tickets_given_one_ready_ticket_then_runs_original_ticket() {
    let (step, dir, transport) = triage_run(common::TriageScript::AllReady);
    assert!(matches!(step, Step::Complete), "{step:?}");

    let status_text = fs::read_to_string(dir.join("tickets/queue-status.json"))
        .unwrap_or_else(|e| common::failed("the queue status artifact", &e.to_string()));
    let status: serde_json::Value =
        serde_json::from_str(&status_text).expect("the queue status is JSON");
    assert_eq!(
        status.as_object().map(serde_json::Map::len),
        Some(1),
        "the smallest set runs exactly its one ticket"
    );
    assert_eq!(status["1-verify-command-output"], "completed");

    let artifact = fs::read_to_string(dir.join("tickets/1-verify-command-output.md"))
        .unwrap_or_else(|e| common::failed("the ticket artifact", &e.to_string()));
    assert!(
        code_payloads(&transport)
            .iter()
            .any(|payload| payload.ends_with(&artifact)),
        "the original ticket is exactly what the code agent ran"
    );

    let _ = fs::remove_dir_all(&dir);
}

// -- Maximum limit / Upper boundary ----------------------------------------

#[test]
fn when_triaging_tickets_given_multi_ticket_report_then_preserves_every_decision_before_queue() {
    let (step, dir, _transport) = multi_triage_run(common::TriageScript::AllReady);
    assert!(
        matches!(step, Step::Complete),
        "the largest fixture must run every ticket: {step:?}"
    );

    let report = triage_artifact(&dir);
    let first = report
        .find("## 1-verify-command-output: ")
        .expect("the first ticket renders");
    let second = report
        .find("## 2-verify-command-failure: ")
        .expect("the second ticket renders");
    let third = report
        .find("## 3-verify-command-empty: ")
        .expect("the third ticket renders");
    assert!(
        first < second && second < third,
        "the report keeps draft order: {report}"
    );
    assert_eq!(
        report.matches("- **State:** ready-for-agent").count(),
        3,
        "no classification is dropped or truncated: {report}"
    );
    assert_eq!(
        report.matches("### Agent Brief").count(),
        3,
        "every ready classification keeps its brief: {report}"
    );

    // Every decision also survived into the queue.
    let status_text = fs::read_to_string(dir.join("tickets/queue-status.json"))
        .unwrap_or_else(|e| common::failed("the queue status artifact", &e.to_string()));
    let status: serde_json::Value =
        serde_json::from_str(&status_text).expect("the queue status is JSON");
    for id in [
        "1-verify-command-output",
        "2-verify-command-failure",
        "3-verify-command-empty",
    ] {
        assert_eq!(status[id], "completed", "ticket `{id}` did not run");
    }

    let _ = fs::remove_dir_all(&dir);
}

// -- Malformed/Invalid payload ---------------------------------------------

#[test]
fn when_triaging_tickets_given_rejected_handoff_then_corrects_or_retries_without_queue() {
    // A rejected payload is a correction, not a failure: the corrected report
    // in the next turn is what authorizes the queue.
    let (step, dir, transport) = triage_run(common::TriageScript::RejectedOnceThenReady);
    assert!(
        matches!(step, Step::Complete),
        "a corrected triage handoff must run the queue: {step:?}"
    );
    assert!(dir.join("ticket-triage.md").is_file());
    assert!(dir.join("tickets/queue-status.json").is_file());
    assert_eq!(
        transport.urls_called.lock().unwrap().len(),
        9,
        "the rejected turn, the corrected turn and the ordinary run"
    );
    let _ = fs::remove_dir_all(&dir);

    // A pass that never hands over a valid report cannot authorize anything.
    let (step, dir, transport) = triage_run(common::TriageScript::AlwaysRejected);
    assert!(matches!(step, Step::Retry), "{step:?}");
    assert!(!dir.join("ticket-triage.md").exists());
    assert_no_queue(&dir, &transport);
    assert!(
        transport.urls_called.lock().unwrap().len() > 4,
        "the rejected call must be retried as a tool result, not accepted"
    );
    let _ = fs::remove_dir_all(&dir);

    // An agent failure fails closed too.
    let (step, dir, transport) = triage_run(common::TriageScript::ApiError);
    assert!(matches!(step, Step::Retry), "{step:?}");
    assert_no_queue(&dir, &transport);
    let _ = fs::remove_dir_all(&dir);
}

// -- Boundary unauthorized/Forbidden access --------------------------------

#[test]
fn when_inspecting_repository_given_triage_phase_then_has_no_write_replace_or_bash_access() {
    use ejaj::script::Capabilities;
    use ejaj::tools::{Phase, Source, Tool};

    // The pass is offered the read-only inspection tool and its handoff, never
    // `bash`, and its script capabilities expose no mutating binding.
    let names: Vec<&str> = Tool::definitions(Phase::Triage)
        .into_iter()
        .map(|definition| definition.function.name)
        .collect();
    assert_eq!(names, ["execute_script", "finish_triage"]);
    assert!(!Capabilities::for_phase(Phase::Triage).writable);
    assert!(
        Tool::parse("bash", r#"{"command":"ls"}"#, Phase::Triage, Source::None).is_err(),
        "`bash` must not be available to the triage pass"
    );

    // The real request advertises exactly that read-only pair.
    let (_step, dir, transport) = triage_run(common::TriageScript::AllReady);
    let index = triage_request_index(&transport);
    let body = transport.requests.lock().unwrap()[index].clone();
    let advertised: Vec<String> = body["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|tool| tool["function"]["name"].as_str().map(str::to_string))
        .collect();
    assert_eq!(advertised, ["execute_script", "finish_triage"]);

    // And the prompt states the read-only contract the pass is held to.
    let prompt = user_contents(&transport)[index].clone();
    assert!(prompt.contains("read-only"), "{prompt}");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn when_triaging_tickets_given_no_authorization_contract_then_records_not_applicable() {
    let boundary = ejaj::triage::AUTHORIZATION_BOUNDARY;
    assert!(boundary.starts_with("Not applicable"), "{boundary}");
    assert!(
        boundary.contains("no user authorization boundary"),
        "{boundary}"
    );
}

// ---------------------------------------------------------------------------
// The summary pass's single corrective turn
//
// A present answer the canonicalizer rejects buys exactly one correction in the
// same transcript; an absent answer, a failed call and a rejected correction
// are fatal. Every test drives the real `Pipeline::ask` over the scripted
// external transport and asserts the requests the production code sent, the
// terminal `Step` and the exact artifact bytes.
// ---------------------------------------------------------------------------

/// A summary answer the canonicalizer rejects: its description is not
/// lowercase.
const REJECTED_SUMMARY: &str = "feat: Add the thing\n\nBody line.\n";

/// Two more present answers the unchanged validator refuses for distinct
/// reasons: a header with no body, and a body whose footer is malformed.
const REJECTED_NO_BODY: &str = "feat: add the thing\n";
const REJECTED_FOOTER: &str = "feat: add the thing\n\nBody line.\n\nFixes #\n";

/// The canonical summary accepted as the corrected answer.
const CORRECTED_SUMMARY: &str =
    "test(pipeline): retry the change summary\n\nA corrected body line.\n";

/// The canonical summary accepted as the fourth answer candidate.
const FOURTH_SUMMARY: &str =
    "test(pipeline): accept the fourth candidate\n\nOnly the fourth answer is written.\n";

/// Every summary request the run made, in order, as text. The summary prompt
/// alone tells the pass's own requests from the queue's, so the count and the
/// order are the production pass's own.
fn summary_requests(transport: &common::ScriptedTransport) -> Vec<String> {
    transport
        .requests
        .lock()
        .unwrap()
        .iter()
        .map(|body| body.to_string())
        .filter(|body| body.contains(common::SUMMARY_MARKER))
        .collect()
}

/// Every summary request the run made, decoded from its captured JSON.
fn summary_request_values(transport: &common::ScriptedTransport) -> Vec<serde_json::Value> {
    summary_requests(transport)
        .iter()
        .map(|request| {
            serde_json::from_str(request).expect("every captured summary request is JSON")
        })
        .collect()
}

/// Every text block one native Anthropic request carries, in wire order.
fn summary_text_blocks(body: &serde_json::Value) -> Vec<String> {
    body["messages"]
        .as_array()
        .expect("the native request carries messages")
        .iter()
        .flat_map(|message| message["content"].as_array().into_iter().flatten())
        .filter(|block| block["type"].as_str() == Some("text"))
        .filter_map(|block| block["text"].as_str().map(str::to_string))
        .collect()
}

/// The correction messages one native Anthropic request carries, in wire
/// order: each is the user turn the pass appends at a correction boundary.
fn summary_corrections(body: &serde_json::Value) -> Vec<String> {
    summary_text_blocks(body)
        .into_iter()
        .filter(|text| {
            text.starts_with("Your previous answer was rejected by the summary format check:")
        })
        .collect()
}

/// The content of the native `tool_result` block addressed to `tool_use_id`
/// in one Anthropic Messages request body, if the transcript carries one.
fn summary_tool_result(body: &serde_json::Value, tool_use_id: &str) -> Option<String> {
    body["messages"]
        .as_array()?
        .iter()
        .flat_map(|message| message["content"].as_array().into_iter().flatten())
        .find(|block| {
            block["type"].as_str() == Some("tool_result")
                && block["tool_use_id"].as_str() == Some(tool_use_id)
        })
        .and_then(|block| block["content"].as_str())
        .map(str::to_string)
}

/// Core entry-point behavior: the summary pass runs one real native tool-use
/// round trip through the production agent loop. The read-only script executes
/// through Summary's existing capability set, its result is replayed in the next
/// Anthropic request, and only then is the canonical summary accepted and
/// written to `change-summary.md`.
#[test]
fn when_summarizing_given_scripted_tool_exchange_then_writes_canonical_artifact() {
    let (step, dir, transport) =
        scripted_run(QueueOutcome::Resolved, common::SummaryScript::ToolExchange);

    assert!(
        matches!(step, Step::Complete),
        "the tool exchange ends on an accepted summary: {step:?}"
    );

    let requests = summary_requests(&transport);
    assert_eq!(
        requests.len(),
        2,
        "the summary pass makes one tool step and one final answer"
    );

    let urls = transport.urls_called.lock().unwrap().clone();
    assert_eq!(
        urls.last().map(String::as_str),
        Some(common::TEST_SMOLL_URL),
        "the summary exchange runs on the Smoll Anthropic Messages provider"
    );

    let first: serde_json::Value =
        serde_json::from_str(&requests[0]).expect("the first summary request is JSON");
    let advertised: Vec<&str> = first["tools"]
        .as_array()
        .expect("the native request carries tools")
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert_eq!(
        advertised,
        ["execute_script"],
        "the summary phase advertises only its read-only inspection tool"
    );

    let second: serde_json::Value =
        serde_json::from_str(&requests[1]).expect("the second summary request is JSON");
    assert_eq!(
        summary_tool_result(&second, common::SUMMARY_TOOL_ID).as_deref(),
        Some(common::SUMMARY_SCRIPT_OUTPUT),
        "the next native request must replay the script's own tool result"
    );

    let artifact = summary_artifact(&dir);
    assert_eq!(
        artifact,
        common::SCRIPTED_SUMMARY,
        "the canonical summary is written after the tool result is observed"
    );
    assert!(
        artifact.ends_with('\n') && !artifact.ends_with("\n\n"),
        "the change summary must end with exactly one newline: {artifact:?}"
    );
    assert!(
        !artifact.contains('\r'),
        "the change summary must be LF only: {artifact:?}"
    );

    let _ = fs::remove_dir_all(&dir);
}

/// Edge-case table: a tool the summary phase does not offer is rejected by the
/// existing phase/tool boundary, its rejection is replayed to the model as a
/// tool result, and the pass then accepts the canonical summary.
#[test]
fn when_summarizing_given_a_tool_not_offered_in_summary_then_returns_a_tool_error() {
    let (step, dir, transport) = scripted_run(
        QueueOutcome::Resolved,
        common::SummaryScript::UnofferedToolCall,
    );

    assert!(
        matches!(step, Step::Complete),
        "the pass recovers from the rejected call and completes: {step:?}"
    );

    let requests = summary_requests(&transport);
    assert_eq!(
        requests.len(),
        2,
        "the rejected tool call and the final answer are two turns"
    );
    let second: serde_json::Value =
        serde_json::from_str(&requests[1]).expect("the second summary request is JSON");
    let result = summary_tool_result(&second, common::SUMMARY_UNOFFERED_TOOL_ID)
        .expect("the unoffered call must be replayed as a matching tool_result");

    assert!(
        result.contains("bash"),
        "the tool result must name the rejected tool: {result}"
    );
    assert!(
        result.contains("only available in the code phase"),
        "the existing phase/tool boundary must reject the call: {result}"
    );
    assert_eq!(
        summary_artifact(&dir),
        common::SCRIPTED_SUMMARY,
        "the rejected call never blocks the accepted summary"
    );

    let _ = fs::remove_dir_all(&dir);
}

/// Contract and invariant: the public scripted requests replay the assistant's
/// tool_use and the matching tool_result with the same id, the result carries
/// the script's own bytes, and the exchange ran with Summary's read-only
/// capability set -- the script's successful output proves `tools.write` was
/// never exposed.
#[test]
fn when_replaying_given_summary_tool_use_then_preserves_tool_id_result_and_read_only_capabilities()
{
    let (step, dir, transport) =
        scripted_run(QueueOutcome::Resolved, common::SummaryScript::ToolExchange);
    assert!(matches!(step, Step::Complete), "{step:?}");

    let requests = summary_requests(&transport);
    assert_eq!(requests.len(), 2, "one tool_use and one final answer");

    let first: serde_json::Value =
        serde_json::from_str(&requests[0]).expect("the first summary request is JSON");
    let advertised: Vec<&str> = first["tools"]
        .as_array()
        .expect("the native request carries tools")
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert_eq!(advertised, ["execute_script"]);
    assert!(
        !advertised.contains(&"bash"),
        "bash must never be offered to the summary phase: {advertised:?}"
    );

    let replay: serde_json::Value =
        serde_json::from_str(&requests[1]).expect("the second summary request is JSON");
    let blocks: Vec<&serde_json::Value> = replay["messages"]
        .as_array()
        .expect("the native request carries messages")
        .iter()
        .flat_map(|message| message["content"].as_array().into_iter().flatten())
        .collect();

    let tool_use = blocks
        .iter()
        .find(|block| {
            block["type"].as_str() == Some("tool_use")
                && block["id"].as_str() == Some(common::SUMMARY_TOOL_ID)
        })
        .expect("the assistant tool_use must be replayed");
    assert_eq!(tool_use["name"].as_str(), Some("execute_script"));
    assert_eq!(
        tool_use["input"]["code"].as_str(),
        Some(common::SUMMARY_SCRIPT_CODE),
        "the replayed call carries the read-only script that really ran"
    );

    let tool_result = blocks
        .iter()
        .find(|block| {
            block["type"].as_str() == Some("tool_result")
                && block["tool_use_id"].as_str() == Some(common::SUMMARY_TOOL_ID)
        })
        .expect("the matching tool_result must be replayed");
    assert_eq!(
        tool_result["content"].as_str(),
        Some(common::SUMMARY_SCRIPT_OUTPUT),
        "the tool result is the script's own output, not a rewritten value"
    );

    // Read-only execution: the script throws when `tools.write` is defined, so
    // its successful output is the proof that the write binding was absent.
    assert!(
        common::SUMMARY_SCRIPT_CODE.contains("typeof tools.write"),
        "the script must probe the mutating binding"
    );
    assert!(
        !common::SUMMARY_SCRIPT_CODE.contains("tools.write("),
        "the script must never invoke the mutating binding"
    );

    assert_eq!(summary_artifact(&dir), common::SCRIPTED_SUMMARY);
    let _ = fs::remove_dir_all(&dir);
}

/// Minimum valid boundary: the smallest successful round trip -- one read-only
/// script call whose result is replayed, then one canonical answer -- is
/// accepted and written as the artifact.
#[test]
fn when_summarizing_given_one_read_only_script_call_then_accepts_a_valid_summary() {
    let script = common::SummaryScript::Turns(vec![
        common::SummaryReply::ToolCall,
        common::SummaryReply::Answer(common::SCRIPTED_SUMMARY.to_string()),
    ]);
    let (step, dir, transport) = scripted_run(QueueOutcome::Resolved, script);

    assert!(
        matches!(step, Step::Complete),
        "one script call followed by a valid summary completes: {step:?}"
    );

    let requests = summary_requests(&transport);
    assert_eq!(
        requests.len(),
        2,
        "the smallest round trip is exactly one tool step and one answer"
    );
    let second: serde_json::Value =
        serde_json::from_str(&requests[1]).expect("the second summary request is JSON");
    let results: Vec<&str> = second["messages"]
        .as_array()
        .expect("the native request carries messages")
        .iter()
        .flat_map(|message| message["content"].as_array().into_iter().flatten())
        .filter(|block| block["type"].as_str() == Some("tool_result"))
        .filter_map(|block| block["content"].as_str())
        .collect();
    assert_eq!(
        results,
        ["1"],
        "the read-only script's result is replayed before the answer"
    );

    assert_eq!(summary_artifact(&dir), common::SCRIPTED_SUMMARY);

    let _ = fs::remove_dir_all(&dir);
}

/// Core entry-point behavior: the initial-valid path accepts a minimal header,
/// its separator and a non-empty body on the first answer, so the pass sends no
/// correction request, keeps the queue's own outcome and writes exactly the
/// canonical answer.
#[test]
fn when_summarizing_given_valid_first_candidate_then_writes_canonical_summary() {
    const MINIMAL: &str = "test(pipeline): write the change summary\n\nOne body line.\n";
    let script =
        common::SummaryScript::Turns(vec![common::SummaryReply::Answer(MINIMAL.to_string())]);
    let (step, dir, transport) = scripted_run(QueueOutcome::Resolved, script);

    assert!(
        matches!(step, Step::Complete),
        "a valid summary keeps the queue's own successful outcome: {step:?}"
    );
    assert_eq!(
        summary_requests(&transport).len(),
        1,
        "a valid first candidate must not be corrected"
    );
    let artifact = summary_artifact(&dir);
    assert_eq!(artifact, MINIMAL, "the artifact is the canonical answer");
    assert!(
        artifact.ends_with('\n') && !artifact.ends_with("\n\n"),
        "the artifact must end with exactly one LF: {artifact:?}"
    );

    let _ = fs::remove_dir_all(&dir);
}

/// An initial answer the validator rejects: the correction request is appended
/// to the same transcript, carries the exact rejection reason, the original
/// context, the prior tool result and the rejected answer, and the corrected
/// canonical bytes become the artifact.
#[test]
fn when_summarizing_given_one_invalid_candidate_then_accepts_valid_correction() {
    let script = common::SummaryScript::Turns(vec![
        // The initial generation inspects the change set once before it answers
        // with text the format rejects.
        common::SummaryReply::ToolCall,
        common::SummaryReply::Answer(REJECTED_SUMMARY.to_string()),
        common::SummaryReply::Answer(CORRECTED_SUMMARY.to_string()),
    ]);
    let (step, dir, transport) = scripted_run(QueueOutcome::Resolved, script);

    assert!(
        matches!(step, Step::Complete),
        "a corrected summary keeps the queue's own successful outcome: {step:?}"
    );

    let requests = summary_requests(&transport);
    assert_eq!(
        requests.len(),
        3,
        "one initial tool step, one rejected answer and exactly one correction: {}",
        requests.len()
    );

    let correction = &requests[2];
    // The validator's own reason, handed over as data.
    assert!(
        correction.contains("the description `Add the thing` must be lowercase"),
        "the correction must carry the exact canonicalization error: {correction}"
    );
    // The same transcript: the summary scope, the kickoff context and the prior
    // tool result all survive.
    assert!(
        correction.contains("No human is available in this session"),
        "the summary scope must still be in the transcript: {correction}"
    );
    assert!(
        correction.contains("The ticket queue has finished"),
        "the kickoff must still be in the transcript: {correction}"
    );
    assert!(correction.contains("## Outcome"), "{correction}");
    assert!(correction.contains("## Report"), "{correction}");
    assert!(correction.contains("## Review"), "{correction}");
    assert!(
        correction.contains("\"type\":\"tool_result\""),
        "the prior tool result must be replayed as a native tool_result block: {correction}"
    );
    // The rejected answer is still there, and the instruction asks for a
    // corrected summary alone rather than repairing the text in application
    // code.
    assert!(
        correction.contains("feat: Add the thing"),
        "the rejected answer must be replayed: {correction}"
    );
    assert!(
        correction.contains("Return only the corrected summary"),
        "the correction must ask for the corrected summary alone: {correction}"
    );
    assert!(
        correction.contains("does not wrap, normalize or otherwise repair"),
        "the correction must not authorize application-side rewriting: {correction}"
    );

    assert_eq!(summary_artifact(&dir), CORRECTED_SUMMARY);
    assert!(
        summary_artifact(&dir).ends_with('\n') && !summary_artifact(&dir).ends_with("\n\n"),
        "the corrected artifact ends with exactly one LF"
    );

    let _ = fs::remove_dir_all(&dir);
}

/// Malformed/invalid boundary: four present candidates the unchanged validator
/// rejects produce exactly four answer-generation attempts, the failure carries
/// candidate four's own reason, and no invalid text is ever persisted.
#[test]
fn when_summarizing_given_four_rejected_candidates_then_fails_with_fourth_reason() {
    let fourth_reason = ejaj::loops::summary::canonicalize(REJECTED_FOOTER)
        .expect_err("the fourth candidate must be invalid");
    let script = common::SummaryScript::Turns(vec![
        common::SummaryReply::Answer(REJECTED_SUMMARY.to_string()),
        common::SummaryReply::Answer(REJECTED_NO_BODY.to_string()),
        common::SummaryReply::Answer(REJECTED_SUMMARY.to_string()),
        common::SummaryReply::Answer(REJECTED_FOOTER.to_string()),
    ]);
    let (step, dir, transport) = scripted_run(QueueOutcome::Resolved, script);

    let reason = summary_reason(&step);
    assert_eq!(
        reason, fourth_reason,
        "the final failure is candidate four's own validator reason"
    );
    for earlier in [
        ejaj::loops::summary::canonicalize(REJECTED_SUMMARY).unwrap_err(),
        ejaj::loops::summary::canonicalize(REJECTED_NO_BODY).unwrap_err(),
    ] {
        assert_ne!(reason, earlier, "the final reason is never an older one");
    }
    assert_eq!(
        summary_requests(&transport).len(),
        4,
        "four candidates buy exactly four generation attempts"
    );
    assert!(
        !dir.join("change-summary.md").exists(),
        "four rejected candidates never persist a summary"
    );

    let _ = fs::remove_dir_all(&dir);
}

/// The correction call itself fails on the API: the initial and correction
/// requests are in order, the failure names the API error, and no artifact is
/// written.
#[test]
fn when_summarizing_given_correction_failure_then_preserves_generation_failure() {
    let script = common::SummaryScript::Turns(vec![
        common::SummaryReply::Answer(REJECTED_SUMMARY.to_string()),
        common::SummaryReply::ApiError,
    ]);
    let (step, dir, transport) = scripted_run(QueueOutcome::Resolved, script);

    let reason = summary_reason(&step);
    assert!(
        reason.contains("API error"),
        "the failure must name the API error: {reason}"
    );
    let requests = summary_requests(&transport);
    assert_eq!(
        requests.len(),
        2,
        "the rejected initial answer is followed by exactly one correction request"
    );
    assert!(
        requests[0].contains("Conventional-Commits-style header"),
        "the first summary request is the initial generation: {}",
        requests[0]
    );
    assert!(
        !dir.join("change-summary.md").exists(),
        "a failed correction leaves no artifact"
    );

    let _ = fs::remove_dir_all(&dir);
}

/// A correction turn that returns no usable native content is not correctable
/// again: the rejected initial answer buys exactly one correction request, that
/// correction's unusable response keeps the pass's fatal malformed reason, and
/// no summary artifact is left behind.
#[test]
fn when_summarizing_given_absent_correction_answer_then_fails_without_another_request() {
    let script = common::SummaryScript::Turns(vec![
        common::SummaryReply::Answer(REJECTED_SUMMARY.to_string()),
        common::SummaryReply::NoContent,
    ]);
    let (step, dir, transport) = scripted_run(QueueOutcome::Resolved, script);

    let reason = summary_reason(&step);
    assert!(
        reason.contains("malformed response"),
        "an unusable correction keeps the fatal malformed-response reason: {reason}"
    );

    let requests = summary_requests(&transport);
    assert_eq!(
        requests.len(),
        2,
        "the rejected initial answer is followed by exactly one correction request"
    );
    assert!(
        requests[0].contains("Conventional-Commits-style header"),
        "the first summary request is the initial generation: {}",
        requests[0]
    );
    assert!(
        requests[1].contains("Return only the corrected summary"),
        "the second summary request is the one correction: {}",
        requests[1]
    );
    assert!(
        !dir.join("change-summary.md").exists(),
        "an absent correction leaves no summary artifact"
    );

    let _ = fs::remove_dir_all(&dir);
}

/// A final turn with no usable native content is not correctable: the pass
/// sends the initial request only, keeps its fatal malformed reason, and writes
/// no artifact.
#[test]
fn when_summarizing_given_absent_initial_answer_then_fails_without_correction() {
    let (step, dir, transport) =
        scripted_run(QueueOutcome::Resolved, common::SummaryScript::NoContent);

    let reason = summary_reason(&step);
    assert!(
        reason.contains("malformed response"),
        "an unusable answer keeps the fatal malformed reason: {reason}"
    );
    assert_eq!(
        summary_requests(&transport).len(),
        1,
        "unusable content is never corrected"
    );
    assert!(!dir.join("change-summary.md").exists());

    let _ = fs::remove_dir_all(&dir);
}

/// Empty/zero boundary: whitespace-only text is a present candidate, not an
/// absent answer, so the unchanged validator rejects it and it may be corrected
/// -- but the four-candidate limit stops the pass after exactly four attempts.
#[test]
fn when_summarizing_given_whitespace_candidates_then_exhausts_after_four_attempts() {
    let (step, dir, transport) = scripted_run(QueueOutcome::Resolved, common::SummaryScript::Blank);

    let reason = summary_reason(&step);
    assert!(
        reason.contains("empty"),
        "the fatal reason must name the empty rejection: {reason}"
    );
    assert_eq!(
        summary_requests(&transport).len(),
        4,
        "whitespace is corrected up to the four-candidate limit and no further"
    );
    assert!(!dir.join("change-summary.md").exists());

    let _ = fs::remove_dir_all(&dir);
}

/// Malformed/invalid boundary: a native text block with no characters is
/// unusable content, so it is fatal without a correction request.
#[test]
fn when_summarizing_given_empty_text_then_fails_without_correction() {
    let (step, dir, transport) =
        scripted_run(QueueOutcome::Resolved, common::SummaryScript::Answer(""));
    let reason = summary_reason(&step);
    assert!(
        reason.contains("malformed response"),
        "an empty native text block is unusable: {reason}"
    );
    assert_eq!(
        summary_requests(&transport).len(),
        1,
        "unusable content is never corrected"
    );
    assert!(!dir.join("change-summary.md").exists());

    let _ = fs::remove_dir_all(&dir);
}

/// Maximum boundary: the unchanged 72-Unicode-character body-line limit
/// accepts an at-limit candidate and its canonical bytes become the artifact,
/// with one request and no correction.
#[test]
fn when_summarizing_given_body_line_at_limit_then_accepts() {
    let limit = ejaj::loops::summary::BODY_LINE_LIMIT;

    let at_limit = "é".repeat(limit);
    let accepted = format!("feat: retry the change summary\n\n{at_limit}\n");
    let script = common::SummaryScript::Turns(vec![common::SummaryReply::Answer(accepted.clone())]);
    let (step, dir, transport) = scripted_run(QueueOutcome::Resolved, script);
    assert!(
        matches!(step, Step::Complete),
        "a {limit}-character body line is within the limit: {step:?}"
    );
    assert_eq!(
        summary_requests(&transport).len(),
        1,
        "the at-limit first candidate is accepted immediately"
    );
    assert_eq!(summary_artifact(&dir), accepted);
    let _ = fs::remove_dir_all(&dir);
}

/// Over-limit boundary: a 73-character candidate is rejected on every one of
/// the four allowed attempts with the validator's own reason and leaves no
/// summary artifact.
#[test]
fn when_summarizing_given_body_line_over_limit_then_rejects() {
    let limit = ejaj::loops::summary::BODY_LINE_LIMIT;

    let over_limit = "é".repeat(limit + 1);
    let rejected = format!("feat: retry the change summary\n\n{over_limit}\n");
    let script = common::SummaryScript::Turns(vec![common::SummaryReply::Answer(rejected)]);
    let (step, dir, transport) = scripted_run(QueueOutcome::Resolved, script);
    let reason = summary_reason(&step);
    assert!(
        reason.contains(&format!("{} characters", limit + 1)),
        "a {}-character body line exceeds the limit: {reason}",
        limit + 1
    );
    assert_eq!(
        summary_requests(&transport).len(),
        4,
        "an over-limit candidate exhausts the four-candidate limit"
    );
    assert!(!dir.join("change-summary.md").exists());
    let _ = fs::remove_dir_all(&dir);
}

/// Core entry-point behavior: three rejected candidate answers followed by a
/// valid fourth answer keep the queue's original outcome, send exactly four
/// answer-generation requests in a no-tool exchange, and persist only the fourth
/// canonical result.
#[test]
fn when_summarizing_given_three_rejections_then_accepts_fourth_answer() {
    let rejected = [REJECTED_SUMMARY, REJECTED_NO_BODY, REJECTED_FOOTER];
    let script = common::SummaryScript::Turns(vec![
        common::SummaryReply::Answer(rejected[0].to_string()),
        common::SummaryReply::Answer(rejected[1].to_string()),
        common::SummaryReply::Answer(rejected[2].to_string()),
        common::SummaryReply::Answer(FOURTH_SUMMARY.to_string()),
    ]);
    let (step, dir, transport) = scripted_run(QueueOutcome::Resolved, script);

    assert!(
        matches!(step, Step::Complete),
        "accepting the fourth candidate keeps the queue's own successful outcome: {step:?}"
    );
    let values = summary_request_values(&transport);
    assert_eq!(
        values.len(),
        4,
        "three rejections and one acceptance are exactly four answer generations"
    );
    for (index, answer) in rejected.iter().enumerate() {
        assert!(
            summary_text_blocks(&values[index + 1])
                .iter()
                .any(|text| text.contains(answer.trim())),
            "request {} must replay rejected candidate {}",
            index + 2,
            index + 1
        );
    }

    let artifact = summary_artifact(&dir);
    assert_eq!(
        artifact, FOURTH_SUMMARY,
        "only the fourth answer's canonical result is written"
    );
    for answer in rejected {
        assert!(
            !artifact.contains(answer.trim()),
            "a rejected candidate is never persisted: {answer:?}"
        );
    }

    let _ = fs::remove_dir_all(&dir);
}

/// Minimum valid boundary: a valid fourth candidate ends generation immediately,
/// so no fifth request is sent and the artifact is exactly that candidate.
#[test]
fn when_summarizing_given_valid_fourth_candidate_then_stops_without_fifth_request() {
    let script = common::SummaryScript::Turns(vec![
        common::SummaryReply::Answer(REJECTED_SUMMARY.to_string()),
        common::SummaryReply::Answer(REJECTED_NO_BODY.to_string()),
        common::SummaryReply::Answer(REJECTED_FOOTER.to_string()),
        common::SummaryReply::Answer(FOURTH_SUMMARY.to_string()),
    ]);
    let (step, dir, transport) = scripted_run(QueueOutcome::Resolved, script);

    assert!(
        matches!(step, Step::Complete),
        "the valid fourth candidate is accepted: {step:?}"
    );
    let values = summary_request_values(&transport);
    assert_eq!(
        values.len(),
        4,
        "acceptance on the fourth attempt stops before a fifth request"
    );
    assert_eq!(
        summary_corrections(&values[3]).len(),
        3,
        "the fourth attempt carries the three corrections that preceded it"
    );
    assert_eq!(summary_artifact(&dir), FOURTH_SUMMARY);

    let _ = fs::remove_dir_all(&dir);
}

/// Maximum limit: four invalid candidates consume the whole allowance, produce
/// exactly four answer-generation attempts and no fifth request, and fail
/// without an artifact.
#[test]
fn when_summarizing_given_four_invalid_candidates_then_stops_without_fifth_request() {
    let script = common::SummaryScript::Turns(vec![
        common::SummaryReply::Answer(REJECTED_SUMMARY.to_string()),
        common::SummaryReply::Answer(REJECTED_NO_BODY.to_string()),
        common::SummaryReply::Answer(REJECTED_FOOTER.to_string()),
        common::SummaryReply::Answer("feat: Still not lowercase\n\nBody line.\n".to_string()),
    ]);
    let (step, dir, transport) = scripted_run(QueueOutcome::Resolved, script);

    assert!(
        matches!(step, Step::SummaryFailed { .. }),
        "four rejected candidates fail the summary: {step:?}"
    );
    assert_eq!(
        summary_requests(&transport).len(),
        4,
        "the candidate limit is four and no fifth request is sent"
    );
    assert!(!dir.join("change-summary.md").exists());

    let _ = fs::remove_dir_all(&dir);
}

/// Unauthorized/forbidden boundary: an HTTP 403 is a generation failure, not a
/// validation rejection, so it buys no correction and writes no artifact.
#[test]
fn when_summarizing_given_provider_forbids_access_then_fails_without_correction() {
    let script = common::SummaryScript::Turns(vec![common::SummaryReply::Forbidden]);
    let (step, dir, transport) = scripted_run(QueueOutcome::Resolved, script);

    let reason = summary_reason(&step);
    assert!(
        reason.contains("API error"),
        "an access denial is reported as the API generation failure: {reason}"
    );
    assert_eq!(
        summary_requests(&transport).len(),
        1,
        "a forbidden response is never corrected"
    );
    assert!(
        !dir.join("change-summary.md").exists(),
        "a forbidden response leaves no summary artifact"
    );

    let _ = fs::remove_dir_all(&dir);
}

/// Contract and invariant check: every correction carries its immediately
/// preceding candidate's exact validator reason, never an older one, while the
/// same transcript retains the summary scope, the kickoff outcome and report,
/// the prior rejected answers and the inspection-tool history.
#[test]
fn when_summarizing_given_three_distinct_rejections_then_sends_each_immediate_reason_in_same_transcript()
 {
    let rejected = [REJECTED_SUMMARY, REJECTED_NO_BODY, REJECTED_FOOTER];
    let reasons: Vec<String> = rejected
        .iter()
        .map(|answer| {
            ejaj::loops::summary::canonicalize(answer)
                .expect_err("every scripted candidate must be invalid")
        })
        .collect();
    let script = common::SummaryScript::Turns(vec![
        common::SummaryReply::ToolCall,
        common::SummaryReply::Answer(rejected[0].to_string()),
        common::SummaryReply::Answer(rejected[1].to_string()),
        common::SummaryReply::Answer(rejected[2].to_string()),
        common::SummaryReply::Answer(FOURTH_SUMMARY.to_string()),
    ]);
    let (step, dir, transport) = scripted_run(QueueOutcome::Resolved, script);

    assert!(
        matches!(step, Step::Complete),
        "the valid fourth answer is accepted after the tool-backed corrections: {step:?}"
    );
    let values = summary_request_values(&transport);
    assert_eq!(
        values.len(),
        5,
        "one inspection turn plus four answer generations share the transcript"
    );
    assert!(
        summary_corrections(&values[0]).is_empty(),
        "the inspection turn carries no correction"
    );

    for (index, reason) in reasons.iter().enumerate() {
        let body = &values[index + 2];
        let corrections = summary_corrections(body);
        assert_eq!(
            corrections.len(),
            index + 1,
            "each rejection appends exactly one correction to the same transcript"
        );
        let immediate = &corrections[index];
        assert!(
            immediate.contains(reason),
            "correction {} must carry the immediately preceding reason {reason:?}: {immediate:?}",
            index + 1
        );
        for older in &reasons[..index] {
            assert!(
                !immediate.contains(older),
                "correction {} must not reuse an older reason {older:?}: {immediate:?}",
                index + 1
            );
        }
        assert!(
            summary_text_blocks(body)
                .iter()
                .any(|text| text.contains(rejected[index].trim())),
            "the request carrying correction {} retains rejected candidate {}",
            index + 1,
            index + 1
        );
    }

    let final_body = &values[4];
    let final_json = serde_json::to_string(final_body).expect("the request serializes");
    for answer in rejected {
        assert!(
            summary_text_blocks(final_body)
                .iter()
                .any(|text| text.contains(answer.trim())),
            "the final request retains the earlier rejected answer {answer:?}"
        );
    }
    assert!(
        final_json.contains("No human is available in this session"),
        "the summary scope stays in the same transcript"
    );
    assert!(
        final_json.contains("The ticket queue has finished"),
        "the kickoff stays in the same transcript"
    );
    for heading in ["## Outcome", "## Report", "## Review"] {
        assert!(
            final_json.contains(heading),
            "the kickoff {heading} section stays in the same transcript"
        );
    }
    assert_eq!(
        summary_tool_result(final_body, "toolu_summary_0").as_deref(),
        Some("1"),
        "the inspection-tool history stays in the same transcript"
    );

    let artifact = summary_artifact(&dir);
    assert_eq!(
        artifact, FOURTH_SUMMARY,
        "only the accepted fourth candidate is persisted canonically"
    );
    for answer in rejected {
        assert!(
            !artifact.contains(answer.trim()),
            "a rejected candidate is never persisted: {answer:?}"
        );
    }

    let _ = fs::remove_dir_all(&dir);
}

/// Regression: an API failure on the initial attempt is fatal without a
/// correction request or a summary artifact.
#[test]
fn when_summarizing_given_api_failure_then_fails_without_correction() {
    let (step, dir, transport) =
        scripted_run(QueueOutcome::Resolved, common::SummaryScript::ApiError);

    let reason = summary_reason(&step);
    assert!(
        reason.contains("API error"),
        "the failure must name the API error: {reason}"
    );
    assert_eq!(
        summary_requests(&transport).len(),
        1,
        "an API failure is never corrected"
    );
    assert!(!dir.join("change-summary.md").exists());

    let _ = fs::remove_dir_all(&dir);
}

/// Regression: a transport failure on the initial attempt is fatal without a
/// correction request or a summary artifact.
#[test]
fn when_summarizing_given_transport_failure_then_fails_without_correction() {
    let (step, dir, transport) = scripted_run(
        QueueOutcome::Resolved,
        common::SummaryScript::TransportError,
    );

    let reason = summary_reason(&step);
    assert!(
        reason.contains("transport"),
        "the failure must name the transport error: {reason}"
    );
    assert_eq!(
        summary_requests(&transport).len(),
        ejaj::retry::MAX_ATTEMPTS as usize,
        "only the transport's own retries run; no correction is requested"
    );
    assert!(!dir.join("change-summary.md").exists());

    let _ = fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Generated ticket ids in the change summary
//
// The summary prompt forbids generated ticket ids and `Refs:` footers, and the
// summary format refuses a canonical answer that reproduces one. The reports the
// summary pass is handed keep their ids, and only the final answer is filtered:
// every test drives the real `Pipeline::ask` over the scripted external
// transport and asserts the requests the production code sent, the terminal
// `Step` and the exact artifact bytes.
// ---------------------------------------------------------------------------

/// The id of the original scripted ticket.
const ORIGINAL_TICKET_ID: &str = "1-verify-command-output";

/// The fix tickets the scripted review rounds report, in round order: the three
/// fix queues the run executes and the fourth review's fix, which is reported
/// and never queued.
const REVIEW_FIX_IDS: [&str; 4] = [
    "r1-1-verify-edge-case",
    "r2-1-verify-edge-case",
    "r3-1-verify-edge-case",
    "r4-1-verify-edge-case",
];

/// A clean, footerless answer that names no generated ticket id.
const CLEAN_SUMMARY: &str =
    "test(pipeline): summarize the run\n\nThe run and its review rounds are described by title.\n";

/// The ids every summary request must keep -- the original ticket and every
/// review-fix ticket -- in one place so a preservation assertion names them all.
fn every_generated_id() -> Vec<&'static str> {
    std::iter::once(ORIGINAL_TICKET_ID)
        .chain(REVIEW_FIX_IDS.iter().copied())
        .collect()
}

/// One scripted run that reaches the review cap with `AlwaysFixes`, so the
/// summary request is handed the original ticket and every review-fix id, the
/// fourth review's unqueued fix included. The queue ends `ReviewCapReached`,
/// which keeps the run recoverable.
fn review_cap_run(
    summary: common::SummaryScript,
) -> (Step, PathBuf, Arc<common::ScriptedTransport>) {
    let (config, transport) =
        common::scripted_pipeline_with_review(common::ReviewScript::AlwaysFixes, summary);
    let dir = scratch_dir();
    fs::create_dir_all(&dir).expect("the scratch directory is creatable");
    let repository = common::TempRepo::at(dir.clone());
    repository.commit_baseline();

    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), &dir);
    repository.keep();
    (step, dir, transport)
}

// -- Core Entry-Point Behaviors --------------------------------------------

/// The original ticket id may not reach the summary: the initial answer that
/// reproduces it is refused before publication, the one correction it buys is
/// accepted, and only the clean corrected text becomes the artifact.
#[test]
fn when_writing_summary_given_generated_ticket_id_then_rejects_before_publication() {
    let contaminated = format!(
        "test(pipeline): summarize the run\n\nThe queue completed {ORIGINAL_TICKET_ID} successfully.\n"
    );
    let script = common::SummaryScript::Turns(vec![
        common::SummaryReply::Answer(contaminated),
        common::SummaryReply::Answer(CLEAN_SUMMARY.to_string()),
    ]);
    let (step, dir, transport) = scripted_run(QueueOutcome::Resolved, script);

    assert!(
        matches!(step, Step::Complete),
        "the clean correction keeps the queue's successful outcome: {step:?}"
    );
    let requests = summary_requests(&transport);
    assert_eq!(
        requests.len(),
        2,
        "an answer naming the original id buys exactly one correction"
    );
    assert!(
        requests[1].contains("generated ticket id"),
        "the correction must name the id violation: {}",
        requests[1]
    );

    let artifact = summary_artifact(&dir);
    assert_eq!(
        artifact, CLEAN_SUMMARY,
        "only the clean corrected text is published"
    );
    assert!(
        !artifact.contains(ORIGINAL_TICKET_ID),
        "the rejected answer must never be published: {artifact:?}"
    );

    let _ = fs::remove_dir_all(&dir);
}

/// Where in an otherwise valid answer the leaked id sits. The acceptance rule
/// is an exact, case-sensitive substring of the canonical text, so the header,
/// the body and the footer are each exercised through the real pass.
#[derive(Debug, Clone, Copy)]
enum LeakRegion {
    Header,
    Body,
    Footer,
}

/// An answer whose format alone is valid and which names exactly one generated
/// ticket id, in `region`: the leak is the only thing the format has to refuse.
fn answer_leaking(id: &str, region: LeakRegion) -> String {
    match region {
        LeakRegion::Header => format!(
            "test(pipeline): summarize the run with {id}\n\n\
             The run and its review rounds are described by title.\n"
        ),
        LeakRegion::Body => format!(
            "test(pipeline): summarize the run\n\n\
             The queue completed {id} successfully.\n"
        ),
        LeakRegion::Footer => format!(
            "test(pipeline): summarize the run\n\n\
             The run and its review rounds are described by title.\n\n\
             BREAKING CHANGE: renames {id}\n"
        ),
    }
}

/// Every id this run generated refuses an otherwise valid summary on its own --
/// the original ticket, each review round's fix ticket and the fourth review's
/// unqueued fix included -- wherever the id sits in the header, the body or the
/// footer: the initial answer is rejected, the exactly one clean correction is
/// accepted, and only the clean summary is ever published.
#[test]
fn when_writing_summary_given_each_generated_ticket_id_then_rejects_before_publication() {
    // The table is the run's whole cumulative id set: an id the fixture started
    // generating without a case here would be a silently uncovered gap.
    let cases: [(&str, LeakRegion); 5] = [
        (ORIGINAL_TICKET_ID, LeakRegion::Header),
        (REVIEW_FIX_IDS[0], LeakRegion::Body),
        (REVIEW_FIX_IDS[1], LeakRegion::Footer),
        (REVIEW_FIX_IDS[2], LeakRegion::Body),
        (REVIEW_FIX_IDS[3], LeakRegion::Footer),
    ];
    let covered: Vec<&str> = cases.iter().map(|(id, _)| *id).collect();
    assert_eq!(
        covered,
        every_generated_id(),
        "every generated id -- the fourth review's unqueued fix included -- needs its own case"
    );

    for (id, region) in cases {
        let contaminated = answer_leaking(id, region);
        let script = common::SummaryScript::Turns(vec![
            common::SummaryReply::Answer(contaminated),
            common::SummaryReply::Answer(CLEAN_SUMMARY.to_string()),
        ]);
        let (step, dir, transport) = review_cap_run(script);

        assert!(
            matches!(step, Step::Retry),
            "the review cap keeps its retryable step: {step:?}"
        );

        let requests = summary_requests(&transport);
        assert_eq!(
            requests.len(),
            2,
            "`{id}` in the {region:?} buys exactly one correction"
        );
        assert!(
            requests[1].contains(id),
            "the correction transcript must replay the rejected answer carrying `{id}`: {}",
            requests[1]
        );
        assert!(
            requests[1].contains("generated ticket id"),
            "the correction must name the id violation for `{id}`: {}",
            requests[1]
        );

        let artifact = summary_artifact(&dir);
        assert_eq!(
            artifact, CLEAN_SUMMARY,
            "only the clean corrected summary is published when `{id}` leaked"
        );
        assert!(
            !artifact.contains(id),
            "the rejected answer must never be published: {artifact:?}"
        );

        let _ = fs::remove_dir_all(&dir);
    }
}

/// The fourth review's fix ticket is reported but never queued or written as a
/// ticket file; its id is still part of the checked set, so an answer that
/// reproduces it is refused at the review cap.
#[test]
fn when_writing_summary_given_review_fix_id_then_rejects_even_at_review_cap() {
    let cap_fix = REVIEW_FIX_IDS[3];
    let contaminated = format!(
        "test(pipeline): summarize the run\n\nThe review cap reported {cap_fix} without queueing it.\n"
    );
    let script = common::SummaryScript::Turns(vec![
        common::SummaryReply::Answer(contaminated),
        common::SummaryReply::Answer(CLEAN_SUMMARY.to_string()),
    ]);
    let (step, dir, transport) = review_cap_run(script);

    assert!(
        matches!(step, Step::Retry),
        "the review cap keeps its retryable step: {step:?}"
    );
    assert!(
        dir.join("tickets/r3-1-verify-edge-case.md").is_file(),
        "the third fix queue's ticket was written and executed"
    );
    assert!(
        !dir.join("tickets/r4-1-verify-edge-case.md").exists(),
        "the fourth review's fix is reported but never written as a ticket file"
    );

    let requests = summary_requests(&transport);
    assert_eq!(
        requests.len(),
        2,
        "the cap-round fix id is checked like every other id"
    );
    assert!(
        requests[0].contains(cap_fix),
        "the summary request must carry the cap-round fix id: {}",
        requests[0]
    );
    assert!(
        requests[1].contains("generated ticket id"),
        "the correction must name the id violation: {}",
        requests[1]
    );
    assert_eq!(summary_artifact(&dir), CLEAN_SUMMARY);

    let _ = fs::remove_dir_all(&dir);
}

// -- Maximum limit / Upper boundary ----------------------------------------

/// Over the full four review rounds the checked set is cumulative -- the
/// original ticket plus every review-fix id, the unqueued fourth-review fix
/// included -- with no ticket-count limit: the summary request carries them all
/// and an answer naming any of them is refused.
#[test]
fn when_writing_summary_given_all_review_rounds_then_checks_cap_round_fix_id() {
    let contaminated = "test(pipeline): summarize the run\n\nFixes r1-1-verify-edge-case and r4-1-verify-edge-case shipped.\n";
    let script = common::SummaryScript::Turns(vec![
        common::SummaryReply::Answer(contaminated.to_string()),
        common::SummaryReply::Answer(CLEAN_SUMMARY.to_string()),
    ]);
    let (step, dir, transport) = review_cap_run(script);

    assert!(matches!(step, Step::Retry), "{step:?}");
    let requests = summary_requests(&transport);
    assert_eq!(requests.len(), 2);

    for id in every_generated_id() {
        assert!(
            requests[0].contains(id),
            "the summary request must carry `{id}`: {}",
            requests[0]
        );
    }
    assert!(
        requests[1].contains("generated ticket id"),
        "an answer naming a fix id from any round is refused: {}",
        requests[1]
    );
    assert_eq!(summary_artifact(&dir), CLEAN_SUMMARY);

    let _ = fs::remove_dir_all(&dir);
}

// -- Contract & Invariant Checks -------------------------------------------

/// The reports keep their id-bearing content: the summary request carries the
/// original and review-fix ids -- the fourth review's fix included -- while a
/// review *finding* id is not a ticket id and so is not checked, and the queue
/// report, review report and ticket files are never redacted.
#[test]
fn when_building_summary_request_given_queue_and_review_reports_then_preserves_internal_ids() {
    let finding_id = "f3-edge-case";
    let answer = format!(
        "test(pipeline): summarize the run\n\nThe review reported {finding_id} without a queued fix.\n"
    );
    let script = common::SummaryScript::Turns(vec![common::SummaryReply::Answer(answer.clone())]);
    let (step, dir, transport) = review_cap_run(script);

    assert!(matches!(step, Step::Retry), "{step:?}");
    let requests = summary_requests(&transport);
    assert_eq!(
        requests.len(),
        1,
        "a review finding id is not a ticket id, so the answer needs no correction"
    );

    let request = &requests[0];
    for id in every_generated_id() {
        assert!(
            request.contains(id),
            "the summary request must keep `{id}`: {request}"
        );
    }
    assert!(
        request.contains(finding_id),
        "the summary request must keep the review finding id too: {request}"
    );
    assert_eq!(
        summary_artifact(&dir),
        answer,
        "the answer naming only a finding id is accepted unchanged"
    );

    // The internal artifacts keep their ids.
    let report = fs::read_to_string(dir.join("queue-report.md"))
        .unwrap_or_else(|e| common::failed("the queue report artifact", &e.to_string()));
    assert!(report.contains(ORIGINAL_TICKET_ID), "{report}");
    let review = fs::read_to_string(dir.join("review.md"))
        .unwrap_or_else(|e| common::failed("the review artifact", &e.to_string()));
    assert!(review.contains("r4-1-verify-edge-case"), "{review}");
    for id in &REVIEW_FIX_IDS[..3] {
        assert!(
            dir.join(format!("tickets/{id}.md")).is_file(),
            "the executed fix ticket {id} must be on disk: {review}"
        );
    }
    assert!(
        !dir.join("tickets/r4-1-verify-edge-case.md").exists(),
        "the fourth review's fix is reported but never written as a ticket file"
    );

    let _ = fs::remove_dir_all(&dir);
}

/// The shared gate runs on both attempts: the initial answer is refused for the
/// id, exactly one correction is sent carrying the validator's reason, and the
/// clean corrected text is accepted.
#[test]
fn when_correcting_summary_given_ticket_id_violation_then_accepts_clean_text() {
    let contaminated = format!(
        "test(pipeline): summarize the run\n\nThe queue completed {ORIGINAL_TICKET_ID} successfully.\n"
    );
    let script = common::SummaryScript::Turns(vec![
        common::SummaryReply::Answer(contaminated),
        common::SummaryReply::Answer(CLEAN_SUMMARY.to_string()),
    ]);
    let (step, dir, transport) = scripted_run(QueueOutcome::Resolved, script);

    assert!(matches!(step, Step::Complete), "{step:?}");
    let requests = summary_requests(&transport);
    assert_eq!(requests.len(), 2, "exactly one correction is sent");
    assert!(
        requests[1].contains("generated ticket id"),
        "the correction must name the id violation: {}",
        requests[1]
    );
    assert_eq!(summary_artifact(&dir), CLEAN_SUMMARY);

    let _ = fs::remove_dir_all(&dir);
}

/// Every candidate that reproduces a generated ticket id is refused: the pass
/// stays a generation failure, its reason names the violation without echoing
/// the id, it exhausts the four-candidate limit, and no artifact is left.
#[test]
fn when_correcting_summary_given_second_ticket_id_violation_then_fails_without_artifact() {
    let first = format!(
        "test(pipeline): summarize the run\n\nThe queue completed {ORIGINAL_TICKET_ID} once more.\n"
    );
    let second = format!(
        "test(pipeline): summarize the run\n\nThe queue completed {ORIGINAL_TICKET_ID} a second time.\n"
    );
    let script = common::SummaryScript::Turns(vec![
        common::SummaryReply::Answer(first),
        common::SummaryReply::Answer(second),
    ]);
    let (step, dir, transport) = scripted_run(QueueOutcome::Resolved, script);

    let reason = summary_reason(&step);
    assert!(reason.contains("generated ticket id"), "{reason}");
    assert!(
        !reason.contains(ORIGINAL_TICKET_ID),
        "the fatal reason must not echo the id: {reason}"
    );
    assert_eq!(
        summary_requests(&transport).len(),
        4,
        "the repeated id violation exhausts the four-candidate limit"
    );
    assert!(
        !dir.join("change-summary.md").exists(),
        "a rejected summary leaves no artifact behind"
    );

    let _ = fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Git preflight: a clean repository is required before any agent runs
//
// Every invocation is anchored to the root it was built with, and the first
// thing `ask` does is read that root's Git status: a plan request may only be
// sent, and an artifact only written, while the working tree is clean. These
// tests drive the real `Pipeline::ask` against disposable repositories in the
// system temporary directory -- with the real `git` process, never a mocked Git
// service -- so the package checkout is never staged, committed or inspected.
// ---------------------------------------------------------------------------

/// A disposable Git root beside the run's artifacts, inside the scratch
/// directory the test already removes.
fn temp_repository(dir: &Path) -> common::TempRepo {
    common::TempRepo::at(dir.join("repository"))
}

/// A disposable repository whose baseline is one committed tracked file plus a
/// committed `.gitignore` covering `ignored.txt` and `.ejaj/`, so a case can
/// plant exactly one porcelain class on top of a tree that starts clean.
fn baselined_repository(dir: &Path) -> common::TempRepo {
    let repository = temp_repository(dir);
    repository.write("tracked.txt", "baseline\n");
    repository.write(".gitignore", "ignored.txt\n.ejaj/\n");
    repository.commit_all("baseline");
    repository
}

/// The fatal Git failure's own reason, with the stage the run reported checked
/// against the preflight the caller planted: the one staged result carries the
/// preflight stage, and any other step is the failure this test exists to rule
/// out.
fn preflight_reason(step: &Step) -> &str {
    match step {
        Step::GitFailed { stage, reason } => {
            assert_eq!(
                *stage, "preflight",
                "an unusable project root must be reported at the preflight stage: {reason}"
            );
            reason.as_str()
        }
        other => panic!("expected a fatal Git failure at the preflight stage, got {other:?}"),
    }
}

/// One scripted invocation against `repository`, with the run's artifacts in
/// `dir` -- a path inside that repository, so a successful run has a change to
/// commit -- and the scripted transport handed back so a test can assert the run
/// never reached it.
fn scripted_invocation(
    repository: &common::TempRepo,
    dir: &Path,
) -> (Step, Arc<common::ScriptedTransport>) {
    let (config, transport) =
        common::scripted_pipeline_with(common::GateScript::Passing, common::SummaryScript::Summary);
    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), dir);
    (step, transport)
}

#[test]
fn when_checking_a_repository_given_no_visible_changes_then_allows_preflight() {
    let dir = scratch_dir();
    let repository = temp_repository(&dir);
    // The default-branch finalizer fast-forwards a real default branch and
    // stashes real changes, so the fixture begins with one published baseline
    // commit rather than an unborn branch.
    repository.commit_baseline();
    assert_eq!(
        repository.status(),
        "",
        "the disposable repository starts with an empty, clean worktree"
    );

    let (step, transport) = scripted_invocation(&repository, repository.path());

    assert!(
        matches!(step, Step::Complete),
        "a clean repository must reach the agents and complete: {step:?}"
    );

    // The plan agent is the first pass reached, and the scripted run went
    // through every later phase from there.
    let requests = transport.requests.lock().unwrap();
    assert_eq!(
        requests.len(),
        8,
        "a clean repository runs the whole scripted pipeline"
    );
    assert!(
        requests[0].to_string().contains("The design is settled."),
        "the first agent reached is the plan agent: {}",
        requests[0]
    );
    drop(requests);

    // The plan artifact -- the run's first write -- is present, and the run
    // left its own repository clean.
    assert!(
        repository.path().join("plan.md").is_file(),
        "the plan artifact is written"
    );
    assert_eq!(
        repository.status(),
        "",
        "the run leaves its own repository clean"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn when_starting_a_pipeline_given_untracked_changes_then_fails_before_agents() {
    let dir = scratch_dir();
    let repository = temp_repository(&dir);
    repository.write("left-behind.txt", "a file no commit knows about\n");

    let (step, transport) = scripted_invocation(&repository, repository.path());

    let reason = preflight_reason(&step);
    assert!(
        reason.contains("clean working tree is required before the agents start"),
        "the failure must identify the clean tree it needs: {reason}"
    );
    assert!(
        reason.contains("left-behind.txt"),
        "the failure must name the untracked change: {reason}"
    );

    // No LLM request of any phase, and no artifact: the gate is the first
    // operation of the invocation.
    assert!(
        transport.requests.lock().unwrap().is_empty(),
        "preflight must run before any agent request"
    );
    assert!(
        transport.urls_called.lock().unwrap().is_empty(),
        "preflight must reach no endpoint at all"
    );
    assert!(
        !repository.path().join("plan.md").exists(),
        "a gated run writes no artifact"
    );
    assert_eq!(
        repository.status(),
        "?? left-behind.txt\n",
        "the untracked file is left exactly as it was"
    );

    let _ = fs::remove_dir_all(&dir);
}

/// One porcelain class planted on a baselined repository, and the path Git must
/// then report as visible.
struct PorcelainCase {
    label: &'static str,
    plant: fn(&common::TempRepo),
    visible: Option<&'static str>,
}

#[test]
fn when_starting_a_pipeline_given_visible_staged_unstaged_and_untracked_changes_then_rejects_start()
{
    // Every class at once, each on its own clean baseline: a staged addition to
    // a tracked file, an unstaged edit to a tracked file, an untracked file, an
    // ignored file and a file under the pipeline's own ignored artifact
    // directory. Only the first three are visible to Git and must gate the run.
    let cases = [
        PorcelainCase {
            label: "a staged change",
            plant: |repository| {
                repository.write("tracked.txt", "staged\n");
                repository.git(&["add", "tracked.txt"]);
            },
            visible: Some("tracked.txt"),
        },
        PorcelainCase {
            label: "an unstaged change to a tracked file",
            plant: |repository| repository.write("tracked.txt", "unstaged\n"),
            visible: Some("tracked.txt"),
        },
        PorcelainCase {
            label: "an untracked file",
            plant: |repository| repository.write("fresh.txt", "untracked\n"),
            visible: Some("fresh.txt"),
        },
        PorcelainCase {
            label: "an ignored file only",
            plant: |repository| repository.write("ignored.txt", "invisible to Git\n"),
            visible: None,
        },
        PorcelainCase {
            label: "a file under .ejaj/ only",
            plant: |repository| repository.write(".ejaj/run/plan.md", "# plan\n"),
            visible: None,
        },
    ];

    for case in cases {
        let dir = scratch_dir();
        let repository = baselined_repository(&dir);
        (case.plant)(&repository);
        let status = repository.status();

        let (step, transport) = scripted_invocation(&repository, repository.path());

        match case.visible {
            Some(path) => {
                assert!(
                    status.contains(path),
                    "{}: the planted change must be visible to Git: {status:?}",
                    case.label
                );
                let reason = preflight_reason(&step);
                assert!(
                    reason.contains(path),
                    "{}: the failure must name the visible change: {reason}",
                    case.label
                );
                assert!(
                    transport.requests.lock().unwrap().is_empty(),
                    "{}: a visible change gates the run before any request",
                    case.label
                );
                assert!(
                    !repository.path().join("plan.md").exists(),
                    "{}: a gated run writes no artifact",
                    case.label
                );
            }
            None => {
                assert_eq!(
                    status, "",
                    "{}: the planted change must be invisible to Git",
                    case.label
                );
                assert!(
                    matches!(step, Step::Complete),
                    "{}: an invisible change must not gate the run: {step:?}",
                    case.label
                );
                assert!(
                    !transport.requests.lock().unwrap().is_empty(),
                    "{}: the run must reach the agents",
                    case.label
                );
                assert!(
                    repository.path().join("plan.md").is_file(),
                    "{}: the run must write its artifacts",
                    case.label
                );
            }
        }

        let _ = fs::remove_dir_all(&dir);
    }
}

#[test]
fn when_rejecting_a_dirty_repository_given_pending_changes_then_preserves_repository_status() {
    let dir = scratch_dir();
    let repository = baselined_repository(&dir);
    repository.write("tracked.txt", "unfinished work\n");
    repository.write("fresh.txt", "unfinished work\n");
    let before = repository.status();
    assert!(
        before.contains("tracked.txt") && before.contains("fresh.txt"),
        "the planted changes must be visible before the run: {before:?}"
    );
    assert_eq!(repository.staged(), "", "nothing is staged before the run");

    let (step, _transport) = scripted_invocation(&repository, repository.path());
    preflight_reason(&step);

    // The check is read-only: it neither stages the pending work nor changes,
    // hides or restores a byte of it.
    assert_eq!(
        repository.status(),
        before,
        "the preflight must leave the dirty repository exactly as dirty"
    );
    assert_eq!(repository.staged(), "", "the preflight must stage nothing");
    assert_eq!(
        fs::read_to_string(repository.path().join("tracked.txt")).unwrap(),
        "unfinished work\n",
        "the tracked edit survives the check"
    );
    assert_eq!(
        fs::read_to_string(repository.path().join("fresh.txt")).unwrap(),
        "unfinished work\n",
        "the untracked file survives the check"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn when_starting_a_pipeline_given_non_repository_root_then_fails_before_agents() {
    let dir = scratch_dir();
    let root = dir.join("not-a-repository");
    fs::create_dir_all(&root).expect("the plain directory is creatable");

    let (config, transport) =
        common::scripted_pipeline_with(common::GateScript::Passing, common::SummaryScript::Summary);
    let mut pipeline = Pipeline::new(&config, common::scope(), &root);
    let step = pipeline.ask(&common::harmless_plan_request(), &dir);

    // An unavailable status is fatal: it is never read as an empty, clean tree.
    let reason = preflight_reason(&step);
    assert!(
        reason.contains("readable status is required before the agents start"),
        "the failure must identify the readable status it needs: {reason}"
    );
    assert!(
        reason.contains(&root.display().to_string()),
        "the failure must name the root it could not read: {reason}"
    );
    assert!(
        transport.requests.lock().unwrap().is_empty(),
        "no agent may be reached without a usable repository"
    );
    assert!(
        transport.urls_called.lock().unwrap().is_empty(),
        "the unusable root must reach no endpoint at all"
    );
    assert!(
        !dir.join("plan.md").exists(),
        "no artifact may be written without a usable repository"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn when_starting_a_pipeline_given_changes_from_a_prior_incomplete_run_then_fails_before_agents() {
    let scratch = scratch_dir();
    let repository = temp_repository(&scratch);

    // A clean baseline with one tracked source fixture and an ignored .ejaj/
    // for the run's own artifacts, so the incomplete run's source edit is the
    // only non-ignored change it can leave behind.
    repository.write(".gitignore", ".ejaj/\n");
    repository.write("src/fixture.rs", "// the tracked source fixture\n");
    repository.commit_all("baseline");
    let baseline_commits = head_commits(&repository);

    // The first invocation runs a real incomplete queue: its gate fails, while
    // its own code-agent command edits the tracked source fixture. The run's
    // artifacts land under the ignored .ejaj/, so the source edit is the one
    // non-ignored change the run really leaves.
    let fixture = repository.path().join("src/fixture.rs");
    let command = format!(
        "printf 'modified by the prior incomplete run\\n' >> '{}'",
        fixture.display()
    );
    let (first_config, first_transport) = common::scripted_pipeline_with_command(
        common::GateScript::Failing,
        common::SummaryScript::Summary,
        command,
    );
    let first_run = repository.path().join(".ejaj/first-run");
    let mut first_pipeline = Pipeline::new(&first_config, common::scope(), repository.path());
    let first = first_pipeline.ask(&common::harmless_plan_request(), &first_run);

    assert!(
        matches!(first, Step::Retry),
        "an incomplete queue stays retryable: {first:?}"
    );
    assert!(
        !first_transport.requests.lock().unwrap().is_empty(),
        "the first invocation really reached the agents"
    );
    assert_eq!(
        head_commits(&repository),
        baseline_commits,
        "an incomplete run commits nothing"
    );
    assert_eq!(repository.staged(), "", "an incomplete run stages nothing");
    assert_eq!(
        repository.status(),
        " M src/fixture.rs\n",
        "the incomplete run leaves exactly its source change uncommitted"
    );
    assert_eq!(
        fs::read_to_string(&fixture).unwrap(),
        "// the tracked source fixture\nmodified by the prior incomplete run\nmodified by the prior incomplete run\n",
        "the source fixture carries the edit made by each retry attempt"
    );

    // The next invocation is a fresh pipeline over the same root, and it is
    // gated at the preflight -- before the plan agent and before any artifact
    // of its own.
    let gated_run = repository.path().join(".ejaj/gated-run");
    let (second, second_transport) = scripted_invocation(&repository, &gated_run);
    let reason = preflight_reason(&second);
    assert!(
        reason.contains("clean working tree is required before the agents start"),
        "the next invocation must demand a clean tree: {reason}"
    );
    assert!(
        reason.contains("src/fixture.rs"),
        "the failure must name the prior run's change: {reason}"
    );
    assert!(
        second_transport.requests.lock().unwrap().is_empty(),
        "the gated invocation must make no request at all"
    );
    assert!(
        second_transport.urls_called.lock().unwrap().is_empty(),
        "the gated invocation must reach no endpoint at all"
    );
    assert!(
        !gated_run.join("plan.md").exists(),
        "the gated invocation must write no artifact"
    );
    assert_eq!(
        repository.status(),
        " M src/fixture.rs\n",
        "the prior run's change is still there and still the only one"
    );

    // Restoring the tracked fixture unlocks the next invocation.
    repository.git(&["checkout", "--", "src/fixture.rs"]);
    assert_eq!(repository.status(), "", "the restored tree is clean again");
    let restored_run = repository.path().join("restored-run");
    let (third, third_transport) = scripted_invocation(&repository, &restored_run);
    assert!(
        matches!(third, Step::Complete),
        "a restored clean tree unlocks the next invocation: {third:?}"
    );
    assert!(
        !third_transport.requests.lock().unwrap().is_empty(),
        "the restored invocation reaches the agents"
    );
    assert!(restored_run.join("plan.md").is_file());

    let _ = fs::remove_dir_all(&scratch);
}

/// The one test the parent re-runs in a child process in which the `git`
/// executable cannot be found at all.
const UNAVAILABLE_GIT_TEST: &str =
    "when_starting_a_pipeline_given_unreadable_repository_status_then_reports_preflight_failure";

/// The variable that tells a copy of this binary it is the child.
const UNAVAILABLE_GIT_CHILD: &str = "EJAJ_PREFLIGHT_UNAVAILABLE_GIT_CHILD";

/// The variable carrying the repository root from the parent to the child.
const UNAVAILABLE_GIT_ROOT: &str = "EJAJ_PREFLIGHT_UNAVAILABLE_GIT_ROOT";

#[test]
fn when_starting_a_pipeline_given_unreadable_repository_status_then_reports_preflight_failure() {
    // The child copy: the parent pointed PATH at a directory that holds no
    // executable, so the real `git` cannot be spawned in this process. The run
    // still goes through the real `Pipeline::ask` and the real process
    // boundary -- there is no mocked Git service, only a missing binary.
    if std::env::var_os(UNAVAILABLE_GIT_CHILD).is_some() {
        let root = std::env::var(UNAVAILABLE_GIT_ROOT)
            .expect("the parent hands the repository root to the child");
        let dir = scratch_dir();
        let (config, transport) = common::scripted_pipeline_with(
            common::GateScript::Passing,
            common::SummaryScript::Summary,
        );
        let mut pipeline = Pipeline::new(&config, common::scope(), root);
        let step = pipeline.ask(&common::harmless_plan_request(), &dir);

        // A spawn failure is fatal too, and it is never an empty, clean status.
        let reason = preflight_reason(&step);
        println!("EJAJ-PREFLIGHT-REASON={reason}");
        println!(
            "EJAJ-PREFLIGHT-REQUESTS={}",
            transport.requests.lock().unwrap().len()
        );
        println!("EJAJ-PREFLIGHT-ARTIFACT={}", dir.join("plan.md").exists());
        return;
    }

    // The parent: the repository itself is a real, clean, usable repository, so
    // only the unavailable `git` can refuse the run.
    let dir = scratch_dir();
    let repository = common::TempRepo::new();
    assert_eq!(repository.status(), "", "the repository is clean");

    let executables = dir.join("no-executables");
    fs::create_dir_all(&executables).expect("the empty PATH directory is creatable");

    let exe = std::env::current_exe().expect("the test binary path is known");
    let output = Command::new(exe)
        .args([
            "--exact",
            UNAVAILABLE_GIT_TEST,
            "--nocapture",
            "--test-threads=1",
        ])
        .env(UNAVAILABLE_GIT_CHILD, "1")
        .env(UNAVAILABLE_GIT_ROOT, repository.path())
        .env("PATH", &executables)
        .output()
        .expect("the child test process starts and is waited for");
    let transcript = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        output.status.success(),
        "the child run must report the fatal preflight failure it saw:\n{transcript}"
    );
    assert!(
        transcript.contains("EJAJ-PREFLIGHT-REASON="),
        "the child must report the reason it saw:\n{transcript}"
    );
    assert!(
        transcript.contains("could not run"),
        "the reason must name the status it could not produce:\n{transcript}"
    );
    assert!(
        transcript.contains("readable status is required before the agents start"),
        "the failure must identify the readable status it needs:\n{transcript}"
    );
    assert!(
        transcript.contains("EJAJ-PREFLIGHT-REQUESTS=0"),
        "no agent request may be made when the status is unavailable:\n{transcript}"
    );
    assert!(
        transcript.contains("EJAJ-PREFLIGHT-ARTIFACT=false"),
        "no artifact may be written when the status is unavailable:\n{transcript}"
    );

    let _ = fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Committing a successful run
//
// A resolved queue that passed review and wrote its change summary is the only
// run that stages and commits. Every Git operation runs against the run's own
// disposable repository with the real Git process: the external LLM boundary is
// scripted, and the Git boundary is real.
// ---------------------------------------------------------------------------

/// A repository with a committed baseline, so HEAD exists and the preflight
/// sees a clean tree before the run.
fn committed_repository() -> common::TempRepo {
    let repository = common::TempRepo::new();
    repository.write(".gitignore", ".ejaj/\n");
    repository.commit_all("baseline");
    repository
}

/// The raw message bytes of the current HEAD commit: the commit object
/// `git cat-file commit HEAD` printed, with its header -- the tree, parents,
/// author and committer lines -- cut off at the blank-line separator. Captured
/// as bytes rather than decoded, so a lossy conversion can never hide a byte
/// the comparison must see, Git's own log rendering never muddies it, and the
/// message's terminal LF is part of what is compared.
fn head_message_bytes(repository: &common::TempRepo) -> Vec<u8> {
    let raw = repository.git_bytes(&["cat-file", "commit", "HEAD"]);
    let separator = raw
        .windows(2)
        .position(|window| window == b"\n\n")
        .expect("a commit object separates its header from its message");
    raw[separator + 2..].to_vec()
}

/// The exact message of the current HEAD commit, decoded from its raw bytes.
fn head_message(repository: &common::TempRepo) -> String {
    String::from_utf8(head_message_bytes(repository))
        .expect("the commit message the run recorded is UTF-8")
}

/// How many commits HEAD reaches.
fn head_commits(repository: &common::TempRepo) -> usize {
    repository
        .git(&["rev-list", "--count", "HEAD"])
        .trim()
        .parse()
        .expect("git rev-list --count prints a number")
}

/// Every path in HEAD's tree, one per line.
fn head_paths(repository: &common::TempRepo) -> Vec<String> {
    repository
        .git(&["ls-tree", "-r", "--name-only", "HEAD"])
        .lines()
        .map(str::to_string)
        .collect()
}

/// The stage and reason of a fatal Git failure, or a panic naming what came
/// instead.
fn git_failure(step: &Step) -> (&'static str, &str) {
    match step {
        Step::GitFailed { stage, reason } => (*stage, reason.as_str()),
        other => panic!("expected a fatal Git failure, got {other:?}"),
    }
}

/// Install a real pre-commit hook that records that Git reached the commit
/// operation, so a test can prove Git was never asked to commit at all.
fn install_commit_marker_hook(repository: &common::TempRepo, marker: &Path) {
    let sentinel = marker.display().to_string();
    let hook = format!("#!/bin/sh\nprintf 'committed\\n' > '{sentinel}'\n");
    install_hook(repository, &hook);
}

/// Install a real pre-commit hook that refuses every commit, so the commit
/// operation fails on a local rejection rather than a staging error.
fn install_rejecting_hook(repository: &common::TempRepo) {
    install_hook(
        repository,
        "#!/bin/sh\necho 'the local pre-commit hook refuses this commit' >&2\nexit 1\n",
    );
}

/// Write an executable pre-commit hook into the repository, pinning the local
/// hooks path so a machine-wide Git configuration cannot redirect it.
fn install_hook(repository: &common::TempRepo, script: &str) {
    repository.git(&["config", "core.hooksPath", ".git/hooks"]);
    let hooks = repository.path().join(".git/hooks");
    fs::create_dir_all(&hooks).expect("the hooks directory is creatable");
    let path = hooks.join("pre-commit");
    fs::write(&path, script).expect("the hook is writable");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(&path).expect("the hook exists").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).expect("the hook is executable");
    }
}

/// The minimum valid success: one resolved ticket, a passing review, a valid
/// summary and a run-produced non-ignored artifact. Completion is observed only
/// after exactly one commit exists, and its message is the summary bytes.
#[test]
fn when_finishing_a_run_given_a_passing_review_then_commits_the_summary() {
    let (config, transport) =
        common::scripted_pipeline_with(common::GateScript::Passing, common::SummaryScript::Summary);
    let repository = committed_repository();
    let before = head_commits(&repository);

    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), repository.path());

    assert!(
        matches!(step, Step::Complete),
        "a passing review that wrote its summary completes: {step:?}"
    );
    assert_eq!(
        head_commits(&repository),
        before + 1,
        "completion is observed only after exactly one commit exists"
    );
    assert_eq!(
        head_message(&repository),
        common::SCRIPTED_SUMMARY,
        "the commit message is the summary artifact bytes"
    );
    assert_eq!(
        transport.urls_called.lock().unwrap().len(),
        8,
        "the run still drove every phase through the external boundary"
    );
}

/// The upper boundary: additions, modifications and deletions all visible, an
/// ignored .ejaj/ artifact excluded, exactly one new commit, and the raw
/// commit-message bytes -- terminal LF included -- equal to the summary.
#[test]
fn when_finishing_a_run_given_multiple_file_changes_then_commits_all_visible_changes_and_exact_summary_bytes()
 {
    let repository = common::TempRepo::new();
    repository.write(".gitignore", ".ejaj/\n");
    repository.write("modified.txt", "before\n");
    repository.write("deleted.txt", "before\n");
    repository.commit_all("baseline");
    let before = head_commits(&repository);

    let repo = repository.path().display().to_string();
    let command = format!(
        "printf 'after\\n' > '{repo}/modified.txt' \
         && rm -f '{repo}/deleted.txt' \
         && mkdir -p '{repo}/.ejaj' \
         && printf 'scratch\\n' > '{repo}/.ejaj/scratch.txt' \
         && printf 'fresh\\n' > '{repo}/added.txt'"
    );
    let (config, _transport) = common::scripted_pipeline_with_command(
        common::GateScript::Passing,
        common::SummaryScript::Summary,
        command,
    );

    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), repository.path());

    assert!(matches!(step, Step::Complete), "{step:?}");
    assert_eq!(
        head_commits(&repository),
        before + 1,
        "a successful run advances HEAD by exactly one commit"
    );
    let written_summary = fs::read(repository.path().join("change-summary.md"))
        .expect("the summary artifact is readable");
    assert_eq!(
        head_message_bytes(&repository),
        written_summary,
        "the raw commit message bytes are exactly the written artifact bytes, terminal LF included"
    );
    assert_eq!(
        written_summary.as_slice(),
        common::SCRIPTED_SUMMARY.as_bytes(),
        "the artifact is the canonical summary bytes"
    );
    assert!(
        written_summary.ends_with(b"\n"),
        "the canonical summary really does end with a terminal LF"
    );

    let paths = head_paths(&repository);
    for expected in [
        "plan.md",
        "spec.md",
        "ticket-triage.md",
        "queue-report.md",
        "review.md",
        "change-summary.md",
        "added.txt",
        "modified.txt",
    ] {
        assert!(
            paths.iter().any(|path| path == expected),
            "{expected} must be committed: {paths:?}"
        );
    }
    assert!(
        !paths.iter().any(|path| path == "deleted.txt"),
        "the deletion must be committed: {paths:?}"
    );
    assert!(
        !paths.iter().any(|path| path.starts_with(".ejaj/")),
        "the ignored .ejaj/ artifact must never be committed: {paths:?}"
    );
    assert!(
        repository.path().join(".ejaj/scratch.txt").is_file(),
        "the ignored artifact exists on disk but stays out of the commit"
    );
    assert_eq!(
        repository.git(&["show", "HEAD:modified.txt"]),
        "after\n",
        "the modification was committed"
    );
    assert_eq!(
        repository.git(&["show", "HEAD:added.txt"]),
        "fresh\n",
        "the addition was committed"
    );
    assert_eq!(
        repository.status(),
        "",
        "a successful run leaves its repository clean"
    );
}

/// Every termination that is not a resolved queue with a passing review: an
/// incomplete queue, a stopped queue, an incomplete review and the four-review
/// cap all keep their retryable step, preserve the summary artifact and commit
/// nothing.
#[test]
fn when_finishing_a_run_given_incomplete_or_review_incomplete_then_does_not_commit() {
    // An incomplete queue: the ticket's gate fails, so the queue drains without
    // completing every ticket.
    {
        let (config, _transport) = common::scripted_pipeline_with(
            common::GateScript::Failing,
            common::SummaryScript::Summary,
        );
        let repository = committed_repository();
        let before = head_commits(&repository);

        let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
        let step = pipeline.ask(&common::harmless_plan_request(), repository.path());

        assert!(
            matches!(step, Step::Retry),
            "an incomplete queue stays retryable: {step:?}"
        );
        assert_eq!(
            head_commits(&repository),
            before,
            "no commit for an incomplete queue"
        );
        assert_eq!(
            repository.staged(),
            "",
            "no staging for an incomplete queue"
        );
        assert!(
            repository.path().join("change-summary.md").is_file(),
            "the summary artifact is preserved"
        );
    }

    // A stopped queue: the status snapshot cannot be persisted, so the queue
    // stops before the ticket starts.
    {
        let (config, _transport) = common::scripted_pipeline_with(
            common::GateScript::Passing,
            common::SummaryScript::Summary,
        );
        let repository = committed_repository();
        fs::create_dir_all(repository.path().join("tickets/queue-status.json"))
            .expect("the status path is plantable as a directory");
        let before = head_commits(&repository);

        let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
        let step = pipeline.ask(&common::harmless_plan_request(), repository.path());

        assert!(
            matches!(step, Step::Retry),
            "a stopped queue stays retryable: {step:?}"
        );
        assert_eq!(
            head_commits(&repository),
            before,
            "no commit for a stopped queue"
        );
        assert_eq!(repository.staged(), "", "no staging for a stopped queue");
        assert!(
            repository.path().join("change-summary.md").is_file(),
            "the summary artifact is preserved"
        );
    }

    // A review that ends without a handoff.
    {
        let (config, _transport) = common::scripted_pipeline_with_review(
            common::ReviewScript::TurnEnd,
            common::SummaryScript::Summary,
        );
        let repository = committed_repository();
        let before = head_commits(&repository);

        let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
        let step = pipeline.ask(&common::harmless_plan_request(), repository.path());

        assert!(
            matches!(step, Step::Retry),
            "an incomplete review stays retryable: {step:?}"
        );
        assert_eq!(
            head_commits(&repository),
            before,
            "no commit for an incomplete review"
        );
        assert_eq!(
            repository.staged(),
            "",
            "no staging for an incomplete review"
        );
        assert!(repository.path().join("change-summary.md").is_file());
    }

    // The four-review cap: the review still reports fixes on the final allowed
    // round.
    {
        let (config, _transport) = common::scripted_pipeline_with_review(
            common::ReviewScript::AlwaysFixes,
            common::SummaryScript::Summary,
        );
        let repository = committed_repository();
        let before = head_commits(&repository);

        let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
        let step = pipeline.ask(&common::harmless_plan_request(), repository.path());

        assert!(
            matches!(step, Step::Retry),
            "the review cap stays retryable: {step:?}"
        );
        assert_eq!(
            head_commits(&repository),
            before,
            "no commit at the review cap"
        );
        assert_eq!(repository.staged(), "", "no staging at the review cap");
        assert!(repository.path().join("change-summary.md").is_file());
    }
}

/// A triage result that does not authorize the queue, a summary answer the
/// format refuses, and a summary artifact that cannot be written: none of them
/// may stage or commit anything.
#[test]
fn when_finishing_a_run_given_triage_or_summary_rejection_then_does_not_commit() {
    // A blocked triage: the queue never starts.
    {
        let (config, _transport) =
            common::scripted_triage_pipeline(common::TriageScript::Blocked(TriageState::NeedsInfo));
        let repository = committed_repository();
        let before = head_commits(&repository);

        let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
        let step = pipeline.ask(&common::harmless_plan_request(), repository.path());

        assert!(
            matches!(step, Step::Retry),
            "a blocked triage stays retryable: {step:?}"
        );
        assert_eq!(
            head_commits(&repository),
            before,
            "a blocked triage creates no commit"
        );
        assert_eq!(repository.staged(), "", "a blocked triage stages nothing");
        assert!(
            !repository.path().join("change-summary.md").exists(),
            "a blocked triage never reaches the summary"
        );
    }

    // An answer the format refuses: the generation fails before any commit.
    {
        let (config, _transport) = common::scripted_pipeline_with(
            common::GateScript::Passing,
            common::SummaryScript::Answer("feat: Add the thing\n\nBody line.\n"),
        );
        let repository = committed_repository();
        let before = head_commits(&repository);

        let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
        let step = pipeline.ask(&common::harmless_plan_request(), repository.path());

        assert!(
            matches!(step, Step::SummaryFailed { .. }),
            "a refused summary is a generation failure: {step:?}"
        );
        assert_eq!(
            head_commits(&repository),
            before,
            "a refused summary creates no commit"
        );
        assert_eq!(repository.staged(), "", "a refused summary stages nothing");
        assert!(!repository.path().join("change-summary.md").exists());
    }

    // The summary artifact cannot be written.
    {
        let (config, _transport) = common::scripted_pipeline_with(
            common::GateScript::Passing,
            common::SummaryScript::Summary,
        );
        let repository = committed_repository();
        fs::create_dir_all(repository.path().join("change-summary.md"))
            .expect("the summary path is plantable as a directory");
        let before = head_commits(&repository);

        let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
        let step = pipeline.ask(&common::harmless_plan_request(), repository.path());

        match &step {
            Step::WriteFailed { artifact, .. } => assert_eq!(*artifact, "change-summary"),
            other => panic!("expected the change-summary write failure, got {other:?}"),
        }
        assert_eq!(
            head_commits(&repository),
            before,
            "a write failure creates no commit"
        );
        assert_eq!(repository.staged(), "", "a write failure stages nothing");
    }
}

/// A summary-generation failure and an unwritable summary artifact: neither may
/// reach the Git commit operation, let alone create a commit.
#[test]
fn when_finishing_a_run_given_summary_generation_or_artifact_write_failure_then_does_not_commit() {
    // A generation failure: the summary pass is refused at the API.
    {
        let (config, _transport) = common::scripted_pipeline_with(
            common::GateScript::Passing,
            common::SummaryScript::ApiError,
        );
        let repository = committed_repository();
        let marker = scratch_dir().join("commit-ran");
        install_commit_marker_hook(&repository, &marker);
        let before = head_commits(&repository);

        let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
        let step = pipeline.ask(&common::harmless_plan_request(), repository.path());

        assert!(
            matches!(step, Step::SummaryFailed { .. }),
            "a generation failure is fatal: {step:?}"
        );
        assert_eq!(
            head_commits(&repository),
            before,
            "a generation failure creates no commit"
        );
        assert_eq!(
            repository.staged(),
            "",
            "a generation failure stages nothing"
        );
        assert!(
            !marker.exists(),
            "Git must never be asked to commit when generation failed"
        );
        assert!(!repository.path().join("change-summary.md").exists());
        if let Some(parent) = marker.parent() {
            let _ = fs::remove_dir_all(parent);
        }
    }

    // The summary artifact cannot be written: the pass succeeded, the write did
    // not, and no Git operation runs.
    {
        let (config, _transport) = common::scripted_pipeline_with(
            common::GateScript::Passing,
            common::SummaryScript::Summary,
        );
        let repository = committed_repository();
        fs::create_dir_all(repository.path().join("change-summary.md"))
            .expect("the summary path is plantable as a directory");
        let before = head_commits(&repository);

        let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
        let step = pipeline.ask(&common::harmless_plan_request(), repository.path());

        match &step {
            Step::WriteFailed { artifact, .. } => assert_eq!(*artifact, "change-summary"),
            other => panic!("expected the change-summary write failure, got {other:?}"),
        }
        assert_eq!(
            head_commits(&repository),
            before,
            "a write failure creates no commit"
        );
        assert_eq!(repository.staged(), "", "a write failure stages nothing");
    }
}

/// The empty boundary: a successful scripted pipeline that leaves no
/// committable change. The commit itself fails, that failure is fatal, and no
/// empty commit is created to conceal the unchanged tree.
#[test]
fn when_committing_changes_given_no_visible_changes_then_does_not_create_an_empty_commit() {
    let (config, _transport) =
        common::scripted_pipeline_with(common::GateScript::Passing, common::SummaryScript::Summary);
    let repository = committed_repository();
    let before = head_commits(&repository);
    // The artifacts land outside the repository, so the successful pipeline
    // leaves it with nothing to commit.
    let artifacts = scratch_dir();
    fs::create_dir_all(&artifacts).expect("the artifact directory is creatable");

    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), &artifacts);

    let (stage, reason) = git_failure(&step);
    assert_eq!(
        stage, "commit",
        "the no-changes failure is the commit operation: {reason}"
    );
    assert_eq!(
        head_commits(&repository),
        before,
        "no empty commit is created to conceal an unchanged tree"
    );
    assert_eq!(repository.staged(), "", "nothing remains staged");
    assert!(
        artifacts.join("change-summary.md").is_file(),
        "the summary artifact was written even though the commit could not be"
    );

    let _ = fs::remove_dir_all(&artifacts);
}

/// A real local Git hook rejects the commit: the pipeline returns a fatal
/// commit failure and never reports completion.
#[test]
fn when_committing_changes_given_a_rejected_git_hook_then_does_not_report_completion() {
    let (config, _transport) =
        common::scripted_pipeline_with(common::GateScript::Passing, common::SummaryScript::Summary);
    let repository = committed_repository();
    install_rejecting_hook(&repository);
    let before = head_commits(&repository);

    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), repository.path());

    let (stage, reason) = git_failure(&step);
    assert_eq!(
        stage, "commit",
        "the hook rejection is a commit failure: {reason}"
    );
    assert_eq!(
        head_commits(&repository),
        before,
        "the refused commit does not advance HEAD"
    );
    let staged = repository.staged();
    assert!(
        staged.contains("change-summary.md"),
        "the staging that preceded the refused commit stays staged: {staged:?}"
    );
}

/// A real locked Git index makes the staging operation fail: the pipeline
/// returns a fatal staging failure and never reaches a commit.
#[test]
fn when_staging_changes_given_a_locked_git_index_then_reports_fatal_failure() {
    let (config, _transport) =
        common::scripted_pipeline_with(common::GateScript::Passing, common::SummaryScript::Summary);
    // The non-default path has no stash step, so the locked index is first met
    // by the staging operation this test exercises.
    let repository = feature_work_repository();
    let before = head_commits(&repository);
    // A real Git lock: a concurrent Git process would leave this behind, and no
    // staging can write the index while it exists.
    fs::write(repository.path().join(".git/index.lock"), "").expect("the lock is writable");

    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), repository.path());

    let (stage, reason) = git_failure(&step);
    assert_eq!(
        stage, "stage",
        "the locked index fails the staging operation: {reason}"
    );
    assert_eq!(
        head_commits(&repository),
        before,
        "no commit follows a failed stage"
    );
    assert_eq!(repository.staged(), "", "nothing was staged");
}

/// A repository whose default branch is committed, with `feature/work`
/// checked out at that commit. `origin/HEAD` still resolves to the default
/// branch, so every finalization test starts from a real non-default branch.
fn feature_work_repository() -> common::TempRepo {
    let repository = committed_repository();
    repository.checkout_new_branch(common::FEATURE_BRANCH);
    repository
}

/// Every remote-tracking ref and the object it points at, captured so a test
/// can prove a run never advanced a remote ref.
fn remote_refs(repository: &common::TempRepo) -> String {
    repository.git(&[
        "for-each-ref",
        "--format=%(refname) %(objectname)",
        "refs/remotes/",
    ])
}

/// Core entry-point behavior: a successful run on `feature/work`, whose
/// `origin/HEAD` resolves to `main`, records its summary locally. Exactly one
/// commit is created on the checked-out branch, the branch stays checked out,
/// and the default and remote refs do not move.
#[test]
fn when_finalizing_given_a_non_default_branch_then_commits_locally() {
    let repository = feature_work_repository();
    assert_eq!(
        repository.current_branch(),
        common::FEATURE_BRANCH,
        "the fixture must be checked out on the non-default branch"
    );
    assert_eq!(
        repository.remote_head(),
        "origin/main",
        "the fixture must expose a readable default branch through origin/HEAD"
    );
    let before = head_commits(&repository);
    let default_before = repository.git(&["rev-parse", "refs/heads/main"]);
    let remote_before = remote_refs(&repository);

    let (config, _transport) =
        common::scripted_pipeline_with(common::GateScript::Passing, common::SummaryScript::Summary);
    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), repository.path());

    assert!(
        matches!(step, Step::Complete),
        "a successful non-default-branch run completes after its local commit: {step:?}"
    );
    assert_eq!(
        repository.current_branch(),
        common::FEATURE_BRANCH,
        "the checked-out branch must not change"
    );
    assert_eq!(
        head_commits(&repository),
        before + 1,
        "exactly one commit is created on the non-default branch"
    );
    assert_eq!(
        head_message(&repository),
        common::SCRIPTED_SUMMARY,
        "the one commit carries the canonical summary as its message"
    );
    assert_eq!(
        repository.git(&["rev-parse", "refs/heads/main"]),
        default_before,
        "the default branch must not advance"
    );
    assert_eq!(
        remote_refs(&repository),
        remote_before,
        "no remote ref may advance on the non-default path"
    );
}

/// Edge case: the local marker for the default branch is readable, but the
/// remote endpoint is gone. A non-default run must complete on its local commit
/// without requiring a writable push endpoint.
#[test]
fn when_finalizing_given_a_non_default_branch_without_a_writable_remote_then_completes_locally() {
    let repository = feature_work_repository();
    repository.make_origin_unavailable();
    assert!(
        !repository.origin_path().exists(),
        "the fixture must have no reachable origin endpoint"
    );
    assert_eq!(
        repository.remote_head(),
        "origin/main",
        "origin/HEAD stays readable locally even though the endpoint is gone"
    );
    let before = head_commits(&repository);

    let (config, _transport) =
        common::scripted_pipeline_with(common::GateScript::Passing, common::SummaryScript::Summary);
    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), repository.path());

    assert!(
        matches!(step, Step::Complete),
        "a non-default run must complete without a writable remote: {step:?}"
    );
    assert_eq!(
        repository.current_branch(),
        common::FEATURE_BRANCH,
        "the checked-out branch must not change"
    );
    assert_eq!(
        head_commits(&repository),
        before + 1,
        "the local commit records the run without reaching the remote"
    );
    assert_eq!(head_message(&repository), common::SCRIPTED_SUMMARY);
}

/// Contract and invariant: the default branch and every remote-tracking ref are
/// exactly the same objects after a successful non-default run as before it.
#[test]
fn when_committing_given_a_non_default_branch_then_preserves_remote_refs() {
    let repository = feature_work_repository();
    let default_before = repository.git(&["rev-parse", "refs/heads/main"]);
    let origin_head_before = repository.git(&["rev-parse", "refs/remotes/origin/HEAD"]);
    let remote_before = remote_refs(&repository);

    let (config, _transport) =
        common::scripted_pipeline_with(common::GateScript::Passing, common::SummaryScript::Summary);
    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), repository.path());

    assert!(matches!(step, Step::Complete), "{step:?}");
    assert_eq!(
        repository.git(&["rev-parse", "refs/heads/main"]),
        default_before,
        "the local default branch must not advance"
    );
    assert_eq!(
        repository.git(&["rev-parse", "refs/remotes/origin/HEAD"]),
        origin_head_before,
        "the readable origin/HEAD must not move"
    );
    assert_eq!(
        remote_refs(&repository),
        remote_before,
        "the remote-tracking refs must be untouched"
    );
}

/// Regression: the raw commit-message bytes, terminal LF included, are exactly
/// the bytes of the `change-summary.md` artifact the same run wrote.
#[test]
fn when_finalizing_given_a_non_default_branch_then_preserves_summary_bytes() {
    let repository = feature_work_repository();

    let (config, _transport) =
        common::scripted_pipeline_with(common::GateScript::Passing, common::SummaryScript::Summary);
    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), repository.path());

    assert!(matches!(step, Step::Complete), "{step:?}");
    let written = fs::read(repository.path().join("change-summary.md"))
        .expect("the summary artifact is readable");
    assert_eq!(
        head_message_bytes(&repository),
        written,
        "the commit message bytes are exactly the written summary bytes"
    );
    assert_eq!(
        written.as_slice(),
        common::SCRIPTED_SUMMARY.as_bytes(),
        "the artifact is the canonical summary"
    );
    assert!(
        written.ends_with(b"\n"),
        "the canonical summary's terminal LF is part of the comparison"
    );
}

/// Empty boundary: with every artifact outside the repository there is nothing
/// to commit, so the ordinary empty-commit refusal is reported instead of a
/// forced completion or an empty commit.
#[test]
fn when_finalizing_given_no_visible_changes_on_a_non_default_branch_then_reports_commit_failure() {
    let repository = feature_work_repository();
    let before = head_commits(&repository);
    let artifacts = scratch_dir();
    fs::create_dir_all(&artifacts).expect("the artifact directory is creatable");

    let (config, _transport) =
        common::scripted_pipeline_with(common::GateScript::Passing, common::SummaryScript::Summary);
    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), &artifacts);

    let (stage, reason) = git_failure(&step);
    assert_eq!(
        stage, "commit",
        "the no-changes failure is the ordinary commit refusal: {reason}"
    );
    assert_eq!(
        head_commits(&repository),
        before,
        "no empty commit is forced on the non-default branch"
    );
    assert_eq!(repository.staged(), "", "nothing remains staged");
    assert!(
        artifacts.join("change-summary.md").is_file(),
        "the summary artifact was written even though the commit was refused"
    );

    let _ = fs::remove_dir_all(&artifacts);
}

/// Minimum valid boundary: one visible file change, produced by the run, is
/// committed together with the canonical summary message.
#[test]
fn when_finalizing_given_one_visible_change_on_a_non_default_branch_then_commits_the_change() {
    let repository = feature_work_repository();
    let before = head_commits(&repository);
    let artifacts = repository.path().join(".ejaj/one-change");
    let target = repository.path().join("minimal.txt");
    let command = format!("printf 'one visible change\\n' > '{}'", target.display());
    let (config, _transport) = common::scripted_pipeline_with_command(
        common::GateScript::Passing,
        common::SummaryScript::Summary,
        command,
    );

    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), &artifacts);

    assert!(matches!(step, Step::Complete), "{step:?}");
    assert_eq!(
        repository.current_branch(),
        common::FEATURE_BRANCH,
        "the checked-out branch must not change"
    );
    assert_eq!(
        head_commits(&repository),
        before + 1,
        "the minimal visible change creates exactly one commit"
    );
    assert_eq!(
        repository.git(&["show", "HEAD:minimal.txt"]),
        "one visible change\n",
        "the minimal visible modification is committed"
    );
    let paths = head_paths(&repository);
    assert!(
        paths.iter().any(|path| path == "minimal.txt"),
        "the minimal change is in the commit: {paths:?}"
    );
    assert!(
        !paths.iter().any(|path| path == "plan.md"),
        "the ignored artifact directory must stay out of the commit: {paths:?}"
    );
    assert_eq!(
        head_message_bytes(&repository),
        fs::read(artifacts.join("change-summary.md")).expect("the summary artifact is readable"),
        "the canonical summary is committed as the message"
    );
    assert_eq!(
        repository.status(),
        "",
        "the run leaves the non-default branch clean"
    );
}

/// Upper boundary: multiple additions, modifications and a deletion are all
/// included by the local non-default path in one commit.
#[test]
fn when_finalizing_given_many_visible_changes_on_a_non_default_branch_then_commits_all_changes() {
    let repository = common::TempRepo::new();
    repository.write(".gitignore", ".ejaj/\n");
    repository.write("modified-a.txt", "before a\n");
    repository.write("modified-b.txt", "before b\n");
    repository.write("deleted.txt", "before\n");
    repository.commit_all("baseline");
    repository.checkout_new_branch(common::FEATURE_BRANCH);
    let before = head_commits(&repository);
    let artifacts = repository.path().join(".ejaj/many-changes");
    let root = repository.path().display().to_string();
    let command = format!(
        "printf 'after a\\n' > '{root}/modified-a.txt' \
         && printf 'after b\\n' > '{root}/modified-b.txt' \
         && rm -f '{root}/deleted.txt' \
         && printf 'fresh a\\n' > '{root}/added-a.txt' \
         && printf 'fresh b\\n' > '{root}/added-b.txt'"
    );
    let (config, _transport) = common::scripted_pipeline_with_command(
        common::GateScript::Passing,
        common::SummaryScript::Summary,
        command,
    );

    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), &artifacts);

    assert!(matches!(step, Step::Complete), "{step:?}");
    assert_eq!(
        head_commits(&repository),
        before + 1,
        "all visible changes are recorded by exactly one local commit"
    );
    assert_eq!(
        repository.git(&["show", "HEAD:modified-a.txt"]),
        "after a\n"
    );
    assert_eq!(
        repository.git(&["show", "HEAD:modified-b.txt"]),
        "after b\n"
    );
    assert_eq!(repository.git(&["show", "HEAD:added-a.txt"]), "fresh a\n");
    assert_eq!(repository.git(&["show", "HEAD:added-b.txt"]), "fresh b\n");
    let paths = head_paths(&repository);
    assert!(
        !paths.iter().any(|path| path == "deleted.txt"),
        "the deletion is committed: {paths:?}"
    );
    assert_eq!(
        repository.status(),
        "",
        "every visible change was committed on the non-default branch"
    );
}

/// Core entry-point behavior: with `origin/HEAD` genuinely absent, a run on a
/// non-default branch records its summary in exactly one local commit on the
/// branch that was already checked out.
#[test]
fn when_finalizing_given_a_missing_origin_head_on_a_non_default_branch_then_commits_locally() {
    let repository = feature_work_repository();
    repository.remove_remote_head();
    assert!(
        repository.git_ref("refs/remotes/origin/HEAD").is_none(),
        "the fixture must have no origin/HEAD"
    );
    assert_eq!(
        repository.current_branch(),
        common::FEATURE_BRANCH,
        "the fixture must be checked out on the non-default branch"
    );
    let before = head_commits(&repository);
    let branch_before = repository.current_branch();
    let default_before = repository.git(&["rev-parse", "refs/heads/main"]);
    let remote_before = remote_refs(&repository);
    let artifacts = repository.path().join(".ejaj/missing-head-non-default");
    let target = repository.path().join("visible.txt");
    let command = format!("printf 'visible change\\n' > '{}'", target.display());
    let (config, _transport) = common::scripted_pipeline_with_command(
        common::GateScript::Passing,
        common::SummaryScript::Summary,
        command,
    );

    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), &artifacts);

    assert!(
        matches!(step, Step::Complete),
        "an absent origin/HEAD must not block the local recording: {step:?}"
    );
    assert_eq!(
        repository.current_branch(),
        branch_before,
        "the original branch stays checked out"
    );
    assert_eq!(
        head_commits(&repository),
        before + 1,
        "exactly one new commit is created on the same branch"
    );
    assert_eq!(
        head_message_bytes(&repository),
        fs::read(artifacts.join("change-summary.md")).expect("the summary artifact is readable"),
        "the commit message is exactly the canonical summary artifact"
    );
    assert_eq!(
        head_message_bytes(&repository),
        common::SCRIPTED_SUMMARY.as_bytes(),
        "the commit message is exactly the canonical summary bytes"
    );
    assert_eq!(
        repository.git(&["rev-parse", "refs/heads/main"]),
        default_before,
        "the default local branch remains unchanged"
    );
    assert_eq!(
        remote_refs(&repository),
        remote_before,
        "every remote-tracking ref remains unchanged"
    );
    assert_eq!(
        repository.stash_count(),
        0,
        "the local route creates no stash"
    );
    assert!(
        repository
            .git_ref(&format!("refs/heads/{}", common::SUMMARY_BRANCH))
            .is_none(),
        "no ejaj/ summary branch is created"
    );
    assert_eq!(
        repository.status(),
        "",
        "the local commit leaves the branch clean"
    );
}

/// Forbidden boundary: the readable `origin/HEAD` remains, but the remote
/// refuses writes. The non-default path needs no remote write access and still
/// completes locally.
#[test]
fn when_finalizing_given_a_non_default_branch_without_remote_write_access_then_completes_locally() {
    let repository = feature_work_repository();
    repository.forbid_remote_writes();
    let before = head_commits(&repository);
    let remote_before = remote_refs(&repository);

    let (config, _transport) =
        common::scripted_pipeline_with(common::GateScript::Passing, common::SummaryScript::Summary);
    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), repository.path());

    assert!(
        matches!(step, Step::Complete),
        "a non-default run needs no remote write access: {step:?}"
    );
    assert_eq!(
        repository.current_branch(),
        common::FEATURE_BRANCH,
        "the checked-out branch must not change"
    );
    assert_eq!(
        head_commits(&repository),
        before + 1,
        "the local commit records the run without pushing"
    );
    assert_eq!(
        remote_refs(&repository),
        remote_before,
        "no remote ref may advance when the remote writes are forbidden"
    );
}

/// Drive one real scripted pipeline whose run creates a visible file against a
/// repository whose branch identity is invalid, and assert the fatal
/// branch-detection result before any finalization mutation. Every expected
/// reason fragment is checked so a case names the invalid form it planted.
fn assert_branch_detection_fails_before_mutation(
    repository: &common::TempRepo,
    label: &str,
    expected: &[&str],
) -> String {
    let before_refs = repository.all_refs();
    let before_head = repository.git_ref("HEAD");
    let before_checked_out = fs::read_to_string(repository.path().join(".git/HEAD"))
        .expect("the checked-out HEAD reference is readable");
    let before_index = repository.git(&["write-tree"]);
    let before_tip = default_tip(repository);
    let before_stash = repository.stash_count();
    let remote_before = remote_refs(repository);
    let artifacts = scratch_dir();
    fs::create_dir_all(&artifacts).expect("the external artifact directory is creatable");
    let target = repository.path().join("visible-change.txt");
    let command = format!("printf 'visible change\\n' > '{}'", target.display());
    let (config, _transport) = common::scripted_pipeline_with_command(
        common::GateScript::Passing,
        common::SummaryScript::Summary,
        command,
    );

    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), &artifacts);

    let (stage, reason) = git_failure(&step);
    assert_eq!(
        stage, "branch-detection",
        "{label}: an invalid branch identity is fatal before mutation: {reason}"
    );
    for &needle in expected {
        assert!(
            reason.contains(needle),
            "{label}: the failure names {needle}: {reason}"
        );
    }
    assert_eq!(
        repository.all_refs(),
        before_refs,
        "{label}: no ref changes"
    );
    assert_eq!(
        repository.git_ref("HEAD"),
        before_head,
        "{label}: HEAD does not move"
    );
    assert_eq!(
        fs::read_to_string(repository.path().join(".git/HEAD"))
            .expect("the checked-out HEAD reference stays readable"),
        before_checked_out,
        "{label}: the checked-out HEAD does not move"
    );
    assert_eq!(
        repository.git(&["write-tree"]),
        before_index,
        "{label}: the index is untouched"
    );
    assert_eq!(
        repository.stash_count(),
        before_stash,
        "{label}: no stash is created"
    );
    assert_eq!(repository.staged(), "", "{label}: nothing is staged");
    assert_eq!(
        default_tip(repository),
        before_tip,
        "{label}: the default branch does not move"
    );
    assert_eq!(
        remote_refs(repository),
        remote_before,
        "{label}: no remote ref changes"
    );
    assert!(
        repository
            .git_ref(&format!("refs/heads/{}", common::SUMMARY_BRANCH))
            .is_none(),
        "{label}: no summary branch is created"
    );
    assert!(
        artifacts.join("change-summary.md").is_file(),
        "{label}: the summary artifact is still written"
    );
    let _ = fs::remove_dir_all(&artifacts);
    reason.to_string()
}

/// Core entry-point behavior: with `origin/HEAD` absent, a run on the default
/// branch records its summary in exactly one local commit on that same branch.
#[test]
fn when_finalizing_given_a_missing_origin_head_on_the_default_branch_then_commits_on_that_branch() {
    let repository = default_branch_repository();
    repository.remove_remote_head();
    assert!(
        repository.git_ref("refs/remotes/origin/HEAD").is_none(),
        "the fixture must have no origin/HEAD"
    );
    assert_eq!(
        repository.current_branch(),
        common::DEFAULT_BRANCH,
        "the fixture must be checked out on the default branch"
    );
    let before_commits = head_commits(&repository);
    let before_tip = default_tip(&repository);
    let origin_main_before = repository.origin_ref("refs/heads/main");
    let remote_before = remote_refs(&repository);
    let artifacts = repository.path().join(".ejaj/missing-head-default");
    let (config, _transport) = common::scripted_pipeline_with_command(
        common::GateScript::Passing,
        common::SummaryScript::Summary,
        change_tracked_file(&repository),
    );

    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), &artifacts);

    assert!(
        matches!(step, Step::Complete),
        "an absent origin/HEAD must not block a default-branch local recording: {step:?}"
    );
    assert_eq!(
        repository.current_branch(),
        common::DEFAULT_BRANCH,
        "the default branch stays checked out"
    );
    assert_eq!(
        head_commits(&repository),
        before_commits + 1,
        "the default branch advances by exactly one run commit"
    );
    assert_ne!(
        default_tip(&repository),
        before_tip,
        "the default branch really advanced"
    );
    assert_eq!(
        repository.git(&["rev-parse", "HEAD"]).trim(),
        default_tip(&repository),
        "the checked-out default branch names the run commit"
    );
    assert_eq!(
        head_message_bytes(&repository),
        fs::read(artifacts.join("change-summary.md")).expect("the summary artifact is readable"),
        "the commit message is exactly the canonical summary artifact"
    );
    assert_eq!(
        head_message_bytes(&repository),
        common::SCRIPTED_SUMMARY.as_bytes(),
        "the commit message is exactly the canonical summary bytes"
    );
    assert_eq!(
        repository.stash_count(),
        0,
        "the default-branch local route creates no stash"
    );
    assert!(
        repository
            .git_ref(&format!("refs/heads/{}", common::SUMMARY_BRANCH))
            .is_none(),
        "no ejaj/ summary branch is created"
    );
    assert_eq!(
        remote_refs(&repository),
        remote_before,
        "no remote-tracking ref changes"
    );
    assert_eq!(
        repository.origin_ref("refs/heads/main"),
        origin_main_before,
        "the remote default branch remains unchanged"
    );
    assert_eq!(
        repository.status(),
        "",
        "the run leaves the default branch clean"
    );
}

/// Forbidden boundary: the default branch is checked out, `origin/HEAD` is
/// absent and the origin endpoint is gone. The local fallback completes on the
/// default branch without contacting the remote, records exactly one commit with
/// the canonical summary bytes, and leaves every remote ref, the stash and the
/// summary-branch namespace untouched.
#[test]
fn when_finalizing_given_a_missing_origin_head_on_the_default_branch_and_unavailable_origin_then_completes_locally()
 {
    let repository = default_branch_repository();
    assert!(
        repository.origin_ref("refs/heads/main").is_some(),
        "the fixture's real origin must carry the default branch before it is made unavailable"
    );
    repository.remove_remote_head();
    repository.make_origin_unavailable();
    assert!(
        repository.git_ref("refs/remotes/origin/HEAD").is_none(),
        "the fixture must have no origin/HEAD"
    );
    assert!(
        !repository.origin_path().exists(),
        "the fixture must have no reachable origin endpoint"
    );
    assert_eq!(
        repository.current_branch(),
        common::DEFAULT_BRANCH,
        "the fixture must be checked out on the default branch"
    );
    let before_commits = head_commits(&repository);
    let before_tip = default_tip(&repository);
    let remote_before = remote_refs(&repository);
    let stash_before = repository.stash_list();
    let artifacts = repository
        .path()
        .join(".ejaj/missing-head-default-unavailable");
    let (config, _transport) = common::scripted_pipeline_with_command(
        common::GateScript::Passing,
        common::SummaryScript::Summary,
        change_tracked_file(&repository),
    );

    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), &artifacts);

    assert!(
        matches!(step, Step::Complete),
        "an absent origin/HEAD must not block a default-branch local recording without an \
         origin: {step:?}"
    );
    assert_eq!(
        repository.current_branch(),
        common::DEFAULT_BRANCH,
        "the original default branch stays checked out"
    );
    assert_eq!(
        head_commits(&repository),
        before_commits + 1,
        "the default branch advances by exactly one run commit"
    );
    assert_ne!(
        default_tip(&repository),
        before_tip,
        "the default branch really advanced"
    );
    assert_eq!(
        repository.git(&["rev-parse", "HEAD"]).trim(),
        default_tip(&repository),
        "the checked-out default branch names the run commit"
    );
    assert_eq!(
        head_message_bytes(&repository),
        fs::read(artifacts.join("change-summary.md")).expect("the summary artifact is readable"),
        "the commit message is exactly the canonical summary artifact"
    );
    assert_eq!(
        head_message_bytes(&repository),
        common::SCRIPTED_SUMMARY.as_bytes(),
        "the commit message is exactly the canonical summary bytes"
    );
    assert_eq!(
        remote_refs(&repository),
        remote_before,
        "no remote-tracking ref changes without an endpoint"
    );
    assert_eq!(
        repository.stash_list(),
        stash_before,
        "the stash state is unchanged by the local route"
    );
    assert_eq!(
        repository.stash_count(),
        0,
        "the local route creates no stash"
    );
    assert!(
        repository
            .git_ref(&format!("refs/heads/{}", common::SUMMARY_BRANCH))
            .is_none(),
        "no ejaj/ summary branch is created"
    );
    assert_eq!(
        repository.status(),
        "",
        "the local commit leaves the default branch clean"
    );
}

/// Empty boundary: with no visible changes the ordinary empty-commit refusal is
/// fatal even though the absent marker no longer blocks finalization.
#[test]
fn when_finalizing_given_empty_changes_and_a_missing_origin_head_then_reports_commit_failure() {
    let repository = feature_work_repository();
    repository.remove_remote_head();
    let before = head_commits(&repository);
    let branch_before = repository.current_branch();
    let artifacts = scratch_dir();
    fs::create_dir_all(&artifacts).expect("the external artifact directory is creatable");
    let (config, _transport) =
        common::scripted_pipeline_with(common::GateScript::Passing, common::SummaryScript::Summary);

    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), &artifacts);

    let (stage, reason) = git_failure(&step);
    assert_eq!(
        stage, "commit",
        "the no-changes failure is the ordinary commit refusal: {reason}"
    );
    assert_eq!(
        head_commits(&repository),
        before,
        "no empty commit is forced"
    );
    assert_eq!(
        repository.current_branch(),
        branch_before,
        "the checked-out branch stays the same"
    );
    assert_eq!(repository.staged(), "", "nothing remains staged");
    assert_eq!(repository.stash_count(), 0, "no stash is created");
    assert!(
        repository
            .git_ref(&format!("refs/heads/{}", common::SUMMARY_BRANCH))
            .is_none(),
        "no summary branch is created"
    );
    assert!(
        artifacts.join("change-summary.md").is_file(),
        "the summary artifact is still written"
    );
    let _ = fs::remove_dir_all(&artifacts);
}

/// Forbidden boundary: the origin endpoint is unavailable while the marker is
/// absent. The local route needs no remote operation and still completes.
#[test]
fn when_finalizing_given_a_missing_origin_head_and_unavailable_origin_then_completes_locally() {
    let repository = feature_work_repository();
    repository.remove_remote_head();
    repository.make_origin_unavailable();
    assert!(
        !repository.origin_path().exists(),
        "the fixture must have no reachable origin endpoint"
    );
    let before = head_commits(&repository);
    let branch_before = repository.current_branch();
    let remote_before = remote_refs(&repository);
    let artifacts = repository.path().join(".ejaj/missing-head-unavailable");
    let target = repository.path().join("visible.txt");
    let command = format!("printf 'visible change\\n' > '{}'", target.display());
    let (config, _transport) = common::scripted_pipeline_with_command(
        common::GateScript::Passing,
        common::SummaryScript::Summary,
        command,
    );

    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), &artifacts);

    assert!(
        matches!(step, Step::Complete),
        "no remote operation may be required while the marker is absent: {step:?}"
    );
    assert_eq!(
        repository.current_branch(),
        branch_before,
        "the original branch stays checked out"
    );
    assert_eq!(
        head_commits(&repository),
        before + 1,
        "the local run commit is still recorded"
    );
    assert_eq!(
        head_message_bytes(&repository),
        common::SCRIPTED_SUMMARY.as_bytes(),
        "the commit message is exactly the canonical summary bytes"
    );
    assert_eq!(
        remote_refs(&repository),
        remote_before,
        "no remote-tracking ref changes without an endpoint"
    );
    assert_eq!(repository.stash_count(), 0, "no stash is created");
    assert!(
        repository
            .git_ref(&format!("refs/heads/{}", common::SUMMARY_BRANCH))
            .is_none(),
        "no summary branch is created"
    );
    assert_eq!(
        repository.status(),
        "",
        "the local commit leaves the branch clean"
    );
}

/// Edge-case table: every present but invalid origin/HEAD form -- a direct
/// reference, an unreadable path, a malformed identity, a dangling target and
/// an out-of-prefix target -- and every invalid or detached current-branch
/// identity is fatal at branch detection before any finalization mutation.
/// Every case drives the real scripted pipeline against a real repository and
/// its real bare origin; only the LLM transport and the command script are
/// scripted, and both are external boundaries.
#[test]
fn when_finalizing_given_invalid_branch_identities_then_fails_before_mutation() {
    struct Case {
        label: &'static str,
        repository: fn() -> common::TempRepo,
        expected: &'static [&'static str],
    }

    fn direct_origin_head() -> common::TempRepo {
        let repository = feature_work_repository();
        let oid = repository.git(&["rev-parse", "HEAD"]).trim().to_string();
        repository.write_remote_head_raw(&format!("{oid}\n"));
        assert!(
            repository.git_ref("refs/remotes/origin/HEAD").is_some(),
            "the fixture must expose a present direct reference"
        );
        repository
    }

    fn unreadable_origin_head() -> common::TempRepo {
        let repository = feature_work_repository();
        repository.make_remote_head_unreadable();
        repository
    }

    fn malformed_origin_head() -> common::TempRepo {
        let repository = feature_work_repository();
        repository.git(&[
            "update-ref",
            "refs/remotes/origin/main\u{a0}",
            "refs/remotes/origin/main",
        ]);
        repository.write_remote_head_raw("ref: refs/remotes/origin/main\u{a0}\n");
        assert_eq!(
            repository.git(&["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"]),
            "refs/remotes/origin/main\u{a0}\n",
            "the fixture exposes the malformed default identity through real Git"
        );
        repository
    }

    fn dangling_origin_head() -> common::TempRepo {
        let repository = feature_work_repository();
        repository.remove_remote_tracking_branch(common::DEFAULT_BRANCH);
        repository
    }

    fn out_of_prefix_origin_head() -> common::TempRepo {
        let repository = default_branch_repository();
        repository.git(&["branch", "origin/main"]);
        repository.write_remote_head_raw("ref: refs/heads/origin/main\n");
        assert_eq!(
            repository.remote_head(),
            "origin/main",
            "the planted target's shortened form must resemble origin/<branch>"
        );
        assert_eq!(
            repository
                .git(&["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"])
                .trim(),
            "refs/heads/origin/main",
            "the full target must really be outside the remote-tracking namespace"
        );
        repository
    }

    fn detached_head() -> common::TempRepo {
        let repository = default_branch_repository();
        repository.detach_head();
        repository
    }

    fn detached_head_with_missing_origin_head() -> common::TempRepo {
        let repository = default_branch_repository();
        repository.remove_remote_head();
        repository.detach_head();
        repository
    }

    fn invalid_current_branch() -> common::TempRepo {
        let repository = default_branch_repository();
        repository.git(&["tag", "not-a-branch"]);
        repository.write_head_raw("ref: refs/tags/not-a-branch\n");
        repository
    }

    fn malformed_current_branch() -> common::TempRepo {
        let repository = default_branch_repository();
        repository.git(&["update-ref", "refs/heads/main\u{a0}", "HEAD"]);
        repository.write_head_raw("ref: refs/heads/main\u{a0}\n");
        assert_eq!(
            repository.git(&["symbolic-ref", "--quiet", "HEAD"]),
            "refs/heads/main\u{a0}\n",
            "the fixture exposes the malformed current identity through real Git"
        );
        repository
    }

    let cases = [
        Case {
            label: "a direct origin/HEAD",
            repository: direct_origin_head,
            expected: &["origin/HEAD"],
        },
        Case {
            label: "an unreadable origin/HEAD",
            repository: unreadable_origin_head,
            expected: &["origin/HEAD"],
        },
        Case {
            label: "a malformed origin/HEAD",
            repository: malformed_origin_head,
            expected: &["malformed", "\u{a0}"],
        },
        Case {
            label: "a dangling origin/HEAD",
            repository: dangling_origin_head,
            expected: &["dangling"],
        },
        Case {
            label: "an out-of-prefix origin/HEAD",
            repository: out_of_prefix_origin_head,
            expected: &["refs/remotes/origin/", "refs/heads/origin/main"],
        },
        Case {
            label: "a detached HEAD",
            repository: detached_head,
            expected: &["HEAD"],
        },
        Case {
            label: "a detached HEAD with a missing origin/HEAD",
            repository: detached_head_with_missing_origin_head,
            expected: &["HEAD"],
        },
        Case {
            label: "an invalid current branch",
            repository: invalid_current_branch,
            expected: &["refs/heads/"],
        },
        Case {
            label: "a malformed current branch",
            repository: malformed_current_branch,
            expected: &["malformed", "\u{a0}"],
        },
    ];

    for case in &cases {
        let repository = (case.repository)();
        assert_branch_detection_fails_before_mutation(&repository, case.label, case.expected);
    }
}

/// Edge-case table: a real locked index makes the missing-marker route fail at
/// the staging operation, before any commit.
#[test]
fn when_finalizing_given_a_missing_origin_head_and_a_locked_index_then_reports_stage_failure() {
    let repository = feature_work_repository();
    repository.remove_remote_head();
    repository.lock_index();
    let before = head_commits(&repository);
    let branch_before = repository.current_branch();
    let remote_before = remote_refs(&repository);
    let artifacts = repository.path().join(".ejaj/locked-index-missing");
    let target = repository.path().join("visible.txt");
    let command = format!("printf 'visible change\\n' > '{}'", target.display());
    let (config, _transport) = common::scripted_pipeline_with_command(
        common::GateScript::Passing,
        common::SummaryScript::Summary,
        command,
    );

    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), &artifacts);

    let (stage, reason) = git_failure(&step);
    assert_eq!(
        stage, "stage",
        "the locked index fails the staging operation: {reason}"
    );
    let _ = fs::remove_file(repository.path().join(".git/index.lock"));
    assert_eq!(
        head_commits(&repository),
        before,
        "no commit follows a failed stage"
    );
    assert_eq!(
        repository.current_branch(),
        branch_before,
        "the checked-out branch stays the same"
    );
    assert_eq!(repository.staged(), "", "nothing was staged");
    assert_eq!(repository.stash_count(), 0, "no stash is created");
    assert_eq!(
        remote_refs(&repository),
        remote_before,
        "no remote ref changes"
    );
    assert!(
        repository
            .git_ref(&format!("refs/heads/{}", common::SUMMARY_BRANCH))
            .is_none(),
        "no summary branch is created"
    );
    assert!(
        artifacts.join("change-summary.md").is_file(),
        "the summary artifact is still written"
    );
}

/// Edge-case table: a real pre-commit hook refuses the ordinary commit after
/// staging succeeded. The failure is reported at the commit stage with no
/// rollback, never as completion.
#[test]
fn when_finalizing_given_a_missing_origin_head_and_a_rejected_commit_then_reports_commit_failure() {
    let repository = feature_work_repository();
    repository.remove_remote_head();
    install_rejecting_hook(&repository);
    let before = head_commits(&repository);
    let branch_before = repository.current_branch();
    let artifacts = repository.path().join(".ejaj/rejected-commit-missing");
    let target = repository.path().join("visible.txt");
    let command = format!("printf 'visible change\\n' > '{}'", target.display());
    let (config, _transport) = common::scripted_pipeline_with_command(
        common::GateScript::Passing,
        common::SummaryScript::Summary,
        command,
    );

    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), &artifacts);

    let (stage, reason) = git_failure(&step);
    assert_eq!(
        stage, "commit",
        "the hook refusal is a commit failure: {reason}"
    );
    assert!(
        reason.contains("refuses this commit"),
        "the hook's diagnostic is preserved: {reason}"
    );
    assert_eq!(
        head_commits(&repository),
        before,
        "the refused commit does not advance HEAD"
    );
    assert_eq!(
        repository.current_branch(),
        branch_before,
        "the checked-out branch stays the same"
    );
    let staged = repository.staged();
    assert!(
        staged.contains("visible.txt"),
        "the staging that preceded the refused commit remains: {staged:?}"
    );
    assert_eq!(repository.stash_count(), 0, "no stash is created");
    assert!(
        repository
            .git_ref(&format!("refs/heads/{}", common::SUMMARY_BRANCH))
            .is_none(),
        "no summary branch is created"
    );
    assert!(
        artifacts.join("change-summary.md").is_file(),
        "the summary artifact is still written"
    );
}

/// Upper boundary: every non-ignored addition, modification and deletion is
/// staged into one commit while an ignored file stays out.
#[test]
fn when_finalizing_given_a_missing_origin_head_with_multiple_changes_then_commits_visible_changes()
{
    let repository = common::TempRepo::new();
    repository.write(".gitignore", "ignored.txt\n.ejaj/\n");
    repository.write("modified-a.txt", "before a\n");
    repository.write("modified-b.txt", "before b\n");
    repository.write("deleted.txt", "before\n");
    repository.commit_all("baseline");
    repository.checkout_new_branch(common::FEATURE_BRANCH);
    repository.remove_remote_head();
    let before = head_commits(&repository);
    let default_before = repository.git(&["rev-parse", "refs/heads/main"]);
    let remote_before = remote_refs(&repository);
    let artifacts = repository.path().join(".ejaj/missing-head-multiple");
    let root = repository.path().display().to_string();
    let command = format!(
        "printf 'after a\\n' > '{root}/modified-a.txt' \
         && printf 'after b\\n' > '{root}/modified-b.txt' \
         && rm -f '{root}/deleted.txt' \
         && printf 'fresh a\\n' > '{root}/added-a.txt' \
         && printf 'fresh b\\n' > '{root}/added-b.txt' \
         && printf 'invisible\\n' > '{root}/ignored.txt'"
    );
    let (config, _transport) = common::scripted_pipeline_with_command(
        common::GateScript::Passing,
        common::SummaryScript::Summary,
        command,
    );

    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), &artifacts);

    assert!(matches!(step, Step::Complete), "{step:?}");
    assert_eq!(
        repository.current_branch(),
        common::FEATURE_BRANCH,
        "the original branch stays checked out"
    );
    assert_eq!(
        head_commits(&repository),
        before + 1,
        "all visible changes are recorded by exactly one local commit"
    );
    assert_eq!(
        repository.git(&["show", "HEAD:modified-a.txt"]),
        "after a\n"
    );
    assert_eq!(
        repository.git(&["show", "HEAD:modified-b.txt"]),
        "after b\n"
    );
    assert_eq!(repository.git(&["show", "HEAD:added-a.txt"]), "fresh a\n");
    assert_eq!(repository.git(&["show", "HEAD:added-b.txt"]), "fresh b\n");
    let paths = head_paths(&repository);
    assert!(
        !paths.iter().any(|path| path == "deleted.txt"),
        "the deletion is committed: {paths:?}"
    );
    assert!(
        !paths.iter().any(|path| path == "ignored.txt"),
        "the ignored file stays out of the commit: {paths:?}"
    );
    assert!(
        !paths.iter().any(|path| path.starts_with(".ejaj/")),
        "the ignored artifact directory stays out of the commit: {paths:?}"
    );
    assert_eq!(
        repository.status(),
        "",
        "every visible change was committed"
    );
    assert_eq!(
        repository.git(&["rev-parse", "refs/heads/main"]),
        default_before,
        "the default local branch remains unchanged"
    );
    assert_eq!(
        remote_refs(&repository),
        remote_before,
        "every remote-tracking ref remains unchanged"
    );
    assert_eq!(repository.stash_count(), 0, "no stash is created");
    assert!(
        repository
            .git_ref(&format!("refs/heads/{}", common::SUMMARY_BRANCH))
            .is_none(),
        "no summary branch is created"
    );
    assert_eq!(
        head_message_bytes(&repository),
        common::SCRIPTED_SUMMARY.as_bytes(),
        "the one commit carries the exact canonical summary bytes"
    );
}

/// Contract and invariant: a missing-marker default-branch run advances only
/// the checked-out default branch and leaves the stash, refs, summary branches
/// and remote state alone.
#[test]
fn when_finalizing_given_a_missing_origin_head_on_the_default_branch_then_preserves_local_state_invariants()
 {
    let repository = default_branch_repository();
    repository.write("stashed.txt", "work that belongs to an earlier run\n");
    repository.stash_push("preexisting stash");
    let stash_before = repository.stash_list();
    assert_eq!(repository.stash_count(), 1, "the fixture carries one stash");
    repository.remove_remote_head();
    let before_commits = head_commits(&repository);
    let before_tip = default_tip(&repository);
    let origin_main_before = repository.origin_ref("refs/heads/main");
    let remote_before = remote_refs(&repository);
    let artifacts = repository.path().join(".ejaj/default-invariants-missing");
    let (config, _transport) = common::scripted_pipeline_with_command(
        common::GateScript::Passing,
        common::SummaryScript::Summary,
        change_tracked_file(&repository),
    );

    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), &artifacts);

    assert!(matches!(step, Step::Complete), "{step:?}");
    assert_eq!(
        repository.current_branch(),
        common::DEFAULT_BRANCH,
        "the default branch remains checked out"
    );
    assert_eq!(
        head_commits(&repository),
        before_commits + 1,
        "the default branch advances by exactly one run commit"
    );
    assert_ne!(
        default_tip(&repository),
        before_tip,
        "the default branch really advanced"
    );
    assert_eq!(
        repository.stash_count(),
        1,
        "no new stash is created and the existing one is not popped"
    );
    assert_eq!(
        repository.stash_list(),
        stash_before,
        "the pre-existing stash entry is unchanged"
    );
    assert_eq!(
        remote_refs(&repository),
        remote_before,
        "no remote-tracking ref changes"
    );
    assert_eq!(
        repository.origin_ref("refs/heads/main"),
        origin_main_before,
        "the remote default branch remains unchanged"
    );
    assert!(
        repository
            .git_ref(&format!("refs/heads/{}", common::SUMMARY_BRANCH))
            .is_none(),
        "no ejaj/ summary branch is created"
    );
    assert_eq!(
        repository.status(),
        "",
        "the run leaves the default branch clean"
    );
}

/// Contract and invariant: a missing-marker non-default run leaves the default
/// branch, remote refs and pre-existing stash exactly where they were.
#[test]
fn when_finalizing_given_a_missing_origin_head_on_a_non_default_branch_then_preserves_local_state_invariants()
 {
    let repository = feature_work_repository();
    repository.write("stashed.txt", "work that belongs to an earlier run\n");
    repository.stash_push("preexisting stash");
    let stash_before = repository.stash_list();
    assert_eq!(repository.stash_count(), 1, "the fixture carries one stash");
    repository.remove_remote_head();
    let before = head_commits(&repository);
    let branch_before = repository.current_branch();
    let default_before = repository.git(&["rev-parse", "refs/heads/main"]);
    let remote_before = remote_refs(&repository);
    let artifacts = repository
        .path()
        .join(".ejaj/non-default-invariants-missing");
    let target = repository.path().join("visible.txt");
    let command = format!("printf 'visible change\\n' > '{}'", target.display());
    let (config, _transport) = common::scripted_pipeline_with_command(
        common::GateScript::Passing,
        common::SummaryScript::Summary,
        command,
    );

    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), &artifacts);

    assert!(matches!(step, Step::Complete), "{step:?}");
    assert_eq!(
        repository.current_branch(),
        branch_before,
        "the original branch stays checked out"
    );
    assert_eq!(
        head_commits(&repository),
        before + 1,
        "exactly one run commit is created"
    );
    assert_eq!(
        repository.git(&["rev-parse", "refs/heads/main"]),
        default_before,
        "the default local branch remains unchanged"
    );
    assert_eq!(
        remote_refs(&repository),
        remote_before,
        "every remote-tracking ref remains unchanged"
    );
    assert_eq!(
        repository.stash_count(),
        1,
        "no new stash is created and the existing one is not popped"
    );
    assert_eq!(
        repository.stash_list(),
        stash_before,
        "the pre-existing stash entry is unchanged"
    );
    assert!(
        repository
            .git_ref(&format!("refs/heads/{}", common::SUMMARY_BRANCH))
            .is_none(),
        "no ejaj/ summary branch is created"
    );
    assert_eq!(
        repository.status(),
        "",
        "the run leaves the original branch clean"
    );
}

/// Contract and invariant: a valid `origin/HEAD` keeps the existing routes --
/// a local commit on a non-default branch and the stash/switch/pull/summary
/// branch/push workflow on the default branch.
#[test]
fn when_finalizing_given_a_valid_origin_head_then_preserves_existing_finalization_routes() {
    // The non-default route records locally and touches no default or remote ref.
    {
        let repository = feature_work_repository();
        assert_eq!(
            repository.remote_head(),
            "origin/main",
            "the fixture must expose a readable default branch"
        );
        let before = head_commits(&repository);
        let default_before = repository.git(&["rev-parse", "refs/heads/main"]);
        let remote_before = remote_refs(&repository);
        let artifacts = repository.path().join(".ejaj/valid-non-default");
        let target = repository.path().join("visible.txt");
        let command = format!("printf 'visible change\\n' > '{}'", target.display());
        let (config, _transport) = common::scripted_pipeline_with_command(
            common::GateScript::Passing,
            common::SummaryScript::Summary,
            command,
        );

        let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
        let step = pipeline.ask(&common::harmless_plan_request(), &artifacts);

        assert!(matches!(step, Step::Complete), "{step:?}");
        assert_eq!(
            repository.current_branch(),
            common::FEATURE_BRANCH,
            "the non-default branch stays checked out"
        );
        assert_eq!(
            head_commits(&repository),
            before + 1,
            "the non-default route records exactly one local commit"
        );
        assert_eq!(
            repository.git(&["rev-parse", "refs/heads/main"]),
            default_before,
            "the default branch does not advance"
        );
        assert_eq!(
            remote_refs(&repository),
            remote_before,
            "no remote ref advances on the non-default route"
        );
        assert_eq!(repository.stash_count(), 0, "no stash is created");
        assert!(
            repository
                .git_ref(&format!("refs/heads/{}", common::SUMMARY_BRANCH))
                .is_none(),
            "no summary branch is created"
        );
    }

    // The default route still saves, fast-forwards, branches, commits and pushes.
    {
        let repository = default_branch_repository();
        assert_eq!(
            repository.remote_head(),
            "origin/main",
            "the fixture must expose a readable default branch"
        );
        let before_tip = default_tip(&repository);
        let artifacts = repository.path().join(".ejaj/valid-default");
        let (config, _transport) = common::scripted_pipeline_with_command(
            common::GateScript::Passing,
            common::SummaryScript::Summary,
            change_tracked_file(&repository),
        );

        let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
        let step = pipeline.ask(&common::harmless_plan_request(), &artifacts);

        assert!(matches!(step, Step::Complete), "{step:?}");
        assert_eq!(
            repository.current_branch(),
            common::SUMMARY_BRANCH,
            "the run is recorded on the unique summary branch"
        );
        assert_eq!(
            repository
                .git(&["rev-list", "--count", &format!("{before_tip}..HEAD")])
                .trim(),
            "1",
            "the summary branch carries exactly one run commit above the default tip"
        );
        assert_eq!(
            head_message_bytes(&repository),
            common::SCRIPTED_SUMMARY.as_bytes(),
            "the summary branch commit carries the canonical summary bytes"
        );
        assert_eq!(
            default_tip(&repository),
            before_tip,
            "the default branch itself does not advance"
        );
        assert_eq!(repository.stash_count(), 0, "the run stash is popped");
        assert!(
            repository
                .origin_ref(&format!("refs/heads/{}", common::SUMMARY_BRANCH))
                .is_some(),
            "the summary branch is published to origin"
        );
    }
}

// ---------------------------------------------------------------------------
// Finalizing a run on the default branch: a unique published summary branch
//
// The default branch is never advanced by the run. Its visible changes are
// saved, the latest default branch is fast-forwarded, a unique branch is
// created from that updated tip, the changes are reapplied and committed there,
// and the branch is published with upstream tracking. Every operation is a real
// Git process against the disposable repository and its real bare origin.
// ---------------------------------------------------------------------------

/// A clean default-branch fixture with a tracked file, an ignored file and an
/// ignored artifact directory. Its baseline is published to the real bare
/// origin, so a fast-forward-only pull has a fast-forward to perform.
fn default_branch_repository() -> common::TempRepo {
    let repository = common::TempRepo::new();
    repository.write(".gitignore", "ignored.txt\n.ejaj/\n");
    repository.write("tracked.txt", "baseline\n");
    repository.commit_all("baseline");
    repository
}

/// The local default branch's tip.
fn default_tip(repository: &common::TempRepo) -> String {
    repository
        .git(&["rev-parse", "refs/heads/main"])
        .trim()
        .to_string()
}

/// The command that plants one visible tracked-file change inside repository.
fn change_tracked_file(repository: &common::TempRepo) -> String {
    let target = repository.path().join("tracked.txt");
    format!("printf 'changed by the run\\n' > '{}'", target.display())
}

/// Core entry-point behavior: a successful default-branch run publishes its
/// one commit on the uniquely named summary branch, leaves the default branch
/// where it was, and sets upstream tracking.
#[test]
fn when_a_run_finishes_given_the_default_branch_then_pushes_a_summary_branch() {
    let repository = default_branch_repository();
    assert_eq!(repository.current_branch(), common::DEFAULT_BRANCH);
    assert_eq!(repository.remote_head(), "origin/main");
    let before = default_tip(&repository);
    let artifacts = repository.path().join(".ejaj/summary-branch");

    let (config, _transport) = common::scripted_pipeline_with_command(
        common::GateScript::Passing,
        common::SummaryScript::Summary,
        change_tracked_file(&repository),
    );
    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), &artifacts);

    assert!(
        matches!(step, Step::Complete),
        "the push and upstream setup succeed before completion: {step:?}"
    );
    let branch = common::SUMMARY_BRANCH;
    assert_eq!(
        repository.current_branch(),
        branch,
        "the run is recorded on the unique summary branch"
    );
    let range = format!("{before}..HEAD");
    assert_eq!(
        repository.git(&["rev-list", "--count", &range]).trim(),
        "1",
        "the branch carries exactly one run commit above the default tip"
    );
    assert_eq!(head_message(&repository), common::SCRIPTED_SUMMARY);
    assert_eq!(
        repository.git(&["show", "HEAD:tracked.txt"]),
        "changed by the run\n",
        "the visible run change is in the branch commit"
    );
    assert_eq!(
        default_tip(&repository),
        before,
        "the default branch itself must not advance"
    );

    let head = repository.git(&["rev-parse", "HEAD"]).trim().to_string();
    assert_eq!(
        repository
            .git(&["rev-parse", &format!("refs/remotes/origin/{branch}")])
            .trim(),
        head,
        "the branch is published to origin"
    );
    assert_eq!(
        repository
            .origin_git(&["rev-parse", &format!("refs/heads/{branch}")])
            .trim(),
        head,
        "the real bare origin carries the branch"
    );
    assert_eq!(
        repository
            .git(&["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"])
            .trim(),
        format!("origin/{branch}"),
        "the published branch tracks origin"
    );
}

/// Minimum valid boundary: one visible change is enough for a default-branch
/// run to create and publish its summary branch.
#[test]
fn when_finalizing_given_one_visible_change_on_the_default_branch_then_publishes_a_summary_branch()
{
    let repository = default_branch_repository();
    let before = default_tip(&repository);
    let artifacts = repository.path().join(".ejaj/minimal");
    let target = repository.path().join("minimal.txt");
    let command = format!("printf 'one visible change\\n' > '{}'", target.display());

    let (config, _transport) = common::scripted_pipeline_with_command(
        common::GateScript::Passing,
        common::SummaryScript::Summary,
        command,
    );
    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), &artifacts);

    assert!(matches!(step, Step::Complete), "{step:?}");
    assert_eq!(repository.current_branch(), common::SUMMARY_BRANCH);
    assert_eq!(
        repository.git(&["show", "HEAD:minimal.txt"]),
        "one visible change\n"
    );
    assert_eq!(
        repository
            .git(&[
                "rev-parse",
                &format!("refs/heads/{}", common::SUMMARY_BRANCH)
            ])
            .trim(),
        repository.git(&["rev-parse", "HEAD"]).trim(),
        "the branch's local ref names its one commit"
    );
    assert_eq!(default_tip(&repository), before);
    assert_eq!(
        repository
            .origin_git(&[
                "rev-parse",
                &format!("refs/heads/{}", common::SUMMARY_BRANCH)
            ])
            .trim(),
        repository.git(&["rev-parse", "HEAD"]).trim(),
        "the minimal run still publishes its branch"
    );
}

/// Edge-case table: a canonical header whose scope is far longer than the slug
/// cap still derives a valid, bounded branch name with no trailing hyphen.
#[test]
fn when_deriving_a_branch_given_a_long_summary_header_then_respects_the_slug_limit() {
    let repository = default_branch_repository();
    let artifacts = repository.path().join(".ejaj/long-header");
    let scope = "a".repeat(300);
    let summary = format!("test({scope}): script the change summary\n\nA body line.\n");
    let (config, _transport) = common::scripted_pipeline_with_command(
        common::GateScript::Passing,
        common::SummaryScript::Turns(vec![common::SummaryReply::Answer(summary)]),
        change_tracked_file(&repository),
    );

    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), &artifacts);

    assert!(matches!(step, Step::Complete), "{step:?}");
    let branch = repository.current_branch();
    let slug = branch
        .strip_prefix("ejaj/")
        .unwrap_or_else(|| panic!("the branch keeps the fixed prefix: {branch}"));
    assert_eq!(slug.len(), 235, "the slug is capped at 235 characters");
    assert_eq!(
        branch.len(),
        240,
        "the branch name stays within 240 characters"
    );
    assert!(!slug.ends_with('-'), "a trailing hyphen is trimmed: {slug}");
    assert!(!slug.starts_with('-'), "no leading hyphen survives: {slug}");
    assert!(
        !slug.contains("--"),
        "separators are single hyphens: {slug}"
    );
    assert!(
        slug.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
        "the slug is lowercase ASCII letters, digits and hyphens: {slug}"
    );
    assert_eq!(
        repository
            .origin_git(&["rev-parse", &format!("refs/heads/{branch}")])
            .trim(),
        repository.git(&["rev-parse", "HEAD"]).trim(),
        "the bounded branch is still a valid published Git branch"
    );
}

/// Contract and invariant: an independently advanced origin becomes the run
/// branch's parent, and the default branch stays at that updated tip.
#[test]
fn when_publishing_given_a_default_branch_run_then_uses_the_updated_default_as_parent() {
    let repository = default_branch_repository();
    let old_tip = default_tip(&repository);
    let updated = repository.advance_origin_default("advanced.txt", "advanced remotely\n");
    assert_ne!(updated, old_tip, "the remote default branch really moved");
    assert_eq!(
        default_tip(&repository),
        old_tip,
        "the independent advance leaves the local default branch behind"
    );
    let artifacts = repository.path().join(".ejaj/updated-default");

    let (config, _transport) = common::scripted_pipeline_with_command(
        common::GateScript::Passing,
        common::SummaryScript::Summary,
        change_tracked_file(&repository),
    );
    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), &artifacts);

    assert!(matches!(step, Step::Complete), "{step:?}");
    assert_eq!(repository.current_branch(), common::SUMMARY_BRANCH);
    assert_eq!(
        repository.git(&["rev-parse", "HEAD^"]).trim(),
        updated,
        "the run commit's parent is the updated default-branch tip"
    );
    assert_eq!(
        default_tip(&repository),
        updated,
        "the default branch is fast-forwarded to the remote tip and no further"
    );
    assert_eq!(
        repository
            .origin_git(&["rev-parse", "refs/heads/main"])
            .trim(),
        updated,
        "the remote default branch is unchanged by the run commit"
    );
    assert_eq!(
        repository.git(&["show", "HEAD:advanced.txt"]),
        "advanced remotely\n",
        "the fast-forwarded remote change is part of the branch's history"
    );
    assert_eq!(
        repository.git(&["show", "HEAD:tracked.txt"]),
        "changed by the run\n"
    );
}

/// Regression: the commit's raw message bytes, including the terminal LF and
/// comment-like body lines, are exactly the written summary artifact's bytes.
#[test]
fn when_committing_given_a_canonical_summary_then_preserves_exact_artifact_bytes() {
    let repository = default_branch_repository();
    let artifacts = repository.path().join(".ejaj/exact-summary");
    let summary = "test(pipeline): script the change summary\n\n# comment-like line stays\n\nA body line with  two spaces.\n";
    let (config, _transport) = common::scripted_pipeline_with_command(
        common::GateScript::Passing,
        common::SummaryScript::Turns(vec![common::SummaryReply::Answer(summary.to_string())]),
        change_tracked_file(&repository),
    );

    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), &artifacts);

    assert!(matches!(step, Step::Complete), "{step:?}");
    let written =
        fs::read(artifacts.join("change-summary.md")).expect("the summary artifact is readable");
    assert_eq!(
        written.as_slice(),
        summary.as_bytes(),
        "the artifact is the canonical summary exactly"
    );
    assert!(
        written.ends_with(b"\n"),
        "the canonical summary's terminal LF is part of the comparison"
    );
    assert_eq!(
        head_message_bytes(&repository),
        written,
        "the raw commit message bytes equal the artifact bytes"
    );
    assert!(
        head_message(&repository).contains("# comment-like line stays"),
        "comment-like lines are preserved verbatim"
    );
}

/// Upper boundary: every visible change kind reaches the one commit while an
/// ignored file and the ignored artifact directory stay out of it.
#[test]
fn when_finalizing_given_all_visible_change_kinds_and_an_ignored_file_then_commits_only_visible_changes()
 {
    let repository = common::TempRepo::new();
    repository.write(".gitignore", "ignored.txt\n.ejaj/\n");
    repository.write("modified.txt", "before\n");
    repository.write("deleted.txt", "before\n");
    repository.commit_all("baseline");
    let before = default_tip(&repository);
    let artifacts = repository.path().join(".ejaj/all-visible");
    let root = repository.path().display().to_string();
    let command = format!(
        "printf 'after\\n' > '{root}/modified.txt' \
         && rm -f '{root}/deleted.txt' \
         && printf 'staged addition\\n' > '{root}/staged-added.txt' \
         && git -C '{root}' add staged-added.txt \
         && printf 'untracked file\\n' > '{root}/untracked.txt' \
         && printf 'invisible\\n' > '{root}/ignored.txt' \
         && mkdir -p '{root}/.ejaj' \
         && printf 'scratch\\n' > '{root}/.ejaj/scratch.txt'"
    );
    let (config, _transport) = common::scripted_pipeline_with_command(
        common::GateScript::Passing,
        common::SummaryScript::Summary,
        command,
    );

    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), &artifacts);

    assert!(matches!(step, Step::Complete), "{step:?}");
    assert_eq!(repository.current_branch(), common::SUMMARY_BRANCH);
    assert_eq!(
        repository.git(&["show", "HEAD:modified.txt"]),
        "after\n",
        "the tracked modification is committed"
    );
    assert_eq!(
        repository.git(&["show", "HEAD:staged-added.txt"]),
        "staged addition\n",
        "the staged addition is committed"
    );
    assert_eq!(
        repository.git(&["show", "HEAD:untracked.txt"]),
        "untracked file\n",
        "the untracked non-ignored file is committed"
    );
    let paths = head_paths(&repository);
    assert!(
        !paths.iter().any(|path| path == "deleted.txt"),
        "the tracked deletion is committed: {paths:?}"
    );
    assert!(
        !paths.iter().any(|path| path == "ignored.txt"),
        "the ignored file is never added by the finalizer: {paths:?}"
    );
    assert!(
        !paths.iter().any(|path| path.starts_with(".ejaj/")),
        "the ignored artifact directory is never added: {paths:?}"
    );
    assert!(
        repository.path().join(".ejaj/scratch.txt").is_file(),
        "the ignored scratch file exists on disk but stays out of the commit"
    );
    assert_eq!(repository.status(), "", "the published branch is clean");
    assert_eq!(default_tip(&repository), before);
}

/// Empty boundary: with no visible change, no stash is made, a pre-existing
/// stash is never popped, the ordinary empty-commit refusal is fatal, and no
/// branch is published.
#[test]
fn when_finalizing_given_no_visible_changes_and_a_preexisting_stash_then_skips_stash_and_pop() {
    let repository = default_branch_repository();
    repository.write("stashed.txt", "work that belongs to an earlier run\n");
    repository.stash_push("preexisting stash");
    assert_eq!(repository.stash_count(), 1, "the fixture carries one stash");
    let stash_before = repository.stash_list();
    let stash_object_before = repository.git_ref("refs/stash");
    let before = default_tip(&repository);
    let remote_before = remote_refs(&repository);

    let artifacts = scratch_dir();
    fs::create_dir_all(&artifacts).expect("the external artifact directory is creatable");
    let (config, _transport) =
        common::scripted_pipeline_with(common::GateScript::Passing, common::SummaryScript::Summary);
    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), &artifacts);

    let (stage, reason) = git_failure(&step);
    assert_eq!(
        stage, "commit",
        "the no-changes failure is the ordinary commit refusal: {reason}"
    );
    assert_eq!(
        repository.stash_count(),
        1,
        "no new stash is created and the existing one is not popped"
    );
    assert_eq!(
        repository.stash_list(),
        stash_before,
        "the pre-existing stash entry is unchanged"
    );
    assert_eq!(
        repository.git_ref("refs/stash"),
        stash_object_before,
        "the pre-existing stash object is the same object"
    );
    assert_eq!(default_tip(&repository), before);
    assert_eq!(
        remote_refs(&repository),
        remote_before,
        "no branch is published when the commit is refused"
    );
    assert!(
        repository
            .origin_ref(&format!("refs/heads/{}", common::SUMMARY_BRANCH))
            .is_none(),
        "the refused run leaves no remote summary branch"
    );
    assert!(
        artifacts.join("change-summary.md").is_file(),
        "the summary artifact was written even though the commit was refused"
    );

    let _ = fs::remove_dir_all(&artifacts);
}

/// Core entry-point behavior: visible run work is saved in a new stash entry and
/// the worktree is verified clean before the default branch is switched. A
/// pre-existing entry is present, so a run that saved nothing or saved over the
/// wrong entry would be visible.
#[test]
fn when_stashing_given_visible_changes_then_verifies_the_worktree_is_clean() {
    let repository = default_branch_repository();
    // One pre-existing entry, so a run that popped by position rather than by
    // the identity it created would restore the wrong work.
    repository.write("earlier.txt", "work that belongs to an earlier run\n");
    repository.stash_push("preexisting stash");
    let earlier_stash = repository.stash_list();
    assert_eq!(repository.stash_count(), 1, "the fixture carries one stash");

    // The observation files live inside `.git`, so recording them cannot itself
    // become visible run work. The first post-checkout happens immediately
    // after the run's changes were saved.
    let observed_status = repository.path().join(".git/observed-status.txt");
    let observed_stash = repository.path().join(".git/observed-stash.txt");
    let observation_marker = repository.path().join(".git/stash-observation-done");
    let script = format!(
        "#!/bin/sh\n\
         if [ ! -f '{marker}' ]; then\n\
           git status --porcelain --untracked-files=all > '{status}'\n\
           git stash list --format='%H %s' > '{stash}'\n\
           printf 'observed\\n' > '{marker}'\n\
         fi\n",
        marker = observation_marker.display(),
        status = observed_status.display(),
        stash = observed_stash.display(),
    );
    repository.install_hook("post-checkout", &script);

    let artifacts = repository.path().join(".ejaj/stash-verification");
    let (config, _transport) = common::scripted_pipeline_with_command(
        common::GateScript::Passing,
        common::SummaryScript::Summary,
        change_tracked_file(&repository),
    );
    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), &artifacts);

    assert!(
        matches!(step, Step::Complete),
        "a saved run completes after its summary branch is published: {step:?}"
    );
    assert_eq!(
        fs::read_to_string(&observed_status).expect("the post-checkout status is recorded"),
        "",
        "no visible run change may remain after the stash: the worktree is clean"
    );
    let observed_stashes =
        fs::read_to_string(&observed_stash).expect("the post-checkout stash list is recorded");
    assert!(
        observed_stashes.contains("ejaj run changes"),
        "the run's own entry exists before the branch switch: {observed_stashes}"
    );
    assert!(
        observed_stashes.contains("preexisting stash"),
        "the pre-existing entry is not mistaken for the run's: {observed_stashes}"
    );
    assert_eq!(
        repository.stash_count(),
        1,
        "the run pops exactly the entry it created"
    );
    assert_eq!(
        repository.stash_list(),
        earlier_stash,
        "the pre-existing stash entry is unchanged"
    );
    assert_eq!(
        repository.git(&["show", "HEAD:tracked.txt"]),
        "changed by the run\n",
        "the saved run change is committed on the summary branch"
    );
    assert_eq!(
        repository.status(),
        "",
        "the published summary branch is clean"
    );
}

/// Contract and invariant check: an unrelated stash entry inserted after the
/// run's own entry was saved must not be popped. Only the retained run stash is
/// reapplied, so the run's change is restored and the unrelated entry survives.
#[test]
fn when_finalizing_given_interleaved_stash_entries_then_pops_only_the_run_stash() {
    let repository = default_branch_repository();
    // A pre-existing stash that predates the run.
    repository.write("earlier.txt", "work that belongs to an earlier run\n");
    repository.stash_push("preexisting stash");
    assert_eq!(repository.stash_count(), 1, "the fixture carries one stash");

    // A real Git hook inserts an unrelated stash entry during finalization --
    // after the run's changes are saved, before they are reapplied. The marker
    // makes it happen exactly once.
    let interleaved_file = repository.path().join("interleaved.txt");
    let marker = repository.path().join(".git/interleaved-stash-done");
    let script = format!(
        "#!/bin/sh\n\
         if [ ! -f '{marker}' ]; then\n\
           printf 'unrelated work\\n' > '{interleaved}'\n\
           git stash push --include-untracked --quiet --message 'interleaved stash'\n\
           printf 'done\\n' > '{marker}'\n\
         fi\n",
        marker = marker.display(),
        interleaved = interleaved_file.display(),
    );
    repository.install_hook("post-checkout", &script);

    let before = default_tip(&repository);
    let artifacts = repository.path().join(".ejaj/interleaved-stashes");
    let (config, _transport) = common::scripted_pipeline_with_command(
        common::GateScript::Passing,
        common::SummaryScript::Summary,
        change_tracked_file(&repository),
    );
    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), &artifacts);

    assert!(
        matches!(step, Step::Complete),
        "the run completes although an unrelated stash was inserted: {step:?}"
    );
    assert_eq!(
        repository.git(&["show", "HEAD:tracked.txt"]),
        "changed by the run\n",
        "only this run's saved change is restored and committed"
    );
    assert!(
        !head_paths(&repository)
            .iter()
            .any(|path| path == "interleaved.txt"),
        "the unrelated stash is not restored into the summary branch"
    );
    assert_eq!(
        repository.stash_count(),
        2,
        "the pre-existing and interleaved entries survive"
    );
    let stash_subjects = repository.git(&["stash", "list", "--format=%s"]);
    assert!(
        stash_subjects.contains("preexisting stash"),
        "the pre-existing stash remains: {stash_subjects}"
    );
    assert!(
        stash_subjects.contains("interleaved stash"),
        "the interleaved stash remains: {stash_subjects}"
    );
    assert!(
        !stash_subjects.contains("ejaj run changes"),
        "only the run's own entry was popped: {stash_subjects}"
    );
    assert_eq!(
        repository.status(),
        "",
        "the summary branch is clean after the run's own entry is popped"
    );
    assert_eq!(
        default_tip(&repository),
        before,
        "the default branch does not advance"
    );
}

/// Malformed boundary: an exact local or remote collision with the derived name
/// is refused at branch creation, without checking it out, reusing it or
/// overwriting its object.
#[test]
fn when_creating_a_branch_given_an_existing_local_or_remote_name_then_rejects_without_reuse() {
    // A local branch already carries the derived name.
    {
        let repository = default_branch_repository();
        let branch = common::SUMMARY_BRANCH;
        repository.git(&["branch", branch, "main"]);
        let local_before = repository.git_ref(&format!("refs/heads/{branch}"));
        let remote_before = remote_refs(&repository);
        let artifacts = repository.path().join(".ejaj/local-collision");
        let (config, _transport) = common::scripted_pipeline_with_command(
            common::GateScript::Passing,
            common::SummaryScript::Summary,
            change_tracked_file(&repository),
        );

        let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
        let step = pipeline.ask(&common::harmless_plan_request(), &artifacts);

        let (stage, reason) = git_failure(&step);
        assert_eq!(
            stage, "branch",
            "the local collision fails at branch creation: {reason}"
        );
        assert!(
            reason.contains(branch),
            "the failure names the colliding branch: {reason}"
        );
        assert_eq!(
            repository.current_branch(),
            common::DEFAULT_BRANCH,
            "the existing branch is not checked out or reused"
        );
        assert_eq!(
            repository.git_ref(&format!("refs/heads/{branch}")),
            local_before,
            "the existing local ref is not moved"
        );
        assert_eq!(
            remote_refs(&repository),
            remote_before,
            "the local collision publishes nothing"
        );
    }

    // Only the remote carries the derived name.
    {
        let repository = default_branch_repository();
        let branch = common::SUMMARY_BRANCH;
        let push = format!("main:refs/heads/{branch}");
        repository.git(&["push", "--quiet", "origin", &push]);
        let remote_object = repository.origin_ref(&format!("refs/heads/{branch}"));
        assert!(remote_object.is_some(), "the remote collision exists");
        let remote_before = remote_refs(&repository);
        let artifacts = repository.path().join(".ejaj/remote-collision");
        let (config, _transport) = common::scripted_pipeline_with_command(
            common::GateScript::Passing,
            common::SummaryScript::Summary,
            change_tracked_file(&repository),
        );

        let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
        let step = pipeline.ask(&common::harmless_plan_request(), &artifacts);

        let (stage, reason) = git_failure(&step);
        assert_eq!(
            stage, "branch",
            "the remote collision fails at branch creation: {reason}"
        );
        assert!(
            reason.contains(branch),
            "the failure names the colliding branch: {reason}"
        );
        assert!(
            repository
                .git_ref(&format!("refs/heads/{branch}"))
                .is_none(),
            "no local branch is created for an existing remote name"
        );
        assert_eq!(
            repository.origin_ref(&format!("refs/heads/{branch}")),
            remote_object,
            "the existing remote ref is not overwritten"
        );
        assert_eq!(
            remote_refs(&repository),
            remote_before,
            "the remote collision creates no further remote-tracking ref"
        );
    }
}

/// Forbidden boundary: a remote branch appearing after the uniqueness check
/// makes the push reject rather than force-update or reuse the remote ref, even
/// when the racing ref is an ancestor of the run commit.
#[test]
fn when_publishing_given_a_remote_ref_appears_after_the_uniqueness_check_then_rejects_without_overwrite()
 {
    let repository = default_branch_repository();
    let branch = common::SUMMARY_BRANCH;

    // Move the real bare origin's default branch forward from an independent
    // clone. The run fast-forwards to that commit before it branches, so the
    // commit is the run commit's parent -- a genuine ancestor, not a divergent
    // tip that an ordinary non-fast-forward push would already refuse.
    let race = repository.advance_origin_default("advanced.txt", "advanced remotely\n");

    let origin = repository.origin_path().display().to_string();
    let hook = format!(
        "#!/bin/sh\ngit --git-dir='{origin}' update-ref refs/heads/{branch} {race}\nexit 0\n"
    );
    install_hook(&repository, &hook);

    let artifacts = repository.path().join(".ejaj/remote-race");
    let (config, _transport) = common::scripted_pipeline_with_command(
        common::GateScript::Passing,
        common::SummaryScript::Summary,
        change_tracked_file(&repository),
    );
    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), &artifacts);

    let (stage, reason) = git_failure(&step);
    assert_eq!(
        stage, "push",
        "the raced publish fails at the push operation: {reason}"
    );
    assert_eq!(
        repository.origin_ref(&format!("refs/heads/{branch}")),
        Some(race.clone()),
        "the racing remote ref is neither force-updated nor reused"
    );
    assert_eq!(
        repository
            .origin_git(&["log", "-1", "--format=%s", &format!("refs/heads/{branch}")])
            .trim(),
        "independent origin advance",
        "the remote branch still carries the racing default-parent commit"
    );
    assert_eq!(
        repository.git(&["rev-parse", "HEAD^"]).trim(),
        race,
        "the racing remote ref is the run commit's ancestor, so only a create-only push rejects it"
    );
}

// ---------------------------------------------------------------------------
// Phase timing: one elapsed line per pipeline phase invocation
//
// Every test here drives the real Pipeline::ask over the scripted external
// transport and asserts the exact lines the production run printed. The
// transcript is read back at the process boundary: a child copy of this test
// binary performs the scripted run for real, so the parent reads the lines the
// pipeline actually wrote, without mocking a stream, a pipeline service or the
// persistence layer. The child asserts the non-timing behaviour -- the terminal
// Step, the artifacts and the requests each phase made -- that the additive
// lines must not change.
// ---------------------------------------------------------------------------

/// The environment variable a child copy of a phase-timing test sets on itself.
const PHASE_TIMING_CHILD: &str = "EJAJ_PIPELINE_PHASE_TIMING_CHILD";

/// The variable that names the scripted run a child copy must perform.
const PHASE_TIMING_CASE: &str = "EJAJ_PIPELINE_PHASE_TIMING_CASE";

/// The phase labels of one complete run, in invocation order: the plan ask, the
/// three unattended passes, and the queue, review and summary passes.
const PHASE_LABELS: [&str; 7] = [
    "plan-agent ask",
    "spec pass",
    "tickets pass",
    "triage pass",
    "ticket-queue round 1",
    "review round 1",
    "summary pass",
];

/// The labels an agent tool execution uses. They are not phases: the code
/// agent's own bash call is echoed inline, and the queue's verification gate is
/// reported under its own ticket-scoped label rather than as a tool or a phase.
const TOOL_LABELS: [&str; 2] = ["bash", "execute_script"];

/// Whether this process is the child copy of a phase-timing test.
fn is_phase_timing_child() -> bool {
    std::env::var_os(PHASE_TIMING_CHILD).is_some()
}

/// Run the phase-timing test named test for each of cases: the child copy
/// performs the one case the parent named and returns an empty vector, while the
/// parent captures one whole process transcript per case, in the order given.
fn phase_timing_transcripts(test: &str, cases: &[&str]) -> Vec<String> {
    if is_phase_timing_child() {
        // The test harness prints its own "test <name> ... " progress line with
        // no terminator before the body runs, so the first line the pipeline
        // writes would otherwise be appended to it. One newline puts every
        // message the pipeline prints on a line of its own.
        println!();
        let case =
            std::env::var(PHASE_TIMING_CASE).expect("the parent names the case for the child");
        run_phase_timing_case(&case);
        return Vec::new();
    }
    cases
        .iter()
        .map(|case| {
            common::captured_transcript(
                test,
                &[(PHASE_TIMING_CHILD, "1"), (PHASE_TIMING_CASE, case)],
            )
        })
        .collect()
}

/// The transcript of one scripted run: None in the child copy, which performs
/// the case instead, and the captured transcript in the parent.
fn phase_timing_transcript(test: &str, case: &str) -> Option<String> {
    phase_timing_transcripts(test, &[case]).into_iter().next()
}

/// Every elapsed line in transcript, in order.
fn elapsed_lines(transcript: &str) -> Vec<String> {
    transcript
        .lines()
        .map(str::trim_end)
        .filter(|line| line.starts_with("[INFO] ") && line.contains(" elapsed: "))
        .map(str::to_string)
        .collect()
}

/// The label of every elapsed line in transcript, in order.
fn elapsed_labels(transcript: &str) -> Vec<String> {
    elapsed_lines(transcript)
        .iter()
        .map(|line| {
            let rest = line
                .strip_prefix("[INFO] ")
                .expect("an elapsed line carries its severity marker");
            let (label, _) = rest
                .split_once(" elapsed: ")
                .expect("an elapsed line names its operation");
            label.to_string()
        })
        .collect()
}

/// The label of one ticket's verification-gate elapsed line.
fn gate_label(id: &str) -> String {
    format!("ticket {id} verification gate")
}

/// Whether `label` is one ticket's verification-gate elapsed label.
fn is_gate_label(label: &str) -> bool {
    label.starts_with("ticket ") && label.ends_with(" verification gate")
}

/// Every verification-gate elapsed line in transcript, in order.
fn gate_lines(transcript: &str) -> Vec<String> {
    elapsed_lines(transcript)
        .into_iter()
        .filter(|line| line.contains(" verification gate elapsed: "))
        .collect()
}

/// Whether label is a queue round's residual (overhead) line rather than the
/// round's own phase line.
fn is_overhead_label(label: &str) -> bool {
    label.starts_with("ticket-queue round ") && label.ends_with(" overhead")
}

/// The phase labels of transcript, in order: every elapsed line that is not an
/// agent tool execution, a per-attempt thinking report, a ticket verification
/// gate or a queue round's residual line, so the pipeline's own phases can be
/// asserted apart from the work and accounting the queue measured inside them.
fn phase_labels(transcript: &str) -> Vec<String> {
    elapsed_labels(transcript)
        .into_iter()
        .filter(|label| {
            label != "thinking"
                && !TOOL_LABELS.contains(&label.as_str())
                && !is_gate_label(label)
                && !is_overhead_label(label)
        })
        .collect()
}

/// Every elapsed line of transcript whose label is exactly label.
fn labelled(transcript: &str, label: &str) -> Vec<String> {
    elapsed_lines(transcript)
        .into_iter()
        .filter(|line| line.starts_with(&format!("[INFO] {label} elapsed: ")))
        .collect()
}

/// The duration of one elapsed line, with the exact established shape asserted:
/// digits on both sides of the point, exactly three fractional digits, the
/// seconds unit, and no escape sequence.
fn elapsed_duration(line: &str, label: &str) -> Duration {
    let tail = line
        .strip_prefix(&format!("[INFO] {label} elapsed: "))
        .unwrap_or_else(|| panic!("the line does not name {label}: {line:?}"));
    let (seconds, rest) = tail
        .split_once('.')
        .unwrap_or_else(|| panic!("the line has no fraction: {line:?}"));
    let (millis, unit) = rest
        .split_once(' ')
        .unwrap_or_else(|| panic!("the line has no unit: {line:?}"));
    assert!(
        !seconds.is_empty() && seconds.chars().all(|c| c.is_ascii_digit()),
        "the whole seconds must be digits: {line:?}"
    );
    assert_eq!(millis.len(), 3, "the fraction is three digits: {line:?}");
    assert!(
        millis.chars().all(|c| c.is_ascii_digit()),
        "the fraction must be digits: {line:?}"
    );
    assert_eq!(unit, "seconds", "the line's unit: {line:?}");
    assert!(
        !line.contains('\u{1b}'),
        "a redirected transcript stays plain: {line:?}"
    );
    let seconds: u64 = seconds
        .parse()
        .unwrap_or_else(|e| panic!("the whole seconds must parse ({e}): {line:?}"));
    let millis: u64 = millis
        .parse()
        .unwrap_or_else(|e| panic!("the fraction must parse ({e}): {line:?}"));
    Duration::from_secs(seconds) + Duration::from_millis(millis)
}

/// The whole seconds of one elapsed line, with its established shape asserted.
fn elapsed_seconds(line: &str, label: &str) -> u64 {
    elapsed_duration(line, label).as_secs()
}

/// The one-based round a queue total label names, if it is one.
fn queue_total_round(label: &str) -> Option<usize> {
    let rest = label.strip_prefix("ticket-queue round ")?;
    if rest.contains(' ') {
        return None;
    }
    rest.parse().ok()
}

/// The one-based round a queue residual label names, if it is one.
fn queue_overhead_round(label: &str) -> Option<usize> {
    let rest = label.strip_prefix("ticket-queue round ")?;
    let rest = rest.strip_suffix(" overhead")?;
    rest.parse().ok()
}

/// The inline duration carried by one executed call line, or None for a line
/// that is not a call line and for a rejected call, which never ran and so has
/// no duration.
fn call_line_elapsed(line: &str) -> Option<Duration> {
    let rest = line.strip_prefix('[')?;
    let (counter, rest) = rest.split_once("] (")?;
    let (step, max) = counter.split_once('/')?;
    let numeric = |value: &str| !value.is_empty() && value.chars().all(|c| c.is_ascii_digit());
    if !numeric(step) || !numeric(max) {
        return None;
    }
    let (fraction, rest) = rest.split_once("s)")?;
    if !rest.starts_with(" $") && !rest.starts_with('$') {
        return None;
    }
    let (seconds, millis) = fraction.split_once('.')?;
    if millis.len() != 3 || !numeric(seconds) || !numeric(millis) {
        return None;
    }
    let seconds: u64 = seconds.parse().ok()?;
    let millis: u64 = millis.parse().ok()?;
    Some(Duration::from_secs(seconds) + Duration::from_millis(millis))
}

/// One queue round's displayed accounting: its total, the residual line that
/// follows it, and the model attempts, executed commands and gates emitted
/// between the previous round and this round's total.
#[derive(Debug)]
struct QueueRoundTiming {
    round: usize,
    total: Duration,
    overhead: Option<Duration>,
    model_attempts: Vec<Duration>,
    executed_tools: Vec<Duration>,
    gates: Vec<Duration>,
}

impl QueueRoundTiming {
    fn contributor_total(&self) -> Duration {
        self.model_attempts
            .iter()
            .chain(&self.executed_tools)
            .chain(&self.gates)
            .fold(Duration::ZERO, |sum, elapsed| sum.saturating_add(*elapsed))
    }

    fn contributor_count(&self) -> usize {
        self.model_attempts.len() + self.executed_tools.len() + self.gates.len()
    }

    fn overhead(&self) -> Duration {
        self.overhead
            .unwrap_or_else(|| panic!("queue round {} has no overhead elapsed line", self.round))
    }
}

/// Every queue round in transcript, in order, with its own contributors and
/// residual. Each total line closes the contributors emitted since the previous
/// total, so one round's measurement can never be attributed to another.
fn queue_round_timings(transcript: &str) -> Vec<QueueRoundTiming> {
    let mut rounds: Vec<QueueRoundTiming> = Vec::new();
    let mut model_attempts: Vec<Duration> = Vec::new();
    let mut executed_tools: Vec<Duration> = Vec::new();
    let mut gates: Vec<Duration> = Vec::new();

    for raw in transcript.lines() {
        let line = raw.trim_end();
        // The queue invocation begins at its own banner: earlier phases'
        // thinking lines belong to no queue round and must not be attributed to
        // the first one.
        if line.trim() == "=== Ticket queue ===" {
            model_attempts.clear();
            executed_tools.clear();
            gates.clear();
            continue;
        }
        if let Some(rest) = line.strip_prefix("[INFO] ") {
            let Some((label, _)) = rest.split_once(" elapsed: ") else {
                continue;
            };
            if label == "thinking" {
                model_attempts.push(elapsed_duration(line, label));
            } else if is_gate_label(label) {
                gates.push(elapsed_duration(line, label));
            } else if let Some(round) = queue_total_round(label) {
                rounds.push(QueueRoundTiming {
                    round,
                    total: elapsed_duration(line, label),
                    overhead: None,
                    model_attempts: std::mem::take(&mut model_attempts),
                    executed_tools: std::mem::take(&mut executed_tools),
                    gates: std::mem::take(&mut gates),
                });
            } else if let Some(round) = queue_overhead_round(label) {
                let overhead = elapsed_duration(line, label);
                let timing = rounds
                    .iter_mut()
                    .find(|timing| timing.round == round)
                    .unwrap_or_else(|| panic!("queue overhead for round {round} has no total"));
                assert!(
                    timing.overhead.replace(overhead).is_none(),
                    "queue round {round} reports its overhead more than once"
                );
            }
        } else if let Some(elapsed) = thinking_line_elapsed(line) {
            model_attempts.push(elapsed);
        } else if let Some(elapsed) = call_line_elapsed(line) {
            executed_tools.push(elapsed);
        }
    }
    rounds
}

/// The duration carried by a model-attempt report line
/// (#thinking (0.002s) ...), or None for any other line.
fn thinking_line_elapsed(line: &str) -> Option<Duration> {
    let rest = line.strip_prefix("#thinking (")?;
    let (fraction, _) = rest.split_once("s) ")?;
    let (seconds, millis) = fraction.split_once('.')?;
    if millis.len() != 3
        || !seconds.chars().all(|c| c.is_ascii_digit())
        || !millis.chars().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    Some(Duration::from_secs(seconds.parse().ok()?) + Duration::from_millis(millis.parse().ok()?))
}

/// The displayed residual is the displayed total minus the displayed
/// contributors, within the independent flooring tolerance of one millisecond
/// per value, and never exceeds its own total.
fn assert_queue_round_accounting(timing: &QueueRoundTiming, transcript: &str) {
    let overhead = timing.overhead();
    let contributors = timing.contributor_total();
    let floor_of_components = timing.total.saturating_sub(contributors);
    let tolerance = Duration::from_millis(timing.contributor_count() as u64 + 1);
    assert!(
        overhead <= timing.total,
        "queue round {} overhead {overhead:?} exceeds its total {:?}:\n{transcript}",
        timing.round,
        timing.total
    );
    assert!(
        overhead.abs_diff(floor_of_components) <= tolerance,
        "queue round {} total {:?} minus contributors {contributors:?} does not explain overhead          {overhead:?} within {tolerance:?}:\n{transcript}",
        timing.round,
        timing.total
    );
}

/// Every tool-call transcript line (the permanent call echo) in transcript, in
/// order: one per agent tool call, and none for the queue's own gate.
fn call_lines(transcript: &str) -> Vec<String> {
    transcript
        .lines()
        .map(str::trim_end)
        .filter(|line| {
            let Some(rest) = line.strip_prefix('[') else {
                return false;
            };
            let Some((counter, rest)) = rest.split_once("] ") else {
                return false;
            };
            let Some((step, max)) = counter.split_once('/') else {
                return false;
            };
            let numeric =
                |value: &str| !value.is_empty() && value.chars().all(|c| c.is_ascii_digit());
            numeric(step) && numeric(max) && (rest.starts_with('(') || rest.starts_with("$ "))
        })
        .map(str::to_string)
        .collect()
}

/// The scripted runs the phase-timing tests observe. Each one is a whole
/// production Pipeline::ask over the scripted external transport; the terminal
/// Step, the artifacts and the requests the run made are asserted here, in the
/// child process, so the parent's transcript assertions are the only
/// timing-specific ones.
fn run_phase_timing_case(case: &str) {
    match case {
        // The complete run: a resolved queue, a passing review and the canonical
        // summary.
        "resolved" => {
            let (step, dir, transport) =
                scripted_run(QueueOutcome::Resolved, common::SummaryScript::Summary);
            assert!(matches!(step, Step::Complete), "{step:?}");
            assert_eq!(
                transport.urls_called.lock().unwrap().len(),
                8,
                "three handoffs, the triage pass, two code steps, the review and the summary"
            );
            let status_text = fs::read_to_string(dir.join("tickets/queue-status.json"))
                .expect("the queue status artifact is readable");
            let status: serde_json::Value =
                serde_json::from_str(&status_text).expect("the queue status is JSON");
            assert_eq!(
                status["1-verify-command-output"], "completed",
                "the ticket's verification gate really ran and passed"
            );
            assert!(
                !status_text.contains("elapsed") && !status_text.contains("overhead"),
                "timing must not be persisted to the queue status snapshot: {status_text}"
            );
            let report = fs::read_to_string(dir.join("queue-report.md"))
                .expect("the queue report artifact is readable");
            assert!(report.starts_with("# Ticket queue report\n"), "{report}");
            assert!(report.contains("- Status: completed"), "{report}");
            assert!(
                !report.contains("elapsed") && !report.contains("overhead"),
                "timing must not be persisted to the queue report: {report}"
            );
            assert_eq!(summary_artifact(&dir), common::SCRIPTED_SUMMARY);
            let _ = fs::remove_dir_all(&dir);
        }
        // Two REPL lines over one plan session: the first line completes, the
        // second line's plan ask never hands over a plan.
        "two-asks" => {
            let (config, transport) = common::scripted_pipeline_with(
                common::GateScript::Passing,
                common::SummaryScript::Summary,
            );
            let dir = scratch_dir();
            let repository = common::TempRepo::at(dir.clone());
            repository.commit_baseline();
            let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());

            let first = pipeline.ask(&common::harmless_plan_request(), &dir);
            assert!(
                matches!(first, Step::Complete),
                "the first REPL line completes: {first:?}"
            );
            assert_eq!(
                repository.status(),
                "",
                "the completed line left its own repository clean"
            );
            let second = pipeline.ask(&common::harmless_plan_request(), &dir);
            assert!(
                matches!(second, Step::Retry),
                "the second line's plan ask never hands over a plan: {second:?}"
            );
            assert!(
                transport.requests.lock().unwrap().len() > 8,
                "the second REPL line really reached the plan agent"
            );
            let _ = fs::remove_dir_all(&dir);
        }
        // Three review-generated fix queues, each followed by its own review.
        "fix-three-rounds" => {
            let (step, repository, _transport) =
                review_run(common::ReviewScript::FixThreeRoundsThenPass, |_| {});
            assert!(matches!(step, Step::Complete), "{step:?}");
            let review = artifact(repository.path(), "review.md");
            for round in 1..=4 {
                assert!(review.contains(&format!("## Round {round}")), "{review}");
            }
        }
        // The same four queue rounds, each with a real uninstrumented retry
        // backoff, so cross-round accounting leakage is visible.
        "fix-three-rounds-retry" => {
            let (step, repository, _transport) = review_run_with_code(
                common::ReviewScript::FixThreeRoundsThenPass,
                common::CodeScript::TransientOncePerRound,
            );
            assert!(matches!(step, Step::Complete), "{step:?}");
            let review = artifact(repository.path(), "review.md");
            for round in 1..=4 {
                assert!(review.contains(&format!("## Round {round}")), "{review}");
            }
        }
        // The smallest repeated pair: one fix queue and the review behind it.
        "fix-once-then-pass" => {
            let (step, repository, _transport) =
                review_run(common::ReviewScript::FixOnceThenPass, |_| {});
            assert!(matches!(step, Step::Complete), "{step:?}");
            assert!(
                artifact(repository.path(), "review.md").contains("## Round 2"),
                "the second review ran"
            );
        }
        // The queue drains without resolving its ticket.
        "incomplete-queue" => {
            let (step, dir, transport) =
                scripted_run(QueueOutcome::Incomplete, common::SummaryScript::Summary);
            assert!(matches!(step, Step::Retry), "{step:?}");
            assert_eq!(
                transport.urls_called.lock().unwrap().len(),
                9,
                "the failed first gate retries once, and no review request follows an incomplete queue"
            );
            assert!(!dir.join("review.md").exists());
            let status: serde_json::Value = serde_json::from_str(
                &fs::read_to_string(dir.join("tickets/queue-status.json"))
                    .expect("the queue status artifact is readable"),
            )
            .expect("the queue status is JSON");
            assert_eq!(
                status[GATE_TICKET_ID], "failed",
                "the failing gate really ran and failed the ticket"
            );
            let report = fs::read_to_string(dir.join("queue-report.md"))
                .expect("the queue report artifact is readable");
            assert!(report.contains("- Status: failed"), "{report}");
            assert!(report.contains("- Ended: gate failed"), "{report}");
            assert_eq!(summary_artifact(&dir), common::SCRIPTED_SUMMARY);
            let _ = fs::remove_dir_all(&dir);
        }
        // The ticket's gate fails after a measurable pause: the queue result,
        // status snapshot and report are the same as any failed gate.
        "slow-gate-error" => {
            let (step, dir, transport) =
                scripted_run(QueueOutcome::SlowIncomplete, common::SummaryScript::Summary);
            assert!(matches!(step, Step::Retry), "{step:?}");
            assert_eq!(
                transport.urls_called.lock().unwrap().len(),
                9,
                "the failed first gate retries once, and no review request follows an incomplete queue"
            );
            assert!(!dir.join("review.md").exists());
            let status: serde_json::Value = serde_json::from_str(
                &fs::read_to_string(dir.join("tickets/queue-status.json"))
                    .expect("the queue status artifact is readable"),
            )
            .expect("the queue status is JSON");
            assert_eq!(status[GATE_TICKET_ID], "failed");
            let report = fs::read_to_string(dir.join("queue-report.md"))
                .expect("the queue report artifact is readable");
            assert!(report.starts_with("# Ticket queue report\n"), "{report}");
            assert!(report.contains(&format!("## {GATE_TICKET_ID}")), "{report}");
            assert!(report.contains("- Status: failed"), "{report}");
            assert!(report.contains("- Ended: gate failed"), "{report}");
            assert!(report.contains("- Gate: `sleep 0.1; exit 1`"), "{report}");
            assert_eq!(summary_artifact(&dir), common::SCRIPTED_SUMMARY);
            let _ = fs::remove_dir_all(&dir);
        }
        // The code agent is refused before the gate: the ticket fails as an
        // agent failure and no verification gate is reached.
        "code-api-failure" => {
            let (config, transport) = common::scripted_pipeline_with_code(
                common::CodeScript::ApiError,
                common::SummaryScript::Summary,
            );
            let dir = scratch_dir();
            let repository = common::TempRepo::at(dir.clone());
            repository.commit_baseline();
            let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
            let step = pipeline.ask(&common::harmless_plan_request(), &dir);
            assert!(matches!(step, Step::Retry), "{step:?}");
            assert_eq!(
                transport.urls_called.lock().unwrap().len(),
                6,
                "three handoffs, the triage pass, the refused code request and the summary"
            );
            assert!(!dir.join("review.md").exists());
            let status: serde_json::Value = serde_json::from_str(
                &fs::read_to_string(dir.join("tickets/queue-status.json"))
                    .expect("the queue status artifact is readable"),
            )
            .expect("the queue status is JSON");
            assert_eq!(status[GATE_TICKET_ID], "failed");
            let report = fs::read_to_string(dir.join("queue-report.md"))
                .expect("the queue report artifact is readable");
            assert!(
                report.contains("- Ended: agent failure: API error"),
                "{report}"
            );
            assert_eq!(summary_artifact(&dir), common::SCRIPTED_SUMMARY);
            let _ = fs::remove_dir_all(&dir);
        }
        // The queue stops before its ticket starts.
        "stopped-queue" => {
            let (step, dir, transport) =
                scripted_run(QueueOutcome::Stopped, common::SummaryScript::Summary);
            assert!(matches!(step, Step::Retry), "{step:?}");
            assert_eq!(
                transport.urls_called.lock().unwrap().len(),
                5,
                "three handoff steps, the triage pass and the summary pass"
            );
            assert!(!dir.join("review.md").exists());
            assert_eq!(summary_artifact(&dir), common::SCRIPTED_SUMMARY);
            let _ = fs::remove_dir_all(&dir);
        }
        // The review hands over an empty outcome: no findings, no fix tickets,
        // no later round.
        "empty-review" => {
            let (step, repository, transport) = review_run(common::ReviewScript::Pass, |_| {});
            assert!(matches!(step, Step::Complete), "{step:?}");
            let review = artifact(repository.path(), "review.md");
            assert!(
                review.contains("**Verdict:** Passed (no fix tickets)"),
                "{review}"
            );
            assert!(!review.contains("## Round 2"), "{review}");
            assert_eq!(
                review_contexts(&transport).len(),
                1,
                "an empty outcome starts no further review"
            );
        }
        // The preflight refuses the run before any agent or artifact.
        "preflight-failure" => {
            let (config, transport) = common::scripted_pipeline_with(
                common::GateScript::Passing,
                common::SummaryScript::Summary,
            );
            let dir = scratch_dir();
            let repository = common::TempRepo::at(dir.clone());
            repository.commit_baseline();
            repository.write("left-behind.txt", "a file no commit knows about\n");

            let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
            let step = pipeline.ask(&common::harmless_plan_request(), &dir);

            match &step {
                Step::GitFailed { stage, .. } => assert_eq!(
                    *stage, "preflight",
                    "the gate is the first operation of the invocation"
                ),
                other => panic!("expected the preflight failure, got {other:?}"),
            }
            assert!(
                transport.requests.lock().unwrap().is_empty(),
                "a gated run reaches no agent"
            );
            assert!(
                !dir.join("plan.md").exists(),
                "a gated run writes no artifact"
            );
            let _ = fs::remove_dir_all(&dir);
        }
        // The triage pass is refused at the API: no report, no queue.
        "triage-api-failure" => {
            let (step, dir, transport) = triage_run(common::TriageScript::ApiError);
            assert!(matches!(step, Step::Retry), "{step:?}");
            assert!(
                !transport.requests.lock().unwrap().is_empty(),
                "the triage pass really was invoked"
            );
            assert!(!dir.join("ticket-triage.md").exists());
            assert!(!dir.join("queue-report.md").exists());
            assert_eq!(summary_artifact_or_absent(&dir), None);
            let _ = fs::remove_dir_all(&dir);
        }
        // The triage pass ends its turn without a report, so the queue never
        // starts.
        "triage-turn-end" => {
            let (step, dir, _transport) = triage_run(common::TriageScript::TurnEnd);
            assert!(matches!(step, Step::Retry), "{step:?}");
            assert!(!dir.join("ticket-triage.md").exists());
            assert!(!dir.join("queue-report.md").exists());
            assert!(!dir.join("change-summary.md").exists());
            let _ = fs::remove_dir_all(&dir);
        }
        // The summary pass fails in the transport, after the loop's own bounded
        // retries.
        "summary-transport-failure" => {
            let (step, dir, _transport) = scripted_run(
                QueueOutcome::Resolved,
                common::SummaryScript::TransportError,
            );
            assert!(matches!(step, Step::SummaryFailed { .. }), "{step:?}");
            assert!(!dir.join("change-summary.md").exists());
            let _ = fs::remove_dir_all(&dir);
        }
        // The review pass is refused at the API: the round it reached still has
        // exactly one line, the run keeps its recoverable step, and the summary
        // that follows still runs.
        "review-api-failure" => {
            let (step, repository, transport) = review_run(common::ReviewScript::ApiError, |_| {});
            let dir = repository.path();
            assert!(
                matches!(step, Step::Retry),
                "an early review failure stays recoverable: {step:?}"
            );
            assert_eq!(
                review_contexts(&transport).len(),
                1,
                "a permanent API refusal buys no second review request"
            );
            assert!(
                !dir.join("review.md").exists(),
                "a failed review writes no review artifact"
            );
            assert_eq!(
                summary_artifact(dir),
                common::SCRIPTED_SUMMARY,
                "the summary after the review failure is unchanged"
            );
            assert_eq!(
                head_commits(&repository),
                1,
                "a run with no passing review commits nothing"
            );
        }
        // The summary pass's answer is refused, and the four-candidate
        // allowance is exhausted by the repeated invalid answer.
        "summary-invalid" => {
            let (step, dir, transport) = scripted_run(
                QueueOutcome::Resolved,
                common::SummaryScript::Answer(REJECTED_SUMMARY),
            );
            assert!(matches!(step, Step::SummaryFailed { .. }), "{step:?}");
            assert_eq!(
                summary_requests(&transport).len(),
                4,
                "the rejected answer exhausts the four-candidate allowance inside the pass"
            );
            assert!(!dir.join("change-summary.md").exists());
            let _ = fs::remove_dir_all(&dir);
        }
        other => panic!("unknown phase-timing case {other}"),
    }
}

/// The change summary artifact when the run wrote one, or None when it did not.
fn summary_artifact_or_absent(dir: &Path) -> Option<String> {
    fs::read_to_string(dir.join("change-summary.md")).ok()
}

// -- Core Entry-Point Behaviors --------------------------------------------

/// Every invocation of every phase is reported once, with the exact labels and
/// in invocation order, and nothing else is timed: not the preflight, not the
/// artifact writes, not the queue's own persistence and report work, and not the
/// run as a whole.
#[test]
fn when_pipeline_runs_given_scripted_run_then_logs_each_phase_invocation() {
    let Some(transcript) = phase_timing_transcript(
        "when_pipeline_runs_given_scripted_run_then_logs_each_phase_invocation",
        "resolved",
    ) else {
        return;
    };

    assert_eq!(phase_labels(&transcript), PHASE_LABELS, "{transcript}");
    for (line, label) in elapsed_lines(&transcript)
        .iter()
        .zip(elapsed_labels(&transcript))
    {
        elapsed_seconds(line, &label);
    }
    for label in PHASE_LABELS {
        assert_eq!(
            labelled(&transcript, label).len(),
            1,
            "one line for {label}:\n{transcript}"
        );
    }

    // Tool durations are inline on the permanent call echoes now, a model
    // attempt reports a thinking line of its own, and the queue's one gate
    // reports a ticket-scoped line; no per-command line and no whole-pipeline
    // total joins them.
    let labels = elapsed_labels(&transcript);
    assert!(
        labels
            .iter()
            .all(|label| PHASE_LABELS.contains(&label.as_str())
                || label == "thinking"
                || is_gate_label(label)
                || is_overhead_label(label)),
        "only phase invocations, per-attempt thinking reports, gate timings and queue residuals are \
         timed:\n{transcript}"
    );
    assert_eq!(
        labelled(&transcript, "ticket-queue round 1 overhead").len(),
        1,
        "the queue round reports its residual once:\n{transcript}"
    );
    assert_eq!(
        phase_labels(&transcript).len(),
        PHASE_LABELS.len(),
        "each phase invocation is reported once and no command elapsed line exists:\n{transcript}"
    );
    for call in call_lines(&transcript) {
        assert!(
            call.contains(") $"),
            "an executed call keeps its inline duration: {call:?}"
        );
    }
}

/// The plan ask is timed per REPL input, never as one plan-session lifetime.
#[test]
fn when_plan_is_asked_given_each_repl_input_then_logs_each_ask() {
    let Some(transcript) = phase_timing_transcript(
        "when_plan_is_asked_given_each_repl_input_then_logs_each_ask",
        "two-asks",
    ) else {
        return;
    };

    let asks = labelled(&transcript, "plan-agent ask");
    assert_eq!(
        asks.len(),
        2,
        "one line per REPL input, not one for the session:\n{transcript}"
    );

    // The second line's ask is its own invocation, after every phase of the
    // first line: no single line covers both.
    let expected: Vec<&str> = PHASE_LABELS
        .iter()
        .copied()
        .chain(["plan-agent ask"])
        .collect();
    assert_eq!(phase_labels(&transcript), expected, "{transcript}");
}

/// Every repeated queue and review invocation is numbered one-based, and each
/// one gets its own line.
#[test]
fn when_pipeline_repeats_given_review_fix_rounds_then_numbers_each_queue_and_review_invocation() {
    let Some(transcript) = phase_timing_transcript(
        "when_pipeline_repeats_given_review_fix_rounds_then_numbers_each_queue_and_review_invocation",
        "fix-three-rounds",
    ) else {
        return;
    };

    let expected = [
        "plan-agent ask",
        "spec pass",
        "tickets pass",
        "triage pass",
        "ticket-queue round 1",
        "review round 1",
        "ticket-queue round 2",
        "review round 2",
        "ticket-queue round 3",
        "review round 3",
        "ticket-queue round 4",
        "review round 4",
        "summary pass",
    ];
    assert_eq!(phase_labels(&transcript), expected, "{transcript}");

    for round in 1..=4 {
        let queue = format!("ticket-queue round {round}");
        let review = format!("review round {round}");
        assert_eq!(
            labelled(&transcript, &queue).len(),
            1,
            "queue round {round} needs exactly one line:\n{transcript}"
        );
        assert_eq!(
            labelled(&transcript, &review).len(),
            1,
            "review round {round} needs exactly one line:\n{transcript}"
        );
    }
    assert_eq!(
        labelled(&transcript, "ticket-queue round 5").len(),
        0,
        "there is no fifth queue round:\n{transcript}"
    );
    assert_eq!(
        labelled(&transcript, "review round 5").len(),
        0,
        "there is no fifth review:\n{transcript}"
    );
}

/// A queue that drains incomplete and a queue that stops still report one line
/// for each phase they actually reached, and the caller's step is unchanged.
#[test]
fn when_phase_returns_given_failure_or_incomplete_outcome_then_logs_once_and_preserves_step() {
    const TEST: &str =
        "when_phase_returns_given_failure_or_incomplete_outcome_then_logs_once_and_preserves_step";
    let cases = ["incomplete-queue", "stopped-queue"];

    let transcripts = phase_timing_transcripts(TEST, &cases);
    if transcripts.is_empty() {
        return;
    }

    // The queue drained without resolving: every phase it reached is reported
    // once, the review it never reached has no line, and the child saw the
    // recoverable Retry step this run's outcome still is.
    let incomplete = &transcripts[0];
    let expected = [
        "plan-agent ask",
        "spec pass",
        "tickets pass",
        "triage pass",
        "ticket-queue round 1",
        "summary pass",
    ];
    assert_eq!(phase_labels(incomplete), expected, "{incomplete}");
    for label in expected {
        assert_eq!(
            labelled(incomplete, label).len(),
            1,
            "{label}:\n{incomplete}"
        );
    }
    assert_eq!(
        labelled(incomplete, "review round 1").len(),
        0,
        "a queue that never resolved reaches no review:\n{incomplete}"
    );

    // The queue stopped before its ticket started: its own round is still
    // reported once, and no code-agent tool ran at all.
    let stopped = &transcripts[1];
    assert_eq!(phase_labels(stopped), expected, "{stopped}");
    assert_eq!(
        phase_labels(stopped).len(),
        expected.len(),
        "a stopped queue runs no tool, so no tool line joins the phase lines:\n{stopped}"
    );
}

/// Edge-Case Tables -- Empty/Zero/Nil: a phase whose outcome carries an empty
/// value is still one invocation, so it is reported exactly once, and the later
/// rounds its empty outcome prevents have no line.
#[test]
fn when_phase_returns_given_empty_or_zero_valued_outcome_then_logs_one_elapsed_line() {
    let Some(transcript) = phase_timing_transcript(
        "when_phase_returns_given_empty_or_zero_valued_outcome_then_logs_one_elapsed_line",
        "empty-review",
    ) else {
        return;
    };

    // The review's empty outcome -- no findings and no fix tickets -- is one
    // invocation: exactly one review line, and no later round.
    assert_eq!(
        labelled(&transcript, "review round 1").len(),
        1,
        "{transcript}"
    );
    assert_eq!(
        labelled(&transcript, "review round 2").len(),
        0,
        "{transcript}"
    );
    assert_eq!(phase_labels(&transcript), PHASE_LABELS, "{transcript}");
    for label in PHASE_LABELS {
        assert_eq!(
            labelled(&transcript, label).len(),
            1,
            "an empty outcome still gets its line: {label}\n{transcript}"
        );
    }
}

/// Edge-Case Tables -- Minimum valid boundary: a phase that returns quickly
/// renders whole seconds and exactly three fractional digits, and a duration
/// below one millisecond renders as 0.000 rather than a shorter fraction.
#[test]
fn when_pipeline_phase_finishes_given_short_elapsed_time_then_uses_three_fractional_digits() {
    let Some(transcript) = phase_timing_transcript(
        "when_pipeline_phase_finishes_given_short_elapsed_time_then_uses_three_fractional_digits",
        "resolved",
    ) else {
        return;
    };

    let lines = elapsed_lines(&transcript);
    let labels = elapsed_labels(&transcript);
    assert!(!lines.is_empty(), "{transcript}");
    for (line, label) in lines.iter().zip(labels) {
        elapsed_seconds(line, &label);
    }

    for elapsed in [
        Duration::ZERO,
        Duration::from_micros(1),
        Duration::from_micros(999),
    ] {
        assert_eq!(
            ejaj::status::elapsed_line("plan-agent ask", elapsed),
            "[INFO] plan-agent ask elapsed: 0.000 seconds",
            "{elapsed:?} must floor to zero"
        );
    }
    assert_eq!(
        ejaj::status::elapsed_line("review round 1", Duration::from_micros(1_999)),
        "[INFO] review round 1 elapsed: 0.001 seconds",
        "the millisecond component floors rather than rounding up"
    );
}

/// Edge-Case Tables -- Maximum limit/Upper boundary: whole seconds are
/// unbounded, with no rollover into another unit and no cap. The production
/// transcript is the normal scripted run -- no provider is slowed down to force
/// a duration -- and long-duration formatting is pinned with fixed durations
/// through the real elapsed formatter.
#[test]
fn when_pipeline_phase_finishes_given_long_elapsed_time_then_preserves_unbounded_whole_seconds() {
    let Some(transcript) = phase_timing_transcript(
        "when_pipeline_phase_finishes_given_long_elapsed_time_then_preserves_unbounded_whole_seconds",
        "resolved",
    ) else {
        return;
    };

    // The production run reports its phases on the shared elapsed path: every
    // line keeps the established shape, short though each measured phase is.
    let lines = elapsed_lines(&transcript);
    let labels = elapsed_labels(&transcript);
    assert!(!lines.is_empty(), "{transcript}");
    for (line, label) in lines.iter().zip(labels) {
        elapsed_seconds(line, &label);
    }

    // Past a minute the seconds keep counting, and the unit never changes.
    assert_eq!(
        ejaj::status::elapsed_line("ticket-queue round 2", Duration::from_millis(61_500)),
        "[INFO] ticket-queue round 2 elapsed: 61.500 seconds"
    );
    assert_eq!(
        ejaj::status::elapsed_line("review round 3", Duration::from_millis(119_001)),
        "[INFO] review round 3 elapsed: 119.001 seconds"
    );
    // And there is no maximum: an hour is still seconds, and hours keep
    // counting as seconds with their millisecond component intact.
    assert_eq!(
        ejaj::status::elapsed_line("summary pass", Duration::from_secs(3_600)),
        "[INFO] summary pass elapsed: 3600.000 seconds"
    );
    assert_eq!(
        ejaj::status::elapsed_line(
            "summary pass",
            Duration::from_secs(12_345) + Duration::from_millis(678)
        ),
        "[INFO] summary pass elapsed: 12345.678 seconds"
    );
}

/// Removal check: the scenario-exclusive minute-long setup is gone from both
/// the pipeline tests and the shared scripted helpers, so no ordinary test can
/// wait on it again.
#[test]
fn when_pipeline_sources_are_checked_given_minute_long_scenario_removal_then_contains_no_exclusive_setup()
 {
    let pipeline =
        fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/pipeline.rs"))
            .expect("the pipeline test source is readable");
    let helpers =
        fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/common/mod.rs"))
            .expect("the shared scripted helpers are readable");

    // Each removed name is assembled from its pieces so this test's own source
    // never contains the setup it forbids.
    let removed = [
        ["slow", "-verification-gate"].concat(),
        [
            "when_queue_round_exceeds_one_minute_given_slow",
            "_verification_gate_then_preserves_unbounded_elapsed_format",
        ]
        .concat(),
        ["SLOW_", "PASSING_GATE"].concat(),
        ["Slow", "Passing"].concat(),
        ["Transient", "TwiceThenComplete"].concat(),
        ["sleep", " 61"].concat(),
    ];
    for (name, source) in [
        ("tests/pipeline.rs", &pipeline),
        ("tests/common/mod.rs", &helpers),
    ] {
        for needle in &removed {
            assert!(
                !source.contains(needle),
                "{name} still carries the removed minute-long scenario element `{needle}`"
            );
        }
    }
}

/// Edge-Case Tables -- Malformed/Invalid payload: a phase refused at the API and
/// a phase refused in the transport still earn exactly one line each, and the
/// outcome the caller receives is unchanged.
#[test]
fn when_phase_returns_given_api_or_transport_failure_then_logs_once_without_changing_outcome() {
    const TEST: &str =
        "when_phase_returns_given_api_or_transport_failure_then_logs_once_without_changing_outcome";
    let cases = ["triage-api-failure", "summary-transport-failure"];

    let transcripts = phase_timing_transcripts(TEST, &cases);
    if transcripts.is_empty() {
        return;
    }

    // The triage pass is refused: it is reported once, and the queue, review and
    // summary the failure prevented have no line at all.
    let triage = &transcripts[0];
    let expected = ["plan-agent ask", "spec pass", "tickets pass", "triage pass"];
    assert_eq!(phase_labels(triage), expected, "{triage}");
    assert_eq!(labelled(triage, "triage pass").len(), 1, "{triage}");
    for absent in ["ticket-queue round 1", "review round 1", "summary pass"] {
        assert_eq!(
            labelled(triage, absent).len(),
            0,
            "{absent} was never invoked:\n{triage}"
        );
    }

    // The summary pass fails in the transport after every earlier phase ran:
    // still exactly one summary line, and the fatal step the child saw.
    let summary = &transcripts[1];
    assert_eq!(phase_labels(summary), PHASE_LABELS, "{summary}");
    assert_eq!(labelled(summary, "summary pass").len(), 1, "{summary}");
}

/// Edge-Case Tables -- Malformed/Invalid payload: a review pass refused at the
/// API is still one invocation, so it is reported exactly once under its own
/// one-based label, and the recoverable `Step` the caller receives is
/// unchanged. The child saw the review failure, and the phases around it.
#[test]
fn when_review_returns_failure_given_early_api_error_then_logs_once_and_preserves_retry() {
    let Some(transcript) = phase_timing_transcript(
        "when_review_returns_failure_given_early_api_error_then_logs_once_and_preserves_retry",
        "review-api-failure",
    ) else {
        return;
    };

    let line = labelled(&transcript, "review round 1");
    assert_eq!(
        line.len(),
        1,
        "the failed review is one invocation and one line:\n{transcript}"
    );
    elapsed_seconds(&line[0], "review round 1");

    assert_eq!(
        labelled(&transcript, "review round 2").len(),
        0,
        "a failed review starts no later round:\n{transcript}"
    );

    // Every phase the run reached is still reported once -- the failed review
    // and the summary behind it included: the failure changes which outcome the
    // caller gets, never which lines are printed.
    assert_eq!(phase_labels(&transcript), PHASE_LABELS, "{transcript}");
    for label in PHASE_LABELS {
        assert_eq!(
            labelled(&transcript, label).len(),
            1,
            "one line for {label}:\n{transcript}"
        );
    }
    assert!(
        transcript.contains("[WARN] Review agent ended early (API error)"),
        "the existing review-failure warning must survive:\n{transcript}"
    );
}

/// Edge-Case Tables -- Malformed/Invalid payload: a summary answer the format
/// refuses, and the four-candidate allowance it exhausts inside the same pass,
/// are still one invocation, and the fatal failure is preserved.
#[test]
fn when_summary_pass_fails_given_invalid_answer_then_logs_once_and_preserves_failure() {
    let Some(transcript) = phase_timing_transcript(
        "when_summary_pass_fails_given_invalid_answer_then_logs_once_and_preserves_failure",
        "summary-invalid",
    ) else {
        return;
    };

    assert_eq!(
        labelled(&transcript, "summary pass").len(),
        1,
        "{transcript}"
    );
    assert_eq!(phase_labels(&transcript), PHASE_LABELS, "{transcript}");
}

/// Edge-Case Tables -- Boundary unauthorized/Forbidden access: the queue's own
/// verification gate keeps its ticket-scoped elapsed line, while the agent's
/// command duration stays inline on the call echo -- neither becomes a separate
/// `bash` tool timing line.
#[test]
fn when_queue_gate_runs_given_verification_command_then_emits_no_separate_tool_timing_line() {
    let Some(transcript) = phase_timing_transcript(
        "when_queue_gate_runs_given_verification_command_then_emits_no_separate_tool_timing_line",
        "resolved",
    ) else {
        return;
    };

    // The gate really ran: the child saw the ticket completed, which only a gate
    // that exited 0 records.
    let calls = call_lines(&transcript);
    assert_eq!(
        calls.len(),
        1,
        "the code agent made its one call:\n{transcript}"
    );
    assert!(
        calls[0].starts_with("[1/100] (") && calls[0].ends_with("$ echo ejaj-e2e-ok"),
        "the agent's own call echo carries the inline timing:\n{transcript}"
    );
    assert!(
        labelled(&transcript, "bash").is_empty(),
        "a separate tool elapsed line would break this count:\n{transcript}"
    );
    assert_eq!(
        labelled(&transcript, &gate_label(GATE_TICKET_ID)).len(),
        1,
        "the gate keeps its own ticket-scoped line:\n{transcript}"
    );
}

// -- Verification-gate timing ----------------------------------------------

/// Core entry-point behavior: the production verification gate succeeds, and
/// the run reports exactly one standard elapsed line naming the ticket's
/// trimmed id.
#[test]
fn when_ticket_gate_finishes_given_success_then_reports_elapsed_time() {
    let Some(transcript) = phase_timing_transcript(
        "when_ticket_gate_finishes_given_success_then_reports_elapsed_time",
        "resolved",
    ) else {
        return;
    };

    let label = gate_label(GATE_TICKET_ID);
    let lines = labelled(&transcript, &label);
    assert_eq!(
        lines.len(),
        1,
        "one successful gate invocation is reported once:\n{transcript}"
    );
    // Minimum valid boundary: whole seconds plus exactly three floored
    // millisecond digits, one line for the one invocation.
    elapsed_seconds(&lines[0], &label);
    assert_eq!(gate_lines(&transcript), lines, "{transcript}");
}

/// Edge-Case Tables -- Empty/Zero/Nil: a status write that stops the queue
/// before any ticket work starts leaves no gate invocation, so no gate line is
/// emitted even though the queue round itself is reported.
#[test]
fn when_queue_stops_given_status_persistence_failure_then_emits_no_gate_duration() {
    let Some(transcript) = phase_timing_transcript(
        "when_queue_stops_given_status_persistence_failure_then_emits_no_gate_duration",
        "stopped-queue",
    ) else {
        return;
    };

    assert!(
        gate_lines(&transcript).is_empty(),
        "persistence stopped the queue before the gate:\n{transcript}"
    );
    assert_eq!(
        labelled(&transcript, "ticket-queue round 1").len(),
        1,
        "the queue invocation that stopped is still reported:\n{transcript}"
    );
}

/// Edge-Case Tables -- Malformed/Invalid payload: a real command failure in the
/// gate still reports its elapsed time and keeps the existing diagnostic and
/// failed ticket outcome.
#[test]
fn when_ticket_gate_finishes_given_command_error_then_reports_elapsed_and_preserves_failure() {
    let Some(transcript) = phase_timing_transcript(
        "when_ticket_gate_finishes_given_command_error_then_reports_elapsed_and_preserves_failure",
        "incomplete-queue",
    ) else {
        return;
    };

    let label = gate_label(GATE_TICKET_ID);
    let lines = labelled(&transcript, &label);
    assert_eq!(
        lines.len(),
        2,
        "the failed first gate and the authoritative final gate are each reported once:\n{transcript}"
    );
    for line in &lines {
        elapsed_seconds(line, &label);
    }
    assert!(
        transcript.contains("[ERROR: exit code 1 for `exit 1`]"),
        "the gate's own diagnostic must survive:\n{transcript}"
    );
    // The child asserted the recoverable Step::Retry, the failed snapshot and
    // the gate-failed report entry.
}

/// Contract & Invariant Checks: no gate line is emitted when the code agent
/// fails before verification is reached, and the existing agent-failure path is
/// unchanged.
#[test]
fn when_code_agent_fails_before_gate_given_ticket_work_then_emits_no_gate_duration() {
    let Some(transcript) = phase_timing_transcript(
        "when_code_agent_fails_before_gate_given_ticket_work_then_emits_no_gate_duration",
        "code-api-failure",
    ) else {
        return;
    };

    assert!(
        gate_lines(&transcript).is_empty(),
        "no gate invocation means no gate line:\n{transcript}"
    );
    assert!(
        transcript.contains("agent failure: API error"),
        "the agent failure still reaches the existing path:\n{transcript}"
    );
    assert_eq!(
        labelled(&transcript, "ticket-queue round 1").len(),
        1,
        "{transcript}"
    );
}

/// Contract & Invariant Checks: a failing gate's elapsed line covers the real
/// command interval and follows the ticket work, and the failed queue result is
/// unchanged.
#[test]
fn when_ticket_gate_finishes_given_error_then_reports_elapsed_time() {
    let Some(transcript) = phase_timing_transcript(
        "when_ticket_gate_finishes_given_error_then_reports_elapsed_time",
        "slow-gate-error",
    ) else {
        return;
    };

    let label = gate_label(GATE_TICKET_ID);
    let lines = labelled(&transcript, &label);
    assert_eq!(
        lines.len(),
        2,
        "both real gate invocations are reported:\n{transcript}"
    );
    for line in &lines {
        let elapsed = elapsed_duration(line, &label);
        assert!(
            elapsed >= Duration::from_millis(100),
            "the reported interval is the real command interval: {elapsed:?}\n{transcript}"
        );
    }

    let call_at = transcript
        .find("$ echo ejaj-e2e-ok")
        .unwrap_or_else(|| panic!("the ticket work must precede the gate:\n{transcript}"));
    let gate_at = transcript
        .find(&format!("[INFO] {label} elapsed: "))
        .unwrap_or_else(|| panic!("the gate elapsed line is missing:\n{transcript}"));
    assert!(
        gate_at > call_at,
        "the gate line follows the measured ticket work:\n{transcript}"
    );
    assert_eq!(
        labelled(&transcript, "review round 1").len(),
        0,
        "a failed queue still starts no review:\n{transcript}"
    );
    // The child asserted Step::Retry and the failed status snapshot.
}

// -- Regression Tests -------------------------------------------------------

/// Regression: a failing gate keeps its own diagnostic, the failed ticket
/// ending, the recoverable queue outcome, the status snapshot and the report --
/// the timing line changes none of them.
#[test]
fn when_gate_fails_given_existing_ticket_outcome_then_preserves_diagnostic_and_report() {
    let Some(transcript) = phase_timing_transcript(
        "when_gate_fails_given_existing_ticket_outcome_then_preserves_diagnostic_and_report",
        "slow-gate-error",
    ) else {
        return;
    };

    assert_eq!(
        gate_lines(&transcript).len(),
        2,
        "the first gate and the final gate each report one line:\n{transcript}"
    );
    assert!(
        transcript.contains("[ERROR: exit code 1 for `sleep 0.1; exit 1`]"),
        "the existing gate diagnostic is intact:\n{transcript}"
    );
    assert!(
        transcript.contains("[INFO] queue incomplete:"),
        "the existing incomplete-queue summary is intact:\n{transcript}"
    );
    // The child asserted the failed snapshot, the gate-failed report ending and
    // the unchanged summary artifact.
}

// -- Contract & Invariant Checks -------------------------------------------

/// The exact labels and the one-based round numbers, over a run with one
/// review-generated fix queue: no round is ever zero, no invocation is skipped,
/// and every line keeps the established shape.
#[test]
fn when_pipeline_runs_given_scripted_transport_then_uses_exact_phase_labels_and_one_based_rounds() {
    let Some(transcript) = phase_timing_transcript(
        "when_pipeline_runs_given_scripted_transport_then_uses_exact_phase_labels_and_one_based_rounds",
        "fix-once-then-pass",
    ) else {
        return;
    };

    let expected = [
        "plan-agent ask",
        "spec pass",
        "tickets pass",
        "triage pass",
        "ticket-queue round 1",
        "review round 1",
        "ticket-queue round 2",
        "review round 2",
        "summary pass",
    ];
    assert_eq!(phase_labels(&transcript), expected, "{transcript}");

    let lines = elapsed_lines(&transcript);
    let labels = elapsed_labels(&transcript);
    for (line, label) in lines.iter().zip(labels.iter()) {
        elapsed_seconds(line, label);
    }
    for label in expected {
        assert!(
            labels.iter().any(|found| found == label),
            "the run must report {label}:\n{transcript}"
        );
    }
    assert!(
        !labels.iter().any(|label| label.contains("round 0")),
        "rounds are one-based:\n{transcript}"
    );
}

/// An early return leaves the phases it never reached without a line: the plan
/// ask, the three unattended passes and nothing else.
#[test]
fn when_phase_is_not_invoked_given_early_pipeline_return_then_emits_no_phase_line() {
    let Some(transcript) = phase_timing_transcript(
        "when_phase_is_not_invoked_given_early_pipeline_return_then_emits_no_phase_line",
        "triage-turn-end",
    ) else {
        return;
    };

    let expected = ["plan-agent ask", "spec pass", "tickets pass", "triage pass"];
    assert_eq!(phase_labels(&transcript), expected, "{transcript}");
    for absent in ["ticket-queue round 1", "review round 1", "summary pass"] {
        assert_eq!(
            labelled(&transcript, absent).len(),
            0,
            "{absent} was never invoked:\n{transcript}"
        );
    }
}

/// Edge-Case Tables -- Empty/Zero/Nil: the gate that refuses a run before any
/// phase is the first operation of the invocation, so a preflight failure leaves
/// no phase line at all. The child saw the fatal preflight stage.
#[test]
fn when_pipeline_stops_before_a_phase_given_preflight_failure_then_emits_no_line_for_that_phase() {
    let Some(transcript) = phase_timing_transcript(
        "when_pipeline_stops_before_a_phase_given_preflight_failure_then_emits_no_line_for_that_phase",
        "preflight-failure",
    ) else {
        return;
    };

    assert!(
        elapsed_lines(&transcript).is_empty(),
        "a gated run invoked no phase:\n{transcript}"
    );
}

// -- Regression Tests -------------------------------------------------------

/// The three ways a scripted queue ends keep their steps and their artifacts:
/// only a resolved queue with a passing review reaches the review artifact, and
/// every one of them is summarised.
#[test]
fn when_pipeline_runs_given_resolved_incomplete_and_stopped_queues_then_preserves_existing_steps() {
    let (resolved, dir, _transport) =
        scripted_run(QueueOutcome::Resolved, common::SummaryScript::Summary);
    assert!(matches!(resolved, Step::Complete), "{resolved:?}");
    assert!(dir.join("review.md").is_file(), "the review phase ran");
    assert_eq!(summary_artifact(&dir), common::SCRIPTED_SUMMARY);
    let _ = fs::remove_dir_all(&dir);

    let (incomplete, dir, _transport) =
        scripted_run(QueueOutcome::Incomplete, common::SummaryScript::Summary);
    assert!(matches!(incomplete, Step::Retry), "{incomplete:?}");
    assert!(
        !dir.join("review.md").exists(),
        "the review phase never ran"
    );
    assert_eq!(summary_artifact(&dir), common::SCRIPTED_SUMMARY);
    let _ = fs::remove_dir_all(&dir);

    let (stopped, dir, _transport) =
        scripted_run(QueueOutcome::Stopped, common::SummaryScript::Summary);
    assert!(matches!(stopped, Step::Retry), "{stopped:?}");
    assert!(!dir.join("review.md").exists());
    assert_eq!(summary_artifact(&dir), common::SCRIPTED_SUMMARY);
    let _ = fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Queue-round accounting: the total first, the residual second
// ---------------------------------------------------------------------------

/// Core Entry-Point Behavior and Contract & Invariant: one production queue
/// round reports its one-based total first and its residual second, the
/// residual is explained by the model attempts, executed command and gate
/// measured inside that same invocation, and the queue's result and artifacts
/// are unchanged.
#[test]
fn when_queue_round_finishes_given_measured_contributors_then_reports_total_and_residual() {
    let Some(transcript) = phase_timing_transcript(
        "when_queue_round_finishes_given_measured_contributors_then_reports_total_and_residual",
        "resolved",
    ) else {
        return;
    };

    let rounds = queue_round_timings(&transcript);
    assert_eq!(
        rounds.len(),
        1,
        "one queue invocation, one round:\n{transcript}"
    );
    let timing = &rounds[0];
    assert_eq!(timing.round, 1, "queue rounds are one-based:\n{transcript}");
    assert!(
        !timing.model_attempts.is_empty(),
        "the ticket's model attempts must be measured:\n{transcript}"
    );
    assert_eq!(
        timing.executed_tools.len(),
        1,
        "the one executed command is a contributor:\n{transcript}"
    );
    assert_eq!(
        timing.gates.len(),
        1,
        "the one verification gate is a contributor:\n{transcript}"
    );
    assert_queue_round_accounting(timing, &transcript);
    assert!(
        timing.overhead() <= timing.total,
        "the residual is nonnegative:\n{transcript}"
    );

    let total = labelled(&transcript, "ticket-queue round 1");
    let overhead = labelled(&transcript, "ticket-queue round 1 overhead");
    assert_eq!(total.len(), 1, "{transcript}");
    assert_eq!(overhead.len(), 1, "{transcript}");
    let total_at = transcript
        .find(&total[0])
        .unwrap_or_else(|| panic!("the total line is missing:\n{transcript}"));
    let overhead_at = transcript
        .find(&overhead[0])
        .unwrap_or_else(|| panic!("the residual line is missing:\n{transcript}"));
    assert!(
        total_at < overhead_at,
        "the total precedes the residual:\n{transcript}"
    );

    // The queue result and artifacts are unchanged: the child saw Step::Complete,
    // a completed status snapshot, a completed report and the canonical summary.
    assert!(
        transcript.contains("[INFO] all 1 tickets resolved"),
        "the existing queue result must survive:\n{transcript}"
    );
    assert!(
        transcript.contains("[INFO] queue report written to"),
        "the existing report artifact must survive:\n{transcript}"
    );
}

/// Edge-Case Table -- Empty/Zero/Nil: a status write that stops the queue before
/// any ticket work starts leaves no measured contributor, so the residual is
/// exactly the queue total and the existing stopped outcome is preserved.
#[test]
fn when_queue_round_has_no_measured_contributors_given_persistence_stops_before_work_then_overhead_equals_total()
 {
    let Some(transcript) = phase_timing_transcript(
        "when_queue_round_has_no_measured_contributors_given_persistence_stops_before_work_then_overhead_equals_total",
        "stopped-queue",
    ) else {
        return;
    };

    let rounds = queue_round_timings(&transcript);
    assert_eq!(
        rounds.len(),
        1,
        "the stopped queue invocation is still one round:\n{transcript}"
    );
    let timing = &rounds[0];
    assert_eq!(timing.round, 1, "{transcript}");
    assert!(
        timing.model_attempts.is_empty(),
        "no model attempt ran before the stop:\n{transcript}"
    );
    assert!(
        timing.executed_tools.is_empty(),
        "no command ran before the stop:\n{transcript}"
    );
    assert!(
        timing.gates.is_empty(),
        "no gate ran before the stop:\n{transcript}"
    );
    assert_eq!(
        timing.overhead(),
        timing.total,
        "with zero contributors the residual equals the whole queue round:\n{transcript}"
    );
    assert_queue_round_accounting(timing, &transcript);

    assert_eq!(
        labelled(&transcript, "ticket-queue round 1 overhead").len(),
        1,
        "{transcript}"
    );
}

/// Edge-Case Table -- Minimum valid boundary: one ticket's real model attempts,
/// one executed command and one gate are each represented once, and the total
/// is an aggregate rather than a component of its own residual.
#[test]
fn when_queue_round_finishes_given_one_ticket_and_gate_then_reports_each_contributor() {
    let Some(transcript) = phase_timing_transcript(
        "when_queue_round_finishes_given_one_ticket_and_gate_then_reports_each_contributor",
        "resolved",
    ) else {
        return;
    };

    let rounds = queue_round_timings(&transcript);
    assert_eq!(rounds.len(), 1, "{transcript}");
    let timing = &rounds[0];

    assert!(
        !timing.model_attempts.is_empty(),
        "at least one real model attempt is represented:\n{transcript}"
    );
    assert_eq!(
        timing.executed_tools.len(),
        1,
        "the executed command is represented once:\n{transcript}"
    );
    assert_eq!(
        timing.gates.len(),
        1,
        "the gate is represented once:\n{transcript}"
    );
    assert_queue_round_accounting(timing, &transcript);

    // The contributor lines inside the queue invocation are exactly the parsed
    // contributors: the aggregate is reported once as the total and is never
    // one of the components.
    let queue_section = transcript
        .split("=== Ticket queue ===")
        .nth(1)
        .and_then(|rest| rest.split("[INFO] ticket-queue round 1 elapsed:").next())
        .unwrap_or_else(|| panic!("the queue invocation is missing:\n{transcript}"));
    let contributor_lines = elapsed_labels(queue_section)
        .into_iter()
        .filter(|label| label == "thinking" || is_gate_label(label))
        .count()
        + queue_section
            .lines()
            .filter(|line| thinking_line_elapsed(line).is_some())
            .count()
        + call_lines(queue_section).len();
    assert_eq!(
        contributor_lines,
        timing.contributor_count(),
        "each contributor must be reported once and the total must not be a component:\n{transcript}"
    );
    assert_eq!(
        elapsed_labels(&transcript)
            .into_iter()
            .filter(|label| queue_total_round(label).is_some())
            .count(),
        1,
        "the aggregate is reported once:\n{transcript}"
    );
}

/// Edge-Case Table -- Maximum limit/Upper boundary: repeated review-generated
/// queue rounds keep the existing one-based numbering and each round's residual
/// is explained only by that round's own contributors.
#[test]
fn when_queue_rounds_repeat_given_review_fix_tickets_then_numbers_each_round_without_timing_leaks()
{
    let Some(transcript) = phase_timing_transcript(
        "when_queue_rounds_repeat_given_review_fix_tickets_then_numbers_each_round_without_timing_leaks",
        "fix-three-rounds-retry",
    ) else {
        return;
    };

    let rounds = queue_round_timings(&transcript);
    let numbers: Vec<usize> = rounds.iter().map(|timing| timing.round).collect();
    assert_eq!(
        numbers,
        [1, 2, 3, 4],
        "every queue invocation keeps its existing one-based number:\n{transcript}"
    );

    let mut previous_overhead_at = 0;
    for timing in &rounds {
        assert_eq!(
            timing.model_attempts.len(),
            3,
            "round {} must account only its own model attempts, the failed              transport attempt included:\n{transcript}",
            timing.round
        );
        assert_eq!(
            timing.executed_tools.len(),
            1,
            "round {} must account only its own executed command:\n{transcript}",
            timing.round
        );
        assert_eq!(
            timing.gates.len(),
            1,
            "round {} must account only its own gate:\n{transcript}",
            timing.round
        );
        assert!(
            timing.overhead() >= Duration::from_millis(900),
            "round {} must carry its own one-second retry backoff in the residual; a carried-forward              contributor would collapse it:\n{transcript}",
            timing.round
        );
        assert_queue_round_accounting(timing, &transcript);

        let total = labelled(&transcript, &format!("ticket-queue round {}", timing.round));
        let overhead = labelled(
            &transcript,
            &format!("ticket-queue round {} overhead", timing.round),
        );
        assert_eq!(
            total.len(),
            1,
            "round {} total:\n{transcript}",
            timing.round
        );
        assert_eq!(
            overhead.len(),
            1,
            "round {} residual:\n{transcript}",
            timing.round
        );
        let total_at = transcript
            .find(&total[0])
            .unwrap_or_else(|| panic!("round {} total is missing:\n{transcript}", timing.round));
        let overhead_at = transcript
            .find(&overhead[0])
            .unwrap_or_else(|| panic!("round {} residual is missing:\n{transcript}", timing.round));
        assert!(
            total_at < overhead_at,
            "round {} reports its total first:\n{transcript}",
            timing.round
        );
        assert!(
            overhead_at > previous_overhead_at,
            "round {} follows the previous round rather than sharing its accounting:\n{transcript}",
            timing.round
        );
        previous_overhead_at = overhead_at;
    }

    let labels = elapsed_labels(&transcript);
    assert!(
        !labels
            .iter()
            .any(|label| label.contains("round 0") || label.contains("round 5")),
        "the existing round numbering is unchanged:\n{transcript}"
    );
}

/// Edge-Case Table -- Malformed/Invalid payload: a real failing gate is measured
/// into the round's residual, and the existing failed queue outcome and
/// diagnostic are preserved.
#[test]
fn when_queue_gate_fails_given_ticket_work_then_reports_gate_and_preserves_queue_outcome() {
    let Some(transcript) = phase_timing_transcript(
        "when_queue_gate_fails_given_ticket_work_then_reports_gate_and_preserves_queue_outcome",
        "slow-gate-error",
    ) else {
        return;
    };

    let rounds = queue_round_timings(&transcript);
    assert_eq!(rounds.len(), 1, "{transcript}");
    let timing = &rounds[0];
    assert!(
        !timing.model_attempts.is_empty(),
        "the ticket work is measured:\n{transcript}"
    );
    assert_eq!(
        timing.executed_tools.len(),
        2,
        "one executed command per attempt is measured:\n{transcript}"
    );
    assert_eq!(
        timing.gates.len(),
        2,
        "the failed first gate and the final gate are both measured:\n{transcript}"
    );
    assert!(
        timing
            .gates
            .iter()
            .all(|gate| *gate >= Duration::from_millis(100)),
        "each gate's real interval reaches the accounting:\n{transcript}"
    );
    assert_queue_round_accounting(timing, &transcript);

    assert!(
        transcript.contains("[ERROR: exit code 1 for `sleep 0.1; exit 1`]"),
        "the existing gate diagnostic must survive:\n{transcript}"
    );
    assert!(
        transcript.contains("[INFO] queue incomplete:"),
        "the existing failed queue outcome must survive:\n{transcript}"
    );
    assert_eq!(
        labelled(&transcript, "review round 1").len(),
        0,
        "a failed queue still starts no review:\n{transcript}"
    );
}

/// Regression: bash and execute_script keep their duration inline on the
/// permanent call line and produce no separate timing line, while the executed
/// call is counted once as a contributor; a rejected call would carry no
/// duration and add nothing.
#[test]
fn when_executed_command_finishes_given_queue_round_then_keeps_inline_duration_and_result() {
    let Some(transcript) = phase_timing_transcript(
        "when_executed_command_finishes_given_queue_round_then_keeps_inline_duration_and_result",
        "resolved",
    ) else {
        return;
    };

    let calls = call_lines(&transcript);
    assert_eq!(
        calls.len(),
        1,
        "the code agent made its one executed call:\n{transcript}"
    );
    assert!(
        calls[0].starts_with("[1/100] (") && calls[0].ends_with("$ echo ejaj-e2e-ok"),
        "the call line keeps its inline duration and result display:\n{transcript}"
    );
    assert!(
        calls[0].contains(") $"),
        "the executed call keeps its inline duration:\n{transcript}"
    );
    assert!(
        !call_line_elapsed(&calls[0]).is_none(),
        "the inline duration must stay parseable:\n{transcript}"
    );
    assert!(
        labelled(&transcript, "bash").is_empty()
            && labelled(&transcript, "execute_script").is_empty(),
        "a separate tool timing line is forbidden:\n{transcript}"
    );

    let rounds = queue_round_timings(&transcript);
    assert_eq!(rounds.len(), 1, "{transcript}");
    let timing = &rounds[0];
    assert_eq!(
        timing.executed_tools,
        vec![call_line_elapsed(&calls[0]).expect("the call line carries its inline duration")],
        "the inline duration is the executed command contribution, counted once:\n{transcript}"
    );
    assert_queue_round_accounting(timing, &transcript);
}

/// Regression: the two informational timing lines change neither the queue
/// result nor the queue artifacts, and no other queue line is added, replaced or
/// reordered.
#[test]
fn when_queue_round_finishes_given_timing_reports_then_preserves_artifacts_and_outcome() {
    let Some(transcript) = phase_timing_transcript(
        "when_queue_round_finishes_given_timing_reports_then_preserves_artifacts_and_outcome",
        "resolved",
    ) else {
        return;
    };

    let queue_labels: Vec<String> = elapsed_labels(&transcript)
        .into_iter()
        .filter(|label| label.starts_with("ticket-queue round "))
        .collect();
    assert_eq!(
        queue_labels,
        ["ticket-queue round 1", "ticket-queue round 1 overhead"],
        "the total is followed by the residual and nothing else is timed for the round:\n{transcript}"
    );

    // The child asserted Step::Complete, the completed status snapshot, the
    // completed report and the canonical summary. The transcript keeps every
    // existing queue/review/summary line around the two new informational ones.
    for existing in [
        "=== Ticket queue ===",
        "[INFO] ticket 1-verify-command-output:",
        "[INFO] all 1 tickets resolved",
        "[INFO] queue report written to",
        "=== Review agent ===",
        "[INFO] review round 1 passed",
        "[INFO] change summary written to",
    ] {
        assert!(
            transcript.contains(existing),
            "the existing output {existing:?} must survive:\n{transcript}"
        );
    }
}

// ---------------------------------------------------------------------------
// Finalization failure boundaries: every failed required Git operation is fatal
//
// The success-only gate already forbids finalization for a non-success step.
// These tests close the other direction: once finalization starts, each
// required Git operation either succeeds or the run stops there with a staged
// fatal failure, leaving recovery state -- refs, commits, stash entries, the
// index and the worktree -- exactly as the failure left it. Every Git process
// is real, against the run's own disposable repository and its real bare
// origin; no Git or pipeline component is mocked.
// ---------------------------------------------------------------------------

/// Core entry-point behavior: a publication the remote rejects is a fatal push
/// failure, never completion. The local summary branch and its one commit
/// survive, the remote keeps exactly the refs it had, and the summary artifact
/// stays written.
#[test]
fn when_finalization_fails_given_a_rejected_push_then_keeps_the_local_commit() {
    let repository = default_branch_repository();
    repository.reject_pushes("the bare origin refuses every push");
    let before = default_tip(&repository);
    let remote_before = remote_refs(&repository);
    let artifacts = repository.path().join(".ejaj/rejected-push");

    let (config, _transport) = common::scripted_pipeline_with_command(
        common::GateScript::Passing,
        common::SummaryScript::Summary,
        change_tracked_file(&repository),
    );
    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), &artifacts);

    let (stage, reason) = git_failure(&step);
    assert_eq!(
        stage, "push",
        "the rejected publication is fatal at the push operation: {reason}"
    );
    assert!(
        reason.contains("the bare origin refuses every push"),
        "the remote's own diagnostic is preserved: {reason}"
    );

    assert_eq!(
        repository.current_branch(),
        common::SUMMARY_BRANCH,
        "the local summary branch remains checked out"
    );
    assert_eq!(
        repository
            .git(&["rev-list", "--count", &format!("{before}..HEAD")])
            .trim(),
        "1",
        "the local branch keeps its one run commit"
    );
    assert_eq!(
        head_message(&repository),
        common::SCRIPTED_SUMMARY,
        "the retained commit is the run's summary commit"
    );
    assert_eq!(
        default_tip(&repository),
        before,
        "the default branch is not advanced by the failed publication"
    );
    assert_eq!(
        remote_refs(&repository),
        remote_before,
        "a rejected push moves no remote-tracking ref"
    );
    assert!(
        repository
            .origin_ref(&format!("refs/heads/{}", common::SUMMARY_BRANCH))
            .is_none(),
        "the rejected branch never reaches the remote"
    );

    let artifact = artifacts.join("change-summary.md");
    assert!(
        artifact.is_file(),
        "the written summary artifact is not removed when publication fails"
    );
    assert_eq!(
        fs::read_to_string(artifact).unwrap(),
        common::SCRIPTED_SUMMARY,
        "the preserved artifact is the canonical summary"
    );
}

/// Forbidden boundary: the remote denies publication outright. The run never
/// reports completion, keeps the local commit that was already made, and leaves
/// the remote without the summary branch.
#[test]
fn when_finalizing_given_push_permission_is_denied_then_never_reports_completion() {
    let repository = default_branch_repository();
    repository.reject_pushes("push permission denied by the remote");
    let before = default_tip(&repository);
    let artifacts = repository.path().join(".ejaj/permission-denied");

    let (config, _transport) = common::scripted_pipeline_with_command(
        common::GateScript::Passing,
        common::SummaryScript::Summary,
        change_tracked_file(&repository),
    );
    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), &artifacts);

    assert!(
        !matches!(step, Step::Complete),
        "a denied publication must never report completion: {step:?}"
    );
    let (stage, reason) = git_failure(&step);
    assert_eq!(stage, "push", "the denial is a push failure: {reason}");
    assert!(
        reason.contains("push permission denied"),
        "the remote's denial is preserved: {reason}"
    );
    assert_eq!(
        repository.current_branch(),
        common::SUMMARY_BRANCH,
        "the local branch survives the denied publication"
    );
    assert_eq!(
        repository
            .git(&["rev-list", "--count", &format!("{before}..HEAD")])
            .trim(),
        "1",
        "the local commit survives the denied publication"
    );
    assert!(
        repository
            .origin_ref(&format!("refs/heads/{}", common::SUMMARY_BRANCH))
            .is_none(),
        "no remote branch is published when permission is denied"
    );
    assert!(
        artifacts.join("change-summary.md").is_file(),
        "the summary artifact remains written"
    );
}

/// Edge-case table: a real failure at each required finalization operation
/// stops the sequence at that stage. No later branch, commit, stash pop or push
/// is attempted, and the repository keeps the state the failure left behind.
#[test]
fn when_finalizing_given_git_operation_failure_then_preserves_existing_stage_and_diagnostic() {
    // A locked index makes the untracked-including stash fail before any branch
    // is switched or created.
    {
        let repository = default_branch_repository();
        repository.lock_index();
        let before_head = repository.git_ref("HEAD");
        let before_tip = default_tip(&repository);
        let remote_before = remote_refs(&repository);
        let artifacts = repository.path().join(".ejaj/stash-failure");
        let (config, _transport) = common::scripted_pipeline_with_command(
            common::GateScript::Passing,
            common::SummaryScript::Summary,
            change_tracked_file(&repository),
        );
        let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
        let step = pipeline.ask(&common::harmless_plan_request(), &artifacts);

        let (stage, reason) = git_failure(&step);
        assert_eq!(stage, "stash", "the locked index fails the stash: {reason}");
        let _ = fs::remove_file(repository.path().join(".git/index.lock"));
        assert_eq!(repository.git_ref("HEAD"), before_head);
        assert_eq!(default_tip(&repository), before_tip);
        assert_eq!(remote_refs(&repository), remote_before);
        assert_eq!(repository.stash_count(), 0, "no stash entry is created");
        assert!(
            repository
                .git_ref(&format!("refs/heads/{}", common::SUMMARY_BRANCH))
                .is_none(),
            "no branch creation follows a failed stash"
        );
        assert!(
            repository
                .origin_ref(&format!("refs/heads/{}", common::SUMMARY_BRANCH))
                .is_none(),
            "no push follows a failed stash"
        );
        assert_eq!(repository.staged(), "", "no later staging runs");
        assert!(artifacts.join("change-summary.md").is_file());
    }

    // A locked HEAD makes the switch to the default branch fail before the pull
    // or any branch creation.
    {
        let repository = default_branch_repository();
        repository.lock_head();
        let before_head = repository.git_ref("HEAD");
        let before_tip = default_tip(&repository);
        let remote_before = remote_refs(&repository);
        let artifacts = scratch_dir();
        fs::create_dir_all(&artifacts).expect("the external artifact directory is creatable");
        let (config, _transport) = common::scripted_pipeline_with(
            common::GateScript::Passing,
            common::SummaryScript::Summary,
        );
        let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
        let step = pipeline.ask(&common::harmless_plan_request(), &artifacts);

        let (stage, reason) = git_failure(&step);
        assert_eq!(
            stage, "switch",
            "the locked HEAD fails the switch: {reason}"
        );
        let _ = fs::remove_file(repository.path().join(".git/HEAD.lock"));
        assert_eq!(repository.git_ref("HEAD"), before_head);
        assert_eq!(default_tip(&repository), before_tip);
        assert_eq!(remote_refs(&repository), remote_before);
        assert_eq!(repository.stash_count(), 0, "no stash entry is created");
        assert!(
            repository
                .git_ref(&format!("refs/heads/{}", common::SUMMARY_BRANCH))
                .is_none(),
            "no branch creation follows a failed switch"
        );
        assert!(
            repository
                .origin_ref(&format!("refs/heads/{}", common::SUMMARY_BRANCH))
                .is_none(),
            "no push follows a failed switch"
        );
        assert_eq!(repository.staged(), "", "no later staging runs");
        assert!(artifacts.join("change-summary.md").is_file());
        let _ = fs::remove_dir_all(&artifacts);
    }

    // An unavailable remote makes the fast-forward-only pull fail before any
    // branch is created or the stash is reapplied.
    {
        let repository = default_branch_repository();
        repository.make_origin_unavailable();
        let before_head = repository.git_ref("HEAD");
        let before_tip = default_tip(&repository);
        let remote_before = remote_refs(&repository);
        let artifacts = scratch_dir();
        fs::create_dir_all(&artifacts).expect("the external artifact directory is creatable");
        let (config, _transport) = common::scripted_pipeline_with(
            common::GateScript::Passing,
            common::SummaryScript::Summary,
        );
        let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
        let step = pipeline.ask(&common::harmless_plan_request(), &artifacts);

        let (stage, reason) = git_failure(&step);
        assert_eq!(
            stage, "pull",
            "the unavailable remote fails the pull: {reason}"
        );
        assert_eq!(repository.git_ref("HEAD"), before_head);
        assert_eq!(default_tip(&repository), before_tip);
        assert_eq!(remote_refs(&repository), remote_before);
        assert_eq!(repository.stash_count(), 0, "no stash entry is created");
        assert!(
            repository
                .git_ref(&format!("refs/heads/{}", common::SUMMARY_BRANCH))
                .is_none(),
            "no branch creation follows a failed pull"
        );
        assert_eq!(repository.staged(), "", "no later staging runs");
        assert!(artifacts.join("change-summary.md").is_file());
        let _ = fs::remove_dir_all(&artifacts);
    }

    // An exact local collision is caught by the uniqueness query before the
    // branch is created, checked out or reused.
    {
        let repository = default_branch_repository();
        let branch = common::SUMMARY_BRANCH;
        repository.git(&["branch", branch, "main"]);
        let local_before = repository.git_ref(&format!("refs/heads/{branch}"));
        let before_tip = default_tip(&repository);
        let artifacts = scratch_dir();
        fs::create_dir_all(&artifacts).expect("the external artifact directory is creatable");
        let (config, _transport) = common::scripted_pipeline_with(
            common::GateScript::Passing,
            common::SummaryScript::Summary,
        );
        let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
        let step = pipeline.ask(&common::harmless_plan_request(), &artifacts);

        let (stage, reason) = git_failure(&step);
        assert_eq!(
            stage, "branch",
            "the local collision fails the branch-existence check: {reason}"
        );
        assert_eq!(
            repository.git_ref(&format!("refs/heads/{branch}")),
            local_before,
            "the existing branch ref is not moved or reused"
        );
        assert_eq!(default_tip(&repository), before_tip);
        assert_eq!(repository.stash_count(), 0);
        assert!(
            repository
                .origin_ref(&format!("refs/heads/{branch}"))
                .is_none(),
            "no push follows a failed branch-existence check"
        );
        assert_eq!(repository.staged(), "", "no later staging runs");
        assert!(artifacts.join("change-summary.md").is_file());
        let _ = fs::remove_dir_all(&artifacts);
    }

    // A real reference transaction hook refuses the summary-branch creation
    // after the uniqueness check accepted the name.
    {
        let repository = default_branch_repository();
        let branch = common::SUMMARY_BRANCH;
        let hook = format!(
            "#!/bin/sh\nstate=\"$1\"\nwhile read -r old new ref; do\n  case \"$ref\" in\n    refs/heads/{branch}*)\n      if [ \"$state\" = prepared ]; then\n        echo 'the local ref transaction refuses the summary branch' >&2\n        exit 1\n      fi\n      ;;\n  esac\ndone\nexit 0\n"
        );
        repository.install_hook("reference-transaction", &hook);
        let before_tip = default_tip(&repository);
        let remote_before = remote_refs(&repository);
        let artifacts = scratch_dir();
        fs::create_dir_all(&artifacts).expect("the external artifact directory is creatable");
        let (config, _transport) = common::scripted_pipeline_with(
            common::GateScript::Passing,
            common::SummaryScript::Summary,
        );
        let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
        let step = pipeline.ask(&common::harmless_plan_request(), &artifacts);

        let (stage, reason) = git_failure(&step);
        assert_eq!(
            stage, "branch",
            "the refused branch creation is a branch-stage failure: {reason}"
        );
        assert!(
            reason.contains("refuses the summary branch"),
            "the hook's diagnostic is preserved: {reason}"
        );
        assert!(
            repository
                .git_ref(&format!("refs/heads/{branch}"))
                .is_none(),
            "the refused branch does not exist locally"
        );
        assert_eq!(default_tip(&repository), before_tip);
        assert_eq!(remote_refs(&repository), remote_before);
        assert_eq!(repository.stash_count(), 0);
        assert!(
            repository
                .origin_ref(&format!("refs/heads/{branch}"))
                .is_none(),
            "no push follows a failed branch creation"
        );
        assert_eq!(repository.staged(), "", "no later staging runs");
        assert!(artifacts.join("change-summary.md").is_file());
        let _ = fs::remove_dir_all(&artifacts);
    }

    // A real locked index on the non-default path fails the staging operation
    // before the commit.
    {
        let repository = feature_work_repository();
        repository.lock_index();
        let before_commits = head_commits(&repository);
        let before_head = repository.git_ref("HEAD");
        let artifacts = scratch_dir();
        fs::create_dir_all(&artifacts).expect("the external artifact directory is creatable");
        let (config, _transport) = common::scripted_pipeline_with(
            common::GateScript::Passing,
            common::SummaryScript::Summary,
        );
        let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
        let step = pipeline.ask(&common::harmless_plan_request(), &artifacts);

        let (stage, reason) = git_failure(&step);
        assert_eq!(
            stage, "stage",
            "the locked index fails the staging: {reason}"
        );
        let _ = fs::remove_file(repository.path().join(".git/index.lock"));
        assert_eq!(head_commits(&repository), before_commits);
        assert_eq!(repository.git_ref("HEAD"), before_head);
        assert_eq!(repository.staged(), "", "nothing was staged");
        assert_eq!(repository.stash_count(), 0);
        assert!(artifacts.join("change-summary.md").is_file());
        let _ = fs::remove_dir_all(&artifacts);
    }

    // A real pre-commit hook refuses the commit after the staging succeeded.
    {
        let repository = feature_work_repository();
        repository.install_hook(
            "pre-commit",
            "#!/bin/sh\necho 'the local pre-commit hook refuses this commit' >&2\nexit 1\n",
        );
        let before_commits = head_commits(&repository);
        let remote_before = remote_refs(&repository);
        // The artifacts land at the root, outside the ignored .ejaj/ directory,
        // so the staging that precedes the refused commit really has content.
        let artifacts = repository.path().to_path_buf();
        let (config, _transport) = common::scripted_pipeline_with(
            common::GateScript::Passing,
            common::SummaryScript::Summary,
        );
        let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
        let step = pipeline.ask(&common::harmless_plan_request(), &artifacts);

        let (stage, reason) = git_failure(&step);
        assert_eq!(
            stage, "commit",
            "the hook refusal is a commit failure: {reason}"
        );
        assert!(
            reason.contains("refuses this commit"),
            "the hook's diagnostic is preserved: {reason}"
        );
        assert_eq!(head_commits(&repository), before_commits);
        let staged = repository.staged();
        assert!(
            staged.contains("change-summary.md"),
            "the staging that preceded the refused commit remains: {staged:?}"
        );
        assert_eq!(remote_refs(&repository), remote_before);
        assert_eq!(repository.stash_count(), 0);
        assert!(
            artifacts.join("change-summary.md").is_file(),
            "the summary artifact is not removed by a commit failure"
        );
    }

    // A remote-side rejection makes the publication fail after the local commit
    // was made.
    {
        let repository = default_branch_repository();
        repository.reject_pushes("the bare origin refuses the push");
        let before = default_tip(&repository);
        let remote_before = remote_refs(&repository);
        let artifacts = repository.path().join(".ejaj/push-failure");
        let (config, _transport) = common::scripted_pipeline_with_command(
            common::GateScript::Passing,
            common::SummaryScript::Summary,
            change_tracked_file(&repository),
        );
        let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
        let step = pipeline.ask(&common::harmless_plan_request(), &artifacts);

        let (stage, reason) = git_failure(&step);
        assert_eq!(
            stage, "push",
            "the rejected publication is fatal at push: {reason}"
        );
        assert_eq!(
            repository.current_branch(),
            common::SUMMARY_BRANCH,
            "the local branch remains after the failed push"
        );
        assert_eq!(
            repository
                .git(&["rev-list", "--count", &format!("{before}..HEAD")])
                .trim(),
            "1",
            "the local commit remains after the failed push"
        );
        assert_eq!(remote_refs(&repository), remote_before);
        assert!(
            repository
                .origin_ref(&format!("refs/heads/{}", common::SUMMARY_BRANCH))
                .is_none(),
            "the rejected branch is not published"
        );
        assert!(artifacts.join("change-summary.md").is_file());
    }
}

/// Regression: every non-success termination -- an incomplete queue, an
/// incomplete review and the four-review cap -- keeps its retryable step and
/// its written summary artifact while performing no finalization at all: no
/// commit, no staging, no stash and no branch.
#[test]
fn when_a_run_finishes_given_an_incomplete_queue_or_review_then_keeps_the_summary_without_git_finalization()
 {
    // An incomplete queue: the ticket's gate fails.
    {
        let (config, _transport) = common::scripted_pipeline_with(
            common::GateScript::Failing,
            common::SummaryScript::Summary,
        );
        let repository = default_branch_repository();
        let before_commits = head_commits(&repository);
        let before_tip = default_tip(&repository);
        let remote_before = remote_refs(&repository);
        let artifacts = repository.path().join(".ejaj/incomplete-queue");
        let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
        let step = pipeline.ask(&common::harmless_plan_request(), &artifacts);

        assert!(
            matches!(step, Step::Retry),
            "an incomplete queue stays retryable: {step:?}"
        );
        assert!(artifacts.join("change-summary.md").is_file());
        assert_eq!(head_commits(&repository), before_commits, "no commit");
        assert_eq!(repository.staged(), "", "no staging");
        assert_eq!(repository.stash_count(), 0, "no stash");
        assert!(
            repository
                .git_ref(&format!("refs/heads/{}", common::SUMMARY_BRANCH))
                .is_none(),
            "no local branch"
        );
        assert!(
            repository
                .origin_ref(&format!("refs/heads/{}", common::SUMMARY_BRANCH))
                .is_none(),
            "no remote branch"
        );
        assert_eq!(default_tip(&repository), before_tip);
        assert_eq!(remote_refs(&repository), remote_before);
    }

    // An incomplete review: the review pass ends without a handoff.
    {
        let (config, _transport) = common::scripted_pipeline_with_review(
            common::ReviewScript::TurnEnd,
            common::SummaryScript::Summary,
        );
        let repository = default_branch_repository();
        let before_commits = head_commits(&repository);
        let before_tip = default_tip(&repository);
        let remote_before = remote_refs(&repository);
        let artifacts = repository.path().join(".ejaj/incomplete-review");
        let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
        let step = pipeline.ask(&common::harmless_plan_request(), &artifacts);

        assert!(
            matches!(step, Step::Retry),
            "an incomplete review stays retryable: {step:?}"
        );
        assert!(artifacts.join("change-summary.md").is_file());
        assert_eq!(head_commits(&repository), before_commits, "no commit");
        assert_eq!(repository.staged(), "", "no staging");
        assert_eq!(repository.stash_count(), 0, "no stash");
        assert!(
            repository
                .git_ref(&format!("refs/heads/{}", common::SUMMARY_BRANCH))
                .is_none(),
            "no local branch"
        );
        assert!(
            repository
                .origin_ref(&format!("refs/heads/{}", common::SUMMARY_BRANCH))
                .is_none(),
            "no remote branch"
        );
        assert_eq!(default_tip(&repository), before_tip);
        assert_eq!(remote_refs(&repository), remote_before);
    }

    // The four-review cap: the final permitted review still reports fixes.
    {
        let (config, _transport) = common::scripted_pipeline_with_review(
            common::ReviewScript::AlwaysFixes,
            common::SummaryScript::Summary,
        );
        let repository = default_branch_repository();
        let before_commits = head_commits(&repository);
        let before_tip = default_tip(&repository);
        let remote_before = remote_refs(&repository);
        let artifacts = repository.path().join(".ejaj/review-cap");
        let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
        let step = pipeline.ask(&common::harmless_plan_request(), &artifacts);

        assert!(
            matches!(step, Step::Retry),
            "the review cap stays retryable: {step:?}"
        );
        assert!(artifacts.join("change-summary.md").is_file());
        assert_eq!(head_commits(&repository), before_commits, "no commit");
        assert_eq!(repository.staged(), "", "no staging");
        assert_eq!(repository.stash_count(), 0, "no stash");
        assert!(
            repository
                .git_ref(&format!("refs/heads/{}", common::SUMMARY_BRANCH))
                .is_none(),
            "no local branch"
        );
        assert!(
            repository
                .origin_ref(&format!("refs/heads/{}", common::SUMMARY_BRANCH))
                .is_none(),
            "no remote branch"
        );
        assert_eq!(default_tip(&repository), before_tip);
        assert_eq!(remote_refs(&repository), remote_before);
    }
}

/// Empty boundary: an artifact path the write cannot use preserves the
/// WriteFailed result and leaves every piece of repository state untouched.
#[test]
fn when_summary_writing_given_an_unwritable_artifact_path_then_preserves_the_write_failure_without_git_mutation()
 {
    let repository = default_branch_repository();
    let artifacts = repository.path().join(".ejaj/unwritable-summary");
    fs::create_dir_all(artifacts.join("change-summary.md"))
        .expect("the summary path is plantable as a directory");
    let before_commits = head_commits(&repository);
    let before_tip = default_tip(&repository);
    let before_refs = repository.all_refs();
    let before_index = repository.git(&["write-tree"]);
    let remote_before = remote_refs(&repository);

    let (config, _transport) =
        common::scripted_pipeline_with(common::GateScript::Passing, common::SummaryScript::Summary);
    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), &artifacts);

    match &step {
        Step::WriteFailed { artifact, .. } => assert_eq!(*artifact, "change-summary"),
        other => panic!("expected the change-summary write failure, got {other:?}"),
    }
    assert_eq!(head_commits(&repository), before_commits, "no commit");
    assert_eq!(repository.staged(), "", "no staging");
    assert_eq!(repository.stash_count(), 0, "no stash");
    assert_eq!(repository.all_refs(), before_refs, "no ref changes");
    assert_eq!(
        repository.git(&["write-tree"]),
        before_index,
        "the index is untouched"
    );
    assert_eq!(default_tip(&repository), before_tip);
    assert_eq!(remote_refs(&repository), remote_before);
    assert!(
        repository
            .origin_ref(&format!("refs/heads/{}", common::SUMMARY_BRANCH))
            .is_none(),
        "no remote branch"
    );
    assert!(
        artifacts.join("change-summary.md").is_dir(),
        "the unwritable path is not replaced by a file"
    );
}

/// Minimum valid boundary: when the stash cannot be reapplied, the run fails at
/// the reapply operation, keeps the stash entry and the conflict in the
/// worktree, and runs no stage, commit or push afterwards.
#[test]
fn when_finalization_fails_given_a_stash_pop_conflict_then_preserves_the_stash_and_conflict_state()
{
    let repository = default_branch_repository();
    let updated = repository.advance_origin_default("tracked.txt", "advanced remotely\n");
    let target = repository.path().join("tracked.txt");
    let command = format!("printf 'changed by the run\\n' > '{}'", target.display());
    let artifacts = repository.path().join(".ejaj/stash-pop-conflict");
    let (config, _transport) = common::scripted_pipeline_with_command(
        common::GateScript::Passing,
        common::SummaryScript::Summary,
        command,
    );

    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), &artifacts);

    let (stage, reason) = git_failure(&step);
    assert_eq!(
        stage, "stash-pop",
        "the unappliable stash fails at the reapply operation: {reason}"
    );
    assert_eq!(
        repository.stash_count(),
        1,
        "the run's stash entry remains available"
    );
    let stash_subjects = repository.git(&["stash", "list", "--format=%s"]);
    assert!(
        stash_subjects.contains("ejaj run changes"),
        "the retained entry is the run's own stash: {stash_subjects}"
    );
    assert_eq!(
        repository.current_branch(),
        common::SUMMARY_BRANCH,
        "the branch was created before the reapply attempted"
    );
    assert_eq!(
        default_tip(&repository),
        updated,
        "the default branch is fast-forwarded and not advanced by the run"
    );
    assert_eq!(
        repository.git(&["rev-parse", "HEAD"]).trim(),
        updated,
        "no commit follows the failed reapply"
    );
    let status = repository.status();
    assert!(
        status.contains("UU tracked.txt"),
        "the conflict is preserved in the worktree: {status}"
    );
    let conflicted = fs::read_to_string(&target).expect("the conflicted file is readable");
    assert!(
        conflicted.contains("<<<<<<<") && conflicted.contains(">>>>>>>"),
        "the conflicted work is not automatically discarded: {conflicted:?}"
    );
    assert!(
        conflicted.contains("changed by the run"),
        "the saved change remains visible in the conflict: {conflicted:?}"
    );
    assert!(
        repository
            .origin_ref(&format!("refs/heads/{}", common::SUMMARY_BRANCH))
            .is_none(),
        "no push follows the failed reapply"
    );
    assert_eq!(
        repository.origin_ref("refs/heads/main").unwrap(),
        updated,
        "the remote default branch is unchanged by the run"
    );
    assert!(artifacts.join("change-summary.md").is_file());
}

/// Regression: a fatal failure after the run's visible changes -- the summary
/// artifact among them -- were saved in the stash and before they are
/// reapplied keeps the summary readable at the path it was written to, while
/// leaving the failed operation's stash and repository state exactly where the
/// failure left them.
#[test]
fn when_finalization_fails_given_an_error_after_stashing_then_keeps_the_summary_artifact() {
    let repository = default_branch_repository();
    let advanced = repository.make_origin_unavailable_after_pull();
    // The artifact directory is inside the repository at a path no ignore rule
    // covers, so the default-branch finalizer's untracked-including stash
    // really removes the summary from the worktree.
    let artifacts = repository.path().join("run-artifacts");
    let summary_path = artifacts.join("change-summary.md");
    let (config, _transport) = common::scripted_pipeline_with_command(
        common::GateScript::Passing,
        common::SummaryScript::Summary,
        change_tracked_file(&repository),
    );

    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), &artifacts);

    let (stage, reason) = git_failure(&step);
    assert_eq!(
        stage, "branch",
        "the failure after the stash is the failed origin query: {reason}"
    );
    assert!(
        reason.contains("ls-remote"),
        "the original branch-query diagnostic is preserved: {reason}"
    );

    // The failed operation's stash retains the run's changes, the summary
    // artifact included, exactly as the failure left them.
    assert_eq!(
        repository.stash_count(),
        1,
        "the run's stash entry is retained"
    );
    let stash_subjects = repository.git(&["stash", "list", "--format=%s"]);
    assert!(
        stash_subjects.contains("ejaj run changes"),
        "the retained entry is the run's own stash: {stash_subjects}"
    );
    let stashed_paths = repository.git(&[
        "stash",
        "show",
        "--include-untracked",
        "--name-only",
        "stash@{0}",
    ]);
    assert!(
        stashed_paths.contains("run-artifacts/change-summary.md"),
        "the saved changes include the summary artifact: {stashed_paths}"
    );

    // The artifact is nevertheless still readable at the exact path the run
    // wrote it to, holding the canonical bytes the run wrote.
    let kept = fs::read_to_string(&summary_path).unwrap_or_else(|e| {
        common::failed("the preserved change summary artifact", &e.to_string())
    });
    assert_eq!(
        kept,
        common::SCRIPTED_SUMMARY,
        "the preserved artifact is the canonical summary the run wrote"
    );

    // The failure state is retained, not rolled back: the pull completed, the
    // worktree stays on the default branch, nothing is staged, and no branch or
    // commit followed the failed operation.
    assert_eq!(
        repository.current_branch(),
        common::DEFAULT_BRANCH,
        "the worktree stays on the pulled default branch"
    );
    assert_eq!(
        repository.git_ref("HEAD").as_deref(),
        Some(advanced.trim()),
        "the fast-forward-only pull completed before the failure"
    );
    assert!(
        repository
            .git_ref(&format!("refs/heads/{}", common::SUMMARY_BRANCH))
            .is_none(),
        "no summary branch is created after the failed query"
    );
    assert!(
        repository
            .origin_ref(&format!("refs/heads/{}", common::SUMMARY_BRANCH))
            .is_none(),
        "nothing is pushed after the failed query"
    );
    assert_eq!(
        repository.staged(),
        "",
        "nothing is staged after the failed query"
    );
    assert_eq!(
        head_commits(&repository),
        2,
        "the baseline and the pulled remote commit, and no run commit"
    );
}

/// Regression: the summary artifact is tracked, so the default-branch
/// finalizer's stash restores it to its committed bytes before the later
/// branch-query failure. This is the case the exists-only recovery missed: the
/// path is present, stale, and must still be rewritten with the canonical
/// bytes the run produced.
#[test]
fn when_finalization_fails_given_a_tracked_summary_artifact_then_restores_canonical_bytes() {
    const PREVIOUS: &[u8] = b"previous tracked summary\n";
    let repository = default_branch_repository();
    repository.write(
        "run-artifacts/change-summary.md",
        "previous tracked summary\n",
    );
    repository.commit_all("track the previous summary artifact");
    let advanced = repository.advance_origin_default("advanced.txt", "advanced remotely\n");

    // The hook observes the restored bytes at the exact path and then makes
    // every later origin query fail, so the pull succeeds and the uniqueness
    // query is the operation that fails.
    let observation = scratch_dir().join("observed-summary.bin");
    fs::create_dir_all(observation.parent().expect("the observation has a parent"))
        .expect("the observation directory is creatable");
    let summary_path = repository.path().join("run-artifacts/change-summary.md");
    let unavailable = repository.origin_path().join("unavailable.git");
    let hook = format!(
        "#!/bin/sh\ncp '{summary}' '{observation}'\ngit remote set-url origin '{unavailable}'\n",
        summary = summary_path.display(),
        observation = observation.display(),
        unavailable = unavailable.display(),
    );
    repository.install_hook("post-merge", &hook);

    let artifacts = repository.path().join("run-artifacts");
    let (config, _transport) = common::scripted_pipeline_with_command(
        common::GateScript::Passing,
        common::SummaryScript::Summary,
        change_tracked_file(&repository),
    );
    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), &artifacts);

    // The failure stays the original branch-query failure: the artifact
    // recovery never turns it into completion, a retry or a write failure.
    assert!(!matches!(&step, Step::Complete));
    assert!(!matches!(&step, Step::Retry));
    assert!(!matches!(&step, Step::WriteFailed { .. }));
    let (stage, reason) = git_failure(&step);
    assert_eq!(
        stage, "branch",
        "the failure after the stash is the failed origin query: {reason}"
    );
    assert!(
        reason.contains("ls-remote"),
        "the original branch-query diagnostic is preserved: {reason}"
    );

    // The stash restored the tracked artifact to its committed bytes before
    // the hook ran; the observation proves this is the stale path the recovery
    // had to overwrite rather than an absent one.
    assert_eq!(
        fs::read(&observation).expect("the hook observation is readable"),
        PREVIOUS,
        "the hook must observe the prior tracked bytes after the stash restoration"
    );

    // The artifact at the run's path holds exactly the canonical summary
    // bytes, even though the stale file was still present at the failure.
    assert_eq!(
        fs::read(&summary_path).expect("the change summary artifact is readable"),
        common::SCRIPTED_SUMMARY.as_bytes(),
        "the still-existing artifact must be rewritten with the canonical summary bytes"
    );

    // The run's own stash remains, and its saved copy of the artifact carries
    // the canonical bytes the run wrote.
    assert_eq!(
        repository.stash_count(),
        1,
        "the run's stash entry is retained"
    );
    let stash_subjects = repository.git(&["stash", "list", "--format=%s"]);
    assert!(
        stash_subjects.contains("ejaj run changes"),
        "the retained entry is the run's own stash: {stash_subjects}"
    );
    assert_eq!(
        repository.git_bytes(&["show", "stash@{0}:run-artifacts/change-summary.md"]),
        common::SCRIPTED_SUMMARY.as_bytes(),
        "the saved artifact in the retained stash is the canonical summary"
    );

    // Apart from the restored artifact, the failed finalization state is not
    // rolled back: the pull completed, the worktree stays on the default
    // branch at the advanced tip, nothing is staged, and no summary branch was
    // created or pushed.
    assert_eq!(
        repository.current_branch(),
        common::DEFAULT_BRANCH,
        "the worktree stays on the pulled default branch"
    );
    assert_eq!(
        repository.git_ref("HEAD").as_deref(),
        Some(advanced.trim()),
        "the fast-forward-only pull completed before the failure"
    );
    assert_eq!(
        default_tip(&repository),
        advanced.trim(),
        "the default branch keeps the fast-forwarded remote tip"
    );
    assert!(
        repository
            .git_ref(&format!("refs/heads/{}", common::SUMMARY_BRANCH))
            .is_none(),
        "no summary branch is created after the failed query"
    );
    assert!(
        repository
            .origin_ref(&format!("refs/heads/{}", common::SUMMARY_BRANCH))
            .is_none(),
        "nothing is pushed after the failed query"
    );
    assert_eq!(
        repository.staged(),
        "",
        "nothing is staged after the failed query"
    );

    if let Some(parent) = observation.parent() {
        let _ = fs::remove_dir_all(parent);
    }
}

/// Upper boundary: a divergent default branch cannot be fast-forwarded. The run
/// stops at the pull, before a summary branch is created or the stash is
/// reapplied, and leaves the local and remote default branches where they were.
#[test]
fn when_finalization_fails_given_a_divergent_default_branch_then_stops_before_branch_creation() {
    let repository = default_branch_repository();
    repository.write("local-only.txt", "local work\n");
    repository.git(&["add", "--all"]);
    repository.git(&["commit", "--quiet", "--message", "local divergent commit"]);
    let local_tip = repository
        .git(&["rev-parse", "refs/heads/main"])
        .trim()
        .to_string();
    let remote_tip = repository.advance_origin_default("remote-only.txt", "remote work\n");
    assert_ne!(
        local_tip, remote_tip,
        "the local and remote branches diverge"
    );
    let artifacts = scratch_dir();
    fs::create_dir_all(&artifacts).expect("the external artifact directory is creatable");

    let (config, _transport) =
        common::scripted_pipeline_with(common::GateScript::Passing, common::SummaryScript::Summary);
    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), &artifacts);

    let (stage, reason) = git_failure(&step);
    assert_eq!(stage, "pull", "the divergence fails the pull: {reason}");
    assert!(
        reason.contains("--ff-only") || reason.to_lowercase().contains("fast-forward"),
        "the failure identifies the fast-forward-only pull: {reason}"
    );
    assert_eq!(
        default_tip(&repository),
        local_tip,
        "the local default branch is not rewritten"
    );
    assert!(
        repository
            .git_ref(&format!("refs/heads/{}", common::SUMMARY_BRANCH))
            .is_none(),
        "no summary branch is created before the pull succeeds"
    );
    assert_eq!(repository.stash_count(), 0, "no stash is created");
    assert_eq!(repository.staged(), "", "no staging runs");
    assert!(
        repository
            .origin_ref(&format!("refs/heads/{}", common::SUMMARY_BRANCH))
            .is_none(),
        "no summary branch is published"
    );
    assert_eq!(
        repository.origin_ref("refs/heads/main").unwrap(),
        remote_tip,
        "the remote default branch keeps its own tip"
    );
    assert!(artifacts.join("change-summary.md").is_file());
    let _ = fs::remove_dir_all(&artifacts);
}

/// Contract and invariant: a failure to query origin for the candidate summary
/// branch is fatal, never read as the branch being absent. The pull that
/// preceded the query really succeeded; no summary branch is created, the run's
/// stash is not popped, and nothing is staged, committed or pushed.
#[test]
fn when_checking_branch_uniqueness_given_an_unavailable_origin_then_fails_before_branch_creation() {
    let repository = default_branch_repository();
    let branch = common::SUMMARY_BRANCH;
    let advanced = repository.make_origin_unavailable_after_pull();
    let artifacts = repository.path().join(".ejaj/uniqueness-query-failure");

    let (config, _transport) = common::scripted_pipeline_with_command(
        common::GateScript::Passing,
        common::SummaryScript::Summary,
        change_tracked_file(&repository),
    );
    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), &artifacts);

    let (stage, reason) = git_failure(&step);
    assert_eq!(
        stage, "branch",
        "a failed origin query is a fatal uniqueness failure: {reason}"
    );
    assert!(
        reason.contains("ls-remote"),
        "the failure names the origin query that could not run: {reason}"
    );
    assert!(
        reason.contains("fatal") && reason.contains("never interpreted"),
        "a failed origin query is fatal, never read as absence: {reason}"
    );

    // The pull before the query really succeeded: HEAD is the default branch
    // at the independently advanced remote tip, not the pre-pull baseline.
    assert_eq!(
        repository.current_branch(),
        common::DEFAULT_BRANCH,
        "no summary branch was checked out"
    );
    assert_eq!(
        repository.git_ref("HEAD").as_deref(),
        Some(advanced.trim()),
        "the fast-forward-only pull completed before the query failed"
    );
    assert_eq!(
        default_tip(&repository),
        advanced.trim(),
        "the pulled default branch keeps the advanced remote tip"
    );

    assert!(
        repository
            .git_ref(&format!("refs/heads/{branch}"))
            .is_none(),
        "no summary branch is created after a failed uniqueness query"
    );
    assert_eq!(
        repository.stash_count(),
        1,
        "the run's saved changes are not reapplied after a failed query"
    );
    let stash_subjects = repository.git(&["stash", "list", "--format=%s"]);
    assert!(
        stash_subjects.contains("ejaj run changes"),
        "the run's own stash entry is still there: {stash_subjects}"
    );
    assert_eq!(
        repository.staged(),
        "",
        "nothing is staged after the failed query"
    );
    assert_eq!(
        head_commits(&repository),
        2,
        "the baseline and the pulled remote commit, and no run commit"
    );
    assert!(
        repository
            .origin_ref(&format!("refs/heads/{branch}"))
            .is_none(),
        "no summary branch is pushed after a failed query"
    );
    assert!(artifacts.join("change-summary.md").is_file());
}

// ---------------------------------------------------------------------------
// Plan handovers across a downstream retry
//
// A plan handoff ends the interview, so the plan session keeps only its pinned
// system message and one assistant handover. A later phase that sends the run
// back to plan>> updates that same handover with one concise, named outcome;
// the next provider request is then system, handover, then the new user line.
// These tests drive the real Pipeline through a scripted external Transport and
// inspect the request bodies it actually received.
// ---------------------------------------------------------------------------

/// One scripted reply for the handover tests: an assistant message with
/// optional narration, reasoning and tool calls, or a boundary failure.
enum HandoverReply {
    Assistant {
        content: Option<String>,
        reasoning: Option<String>,
        calls: Vec<(String, String)>,
    },
    Error(CallError),
}

impl HandoverReply {
    fn assistant(
        content: Option<&str>,
        reasoning: Option<&str>,
        calls: &[(&str, String)],
    ) -> HandoverReply {
        HandoverReply::Assistant {
            content: content.map(str::to_string),
            reasoning: reasoning.map(str::to_string),
            calls: calls
                .iter()
                .map(|(name, arguments)| (name.to_string(), arguments.clone()))
                .collect(),
        }
    }

    fn tool(name: &str, arguments: String) -> HandoverReply {
        HandoverReply::assistant(None, None, &[(name, arguments)])
    }

    fn silent() -> HandoverReply {
        HandoverReply::assistant(None, None, &[])
    }
}

/// A transport that answers exactly the replies a test scripts, in order, and
/// records every request body. Running out of replies is a loud failure: it
/// means the pipeline made a request the test did not predict.
struct HandoverTransport {
    replies: Mutex<VecDeque<HandoverReply>>,
    requests: Mutex<Vec<serde_json::Value>>,
}

impl HandoverTransport {
    fn new(replies: Vec<HandoverReply>) -> HandoverTransport {
        HandoverTransport {
            replies: Mutex::new(VecDeque::from(replies)),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<serde_json::Value> {
        self.requests.lock().unwrap().clone()
    }
}

impl Transport for HandoverTransport {
    fn send(&self, outgoing: &Outgoing) -> Result<RawResponse, CallError> {
        self.requests.lock().unwrap().push(outgoing.body.clone());
        let reply = self.replies.lock().unwrap().pop_front().unwrap_or_else(|| {
            panic!("the pipeline made an unexpected request: {}", outgoing.body)
        });
        match reply {
            HandoverReply::Error(error) => Err(error),
            HandoverReply::Assistant {
                content,
                reasoning,
                calls,
            } => {
                let tool_calls: Vec<serde_json::Value> = calls
                    .iter()
                    .enumerate()
                    .map(|(index, (name, arguments))| {
                        serde_json::json!({
                            "id": format!("call_{index}"),
                            "type": "function",
                            "function": { "name": name, "arguments": arguments },
                        })
                    })
                    .collect();
                let mut message = serde_json::json!({
                    "role": "assistant",
                    "content": content,
                    "tool_calls": tool_calls,
                });
                if let Some(reasoning) = reasoning {
                    message["reasoning_content"] = serde_json::Value::String(reasoning);
                }
                Ok(RawResponse {
                    status: 200,
                    body: serde_json::json!({ "choices": [{ "message": message }] }).to_string(),
                })
            }
        }
    }
}

/// One pipeline and its recorded transport over a clean disposable repository.
/// The artifacts live outside the repository so a second ask still passes the
/// clean-tree preflight without the test committing an intermediate run.
struct HandoverRun {
    pipeline: Pipeline,
    transport: Arc<HandoverTransport>,
    artifacts: PathBuf,
    repository: common::TempRepo,
    dir: PathBuf,
}

fn handover_run(replies: Vec<HandoverReply>) -> HandoverRun {
    let dir = scratch_dir();
    fs::create_dir_all(&dir).expect("the scratch directory is creatable");
    let repository = common::TempRepo::at(dir.join("repo"));
    repository.commit_baseline();
    let artifacts = dir.join("artifacts");
    let transport = Arc::new(HandoverTransport::new(replies));
    let config = Config::new(
        "http://script.test/v1/chat/completions",
        "test-model",
        Some("test-key".to_string()),
    )
    .with_smoll("https://smoll.test/v1/messages", "smoll-model", "smoll-key")
    .with_transport(transport.clone());
    let pipeline = Pipeline::new(&config, common::scope(), repository.path());
    HandoverRun {
        pipeline,
        transport,
        artifacts,
        repository,
        dir,
    }
}

fn finish_plan_reply(plan_markdown: &str) -> HandoverReply {
    HandoverReply::tool(
        "finish_plan",
        serde_json::json!({ "plan": plan_markdown }).to_string(),
    )
}

fn finish_spec_reply() -> HandoverReply {
    HandoverReply::tool(
        "finish_spec",
        serde_json::json!({ "spec": common::harmless_spec_markdown() }).to_string(),
    )
}

fn finish_tickets_reply() -> HandoverReply {
    HandoverReply::tool(
        "finish_tickets",
        common::harmless_tickets_json().to_string(),
    )
}

fn finish_review_pass_reply() -> HandoverReply {
    HandoverReply::tool("finish_review", common::passing_review_json())
}

fn bash_reply() -> HandoverReply {
    HandoverReply::tool(
        "bash",
        serde_json::json!({ "command": "echo ejaj-e2e-ok" }).to_string(),
    )
}

fn task_complete_reply() -> HandoverReply {
    HandoverReply::tool("task_complete", "{}".to_string())
}

fn harmless_ticket_set() -> TicketSet {
    let value: serde_json::Value =
        serde_json::from_str(common::harmless_tickets_json()).expect("the ticket fixture is JSON");
    serde_json::from_value(value["tickets"].clone())
        .expect("the ticket fixture carries a ticket set")
}

/// The triage report the scripted pass hands over when every draft ticket is
/// ready for the code agent.
fn ready_triage_arguments() -> String {
    let set = harmless_ticket_set();
    let tickets: Vec<serde_json::Value> = set
        .tickets
        .iter()
        .map(|ticket| {
            let acceptance_criteria = if ticket.acceptance_criteria.is_empty() {
                vec!["The verification gate exits 0.".to_string()]
            } else {
                ticket.acceptance_criteria.clone()
            };
            let out_of_scope = if ticket.out_of_scope.is_empty() {
                vec!["None.".to_string()]
            } else {
                ticket.out_of_scope.clone()
            };
            serde_json::json!({
                "ticket_id": ticket.id.trim(),
                "ticket_title": ticket.title.trim(),
                "category": "enhancement",
                "state": "ready-for-agent",
                "notes": ["Audited against the repository."],
                "agent_brief": {
                    "target_behavior": format!("Implement {}.", ticket.title.trim()),
                    "acceptance_criteria": acceptance_criteria,
                    "out_of_scope": out_of_scope,
                    "blocked_by": ticket.blocked_by,
                },
            })
        })
        .collect();
    serde_json::json!({ "triage": { "tickets": tickets } }).to_string()
}

/// The triage report the scripted pass hands over when the draft set is gated by
/// a human decision, carrying the blocker explanation the handover must keep.
fn blocked_triage_arguments(explanation: &str) -> String {
    let set = harmless_ticket_set();
    let tickets: Vec<serde_json::Value> = set
        .tickets
        .iter()
        .map(|ticket| {
            serde_json::json!({
                "ticket_id": ticket.id.trim(),
                "ticket_title": ticket.title.trim(),
                "category": "bug",
                "state": "needs-info",
                "notes": ["Audited against the repository."],
                "blocker_explanation": explanation,
            })
        })
        .collect();
    serde_json::json!({ "triage": { "tickets": tickets } }).to_string()
}

/// Every reply a successful queue and review need, ended by summary, plus the
/// plan turn of the next ask.
fn run_then_resume_replies(first: HandoverReply, summary: HandoverReply) -> Vec<HandoverReply> {
    vec![
        first,
        finish_spec_reply(),
        finish_tickets_reply(),
        HandoverReply::tool("finish_triage", ready_triage_arguments()),
        bash_reply(),
        task_complete_reply(),
        finish_review_pass_reply(),
        summary,
        HandoverReply::silent(),
    ]
}

fn sent_messages(request: &serde_json::Value) -> &Vec<serde_json::Value> {
    request["messages"]
        .as_array()
        .expect("the recorded request carries a messages array")
}

fn assert_no_downstream_handover(messages: &[serde_json::Value]) {
    for (index, message) in messages.iter().enumerate() {
        if message["role"] == "system" {
            // The pinned prompt itself contains the plan template.
            continue;
        }
        let text = message["content"].as_str().unwrap_or("");
        assert!(
            !text.contains("[DOWNSTREAM RETRY]"),
            "message {index} fabricates a downstream handover: {text}"
        );
    }
}

fn assert_no_agreed_plan(messages: &[serde_json::Value]) {
    for (index, message) in messages.iter().enumerate() {
        if message["role"] == "system" {
            // The pinned prompt itself contains the plan template.
            continue;
        }
        let text = message["content"].as_str().unwrap_or("");
        assert!(
            !text.contains("## 1. Decisions"),
            "message {index} fabricates an agreed plan: {text}"
        );
    }
}

/// Core entry point: after a specification retry, the next plan request is the
/// pinned system prompt, the agreed handover naming the failed phase, then the
/// new user line -- and no earlier reasoning survives the boundary.
#[test]
fn when_resuming_plan_given_recoverable_downstream_return_then_sends_canonical_plan_handover() {
    let plan = common::harmless_plan_fixture();
    let plan_markdown = plan.to_markdown();
    let run = handover_run(vec![
        HandoverReply::assistant(
            Some("Exploring the plan."),
            Some("private plan reasoning"),
            &[(
                "finish_plan",
                serde_json::json!({ "plan": plan_markdown.clone() }).to_string(),
            )],
        ),
        HandoverReply::assistant(Some("The specification cannot continue."), None, &[]),
        HandoverReply::silent(),
    ]);
    let HandoverRun {
        mut pipeline,
        transport,
        artifacts,
        repository,
        dir,
    } = run;

    let first = pipeline.ask("first request", &artifacts);
    assert!(
        matches!(first, Step::Retry),
        "a specification turn end is recoverable: {first:?}"
    );
    let second = pipeline.ask("second request", &artifacts);
    assert!(
        matches!(second, Step::Retry),
        "the resumed plan turn ends in another retry: {second:?}"
    );

    let requests = transport.requests();
    assert_eq!(
        requests.len(),
        3,
        "plan, specification, then the resumed plan"
    );
    let messages = sent_messages(&requests[2]);
    assert_eq!(
        messages.len(),
        3,
        "the resumed request is the system prompt, the handover and the new user line"
    );
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(messages[2]["role"], "user");
    assert_eq!(messages[2]["content"], "second request");
    assert!(
        messages[2].get("reasoning_content").is_none(),
        "the new user message carries no reasoning"
    );

    let handover = messages[1]["content"]
        .as_str()
        .expect("the assistant handover carries text");
    assert!(
        handover.starts_with(&plan_markdown),
        "the agreed plan leads the handover: {handover}"
    );
    assert!(handover.contains("[DOWNSTREAM RETRY]"), "{handover}");
    assert!(handover.contains("phase: spec"), "{handover}");
    assert!(handover.contains("outcome:"), "{handover}");
    assert!(
        !handover.contains("Exploring the plan."),
        "the interview narration is not part of the handover: {handover}"
    );
    assert!(
        !handover.contains("private plan reasoning"),
        "the plan reasoning is not part of the handover: {handover}"
    );
    for (index, message) in messages.iter().enumerate() {
        assert!(
            message.get("reasoning_content").is_none(),
            "message {index} still carries reasoning: {message}"
        );
    }

    drop(repository);
    let _ = fs::remove_dir_all(&dir);
}

/// Edge-case table: absent, empty and whitespace-only reasoning or narration
/// never becomes synthetic handover content, across consecutive plan asks and
/// across an empty downstream phase.
#[test]
fn when_plan_resumes_given_empty_reasoning_or_narration_then_avoids_fabricated_handover_content() {
    for (content, reasoning) in [
        (None, None),
        (Some("   "), Some("")),
        (Some(""), Some("   ")),
    ] {
        let run = handover_run(vec![
            HandoverReply::assistant(content, reasoning, &[]),
            HandoverReply::silent(),
        ]);
        let HandoverRun {
            mut pipeline,
            transport,
            artifacts,
            repository,
            dir,
        } = run;

        assert!(matches!(pipeline.ask("first", &artifacts), Step::Retry));
        assert!(matches!(pipeline.ask("second", &artifacts), Step::Retry));

        let requests = transport.requests();
        let messages = sent_messages(&requests[1]);
        assert_eq!(
            messages.len(),
            4,
            "system, first user, first reply, second user"
        );
        assert!(
            messages[2].get("reasoning_content").is_none(),
            "empty reasoning is cleared at the next user boundary"
        );
        let text = messages[2]["content"].as_str().unwrap_or("");
        assert!(
            !text.contains("[DOWNSTREAM RETRY]"),
            "a plan conversation retry fabricates no downstream handover: {text}"
        );
        assert_eq!(messages[3]["content"], "second");

        drop(repository);
        let _ = fs::remove_dir_all(&dir);
    }

    let plan = common::harmless_plan_fixture();
    let plan_markdown = plan.to_markdown();
    let run = handover_run(vec![
        HandoverReply::assistant(
            None,
            Some("plan reasoning"),
            &[(
                "finish_plan",
                serde_json::json!({ "plan": plan_markdown.clone() }).to_string(),
            )],
        ),
        HandoverReply::assistant(Some("   "), Some(""), &[]),
        HandoverReply::silent(),
    ]);
    let HandoverRun {
        mut pipeline,
        transport,
        artifacts,
        repository,
        dir,
    } = run;

    assert!(matches!(pipeline.ask("first", &artifacts), Step::Retry));
    assert!(matches!(pipeline.ask("second", &artifacts), Step::Retry));

    let requests = transport.requests();
    let messages = sent_messages(&requests[2]);
    let handover = messages[1]["content"]
        .as_str()
        .expect("the downstream handover carries text");
    assert!(handover.starts_with(&plan_markdown));
    assert!(handover.contains("[DOWNSTREAM RETRY]"));
    assert!(handover.contains("phase: spec"));
    assert!(
        !handover.contains("   \n"),
        "blank narration is not rendered into the handover: {handover}"
    );
    assert!(!handover.contains("plan reasoning"));

    drop(repository);
    let _ = fs::remove_dir_all(&dir);
}

/// Contract and invariant: a successful handoff replaces the whole interview
/// transcript with the pinned system prompt and one assistant message carrying
/// exactly the typed plan's canonical rendering. A later fatal summary failure
/// leaves that handover untouched, so the next plan request proves the reset.
#[test]
fn when_accepting_plan_given_exploratory_interview_then_retains_only_system_and_canonical_plan() {
    let plan = common::harmless_plan_fixture();
    let plan_markdown = plan.to_markdown();
    let raw_plan = format!("\n\n{plan_markdown}\n\n");
    let run = handover_run(run_then_resume_replies(
        HandoverReply::assistant(
            Some("Exploring alternatives."),
            Some("exploratory reasoning"),
            &[(
                "finish_plan",
                serde_json::json!({ "plan": raw_plan }).to_string(),
            )],
        ),
        HandoverReply::Error(CallError::Api {
            status: 400,
            message: "the summary endpoint refused the request".to_string(),
        }),
    ));
    let HandoverRun {
        mut pipeline,
        transport,
        artifacts,
        repository,
        dir,
    } = run;

    let first = pipeline.ask("first request", &artifacts);
    assert!(
        matches!(first, Step::SummaryFailed { .. }),
        "the summary failure is fatal: {first:?}"
    );
    let second = pipeline.ask("second request", &artifacts);
    assert!(matches!(second, Step::Retry), "{second:?}");

    let requests = transport.requests();
    assert_eq!(
        requests.len(),
        9,
        "the full run and the resumed plan request"
    );
    let messages = sent_messages(&requests[8]);
    assert_eq!(
        messages.len(),
        3,
        "only the pinned system prompt and the canonical assistant handover precede the new user line"
    );
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(
        messages[0]["content"], requests[0]["messages"][0]["content"],
        "the system prompt is unchanged"
    );
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(
        messages[1]["content"], plan_markdown,
        "the handover is exactly the typed plan's canonical rendering"
    );
    assert_eq!(messages[2]["role"], "user");
    assert_eq!(messages[2]["content"], "second request");
    assert!(
        !messages[1]["content"]
            .as_str()
            .unwrap()
            .contains("Exploring alternatives."),
        "the exploratory turn is gone"
    );

    drop(repository);
    let _ = fs::remove_dir_all(&dir);
}

/// Regression: fatal artifact-write and summary failures keep their existing
/// fatal steps and never become downstream retry handovers; the typed artifact
/// bytes a successful run wrote stay untouched.
#[test]
fn when_pipeline_returns_given_fatal_write_or_summary_failure_then_preserves_fatal_step_without_retry_handover()
 {
    let plan = common::harmless_plan_fixture();
    let plan_markdown = plan.to_markdown();
    let run = handover_run(vec![
        finish_plan_reply(&plan_markdown),
        HandoverReply::silent(),
    ]);
    let HandoverRun {
        mut pipeline,
        transport,
        artifacts,
        repository,
        dir,
    } = run;
    fs::create_dir_all(artifacts.join("plan.md"))
        .expect("the plan path is plantable as a directory");

    let first = pipeline.ask("first request", &artifacts);
    assert!(
        matches!(
            first,
            Step::WriteFailed {
                artifact: "plan",
                ..
            }
        ),
        "the failed plan write stays fatal: {first:?}"
    );
    let second = pipeline.ask("second request", &artifacts);
    assert!(matches!(second, Step::Retry), "{second:?}");

    let requests = transport.requests();
    let messages = sent_messages(&requests[1]);
    assert_eq!(messages[1]["content"], plan_markdown);
    assert_no_downstream_handover(messages);
    assert!(
        artifacts.join("plan.md").is_dir(),
        "the failed plan write is not turned into a handover artifact"
    );

    drop(repository);
    let _ = fs::remove_dir_all(&dir);

    let plan = common::harmless_plan_fixture();
    let plan_markdown = plan.to_markdown();
    let run = handover_run(run_then_resume_replies(
        finish_plan_reply(&plan_markdown),
        HandoverReply::Error(CallError::Api {
            status: 400,
            message: "the summary endpoint refused the request".to_string(),
        }),
    ));
    let HandoverRun {
        mut pipeline,
        transport,
        artifacts,
        repository,
        dir,
    } = run;

    let first = pipeline.ask("first request", &artifacts);
    assert!(
        matches!(first, Step::SummaryFailed { .. }),
        "the summary failure is fatal: {first:?}"
    );
    let names = [
        "plan.md",
        "spec.md",
        "ticket-triage.md",
        "queue-report.md",
        "review.md",
        "tickets/1-verify-command-output.md",
    ];
    let before: Vec<Vec<u8>> = names
        .iter()
        .map(|name| fs::read(artifacts.join(name)).expect("the run wrote its artifact"))
        .collect();
    let second = pipeline.ask("second request", &artifacts);
    assert!(matches!(second, Step::Retry), "{second:?}");

    let requests = transport.requests();
    let messages = sent_messages(&requests[8]);
    assert_eq!(messages[1]["content"], plan_markdown);
    assert_no_downstream_handover(messages);
    for (name, bytes) in names.iter().zip(before) {
        assert_eq!(
            fs::read(artifacts.join(name)).expect("the artifact survives"),
            bytes,
            "{name} changed after the fatal summary failure"
        );
    }

    drop(repository);
    let _ = fs::remove_dir_all(&dir);
}

/// Empty/zero boundary: a plan reply with no reasoning leaves the next user
/// message plain, in order, with no synthetic reasoning or content.
#[test]
fn when_plan_resumes_given_no_prior_reasoning_then_appends_plain_user_message() {
    let run = handover_run(vec![HandoverReply::silent(), HandoverReply::silent()]);
    let HandoverRun {
        mut pipeline,
        transport,
        artifacts,
        repository,
        dir,
    } = run;

    assert!(matches!(
        pipeline.ask("first request", &artifacts),
        Step::Retry
    ));
    assert!(matches!(
        pipeline.ask("second request", &artifacts),
        Step::Retry
    ));

    let requests = transport.requests();
    let messages = sent_messages(&requests[1]);
    assert_eq!(
        messages.len(),
        4,
        "system, first user, first reply, second user"
    );
    assert_eq!(messages[2]["role"], "assistant");
    assert!(
        messages[2].get("reasoning_content").is_none(),
        "no reasoning was introduced"
    );
    assert!(
        messages[2]["content"].is_null(),
        "no narration was introduced: {}",
        messages[2]
    );
    assert_eq!(messages[3]["role"], "user");
    assert_eq!(messages[3]["content"], "second request");
    assert!(
        messages[3].get("reasoning_content").is_none(),
        "the appended user message is plain"
    );

    drop(repository);
    let _ = fs::remove_dir_all(&dir);
}

/// Minimum valid boundary: even a minimal typed plan is reset through its own
/// canonical renderer, not through the model's original document.
#[test]
fn when_plan_handoff_given_minimal_valid_plan_then_uses_canonical_plan_rendering() {
    let plan = Plan {
        goal: "Minimal handover goal".to_string(),
        decisions: Vec::new(),
        defaults: Vec::new(),
        constraints: Vec::new(),
        open_questions: Vec::new(),
    };
    let canonical = plan.to_markdown();
    let raw = format!("\r\n  {}  \r\n\r\n", canonical.replace('\n', "\r\n"));
    assert_ne!(
        raw, canonical,
        "the raw document must differ from the renderer"
    );

    let run = handover_run(vec![
        HandoverReply::assistant(
            Some("Exploring a minimal plan."),
            None,
            &[(
                "finish_plan",
                serde_json::json!({ "plan": raw }).to_string(),
            )],
        ),
        HandoverReply::assistant(Some("No specification follows."), None, &[]),
        HandoverReply::silent(),
    ]);
    let HandoverRun {
        mut pipeline,
        transport,
        artifacts,
        repository,
        dir,
    } = run;

    assert!(matches!(
        pipeline.ask("first request", &artifacts),
        Step::Retry
    ));
    assert!(matches!(
        pipeline.ask("second request", &artifacts),
        Step::Retry
    ));

    let requests = transport.requests();
    let messages = sent_messages(&requests[2]);
    let handover = messages[1]["content"]
        .as_str()
        .expect("the handover carries the minimal plan");
    assert!(
        handover.starts_with(&canonical),
        "the minimal plan is rendered canonically: {handover}"
    );
    assert!(
        !handover.contains('\r'),
        "the canonical handover uses LF, not the model's CRLF: {handover:?}"
    );
    assert!(!handover.contains("Exploring a minimal plan."));

    drop(repository);
    let _ = fs::remove_dir_all(&dir);
}

/// Core entry point: a long downstream outcome keeps both its beginning and
/// its ending around the explicit omission marker after the agreed plan, and
/// the run's artifacts keep their original typed bytes.
#[test]
fn when_resuming_plan_given_long_downstream_outcome_then_preserves_both_ends_around_omission_marker()
 {
    let plan = common::harmless_plan_fixture();
    let plan_markdown = plan.to_markdown();
    let explanation = format!("BEGIN-DIAGNOSTIC {} END-DIAGNOSTIC", "m".repeat(5_000));
    let source = blocked_triage_source(&explanation);
    let marker = "...[omitted]...";
    let expected_prefix = &source[..293];
    let expected_suffix = &source[source.len() - 292..];
    let run = handover_run(vec![
        finish_plan_reply(&plan_markdown),
        finish_spec_reply(),
        finish_tickets_reply(),
        HandoverReply::tool("finish_triage", blocked_triage_arguments(&explanation)),
        HandoverReply::silent(),
    ]);
    let HandoverRun {
        mut pipeline,
        transport,
        artifacts,
        repository,
        dir,
    } = run;

    let first = pipeline.ask("first request", &artifacts);
    assert!(
        matches!(first, Step::Retry),
        "the gated draft set is retryable: {first:?}"
    );
    let names = [
        "plan.md",
        "spec.md",
        "ticket-triage.md",
        "tickets/1-verify-command-output.md",
    ];
    let before: Vec<Vec<u8>> = names
        .iter()
        .map(|name| fs::read(artifacts.join(name)).expect("the run wrote its artifact"))
        .collect();

    let second = pipeline.ask("second request", &artifacts);
    assert!(matches!(second, Step::Retry), "{second:?}");

    let requests = transport.requests();
    let messages = sent_messages(&requests[4]);
    let handover = messages[1]["content"]
        .as_str()
        .expect("the triage handover carries text");
    assert!(
        handover.starts_with(&plan_markdown),
        "the agreed plan stays first: {handover}"
    );
    assert!(handover.contains("[DOWNSTREAM RETRY]"), "{handover}");
    assert!(handover.contains("phase: triage"), "{handover}");
    let outcome = handover_outcome(handover);
    assert_eq!(
        outcome,
        format!("{expected_prefix}{marker}{expected_suffix}"),
        "the outcome keeps the source beginning and ending around the marker"
    );
    assert_eq!(
        outcome.matches(marker).count(),
        1,
        "exactly one omission marker: {outcome:?}"
    );
    assert!(
        outcome.starts_with(expected_prefix),
        "the source beginning is retained: {outcome:?}"
    );
    assert!(
        outcome.ends_with(expected_suffix),
        "the source ending is retained: {outcome:?}"
    );
    assert!(
        outcome.starts_with(&format!("{}BEGIN-DIAGNOSTIC", blocked_triage_source(""))),
        "the named blocker leads the bounded outcome: {outcome:?}"
    );
    assert!(
        outcome.ends_with("END-DIAGNOSTIC"),
        "the ending diagnostic survives: {outcome:?}"
    );
    assert!(
        !outcome.contains(&"m".repeat(400)),
        "middle source content is omitted: {outcome:?}"
    );
    assert!(
        outcome.len() <= 600,
        "the bounded outcome stays within 600 UTF-8 bytes: {}",
        outcome.len()
    );

    for (name, bytes) in names.iter().zip(before) {
        assert_eq!(
            fs::read(artifacts.join(name)).expect("the artifact survives"),
            bytes,
            "{name} changed while the handover was updated"
        );
    }

    drop(repository);
    let _ = fs::remove_dir_all(&dir);
}

/// Malformed payload: a rejected finish_plan call is a tool error, never an
/// agreed-plan handover, so no plan is fabricated before the boundary accepts
/// one.
#[test]
fn when_plan_handoff_given_rejected_finish_call_then_does_not_create_agreed_plan_handover() {
    let run = handover_run(vec![
        finish_plan_reply("this is not a plan document"),
        HandoverReply::silent(),
        HandoverReply::silent(),
    ]);
    let HandoverRun {
        mut pipeline,
        transport,
        artifacts,
        repository,
        dir,
    } = run;

    let first = pipeline.ask("first request", &artifacts);
    assert!(
        matches!(first, Step::Retry),
        "the rejected handoff is retryable: {first:?}"
    );
    assert!(
        !artifacts.join("plan.md").exists(),
        "a rejected finish call writes no plan artifact"
    );
    let second = pipeline.ask("second request", &artifacts);
    assert!(matches!(second, Step::Retry), "{second:?}");

    let requests = transport.requests();
    let messages = sent_messages(&requests[2]);
    assert_no_downstream_handover(messages);
    assert_no_agreed_plan(messages);
    let rejected = messages
        .iter()
        .find(|message| message["role"] == "tool")
        .and_then(|message| message["content"].as_str())
        .expect("the rejected call produced a tool result");
    assert!(
        rejected.contains("finish_plan") && rejected.contains("could not be parsed"),
        "the tool result names the rejected handoff: {rejected}"
    );

    drop(repository);
    let _ = fs::remove_dir_all(&dir);
}

/// Unauthorized/forbidden boundary: a forbidden tool call and a provider refusal
/// in the plan conversation keep their existing retry boundary; neither becomes
/// a downstream phase outcome.
#[test]
fn when_plan_conversation_given_forbidden_tool_or_provider_failure_then_preserves_existing_retry_boundary()
 {
    let run = handover_run(vec![
        HandoverReply::tool("task_complete", "{}".to_string()),
        HandoverReply::silent(),
        HandoverReply::silent(),
    ]);
    let HandoverRun {
        mut pipeline,
        transport,
        artifacts,
        repository,
        dir,
    } = run;

    let first = pipeline.ask("first request", &artifacts);
    assert!(
        matches!(first, Step::Retry),
        "the forbidden call is retryable: {first:?}"
    );
    assert!(!artifacts.join("plan.md").exists());
    let second = pipeline.ask("second request", &artifacts);
    assert!(matches!(second, Step::Retry), "{second:?}");

    let requests = transport.requests();
    let messages = sent_messages(&requests[2]);
    assert_no_downstream_handover(messages);
    assert_no_agreed_plan(messages);
    let rejected = messages
        .iter()
        .find(|message| message["role"] == "tool")
        .and_then(|message| message["content"].as_str())
        .expect("the forbidden call produced a tool result");
    assert!(
        rejected.contains("task_complete"),
        "the tool result names the forbidden call: {rejected}"
    );

    drop(repository);
    let _ = fs::remove_dir_all(&dir);

    let run = handover_run(vec![
        HandoverReply::Error(CallError::Api {
            status: 400,
            message: "the plan endpoint refused the request".to_string(),
        }),
        HandoverReply::silent(),
    ]);
    let HandoverRun {
        mut pipeline,
        transport,
        artifacts,
        repository,
        dir,
    } = run;

    let first = pipeline.ask("first request", &artifacts);
    assert!(
        matches!(first, Step::Retry),
        "the plan failure is retryable: {first:?}"
    );
    let second = pipeline.ask("second request", &artifacts);
    assert!(matches!(second, Step::Retry), "{second:?}");

    let requests = transport.requests();
    let messages = sent_messages(&requests[1]);
    assert_eq!(
        messages.len(),
        3,
        "system, the failed user turn, then the next user turn"
    );
    assert_eq!(messages[1]["role"], "user");
    assert_eq!(messages[1]["content"], "first request");
    assert_eq!(messages[2]["content"], "second request");
    assert_no_downstream_handover(messages);
    assert_no_agreed_plan(messages);

    drop(repository);
    let _ = fs::remove_dir_all(&dir);
}

/// The one outcome line of a downstream handover, without its label.
fn handover_outcome(handover: &str) -> &str {
    handover
        .lines()
        .find_map(|line| line.strip_prefix("outcome: "))
        .unwrap_or_else(|| panic!("the handover carries no outcome line: {handover}"))
}

/// The exact downstream detail a blocked triage report produces: the draft
/// ticket's id and state followed by the model's blocker explanation.
fn blocked_triage_source(explanation: &str) -> String {
    let ticket = harmless_ticket_set()
        .tickets
        .first()
        .cloned()
        .expect("the harmless fixture has one ticket");
    format!("{} (needs-info): {explanation}", ticket.id.trim())
}

/// Drive one blocked triage pass through a full plan resumption and return the
/// resumed request's messages plus the canonical plan handover.
fn resume_after_blocked_triage(explanation: &str) -> (Vec<serde_json::Value>, String) {
    let plan = common::harmless_plan_fixture();
    let plan_markdown = plan.to_markdown();
    let run = handover_run(vec![
        finish_plan_reply(&plan_markdown),
        finish_spec_reply(),
        finish_tickets_reply(),
        HandoverReply::tool("finish_triage", blocked_triage_arguments(explanation)),
        HandoverReply::silent(),
    ]);
    let HandoverRun {
        mut pipeline,
        transport,
        artifacts,
        repository,
        dir,
    } = run;

    assert!(
        matches!(pipeline.ask("first request", &artifacts), Step::Retry),
        "a blocked draft set is retryable"
    );
    assert!(
        matches!(pipeline.ask("second request", &artifacts), Step::Retry),
        "the resumed plan turn ends in another retry"
    );

    let requests = transport.requests();
    assert_eq!(
        requests.len(),
        5,
        "plan, spec, tickets, triage and the resumed plan"
    );
    let messages = sent_messages(&requests[4]).clone();

    drop(repository);
    let _ = fs::remove_dir_all(&dir);
    (messages, plan_markdown)
}

/// Regression: the next user boundary clears every earlier message's reasoning
/// and changes nothing else -- roles, content, tool calls and tool results stay
/// exactly as the provider exchanged them.
#[test]
fn when_resuming_plan_given_prior_reasoning_then_clears_reasoning_without_changing_other_message_fields()
 {
    let run = handover_run(vec![
        HandoverReply::assistant(
            Some("Reviewing the request."),
            Some("private plan reasoning"),
            &[(
                "finish_plan",
                serde_json::json!({ "plan": "this is not a plan document" }).to_string(),
            )],
        ),
        HandoverReply::silent(),
        HandoverReply::silent(),
    ]);
    let HandoverRun {
        mut pipeline,
        transport,
        artifacts,
        repository,
        dir,
    } = run;

    assert!(matches!(
        pipeline.ask("first request", &artifacts),
        Step::Retry
    ));
    assert!(matches!(
        pipeline.ask("second request", &artifacts),
        Step::Retry
    ));

    let requests = transport.requests();
    assert_eq!(requests.len(), 3, "the tool step and the resumed plan");
    let before = sent_messages(&requests[1]);
    let after = sent_messages(&requests[2]);
    assert_eq!(
        before.len(),
        4,
        "system, the user turn, the assistant tool call and its result"
    );
    assert_eq!(
        after.len(),
        6,
        "the turn-end assistant reply and the appended user turn"
    );
    assert_eq!(
        before[2]["reasoning_content"], "private plan reasoning",
        "the provider reply carried reasoning into the tool-result request"
    );

    let mut without_reasoning = before[2].clone();
    without_reasoning
        .as_object_mut()
        .expect("the assistant message is an object")
        .remove("reasoning_content");
    assert_eq!(
        after[2], without_reasoning,
        "only reasoning_content changes at the user boundary"
    );
    assert_eq!(
        after[3], before[3],
        "the tool result is unchanged at the user boundary"
    );
    assert_eq!(after[4]["role"], "assistant");
    assert_eq!(after[5]["role"], "user");
    assert_eq!(after[5]["content"], "second request");

    drop(repository);
    let _ = fs::remove_dir_all(&dir);
}

/// Empty/zero boundary: with no downstream phase outcome, the next plan request
/// sees the canonical plan handover and no retry section at all.
#[test]
fn when_resuming_plan_given_no_downstream_outcome_then_sends_canonical_plan_without_retry_section()
{
    let plan = common::harmless_plan_fixture();
    let plan_markdown = plan.to_markdown();
    let run = handover_run(vec![
        finish_plan_reply(&plan_markdown),
        HandoverReply::silent(),
    ]);
    let HandoverRun {
        mut pipeline,
        transport,
        artifacts,
        repository,
        dir,
    } = run;
    fs::create_dir_all(artifacts.join("plan.md"))
        .expect("the plan path is plantable as a directory");

    let first = pipeline.ask("first request", &artifacts);
    assert!(
        matches!(
            first,
            Step::WriteFailed {
                artifact: "plan",
                ..
            }
        ),
        "the failed plan write is fatal and records no outcome: {first:?}"
    );
    assert!(matches!(
        pipeline.ask("second request", &artifacts),
        Step::Retry
    ));

    let requests = transport.requests();
    let messages = sent_messages(&requests[1]);
    assert_eq!(
        messages.len(),
        3,
        "system, the canonical handover and the new user line"
    );
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(
        messages[0]["content"], requests[0]["messages"][0]["content"],
        "the system prompt is unchanged"
    );
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(
        messages[1]["content"], plan_markdown,
        "a run with no downstream outcome hands over exactly the canonical plan"
    );
    assert_eq!(messages[2]["role"], "user");
    assert_eq!(messages[2]["content"], "second request");
    assert_no_downstream_handover(messages);

    drop(repository);
    let _ = fs::remove_dir_all(&dir);
}

/// Minimum valid boundary: a collapsed detail of exactly 600 UTF-8 bytes is
/// preserved in full, with no omission marker.
#[test]
fn when_resuming_plan_given_collapsed_outcome_at_six_hundred_bytes_then_preserves_detail_without_marker()
 {
    let plan = common::harmless_plan_fixture();
    let plan_markdown = plan.to_markdown();
    let ticket = harmless_ticket_set()
        .tickets
        .first()
        .cloned()
        .expect("the harmless fixture has one ticket");
    let prefix = format!("{} (needs-info): ", ticket.id.trim());
    let detail = format!("{prefix}{}", "x".repeat(600 - prefix.len()));
    assert_eq!(
        detail.len(),
        600,
        "the scripted detail is exactly at the cap"
    );

    let run = handover_run(vec![
        finish_plan_reply(&plan_markdown),
        finish_spec_reply(),
        finish_tickets_reply(),
        HandoverReply::tool(
            "finish_triage",
            blocked_triage_arguments(&detail[prefix.len()..]),
        ),
        HandoverReply::silent(),
    ]);
    let HandoverRun {
        mut pipeline,
        transport,
        artifacts,
        repository,
        dir,
    } = run;

    assert!(matches!(
        pipeline.ask("first request", &artifacts),
        Step::Retry
    ));
    assert!(matches!(
        pipeline.ask("second request", &artifacts),
        Step::Retry
    ));

    let requests = transport.requests();
    let handover = sent_messages(&requests[4])[1]["content"]
        .as_str()
        .expect("the triage handover carries text");
    assert!(handover.starts_with(&plan_markdown));
    let outcome = handover_outcome(handover);
    assert_eq!(
        outcome, detail,
        "a detail at the 600-byte limit is preserved verbatim"
    );
    assert!(
        !outcome.contains("...[omitted]..."),
        "a detail at the limit carries no omission marker: {outcome:?}"
    );

    drop(repository);
    let _ = fs::remove_dir_all(&dir);
}

/// Empty/zero boundary: an absent or whitespace-only downstream outcome
/// collapses to an empty detail, preserved as an empty outcome line.
#[test]
fn when_resuming_plan_given_empty_downstream_outcome_then_preserves_empty_collapsed_detail() {
    let plan = common::harmless_plan_fixture();
    let plan_markdown = plan.to_markdown();
    for detail in ["", " \t\n\u{2003}\u{00A0} "] {
        let transport = Arc::new(HandoverTransport::new(vec![HandoverReply::silent()]));
        let config = Config::new(
            "http://script.test/v1/chat/completions",
            "test-model",
            Some("test-key".to_string()),
        )
        .with_transport(transport.clone());
        let mut session = Session::new(&config, common::scope());
        session.record_downstream_retry(&plan, "triage", detail);

        assert!(matches!(
            session.ask("second request"),
            PlanOutcome::TurnEnd
        ));

        let requests = transport.requests();
        assert_eq!(requests.len(), 1, "the resumed plan makes one request");
        let messages = sent_messages(&requests[0]);
        assert_eq!(messages.len(), 3, "system, handover and the new user line");
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[2]["role"], "user");
        assert_eq!(messages[2]["content"], "second request");
        let handover = messages[1]["content"]
            .as_str()
            .expect("the downstream handover carries text");
        assert!(handover.starts_with(&plan_markdown));
        assert!(handover.contains("[DOWNSTREAM RETRY]"), "{handover}");
        assert!(handover.contains("phase: triage"), "{handover}");
        let outcome = handover_outcome(handover);
        assert_eq!(
            outcome, "",
            "an empty or whitespace-only detail collapses to an empty preserved outcome"
        );
        assert!(
            !handover.contains("...[omitted]..."),
            "empty collapsed detail carries no omission marker: {handover:?}"
        );
    }
}

/// Upper boundary: a multibyte outcome over the cap retains a whole-character
/// source prefix and source suffix around the marker within the 600-byte bound.
#[test]
fn when_resuming_plan_given_multibyte_outcome_over_limit_then_retains_utf8_safe_prefix_and_suffix_within_byte_cap()
 {
    let explanation = "é".repeat(1_000);
    let source = blocked_triage_source(&explanation);
    let marker = "...[omitted]...";
    let (messages, _plan_markdown) = resume_after_blocked_triage(&explanation);

    assert_eq!(messages.len(), 3);
    let handover = messages[1]["content"]
        .as_str()
        .expect("the triage handover carries text");
    let outcome = handover_outcome(handover);

    assert_eq!(
        outcome.matches(marker).count(),
        1,
        "exactly one omission marker: {outcome:?}"
    );
    let (kept_prefix, kept_suffix) = outcome
        .split_once(marker)
        .expect("the over-limit outcome carries the omission marker");
    assert_eq!(
        kept_prefix.len(),
        292,
        "the prefix backs up to a character boundary instead of splitting a multibyte character"
    );
    assert_eq!(
        kept_suffix.len(),
        292,
        "the suffix keeps a whole-character ending"
    );
    assert_eq!(
        outcome.len(),
        599,
        "the marker and both retained fragments stay within the byte cap"
    );
    assert!(
        outcome.len() <= 600,
        "the rendered outcome is at most 600 UTF-8 bytes: {}",
        outcome.len()
    );
    assert!(
        source.starts_with(kept_prefix),
        "the source beginning is retained: {kept_prefix:?}"
    );
    assert!(
        source.ends_with(kept_suffix),
        "the source ending is retained: {kept_suffix:?}"
    );
    assert!(
        kept_prefix[blocked_triage_source("").len()..]
            .chars()
            .all(|character| character == 'é'),
        "the kept prefix is whole multibyte characters: {kept_prefix:?}"
    );
    assert!(
        kept_suffix.chars().all(|character| character == 'é'),
        "the kept suffix is whole multibyte characters: {kept_suffix:?}"
    );
    assert_eq!(
        outcome.matches('é').count(),
        127 + 146,
        "only the retained whole characters survive"
    );
    assert!(
        !outcome.contains('\u{FFFD}'),
        "the rendered outcome is valid UTF-8 with no replacement character: {outcome:?}"
    );
}

/// Malformed payload: runs of Unicode whitespace collapse to one ASCII space
/// before a multibyte outcome is truncated on character boundaries.
#[test]
fn when_resuming_plan_given_unicode_whitespace_and_multibyte_boundaries_then_collapses_and_truncates_without_invalid_utf8()
 {
    let messy = "DÉTAIL\u{2003}BLOQUÉ\u{00A0}\t🚀\n ".repeat(300);
    let (messages, _plan_markdown) = resume_after_blocked_triage(&messy);
    let handover = messages[1]["content"]
        .as_str()
        .expect("the triage handover carries text")
        .to_string();
    let outcome = handover_outcome(&handover);

    assert!(
        outcome.contains("DÉTAIL BLOQUÉ 🚀"),
        "the multibyte words are collapsed to one line: {outcome:?}"
    );
    for forbidden in ['\n', '\t', '\u{2003}', '\u{00A0}'] {
        assert!(
            !outcome.contains(forbidden),
            "no unicode whitespace survives as {forbidden:?}: {outcome:?}"
        );
    }
    assert!(
        !outcome.contains("  "),
        "runs of whitespace collapse to one space: {outcome:?}"
    );
    assert_eq!(
        outcome.matches("...[omitted]...").count(),
        1,
        "exactly one omission marker: {outcome:?}"
    );
    assert!(
        outcome.len() <= 600,
        "the collapsed outcome is bounded to 600 UTF-8 bytes: {}",
        outcome.len()
    );
    assert!(
        outcome.starts_with(&format!("{}DÉTAIL", blocked_triage_source(""))),
        "the source beginning is retained: {outcome:?}"
    );
    assert!(
        outcome.ends_with('🚀'),
        "the source ending is retained: {outcome:?}"
    );
    assert!(
        !outcome.contains('\u{FFFD}'),
        "the rendered outcome is valid UTF-8 with no replacement character: {outcome:?}"
    );
    assert!(
        !outcome.contains(&"DÉTAIL BLOQUÉ 🚀".repeat(30)),
        "middle source content is omitted: {outcome:?}"
    );
}

/// Contract and invariant: a long outcome leaves the canonical plan, the
/// returning phase and the handover field order in their existing positions.
#[test]
fn when_resuming_plan_given_long_outcome_then_preserves_canonical_plan_phase_and_handover_field_order()
 {
    let explanation = format!("CONTRACT-START {} CONTRACT-END", "c".repeat(3_000));
    let source = blocked_triage_source(&explanation);
    let marker = "...[omitted]...";
    let expected_outcome = format!(
        "{}{}{}",
        &source[..293],
        marker,
        &source[source.len() - 292..]
    );
    let (messages, plan_markdown) = resume_after_blocked_triage(&explanation);

    assert_eq!(messages.len(), 3, "system, handover and the new user line");
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(messages[2]["role"], "user");
    assert_eq!(messages[2]["content"], "second request");
    assert!(
        messages[2].get("reasoning_content").is_none(),
        "the new user message carries no reasoning"
    );

    let handover = messages[1]["content"]
        .as_str()
        .expect("the downstream handover carries text");
    let expected = format!(
        "{plan_markdown}\n[DOWNSTREAM RETRY]\nphase: triage\noutcome: {expected_outcome}\n"
    );
    assert_eq!(
        handover, expected,
        "the canonical plan, returning phase and outcome keep their existing order"
    );
    assert_eq!(
        handover.matches("[DOWNSTREAM RETRY]").count(),
        1,
        "one downstream retry section: {handover:?}"
    );
    assert_eq!(
        handover.matches("phase: triage").count(),
        1,
        "the returning phase is intact: {handover:?}"
    );
    let phase_index = handover
        .find("phase: triage")
        .expect("the phase field is present");
    let outcome_index = handover
        .find("outcome: ")
        .expect("the outcome field is present");
    let marker_index = handover.find(marker).expect("the marker is present");
    assert!(
        phase_index < outcome_index,
        "the phase precedes the outcome"
    );
    assert!(
        outcome_index < marker_index,
        "the marker sits inside the outcome after its label"
    );
}

/// Regression: the bounded downstream outcome keeps the ending diagnostic even
/// when the middle of a long detail is omitted.
#[test]
fn when_recording_downstream_outcome_given_long_detail_then_retains_ending_diagnostics() {
    let explanation = format!("LEADING-DETAIL {} FINAL-GATE-DIAGNOSTIC", "x".repeat(4_000));
    let (messages, plan_markdown) = resume_after_blocked_triage(&explanation);
    let handover = messages[1]["content"]
        .as_str()
        .expect("the triage handover carries text");
    assert!(handover.starts_with(&plan_markdown));
    let outcome = handover_outcome(handover);
    assert!(
        outcome.starts_with(&format!("{}LEADING-DETAIL", blocked_triage_source(""))),
        "the named detail leads the bounded outcome: {outcome:?}"
    );
    assert!(
        outcome.ends_with("FINAL-GATE-DIAGNOSTIC"),
        "the ending diagnostic survives the bound: {outcome:?}"
    );
    assert!(
        outcome.contains("...[omitted]..."),
        "the omitted middle is named: {outcome:?}"
    );
    assert_eq!(
        outcome.len(),
        600,
        "the bounded outcome fills the byte cap without splitting a character: {outcome:?}"
    );
    assert!(
        !outcome.contains(&"x".repeat(400)),
        "middle source content is omitted: {outcome:?}"
    );
}

/// Malformed payload: a finish call with no blocker detail is refused, and the
/// recoverable return records an explicit named triage outcome instead of an
/// empty section.
#[test]
fn when_recovering_given_missing_blocker_detail_then_records_an_explicit_outcome() {
    let plan = common::harmless_plan_fixture();
    let plan_markdown = plan.to_markdown();
    let mut triage: serde_json::Value = serde_json::from_str(&blocked_triage_arguments("ignored"))
        .expect("the blocked triage fixture is JSON");
    for classification in triage["triage"]["tickets"]
        .as_array_mut()
        .expect("the triage fixture carries tickets")
    {
        classification
            .as_object_mut()
            .expect("a classification is an object")
            .remove("blocker_explanation");
    }

    let run = handover_run(vec![
        finish_plan_reply(&plan_markdown),
        finish_spec_reply(),
        finish_tickets_reply(),
        HandoverReply::tool("finish_triage", triage.to_string()),
        HandoverReply::silent(),
        HandoverReply::silent(),
    ]);
    let HandoverRun {
        mut pipeline,
        transport,
        artifacts,
        repository,
        dir,
    } = run;

    let first = pipeline.ask("first request", &artifacts);
    assert!(
        matches!(first, Step::Retry),
        "an invalid triage report is recoverable: {first:?}"
    );
    assert!(matches!(
        pipeline.ask("second request", &artifacts),
        Step::Retry
    ));

    let requests = transport.requests();
    assert_eq!(
        requests.len(),
        6,
        "plan, spec, tickets, rejected triage, triage turn end and the resumed plan"
    );
    let rejected = sent_messages(&requests[4])
        .iter()
        .find(|message| message["role"] == "tool")
        .and_then(|message| message["content"].as_str())
        .expect("the rejected triage call produced a tool result");
    assert!(
        rejected.contains("blocker explanation"),
        "the refusal names the missing blocker detail: {rejected}"
    );

    let handover = sent_messages(&requests[5])[1]["content"]
        .as_str()
        .expect("the triage handover carries text");
    assert!(handover.starts_with(&plan_markdown));
    assert!(handover.contains("[DOWNSTREAM RETRY]"), "{handover}");
    assert!(handover.contains("phase: triage"), "{handover}");
    let outcome = handover_outcome(handover);
    assert!(
        !outcome.trim().is_empty(),
        "a missing blocker detail still records an explicit outcome"
    );
    assert!(
        outcome.contains("triage"),
        "the explicit outcome names the returning phase: {outcome:?}"
    );

    drop(repository);
    let _ = fs::remove_dir_all(&dir);
}

/// Unauthorized/forbidden boundary: a provider refusal on the next plan request
/// is a fatal API result -- one attempt, no retry -- and the request it refused
/// still carried the canonical plan context, which the failure does not
/// overwrite.
#[test]
fn when_resuming_plan_given_provider_denies_next_request_then_keeps_canonical_plan_context_and_fatal_api_result()
 {
    let plan = common::harmless_plan_fixture();
    let plan_markdown = plan.to_markdown();
    let run = handover_run(vec![
        finish_plan_reply(&plan_markdown),
        HandoverReply::silent(),
        HandoverReply::Error(CallError::Api {
            status: 403,
            message: "the plan endpoint forbids the request".to_string(),
        }),
        HandoverReply::silent(),
    ]);
    let HandoverRun {
        mut pipeline,
        transport,
        artifacts,
        repository,
        dir,
    } = run;

    assert!(matches!(
        pipeline.ask("first request", &artifacts),
        Step::Retry
    ));
    let denied = pipeline.ask("second request", &artifacts);
    assert!(
        matches!(denied, Step::Retry),
        "the provider refusal is reported, not retried: {denied:?}"
    );
    assert!(matches!(
        pipeline.ask("third request", &artifacts),
        Step::Retry
    ));

    let requests = transport.requests();
    assert_eq!(
        requests.len(),
        4,
        "the refusal is attempted once, then the next user turn is sent"
    );
    let refused = sent_messages(&requests[2]);
    assert_eq!(
        refused.len(),
        3,
        "the refused request still carried the system prompt, the handover and the new line"
    );
    assert_eq!(
        refused[0]["content"], requests[0]["messages"][0]["content"],
        "the system prompt is unchanged"
    );
    assert_eq!(refused[1]["role"], "assistant");
    let handover = refused[1]["content"]
        .as_str()
        .expect("the assistant handover carries text");
    assert_eq!(
        handover,
        sent_messages(&requests[3])[1]["content"],
        "the fatal API failure does not overwrite the canonical plan handover"
    );
    assert!(
        handover.starts_with(&plan_markdown),
        "the canonical plan leads the refused context: {handover}"
    );
    assert!(handover.contains("[DOWNSTREAM RETRY]"), "{handover}");
    assert!(handover.contains("phase: spec"), "{handover}");
    assert_eq!(refused[2]["role"], "user");
    assert_eq!(refused[2]["content"], "second request");
    for (index, message) in refused.iter().enumerate() {
        assert!(
            message.get("reasoning_content").is_none(),
            "message {index} carries reasoning into the refused request: {message}"
        );
    }

    drop(repository);
    let _ = fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Per-invocation session spill placement
//
// Each ask carries the session directory its artifacts belong to. These tests
// drive the real production pipeline -- real Bash, real filesystem writes and a
// real disposable Git repository -- and script only the external model
// transport. They observe the model-facing tool result and the files the run
// actually wrote, never private pipeline state.
// ---------------------------------------------------------------------------

/// The complete log bytes a Bash spill carries: the verbatim command preamble
/// followed by stdout and then stderr, exactly as captured.
fn spilled_log_bytes(command: &str, output: &[u8]) -> Vec<u8> {
    let mut bytes = format!("Command:\n{command}\n\nOutput:\n").into_bytes();
    bytes.extend_from_slice(output);
    bytes
}

/// The concatenated stdout-then-stderr bytes a fixture command emits.
fn emitted_bytes(stdout: (usize, char), stderr: (usize, char)) -> Vec<u8> {
    let mut bytes = vec![stdout.1 as u8; stdout.0];
    bytes.extend(std::iter::repeat_n(stderr.1 as u8, stderr.0));
    bytes
}

/// A command that makes one visible repository change and then emits exactly
/// the requested stdout and stderr bytes, so the run can commit and complete.
fn emit_with_change(root: &Path, stdout: (usize, char), stderr: (usize, char)) -> String {
    format!(
        "printf 'agent change\\n' >> '{}'; \
         head -c {} /dev/zero | tr '\\0' '{}'; \
         head -c {} /dev/zero | tr '\\0' '{}' >&2",
        root.join("agent-change.txt").display(),
        stdout.0,
        stdout.1,
        stderr.0,
        stderr.1
    )
}

/// Every tool-result message text the scripted transport ever received, in
/// request order. Duplicates across turns are kept: a digest that only survives
/// one turn is still visible here.
fn recorded_tool_results(transport: &common::ScriptedTransport) -> Vec<String> {
    let requests = transport.requests.lock().unwrap();
    let mut results = Vec::new();
    for request in requests.iter() {
        let Some(messages) = request["messages"].as_array() else {
            continue;
        };
        for message in messages {
            if message["role"] == "tool"
                && let Some(content) = message["content"].as_str()
            {
                results.push(content.to_string());
            }
        }
    }
    results
}

/// The digests the model was shown for spilled Bash results.
fn spill_digests(transport: &common::ScriptedTransport) -> Vec<String> {
    recorded_tool_results(transport)
        .into_iter()
        .filter(|content| content.contains("full output saved to"))
        .collect()
}

/// The single digest the model was shown for one spilled Bash result.
fn spill_digest(transport: &common::ScriptedTransport) -> String {
    let mut digests = spill_digests(transport);
    assert_eq!(
        digests.len(),
        1,
        "exactly one spilled Bash result must reach the model: {digests:?}"
    );
    digests.remove(0)
}

/// The tools-output directory under one supplied session directory.
fn session_tools_output(session: &Path) -> PathBuf {
    session.join("tools-output")
}

/// The files in one session's tools-output directory, in name order.
fn session_spill_files(session: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(session_tools_output(session))
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// No newly created spill may appear outside the invocation's session.
fn assert_no_global_spill(repository: &common::TempRepo) {
    let root = repository.path();
    assert!(
        !root.join(".ejaj/tool-output").exists(),
        "the previous project-wide .ejaj/tool-output location must stay unused"
    );
    assert!(
        !root.join("tools-output").exists(),
        "a project-root tools-output directory must stay unused"
    );
    assert!(
        !root.join(".ejaj/step-1-call-1.log").exists(),
        "a spill must not be written directly under .ejaj/"
    );
    let direct_spill = fs::read_dir(root.join(".ejaj"))
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .any(|entry| entry.file_name().to_string_lossy().starts_with("step-"))
        })
        .unwrap_or(false);
    assert!(
        !direct_spill,
        "no spill file may be written directly under .ejaj/"
    );
}

#[test]
fn when_pipeline_ask_given_oversized_bash_output_then_returns_session_path_and_complete_log() {
    let repository = committed_repository();
    let session = repository.path().join(".ejaj/20261018-121255");
    let command = emit_with_change(repository.path(), (16_000, 'a'), (385, 'b'));
    let (config, transport) = common::scripted_pipeline_with_command(
        common::GateScript::Passing,
        common::SummaryScript::Summary,
        command.clone(),
    );

    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), &session);

    assert!(
        matches!(step, Step::Complete),
        "a resolved queue with a passing review completes: {step:?}"
    );

    let relative = ".ejaj/20261018-121255/tools-output/step-1-call-1.log";
    let digest = spill_digest(&transport);
    assert!(
        digest.contains(relative),
        "the model-facing digest must name the session path relative to the project root: {digest:?}"
    );
    assert!(
        digest.contains("tools.read") && digest.contains("execute_script"),
        "the digest must advise inspecting the complete log through execute_script: {digest:?}"
    );
    assert!(
        !digest.contains(".ejaj/tool-output"),
        "the digest must never name the legacy global directory: {digest:?}"
    );

    let spill = session.join("tools-output/step-1-call-1.log");
    assert!(spill.exists(), "the complete session log must exist");
    assert_eq!(
        fs::read(&spill).unwrap(),
        spilled_log_bytes(&command, &emitted_bytes((16_000, 'a'), (385, 'b'))),
        "the session log must carry the exact command preamble and complete stdout-then-stderr bytes"
    );
    assert_no_global_spill(&repository);

    let _ = fs::remove_dir_all(repository.path());
}

#[test]
fn when_pipeline_ask_given_distinct_session_directories_then_spills_each_result_to_its_owner() {
    let repository = committed_repository();
    let first_session = repository.path().join(".ejaj/20261018-121255");
    let second_session = repository.path().join(".ejaj/20261018-121256");
    let command = emit_with_change(repository.path(), (20_000, 'x'), (0, 'z'));
    let transport = Arc::new(
        common::ScriptedTransport::new()
            .with_bash_command(command.clone())
            .with_repeating_handoffs(),
    );
    let config = common::scripted_pipeline_with_transport(transport.clone());

    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let first = pipeline.ask(&common::harmless_plan_request(), &first_session);
    assert!(
        matches!(first, Step::Complete),
        "the first ask completes: {first:?}"
    );
    let second = pipeline.ask(&common::harmless_plan_request(), &second_session);
    assert!(
        matches!(second, Step::Complete),
        "a later ask with a different directory completes: {second:?}"
    );

    assert_eq!(
        session_spill_files(&first_session),
        vec!["step-1-call-1.log".to_string()],
        "the first ask's spill must stay in its own session"
    );
    assert_eq!(
        session_spill_files(&second_session),
        vec!["step-1-call-1.log".to_string()],
        "the later ask's spill must be written in the new directory, not the retained old one"
    );

    let first_log = first_session.join("tools-output/step-1-call-1.log");
    let second_log = second_session.join("tools-output/step-1-call-1.log");
    let expected = spilled_log_bytes(&command, &emitted_bytes((20_000, 'x'), (0, 'z')));
    assert_eq!(
        fs::read(&first_log).unwrap(),
        expected,
        "the first session log must be complete"
    );
    assert_eq!(
        fs::read(&second_log).unwrap(),
        expected,
        "the later session log must be complete"
    );

    let digests = spill_digests(&transport);
    assert!(
        digests
            .iter()
            .any(|digest| digest.contains(".ejaj/20261018-121255/tools-output/step-1-call-1.log")),
        "the first ask's digest must name its own session: {digests:?}"
    );
    assert!(
        digests
            .iter()
            .any(|digest| digest.contains(".ejaj/20261018-121256/tools-output/step-1-call-1.log")),
        "the later ask's digest must name the new session: {digests:?}"
    );
    assert_no_global_spill(&repository);

    let _ = fs::remove_dir_all(repository.path());
}

#[test]
fn when_pipeline_ask_given_session_spill_then_uses_root_relative_path_without_global_fallback() {
    let repository = committed_repository();
    let session = repository.path().join(".ejaj/root-relative");
    let command = emit_with_change(repository.path(), (17_000, 'r'), (0, 'z'));
    let (config, transport) = common::scripted_pipeline_with_command(
        common::GateScript::Passing,
        common::SummaryScript::Summary,
        command,
    );

    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), &session);
    assert!(
        matches!(step, Step::Complete),
        "a resolved queue with a passing review completes: {step:?}"
    );

    let digest = spill_digest(&transport);
    let relative = ".ejaj/root-relative/tools-output/step-1-call-1.log";
    assert!(
        digest.contains(relative),
        "the digest must carry the project-root-relative path: {digest:?}"
    );
    assert!(
        !digest.starts_with('/'),
        "the digest must not carry an absolute fallback path: {digest:?}"
    );
    assert!(
        session.join("tools-output/step-1-call-1.log").exists(),
        "the file the digest names must exist under the supplied session"
    );
    assert_no_global_spill(&repository);

    let _ = fs::remove_dir_all(repository.path());
}

#[test]
fn when_pipeline_ask_given_plan_script_and_gate_output_then_keeps_them_unspilled() {
    let repository = committed_repository();
    let session = repository.path().join(".ejaj/plan-script");
    let plan_script = r#"let text = "PLAN-SCRIPT-HEAD\n"; for (let i = 0; i < 20000; i += 1) { text += "p"; } return text + "\nPLAN-SCRIPT-TAIL";"#;
    let command = emit_with_change(repository.path(), (0, 'z'), (0, 'z'));
    let transport = Arc::new(
        common::ScriptedTransport::new()
            .with_plan_script(plan_script)
            .with_bash_command(command),
    );
    let config = common::scripted_pipeline_with_transport(transport.clone());

    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), &session);
    assert!(
        matches!(step, Step::Complete),
        "a resolved queue with a passing review completes: {step:?}"
    );

    let results = recorded_tool_results(&transport);
    let plan_result = results
        .iter()
        .find(|content| content.contains("PLAN-SCRIPT-HEAD"))
        .expect("the plan script result must be replayed to the plan agent");
    assert!(
        !plan_result.contains("full output saved"),
        "the plan script result must never be spilled: {plan_result:?}"
    );
    assert!(
        !session_tools_output(&session).exists(),
        "the plan script and the harmless code command must create no session spill directory"
    );

    let gate_command =
        "printf 'GATE-HEAD\\n'; head -c 20000 /dev/zero | tr '\\0' 'g'; printf '\\nGATE-TAIL\\n'";
    let gate_output = ejaj::tools::run_command(gate_command)
        .expect("the verification gate command should succeed");
    let expected = format!("GATE-HEAD\n{}\nGATE-TAIL\n", "g".repeat(20_000));
    assert_eq!(
        gate_output, expected,
        "the verification gate must return its complete output unchanged"
    );
    assert!(
        gate_output.len() > 16_384,
        "the gate case must exceed the model delivery cap"
    );
    assert!(
        !gate_output.contains("full output saved"),
        "the verification gate must never spill or name a saved file"
    );
    assert!(
        !session_tools_output(&session).exists(),
        "the gate output must not create a session spill directory"
    );

    let _ = fs::remove_dir_all(repository.path());
}

#[test]
fn when_pipeline_ask_given_empty_bash_output_then_creates_no_session_spill_directory() {
    let repository = committed_repository();
    let session = repository.path().join(".ejaj/empty-output");
    let command = emit_with_change(repository.path(), (0, 'z'), (0, 'z'));
    let (config, transport) = common::scripted_pipeline_with_command(
        common::GateScript::Passing,
        common::SummaryScript::Summary,
        command,
    );

    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), &session);
    assert!(
        matches!(step, Step::Complete),
        "a resolved queue with a passing review completes: {step:?}"
    );

    assert!(
        session.exists(),
        "the session artifact directory itself is created by the run"
    );
    assert!(
        !session_tools_output(&session).exists(),
        "forwarding the session context alone must create no spill directory for empty output"
    );
    assert!(
        spill_digests(&transport).is_empty(),
        "empty output must never produce a spill digest"
    );
    assert_no_global_spill(&repository);

    let _ = fs::remove_dir_all(repository.path());
}

#[test]
fn when_pipeline_ask_given_exact_limit_bash_output_then_creates_no_session_spill() {
    let repository = committed_repository();
    let session = repository.path().join(".ejaj/exact-limit");
    let command = emit_with_change(repository.path(), (16_384, 'm'), (0, 'z'));
    let (config, transport) = common::scripted_pipeline_with_command(
        common::GateScript::Passing,
        common::SummaryScript::Summary,
        command,
    );

    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), &session);
    assert!(
        matches!(step, Step::Complete),
        "a resolved queue with a passing review completes: {step:?}"
    );

    assert!(
        !session_tools_output(&session).exists(),
        "output of exactly the delivery cap must not be spilled"
    );
    assert!(
        spill_digests(&transport).is_empty(),
        "output of exactly the delivery cap must never produce a spill digest"
    );
    assert_no_global_spill(&repository);

    let _ = fs::remove_dir_all(repository.path());
}

#[test]
fn when_pipeline_ask_given_oversized_bash_output_then_creates_session_spill() {
    let repository = committed_repository();
    let session = repository.path().join(".ejaj/upper-boundary");
    let command = emit_with_change(repository.path(), (16_385, 'n'), (0, 'z'));
    let (config, transport) = common::scripted_pipeline_with_command(
        common::GateScript::Passing,
        common::SummaryScript::Summary,
        command.clone(),
    );

    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), &session);
    assert!(
        matches!(step, Step::Complete),
        "a resolved queue with a passing review completes: {step:?}"
    );

    assert_eq!(
        session_spill_files(&session),
        vec!["step-1-call-1.log".to_string()],
        "one byte over the delivery cap must create exactly one session log"
    );
    let spill = session.join("tools-output/step-1-call-1.log");
    assert_eq!(
        fs::read(&spill).unwrap(),
        spilled_log_bytes(&command, &emitted_bytes((16_385, 'n'), (0, 'z'))),
        "the session log must carry the complete output"
    );
    assert!(
        spill_digest(&transport).contains(".ejaj/upper-boundary/tools-output/step-1-call-1.log"),
        "the model-facing digest must name the session path"
    );
    assert_no_global_spill(&repository);

    let _ = fs::remove_dir_all(repository.path());
}

#[test]
fn when_pipeline_ask_given_invalid_utf8_bash_output_then_keeps_complete_raw_log() {
    let repository = committed_repository();
    let session = repository.path().join(".ejaj/invalid-utf8");
    let command = format!(
        "printf 'agent change\\n' >> '{}'; \
         printf 'BEGIN\\n'; \
         head -c 20000 /dev/zero | tr '\\0' '\\377'; \
         printf '\\nEND\\n'",
        repository.path().join("agent-change.txt").display()
    );
    let (config, transport) = common::scripted_pipeline_with_command(
        common::GateScript::Passing,
        common::SummaryScript::Summary,
        command.clone(),
    );

    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), &session);
    assert!(
        matches!(step, Step::Complete),
        "a resolved queue with a passing review completes: {step:?}"
    );

    let mut expected_output = b"BEGIN\n".to_vec();
    expected_output.extend(std::iter::repeat_n(0xffu8, 20_000));
    expected_output.extend_from_slice(b"\nEND\n");
    let spill = session.join("tools-output/step-1-call-1.log");
    assert_eq!(
        fs::read(&spill).unwrap(),
        spilled_log_bytes(&command, &expected_output),
        "the raw invalid bytes must survive after the complete command preamble"
    );

    let digest = spill_digest(&transport);
    assert!(
        digest.len() <= 16_384,
        "the digest must remain bounded: {} bytes",
        digest.len()
    );
    assert!(
        std::str::from_utf8(digest.as_bytes()).is_ok(),
        "the digest must remain valid UTF-8 text"
    );
    assert!(
        digest.contains("BEGIN") && digest.contains("END"),
        "the digest must keep a UTF-8-safe head and tail: {digest:?}"
    );
    assert!(
        digest.contains(".ejaj/invalid-utf8/tools-output/step-1-call-1.log"),
        "the digest must name the session path: {digest:?}"
    );
    assert_no_global_spill(&repository);

    let _ = fs::remove_dir_all(repository.path());
}

#[test]
fn when_pipeline_ask_given_retry_bash_spill_then_keeps_artifact_in_session_directory() {
    let repository = committed_repository();
    let session = repository.path().join(".ejaj/20261018-121255");
    // The first attempt spills and then ends its turn without declaring
    // completion, which is exactly what earns the single fresh retry. The
    // retry spills again from its own step-one bash call, so the second log
    // carries the collision suffix while staying in the same session.
    let first_command = emit_with_change(repository.path(), (16_385, 'r'), (0, 'z'));
    let retry_command = emit_with_change(repository.path(), (16_385, 's'), (0, 'z'));
    let transport = Arc::new(common::ScriptedTransport::new().with_code_replies(vec![
        common::CodeReply::ToolCall {
            narration: None,
            command: first_command.clone(),
        },
        common::CodeReply::TurnEnd(None),
        common::CodeReply::ToolCall {
            narration: None,
            command: retry_command.clone(),
        },
        common::CodeReply::Complete(None),
    ]));
    let config = common::scripted_pipeline_with_transport(transport.clone());

    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), &session);
    assert!(
        matches!(step, Step::Complete),
        "the retried ticket resolves and the run completes: {step:?}"
    );

    let digests = spill_digests(&transport);
    assert_eq!(
        digests.len(),
        2,
        "the first attempt and its fresh retry must each spill exactly once: {digests:?}"
    );
    let retry_digest = &digests[1];
    let relative = ".ejaj/20261018-121255/tools-output/step-1-call-1-1.log";
    assert!(
        retry_digest.contains(relative),
        "the retry digest must name the retry log relative to the project root: {retry_digest:?}"
    );
    assert!(
        !retry_digest.starts_with('/'),
        "the retry digest must never carry an absolute path: {retry_digest:?}"
    );

    let retry_log = session.join("tools-output/step-1-call-1-1.log");
    assert!(
        retry_log.exists(),
        "the complete retry log must exist under the ask's session directory"
    );
    assert_eq!(
        fs::read(&retry_log).unwrap(),
        spilled_log_bytes(&retry_command, &emitted_bytes((16_385, 's'), (0, 'z'))),
        "the retry log must carry the retry command preamble and its complete output"
    );
    let first_log = session.join("tools-output/step-1-call-1.log");
    assert!(
        first_log.exists(),
        "the first attempt's log must stay in the same session directory"
    );
    assert_eq!(
        fs::read(&first_log).unwrap(),
        spilled_log_bytes(&first_command, &emitted_bytes((16_385, 'r'), (0, 'z'))),
        "the first attempt's log must carry its own complete output"
    );
    assert_no_global_spill(&repository);

    let _ = fs::remove_dir_all(repository.path());
}

#[test]
fn when_pipeline_ask_given_review_generated_round_bash_spill_then_keeps_artifact_in_session_directory()
 {
    let repository = committed_repository();
    let session = repository.path().join(".ejaj/20261018-121255");
    // The original queue's code turn stays under the delivery cap. The review
    // then emits one fix ticket, whose queue round spills its own oversized
    // bash output -- proving the later round still uses this ask's session.
    let fix_command = emit_with_change(repository.path(), (16_385, 'q'), (0, 'z'));
    let transport = Arc::new(
        common::ScriptedTransport::new()
            .with_review(common::ReviewScript::FixOnceThenPass)
            .with_code_replies(vec![
                common::CodeReply::ToolCall {
                    narration: None,
                    command: "echo ejaj-e2e-ok".to_string(),
                },
                common::CodeReply::Complete(None),
                common::CodeReply::ToolCall {
                    narration: None,
                    command: fix_command.clone(),
                },
                common::CodeReply::Complete(None),
            ]),
    );
    let config = common::scripted_pipeline_with_transport(transport.clone());

    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), &session);
    assert!(
        matches!(step, Step::Complete),
        "the original queue, the fix queue and the passing review complete: {step:?}"
    );

    let review = fs::read_to_string(session.join("review.md"))
        .expect("the review artifact must be readable");
    assert!(
        review.contains("r1-1-verify-edge-case"),
        "the review must really have generated the queued fix ticket: {review}"
    );

    let digest = spill_digest(&transport);
    let relative = ".ejaj/20261018-121255/tools-output/step-1-call-1.log";
    assert!(
        digest.contains(relative),
        "the review-generated round digest must name this ask's session: {digest:?}"
    );
    assert!(
        !digest.starts_with('/'),
        "the review-generated round digest must never carry an absolute path: {digest:?}"
    );

    let spill = session.join("tools-output/step-1-call-1.log");
    assert!(
        spill.exists(),
        "the complete fix-round log must exist under the ask's session directory"
    );
    assert_eq!(
        fs::read(&spill).unwrap(),
        spilled_log_bytes(&fix_command, &emitted_bytes((16_385, 'q'), (0, 'z'))),
        "the fix-round log must carry the command preamble and its complete output"
    );
    assert_eq!(
        session_spill_files(&session),
        vec!["step-1-call-1.log".to_string()],
        "only the review-generated round spilled, and it stayed in this session"
    );
    assert_no_global_spill(&repository);

    let _ = fs::remove_dir_all(repository.path());
}

#[test]
fn when_pipeline_ask_given_bash_in_plan_phase_then_remains_forbidden() {
    use ejaj::tools::{Phase, Source, Tool};

    let definitions = Tool::definitions(Phase::Plan);
    let names: Vec<&str> = definitions
        .iter()
        .map(|definition| definition.function.name)
        .collect();
    assert!(
        names.contains(&"execute_script"),
        "the plan phase keeps its read-only script tool: {names:?}"
    );
    assert!(
        !names.contains(&"bash"),
        "forwarding the session directory must not add Bash to the plan phase: {names:?}"
    );

    let error = match Tool::parse(
        "bash",
        r#"{"command":"echo hi"}"#,
        Phase::Plan,
        Source::None,
    ) {
        Err(error) => error,
        Ok(_) => panic!("Bash must stay forbidden in the plan phase"),
    };
    assert!(
        error.contains("only available in the code phase"),
        "the read-only boundary must be named: {error}"
    );
}

// ---------------------------------------------------------------------------
// Excluding the run's own .ejaj artifacts from preflight and finalization
//
// The artifact directory is self-ignoring, but these cases deliberately plant
// tracked artifacts and remove the self-ignore file, so the only thing that can
// keep .ejaj out of the status, the stash and the commit is the Git pathspec
// exclusion itself. Every assertion reads real Git state.
// ---------------------------------------------------------------------------

/// A disposable repository with no root .ejaj ignore rule. The tracked
/// artifact file makes an artifact edit visible to ordinary Git status even
/// when untracked files are ignored, so the pathspec exclusion is exercised on
/// a path no ignore rule can hide.
fn repository_without_ejaj_ignore() -> common::TempRepo {
    let repository = common::TempRepo::new();
    repository.write("tracked.txt", "baseline\n");
    repository.write("modified.txt", "before\n");
    repository.write("deleted.txt", "before\n");
    repository.write(".ejaj/tracked-artifact.md", "tracked artifact baseline\n");
    repository.commit_all("baseline");
    repository
}

/// Install a real post-checkout hook that records the run stash's changed paths
/// at the moment the default-branch workflow switches branches, before the run
/// reapplies and drops that entry.
fn observe_stash_paths(repository: &common::TempRepo) -> PathBuf {
    let observed = repository.path().join(".git/observed-stash-paths.txt");
    let script = format!(
        "#!/bin/sh\n\
         git stash show --include-untracked --name-only 'stash@{{0}}' > '{observed}' \
         2>/dev/null || true\n",
        observed = observed.display(),
    );
    repository.install_hook("post-checkout", &script);
    observed
}

/// The paths the current HEAD commit changed relative to its parent, one per
/// line. Unlike the full tree, this is exactly what the run recorded.
fn commit_paths(repository: &common::TempRepo) -> Vec<String> {
    repository
        .git(&["diff-tree", "--no-commit-id", "--name-only", "-r", "HEAD"])
        .lines()
        .map(str::to_string)
        .collect()
}

/// Assert no path in paths is .ejaj itself or one of its descendants.
fn assert_no_artifact_paths(paths: &[String], context: &str) {
    assert!(
        !paths
            .iter()
            .any(|path| path == ".ejaj" || path.starts_with(".ejaj/")),
        "{context} must exclude .ejaj and everything beneath it: {paths:?}"
    );
}

/// Core entry-point behavior: a first incomplete invocation leaves its handoffs
/// beneath .ejaj/, the self-ignore file is removed so those paths are visible
/// to ordinary Git status, and a second invocation against the same repository
/// passes preflight and reaches the scripted transport.
#[test]
fn when_starting_a_pipeline_given_ejaj_handoffs_from_an_incomplete_run_then_reaches_agents() {
    let scratch = scratch_dir();
    let repository = common::TempRepo::new();
    repository.write("tracked.txt", "baseline\n");
    repository.commit_all("baseline");

    let first_run = repository.path().join(".ejaj/first-run");
    let (first_config, first_transport) =
        common::scripted_pipeline_with(common::GateScript::Failing, common::SummaryScript::Summary);
    let mut first_pipeline = Pipeline::new(&first_config, common::scope(), repository.path());
    let first = first_pipeline.ask(&common::harmless_plan_request(), &first_run);

    assert!(
        matches!(first, Step::Retry),
        "the first invocation is the incomplete run: {first:?}"
    );
    for handoff in [
        "plan.md",
        "spec.md",
        "ticket-triage.md",
        "queue-report.md",
        "change-summary.md",
    ] {
        assert!(
            first_run.join(handoff).is_file(),
            "the incomplete invocation leaves its {handoff} handoff beneath .ejaj/"
        );
    }
    assert!(
        !first_transport.requests.lock().unwrap().is_empty(),
        "the first invocation really reached the scripted transport"
    );
    assert_eq!(
        repository.status(),
        "",
        "the incomplete run's ordinary working tree stays clean"
    );

    // The handoff writer created .ejaj/.gitignore. Remove it so the artifacts
    // are not ignored and ordinary Git status must report them; only the
    // pathspec exclusion can then keep the next preflight clean.
    fs::remove_file(repository.path().join(".ejaj/.gitignore"))
        .expect("the handoff writer created the self-ignore file");
    let visible = repository.status();
    assert!(
        visible.contains(".ejaj/first-run/plan.md"),
        "the first run's handoffs are visible to ordinary Git status: {visible:?}"
    );
    assert!(
        !visible.contains("tracked.txt"),
        "the first run leaves no ordinary visible change: {visible:?}"
    );

    let second_run = repository.path().join(".ejaj/second-run");
    let (second_config, second_transport) =
        common::scripted_pipeline_with(common::GateScript::Passing, common::SummaryScript::Summary);
    let mut second_pipeline = Pipeline::new(&second_config, common::scope(), repository.path());
    let second = second_pipeline.ask(&common::harmless_plan_request(), &second_run);

    assert!(
        !matches!(
            second,
            Step::GitFailed {
                stage: "preflight",
                ..
            }
        ),
        "artifact-only state must pass preflight: {second:?}"
    );
    assert!(
        !second_transport.requests.lock().unwrap().is_empty(),
        "the second invocation reaches the scripted transport instead of failing preflight"
    );
    assert!(
        second_run.join("plan.md").is_file(),
        "the second invocation writes its own handoff beneath .ejaj/"
    );

    let _ = fs::remove_dir_all(&scratch);
}

/// Core entry-point behavior: a real repository without any root ignore rule
/// for `.ejaj/` still rejects preflight for each ordinary visible change
/// class -- staged, unstaged and untracked -- even though the status read is
/// paired with the artifact exclusion.
#[test]
fn when_checking_a_repository_given_ordinary_changes_then_rejects_preflight() {
    type ChangeCase = (&'static str, fn(&common::TempRepo), &'static str);
    let cases: [ChangeCase; 3] = [
        (
            "a staged ordinary change",
            |repository| {
                repository.write("tracked.txt", "staged ordinary change\n");
                repository.git(&["add", "tracked.txt"]);
            },
            "tracked.txt",
        ),
        (
            "an unstaged ordinary change",
            |repository| repository.write("tracked.txt", "unstaged ordinary change\n"),
            "tracked.txt",
        ),
        (
            "an untracked ordinary file",
            |repository| repository.write("fresh.txt", "untracked ordinary change\n"),
            "fresh.txt",
        ),
    ];

    for (label, plant, visible) in cases {
        let repository = repository_without_ejaj_ignore();
        plant(&repository);
        assert!(
            !repository.path().join(".gitignore").exists(),
            "{label}: the repository must have no root ignore rule"
        );
        let status = repository.status();
        assert!(
            status.contains(visible),
            "{label}: the ordinary change must be visible to ordinary Git status: {status:?}"
        );

        let artifacts = repository.path().join(".ejaj/ordinary-check");
        let (step, transport) = scripted_invocation(&repository, &artifacts);

        let reason = preflight_reason(&step);
        assert!(
            reason.contains(visible),
            "{label}: preflight must name the ordinary change: {reason}"
        );
        assert!(
            transport.requests.lock().unwrap().is_empty(),
            "{label}: the ordinary change gates the run before any request"
        );
        assert!(
            !artifacts.join("plan.md").exists(),
            "{label}: a gated run writes no artifact"
        );
    }
}

/// Contract and invariant: a repository without a root .ejaj ignore rule
/// allows preflight when the only changes are a tracked artifact edit and an
/// untracked artifact, both visible to ordinary Git status.
#[test]
fn when_checking_a_repository_given_only_ejaj_changes_then_allows_preflight() {
    let repository = repository_without_ejaj_ignore();
    repository.write(".ejaj/tracked-artifact.md", "modified tracked artifact\n");
    repository.write(".ejaj/untracked-artifact.md", "untracked artifact\n");
    let status = repository.status();
    assert!(
        status.contains(".ejaj/tracked-artifact.md"),
        "the tracked artifact edit is visible to ordinary Git status: {status:?}"
    );
    assert!(
        status.contains(".ejaj/untracked-artifact.md"),
        "the untracked artifact is visible to ordinary Git status: {status:?}"
    );

    let artifacts = repository.path().join(".ejaj/check-only");
    let (step, transport) = scripted_invocation(&repository, &artifacts);

    assert!(
        !matches!(
            step,
            Step::GitFailed {
                stage: "preflight",
                ..
            }
        ),
        "artifact-only changes must not gate preflight: {step:?}"
    );
    assert!(
        !transport.requests.lock().unwrap().is_empty(),
        "the run reaches the scripted transport"
    );
    assert!(
        artifacts.join("plan.md").is_file(),
        "the permitted run writes its plan handoff"
    );
}

/// Empty/zero boundary: with only artifact changes and no ordinary visible
/// work, the default-branch finalizer still creates no stash entry, so no
/// artifact is saved by the stash either.
#[test]
fn when_finalizing_given_no_visible_changes_and_ejaj_artifacts_then_skips_artifact_stash() {
    let repository = repository_without_ejaj_ignore();
    repository.write(".ejaj/tracked-artifact.md", "modified tracked artifact\n");
    repository.write(".ejaj/untracked-artifact.md", "untracked artifact\n");
    let before_commits = head_commits(&repository);
    let before_tip = default_tip(&repository);
    let artifacts = repository.path().join(".ejaj/no-visible-changes");

    let (config, _transport) =
        common::scripted_pipeline_with(common::GateScript::Passing, common::SummaryScript::Summary);
    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), &artifacts);

    let (stage, reason) = git_failure(&step);
    assert_eq!(
        stage, "commit",
        "with no ordinary change the ordinary commit refusal is fatal: {reason}"
    );
    assert_eq!(
        repository.stash_count(),
        0,
        "artifact-only state creates no stash entry"
    );
    assert_eq!(
        head_commits(&repository),
        before_commits,
        "no commit is created from artifact-only state"
    );
    assert_eq!(
        default_tip(&repository),
        before_tip,
        "the default branch does not advance"
    );
    assert_eq!(
        fs::read_to_string(repository.path().join(".ejaj/tracked-artifact.md")).unwrap(),
        "modified tracked artifact\n",
        "the tracked artifact stays on disk, uncommitted and unstashed"
    );
    assert!(
        repository
            .path()
            .join(".ejaj/untracked-artifact.md")
            .is_file(),
        "the untracked artifact stays on disk, uncommitted and unstashed"
    );
    assert!(
        artifacts.join("change-summary.md").is_file(),
        "the summary artifact is written even though the commit is refused"
    );
    assert!(
        repository
            .origin_ref(&format!("refs/heads/{}", common::SUMMARY_BRANCH))
            .is_none(),
        "no summary branch is published"
    );
}

/// Core entry-point behavior: local finalization records the ordinary visible
/// change but keeps tracked and untracked .ejaj paths out of the commit,
/// without any root .ejaj ignore rule.
#[test]
fn when_finalizing_given_no_root_ejaj_ignore_then_keeps_artifacts_out_of_git() {
    let repository = repository_without_ejaj_ignore();
    repository.checkout_new_branch(common::FEATURE_BRANCH);
    repository.write(".ejaj/tracked-artifact.md", "modified tracked artifact\n");
    repository.write(".ejaj/untracked-artifact.md", "untracked artifact\n");
    let before_commits = head_commits(&repository);
    let target = repository.path().join("tracked.txt");
    let command = format!("printf 'changed by the run\\n' > '{}'", target.display());
    let artifacts = repository.path().join(".ejaj/local-finalization");

    let (config, _transport) = common::scripted_pipeline_with_command(
        common::GateScript::Passing,
        common::SummaryScript::Summary,
        command,
    );
    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), &artifacts);

    assert!(
        matches!(step, Step::Complete),
        "the ordinary visible change is committed locally: {step:?}"
    );
    assert_eq!(
        repository.current_branch(),
        common::FEATURE_BRANCH,
        "local finalization keeps the checked-out branch"
    );
    assert_eq!(
        head_commits(&repository),
        before_commits + 1,
        "exactly one commit records the ordinary change"
    );
    assert_eq!(
        repository.git(&["show", "HEAD:tracked.txt"]),
        "changed by the run\n",
        "the ordinary visible change is committed"
    );
    assert_eq!(
        repository.git(&["show", "HEAD:.ejaj/tracked-artifact.md"]),
        "tracked artifact baseline\n",
        "the tracked artifact's committed bytes are unchanged"
    );
    let paths = commit_paths(&repository);
    assert_no_artifact_paths(&paths, "the local commit paths");
    assert!(
        paths.iter().any(|path| path == "tracked.txt"),
        "the ordinary change remains in the commit: {paths:?}"
    );
    assert_eq!(
        fs::read_to_string(repository.path().join(".ejaj/tracked-artifact.md")).unwrap(),
        "modified tracked artifact\n",
        "the tracked artifact edit stays on disk outside the commit"
    );
    assert!(
        repository
            .path()
            .join(".ejaj/untracked-artifact.md")
            .is_file(),
        "the untracked artifact stays on disk outside the commit"
    );
    let status = repository.status();
    assert!(
        status.contains(".ejaj/tracked-artifact.md"),
        "the uncommitted tracked artifact edit remains visible: {status:?}"
    );
}

/// Maximum/upper boundary: default-branch finalization stashes and commits the
/// ordinary change while leaving every .ejaj path out of both the recorded
/// stash and the produced commit.
#[test]
fn when_finalizing_given_no_root_ejaj_ignore_on_default_branch_then_keeps_artifacts_out_of_stash_and_commit()
 {
    let repository = repository_without_ejaj_ignore();
    assert_eq!(repository.current_branch(), common::DEFAULT_BRANCH);
    let observed_stash = observe_stash_paths(&repository);
    repository.write(".ejaj/tracked-artifact.md", "modified tracked artifact\n");
    repository.write(".ejaj/untracked-artifact.md", "untracked artifact\n");
    let before_tip = default_tip(&repository);
    let target = repository.path().join("tracked.txt");
    let command = format!("printf 'changed by the run\\n' > '{}'", target.display());
    let artifacts = repository.path().join(".ejaj/default-finalization");

    let (config, _transport) = common::scripted_pipeline_with_command(
        common::GateScript::Passing,
        common::SummaryScript::Summary,
        command,
    );
    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), &artifacts);

    assert!(
        matches!(step, Step::Complete),
        "the ordinary change follows the default-branch workflow: {step:?}"
    );
    assert_eq!(
        repository.current_branch(),
        common::SUMMARY_BRANCH,
        "the run is recorded on its summary branch"
    );
    assert_eq!(
        repository.git(&["show", "HEAD:tracked.txt"]),
        "changed by the run\n",
        "the ordinary change is in the summary commit"
    );
    assert_eq!(
        repository.git(&["show", "HEAD:.ejaj/tracked-artifact.md"]),
        "tracked artifact baseline\n",
        "the tracked artifact is not committed"
    );
    let paths = commit_paths(&repository);
    assert_no_artifact_paths(&paths, "the default-branch commit paths");
    assert!(
        paths.iter().any(|path| path == "tracked.txt"),
        "the ordinary change is part of the commit: {paths:?}"
    );
    assert_eq!(
        default_tip(&repository),
        before_tip,
        "the default branch itself does not advance"
    );

    let stash_text = fs::read_to_string(&observed_stash)
        .expect("the post-checkout hook records the run stash paths");
    let stash_paths: Vec<String> = stash_text.lines().map(str::to_string).collect();
    assert_no_artifact_paths(&stash_paths, "the run stash paths");
    assert!(
        stash_paths.iter().any(|path| path == "tracked.txt"),
        "the ordinary change really went through the stash: {stash_paths:?}"
    );
    assert_eq!(
        repository.stash_count(),
        0,
        "the run reapplies and drops its own stash entry"
    );
    assert_eq!(
        fs::read_to_string(repository.path().join(".ejaj/tracked-artifact.md")).unwrap(),
        "modified tracked artifact\n",
        "the tracked artifact edit stays on disk outside the stash"
    );
    assert!(
        repository
            .path()
            .join(".ejaj/untracked-artifact.md")
            .is_file(),
        "the untracked artifact stays on disk outside the stash"
    );
}

/// Contract and invariant: a tracked .ejaj change is excluded from both the
/// run stash and the commit by the negative pathspec, even when the artifact
/// directory already carries its own self-ignore rule.
#[test]
fn when_finalizing_given_tracked_ejaj_changes_then_commit_and_stash_paths_exclude_artifacts() {
    let repository = repository_without_ejaj_ignore();
    // Commit the self-ignore file first, so the untracked-file ignore behavior
    // cannot be what hides the artifact: only the tracked edit below remains,
    // and Git does not ignore tracked paths.
    repository.write(".ejaj/.gitignore", "*\n");
    repository.git(&["add", "-f", "--", ".ejaj/.gitignore"]);
    repository.commit_all("ignore the artifact directory");
    let observed_stash = observe_stash_paths(&repository);
    repository.write(".ejaj/tracked-artifact.md", "tracked artifact changed\n");
    let status = repository.status();
    assert!(
        status.contains(".ejaj/tracked-artifact.md"),
        "the tracked artifact edit is visible despite the self-ignore file: {status:?}"
    );
    let target = repository.path().join("tracked.txt");
    let command = format!("printf 'ordinary change\\n' > '{}'", target.display());
    let artifacts = repository.path().join(".ejaj/tracked-artifacts");

    let (config, _transport) = common::scripted_pipeline_with_command(
        common::GateScript::Passing,
        common::SummaryScript::Summary,
        command,
    );
    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), &artifacts);

    assert!(matches!(step, Step::Complete), "{step:?}");
    let paths = commit_paths(&repository);
    assert_no_artifact_paths(&paths, "the commit paths");
    assert_eq!(
        repository.git(&["show", "HEAD:.ejaj/tracked-artifact.md"]),
        "tracked artifact baseline\n",
        "the tracked artifact edit is not committed"
    );
    let stash_text = fs::read_to_string(&observed_stash)
        .expect("the post-checkout hook records the run stash paths");
    let stash_paths: Vec<String> = stash_text.lines().map(str::to_string).collect();
    assert_no_artifact_paths(&stash_paths, "the run stash paths");
    assert!(
        stash_paths.iter().any(|path| path == "tracked.txt"),
        "the ordinary change is stashed: {stash_paths:?}"
    );
    assert_eq!(
        fs::read_to_string(repository.path().join(".ejaj/tracked-artifact.md")).unwrap(),
        "tracked artifact changed\n",
        "the tracked artifact edit stays on disk"
    );
}

/// Maximum/upper boundary for local recording: every ordinary visible change
/// kind is committed while a tracked .ejaj path is left untouched.
#[test]
fn when_finalizing_given_multiple_visible_change_kinds_and_tracked_ejaj_paths_then_records_only_visible_changes()
 {
    let repository = repository_without_ejaj_ignore();
    repository.checkout_new_branch(common::FEATURE_BRANCH);
    repository.write(".ejaj/tracked-artifact.md", "modified tracked artifact\n");
    let root = repository.path().display().to_string();
    let command = format!(
        "printf 'after\\n' > '{root}/modified.txt' \
         && rm -f '{root}/deleted.txt' \
         && printf 'fresh\\n' > '{root}/added.txt'"
    );
    let artifacts = repository.path().join(".ejaj/multiple-changes");

    let (config, _transport) = common::scripted_pipeline_with_command(
        common::GateScript::Passing,
        common::SummaryScript::Summary,
        command,
    );
    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), &artifacts);

    assert!(matches!(step, Step::Complete), "{step:?}");
    let paths = commit_paths(&repository);
    assert!(
        paths.iter().any(|path| path == "added.txt"),
        "the ordinary addition is recorded: {paths:?}"
    );
    assert!(
        paths.iter().any(|path| path == "modified.txt"),
        "the ordinary modification is recorded: {paths:?}"
    );
    assert!(
        paths.iter().any(|path| path == "deleted.txt"),
        "the ordinary deletion is recorded: {paths:?}"
    );
    assert!(
        !repository.path().join("deleted.txt").exists(),
        "the ordinary deletion also clears the working tree"
    );
    assert_no_artifact_paths(&paths, "the commit paths");
    assert_eq!(
        repository.git(&["show", "HEAD:added.txt"]),
        "fresh\n",
        "the addition's bytes are committed"
    );
    assert_eq!(
        repository.git(&["show", "HEAD:modified.txt"]),
        "after\n",
        "the modification's bytes are committed"
    );
    assert_eq!(
        repository.git(&["show", "HEAD:.ejaj/tracked-artifact.md"]),
        "tracked artifact baseline\n",
        "the tracked artifact edit is not recorded"
    );
    assert_eq!(
        fs::read_to_string(repository.path().join(".ejaj/tracked-artifact.md")).unwrap(),
        "modified tracked artifact\n",
        "the tracked artifact edit stays on disk"
    );
}

/// A disposable repository whose root ignore rule hides `.ejaj/`, with one
/// artifact force-added to the baseline so an edit stays tracked and visible to
/// ordinary Git even though the directory itself is ignored.
fn repository_with_root_ignored_ejaj() -> common::TempRepo {
    let repository = common::TempRepo::new();
    repository.write(".gitignore", ".ejaj/\n");
    repository.write("tracked.txt", "baseline\n");
    repository.write(".ejaj/tracked-artifact.md", "tracked artifact baseline\n");
    repository.git(&["add", "--force", "--", ".ejaj/tracked-artifact.md"]);
    repository.commit_all("baseline");
    repository
}

/// Contract and invariant: an artifact staged before local finalization never
/// enters the commit, the ordinary visible change still does, and the staged
/// artifact's bytes stay untouched on disk.
#[test]
fn when_finalizing_given_pre_staged_ejaj_changes_then_keeps_artifacts_out_of_commit() {
    let repository = repository_without_ejaj_ignore();
    repository.checkout_new_branch(common::FEATURE_BRANCH);
    repository.write(".ejaj/tracked-artifact.md", "pre-staged artifact change\n");
    repository.git(&["add", "--", ".ejaj/tracked-artifact.md"]);
    assert!(
        repository.staged().contains(".ejaj/tracked-artifact.md"),
        "the fixture really stages the artifact before the run starts"
    );
    let before_commits = head_commits(&repository);
    let target = repository.path().join("tracked.txt");
    let command = format!("printf 'changed by the run\\n' > '{}'", target.display());
    let artifacts = repository.path().join(".ejaj/local-pre-staged");

    let (config, _transport) = common::scripted_pipeline_with_command(
        common::GateScript::Passing,
        common::SummaryScript::Summary,
        command,
    );
    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), &artifacts);

    assert!(
        matches!(step, Step::Complete),
        "the ordinary visible change is committed locally: {step:?}"
    );
    assert_eq!(
        repository.current_branch(),
        common::FEATURE_BRANCH,
        "local finalization keeps the checked-out branch"
    );
    assert_eq!(
        head_commits(&repository),
        before_commits + 1,
        "exactly one commit records the ordinary change"
    );
    assert_eq!(
        repository.git(&["show", "HEAD:tracked.txt"]),
        "changed by the run\n",
        "the ordinary visible change is committed"
    );
    assert_eq!(
        repository.git(&["show", "HEAD:.ejaj/tracked-artifact.md"]),
        "tracked artifact baseline\n",
        "the pre-staged artifact is not committed"
    );
    let paths = commit_paths(&repository);
    assert_no_artifact_paths(&paths, "the local commit paths");
    assert!(
        paths.iter().any(|path| path == "tracked.txt"),
        "the ordinary change remains in the commit: {paths:?}"
    );
    assert_eq!(
        fs::read_to_string(repository.path().join(".ejaj/tracked-artifact.md")).unwrap(),
        "pre-staged artifact change\n",
        "the pre-staged artifact's working-tree contents stay untouched"
    );
}

/// Maximum/upper boundary: default-branch finalization excludes an artifact
/// staged before the run from both the real stash and the produced commit,
/// while the ordinary change follows the existing workflow.
#[test]
fn when_finalizing_given_pre_staged_ejaj_changes_on_default_branch_then_keeps_artifacts_out_of_stash_and_commit()
 {
    let repository = repository_without_ejaj_ignore();
    assert_eq!(repository.current_branch(), common::DEFAULT_BRANCH);
    let observed_stash = observe_stash_paths(&repository);
    repository.write(".ejaj/tracked-artifact.md", "pre-staged artifact change\n");
    repository.git(&["add", "--", ".ejaj/tracked-artifact.md"]);
    assert!(
        repository.staged().contains(".ejaj/tracked-artifact.md"),
        "the fixture really stages the artifact before the run starts"
    );
    let before_tip = default_tip(&repository);
    let target = repository.path().join("tracked.txt");
    let command = format!("printf 'changed by the run\\n' > '{}'", target.display());
    let artifacts = repository.path().join(".ejaj/default-pre-staged");

    let (config, _transport) = common::scripted_pipeline_with_command(
        common::GateScript::Passing,
        common::SummaryScript::Summary,
        command,
    );
    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), &artifacts);

    assert!(
        matches!(step, Step::Complete),
        "the ordinary change follows the default-branch workflow: {step:?}"
    );
    assert_eq!(
        repository.current_branch(),
        common::SUMMARY_BRANCH,
        "the run is recorded on its summary branch"
    );
    assert_eq!(
        repository.git(&["show", "HEAD:tracked.txt"]),
        "changed by the run\n",
        "the ordinary change is in the summary commit"
    );
    assert_eq!(
        repository.git(&["show", "HEAD:.ejaj/tracked-artifact.md"]),
        "tracked artifact baseline\n",
        "the pre-staged artifact is not committed"
    );
    let paths = commit_paths(&repository);
    assert_no_artifact_paths(&paths, "the default-branch commit paths");
    assert!(
        paths.iter().any(|path| path == "tracked.txt"),
        "the ordinary change is part of the commit: {paths:?}"
    );

    let stash_text = fs::read_to_string(&observed_stash)
        .expect("the post-checkout hook records the run stash paths");
    let stash_paths: Vec<String> = stash_text.lines().map(str::to_string).collect();
    assert_no_artifact_paths(&stash_paths, "the run stash paths");
    assert!(
        stash_paths.iter().any(|path| path == "tracked.txt"),
        "the ordinary change really went through the stash: {stash_paths:?}"
    );
    assert_eq!(
        repository.stash_count(),
        0,
        "the run reapplies and drops its own stash entry"
    );
    assert_eq!(
        default_tip(&repository),
        before_tip,
        "the default branch itself does not advance"
    );
    assert_eq!(
        fs::read_to_string(repository.path().join(".ejaj/tracked-artifact.md")).unwrap(),
        "pre-staged artifact change\n",
        "the pre-staged artifact's working-tree contents stay untouched"
    );
}

/// Regression and edge case: a root ignore rule hides `.ejaj/`, yet the
/// visible-change pathspecs still record the ordinary change and exclude a
/// force-tracked artifact that was staged before the run.
#[test]
fn when_finalizing_given_root_ignored_ejaj_directory_then_records_only_visible_changes() {
    let repository = repository_with_root_ignored_ejaj();
    assert_eq!(
        repository
            .git(&["check-ignore", "--no-index", "--", ".ejaj"])
            .trim(),
        ".ejaj",
        "the root ignore rule really hides the artifact directory"
    );
    repository.checkout_new_branch(common::FEATURE_BRANCH);
    repository.write(".ejaj/tracked-artifact.md", "pre-staged artifact change\n");
    repository.git(&["add", "--force", "--", ".ejaj/tracked-artifact.md"]);
    assert!(
        repository.staged().contains(".ejaj/tracked-artifact.md"),
        "the fixture really stages the force-tracked artifact before the run starts"
    );
    let before_commits = head_commits(&repository);
    let target = repository.path().join("tracked.txt");
    let command = format!("printf 'changed by the run\\n' > '{}'", target.display());
    let artifacts = repository.path().join(".ejaj/ignored-directory");

    let (config, _transport) = common::scripted_pipeline_with_command(
        common::GateScript::Passing,
        common::SummaryScript::Summary,
        command,
    );
    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), &artifacts);

    assert!(
        matches!(step, Step::Complete),
        "the visible change is recorded despite the root ignore rule: {step:?}"
    );
    assert_eq!(
        head_commits(&repository),
        before_commits + 1,
        "exactly one commit records the ordinary change"
    );
    assert_eq!(
        repository.git(&["show", "HEAD:tracked.txt"]),
        "changed by the run\n",
        "the ordinary visible change is committed"
    );
    assert_eq!(
        repository.git(&["show", "HEAD:.ejaj/tracked-artifact.md"]),
        "tracked artifact baseline\n",
        "the force-tracked artifact is not committed"
    );
    let paths = commit_paths(&repository);
    assert_no_artifact_paths(&paths, "the root-ignored commit paths");
    assert!(
        paths.iter().any(|path| path == "tracked.txt"),
        "the ordinary change remains in the commit: {paths:?}"
    );
    assert_eq!(
        fs::read_to_string(repository.path().join(".ejaj/tracked-artifact.md")).unwrap(),
        "pre-staged artifact change\n",
        "the pre-staged artifact's working-tree contents stay untouched"
    );
}

/// Maximum/upper boundary: a root ignore rule hides `.ejaj/`, yet the
/// default-branch workflow still stashes, branches, commits and pushes the
/// ordinary visible change while a force-tracked artifact staged before the run
/// stays out of both the real stash and the produced commit.
#[test]
fn when_finalizing_given_ignored_artifact_directory_on_default_branch_then_keeps_artifacts_out_of_stash_and_commit()
 {
    let repository = repository_with_root_ignored_ejaj();
    assert_eq!(
        repository
            .git(&["check-ignore", "--no-index", "--", ".ejaj"])
            .trim(),
        ".ejaj",
        "the root ignore rule really hides the artifact directory"
    );
    assert_eq!(
        repository.current_branch(),
        common::DEFAULT_BRANCH,
        "the fixture starts on the default branch"
    );
    let observed_stash = observe_stash_paths(&repository);
    repository.write(".ejaj/tracked-artifact.md", "pre-staged artifact change\n");
    repository.git(&["add", "--force", "--", ".ejaj/tracked-artifact.md"]);
    assert!(
        repository.staged().contains(".ejaj/tracked-artifact.md"),
        "the fixture really stages the force-tracked artifact before the run starts"
    );
    let before_tip = default_tip(&repository);
    let target = repository.path().join("tracked.txt");
    let command = format!("printf 'changed by the run\\n' > '{}'", target.display());
    let artifacts = repository.path().join(".ejaj/ignored-directory-default");

    let (config, _transport) = common::scripted_pipeline_with_command(
        common::GateScript::Passing,
        common::SummaryScript::Summary,
        command,
    );
    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), &artifacts);

    assert!(
        matches!(step, Step::Complete),
        "the ordinary change follows the default-branch workflow despite the root ignore rule: {step:?}"
    );
    assert_eq!(
        repository.current_branch(),
        common::SUMMARY_BRANCH,
        "the run is recorded on its summary branch"
    );
    assert_eq!(
        repository.git(&["show", "HEAD:tracked.txt"]),
        "changed by the run\n",
        "the ordinary change is in the summary commit"
    );
    assert_eq!(
        repository.git(&["show", "HEAD:.ejaj/tracked-artifact.md"]),
        "tracked artifact baseline\n",
        "the force-tracked artifact is not committed"
    );
    let paths = commit_paths(&repository);
    assert_no_artifact_paths(&paths, "the default-branch commit paths");
    assert!(
        paths.iter().any(|path| path == "tracked.txt"),
        "the ordinary change is part of the commit: {paths:?}"
    );

    let stash_text = fs::read_to_string(&observed_stash)
        .expect("the post-checkout hook records the run stash paths");
    let stash_paths: Vec<String> = stash_text.lines().map(str::to_string).collect();
    assert_no_artifact_paths(&stash_paths, "the run stash paths");
    assert!(
        stash_paths.iter().any(|path| path == "tracked.txt"),
        "the ordinary change really went through the stash: {stash_paths:?}"
    );
    assert_eq!(
        repository.stash_count(),
        0,
        "the run reapplies and drops its own stash entry"
    );
    assert_eq!(
        default_tip(&repository),
        before_tip,
        "the default branch itself does not advance"
    );
    assert!(
        repository
            .origin_ref(&format!("refs/heads/{}", common::SUMMARY_BRANCH))
            .is_some(),
        "the summary branch is published"
    );
    assert_eq!(
        fs::read_to_string(repository.path().join(".ejaj/tracked-artifact.md")).unwrap(),
        "pre-staged artifact change\n",
        "the force-tracked artifact's working-tree contents stay untouched"
    );
}
