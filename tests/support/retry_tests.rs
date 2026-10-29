//! Production pipeline tests for the one-retry ticket handover.
//!
//! Every test drives the real ticket queue through `Pipeline::ask`, the real
//! code-agent loop, real local Git repositories and real shell gates. Only the
//! external LLM transport is scripted, and the gate commands exercise the same
//! command boundary production uses.

use super::common;
use ejaj::Config;
use ejaj::pipeline::{Pipeline, Step};
use ejaj::tickets::TicketSet;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A configuration over the scripted external transport.
fn config_for(transport: Arc<common::ScriptedTransport>) -> Config {
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

/// The harmless one-ticket set with a chosen shell gate.
fn tickets_with_gate(gate: &str) -> TicketSet {
    let value: serde_json::Value =
        serde_json::from_str(common::harmless_tickets_json()).expect("the ticket fixture is JSON");
    let mut set: TicketSet = serde_json::from_value(value["tickets"].clone())
        .expect("the ticket fixture carries a ticket set");
    for ticket in &mut set.tickets {
        ticket.verification_gate = gate.to_string();
    }
    set
}

/// Two independent tickets: the first branch's failure must not stop the second.
fn two_independent_tickets(gate: &str) -> TicketSet {
    let mut set = tickets_with_gate(gate);
    let mut second = set.tickets[0].clone();
    second.id = "2-independent-branch".to_string();
    second.title = "Independent branch".to_string();
    second.blocked_by.clear();
    second.blocks.clear();
    set.tickets.push(second);
    set
}

/// Run one production pipeline over an already-created disposable repository.
fn run_with(
    repository: common::TempRepo,
    replies: Vec<common::CodeReply>,
    gate: common::GateScript,
    tickets: Option<TicketSet>,
    artifacts_in_repo: bool,
) -> (
    Step,
    PathBuf,
    common::TempRepo,
    Arc<common::ScriptedTransport>,
) {
    repository.commit_baseline();
    let mut transport = common::ScriptedTransport::new()
        .with_gate(gate)
        .with_summary(common::SummaryScript::Summary)
        .with_review(common::ReviewScript::Pass)
        .with_code_replies(replies);
    if let Some(tickets) = tickets {
        transport = transport.with_tickets(tickets);
    }
    let transport = Arc::new(transport);
    let config = config_for(transport.clone());
    let dir = if artifacts_in_repo {
        repository.path().to_path_buf()
    } else {
        let dir = super::scratch_dir();
        fs::create_dir_all(&dir).expect("the scratch artifact directory is creatable");
        dir
    };
    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), &dir);
    (step, dir, repository, transport)
}

/// Every code-agent request the run made, in order.
fn code_requests(transport: &common::ScriptedTransport) -> Vec<serde_json::Value> {
    transport
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|body| body.to_string().contains(common::CODE_MARKER))
        .cloned()
        .collect()
}

/// Every user message one request carried, in order.
fn user_messages(body: &serde_json::Value) -> Vec<String> {
    body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|message| message["role"] == "user")
        .filter_map(|message| message["content"].as_str())
        .map(str::to_string)
        .collect()
}

/// The role of every message one request carried, in order.
fn transcript_roles(body: &serde_json::Value) -> Vec<String> {
    body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|message| message["role"].as_str().map(str::to_string))
        .collect()
}

/// The handover user message of the second attempt.
fn retry_handover(requests: &[serde_json::Value]) -> String {
    let users = user_messages(
        requests
            .get(1)
            .expect("the run makes the second attempt's request"),
    );
    assert_eq!(
        users.len(),
        2,
        "the retry adds one separate handover message"
    );
    users[1].clone()
}

/// The distinct request bodies a run made, in order. The provider's transport
/// retry replays the same request, so repeated identical bodies are HTTP
/// attempts inside one ticket attempt rather than separate ticket attempts.
fn distinct_requests(requests: &[serde_json::Value]) -> Vec<serde_json::Value> {
    let mut distinct = Vec::new();
    for request in requests {
        if !distinct.contains(request) {
            distinct.push(request.clone());
        }
    }
    distinct
}

/// Whether one request carried the retry handover.
fn carries_retry_handover(body: &serde_json::Value) -> bool {
    body.to_string().contains("[RETRY HANDOVER]")
}

/// How many times a logging shell gate really ran. A missing log means zero.
fn gate_runs(log: &Path) -> usize {
    fs::read_to_string(log)
        .map(|text| text.lines().count())
        .unwrap_or(0)
}

/// Read the final status snapshot.
fn snapshot(dir: &Path) -> serde_json::Value {
    serde_json::from_str(
        &fs::read_to_string(dir.join("tickets/queue-status.json"))
            .expect("the queue status artifact is readable"),
    )
    .expect("the queue status is JSON")
}

/// Read the queue report.
fn report(dir: &Path) -> String {
    fs::read_to_string(dir.join("queue-report.md")).expect("the queue report is readable")
}

/// A shell gate that logs every invocation and then does the given thing.
fn logging_gate(log: &Path, result: &str) -> String {
    format!("printf 'gate run\\n' >> {}; {result}", log.display())
}

// -- Core Entry-Point Behaviors --------------------------------------------

#[test]
fn when_retrying_given_failed_first_gate_then_preserves_original_kickoff_diagnostic_diff_and_gate_count()
 {
    let repository = common::TempRepo::new();
    let root = repository.path().to_path_buf();
    let state_dir = super::scratch_dir();
    fs::create_dir_all(&state_dir).expect("the gate state directory is creatable");
    let state = state_dir.join("first-gate-failed");
    let log = state_dir.join("gate-runs.log");
    let visible = root.join("visible-change.txt");
    let spill_dir = root.join(".ejaj/tool-output");
    let spill = spill_dir.join("step-1-call-1.log");
    let gate = format!(
        "printf 'gate run\\n' >> {log};          if [ ! -e {state} ]; then            : > {state};            printf 'FIRST-GATE-DIAGNOSTIC\\n' >&2;            printf 'first attempt change\\n' > {visible};            mkdir -p {spill_dir};            printf 'spill bytes\\n' > {spill};            exit 1;          else            printf 'FINAL-GATE-OK\\n';            exit 0;          fi",
        log = log.display(),
        state = state.display(),
        visible = visible.display(),
        spill_dir = spill_dir.display(),
        spill = spill.display(),
    );

    let replies = vec![
        common::CodeReply::Complete(Some("First attempt narration marker".to_string())),
        common::CodeReply::Complete(Some("Second attempt after the handover".to_string())),
    ];
    let (step, dir, repository, transport) = run_with(
        repository,
        replies,
        common::GateScript::Passing,
        Some(tickets_with_gate(&gate)),
        true,
    );

    assert!(
        matches!(step, Step::Complete),
        "the final gate passes, so the queue resolves: {step:?}"
    );
    let requests = code_requests(&transport);
    assert_eq!(
        requests.len(),
        2,
        "one request per attempt: exactly two attempts"
    );
    let first_users = user_messages(&requests[0]);
    let second_users = user_messages(&requests[1]);
    assert_eq!(
        first_users[0], second_users[0],
        "the retry keeps the same rendered ticket kickoff"
    );
    assert_eq!(
        second_users.len(),
        2,
        "the handover is a separate user message, not a replacement"
    );

    let handover = retry_handover(&requests);
    assert!(handover.contains("[RETRY HANDOVER]"), "{handover}");
    assert!(
        handover.contains("First attempt narration marker"),
        "the first attempt's last narration reaches the retry: {handover}"
    );
    assert!(
        handover.contains("task_complete") && handover.contains("gate failed"),
        "the stop reason names the declaration and failed first gate: {handover}"
    );
    assert!(
        handover.contains("FIRST-GATE-DIAGNOSTIC"),
        "the first gate's own output reaches the retry: {handover}"
    );
    let diff_section = handover
        .split("Repository working-tree diff")
        .nth(1)
        .expect("the handover names the repository diff");
    assert!(
        diff_section.contains("visible-change.txt")
            && diff_section.contains("first attempt change"),
        "the actual working-tree diff reaches the retry: {handover}"
    );
    assert!(
        !diff_section.contains("diff --git a/.ejaj")
            && !diff_section.contains("+++ b/.ejaj")
            && !diff_section.contains("+spill bytes"),
        "the ignored spill file never reaches the diff: {handover}"
    );

    assert_eq!(
        fs::read_to_string(&log)
            .expect("the gate log is readable")
            .lines()
            .count(),
        2,
        "the first gate and the authoritative final gate each ran once"
    );
    assert_eq!(snapshot(&dir)["1-verify-command-output"], "completed");
    let report = report(&dir);
    assert!(report.contains("- Status: completed"), "{report}");
    assert!(report.contains("- Ended: declared"), "{report}");

    drop(repository);
    let _ = fs::remove_dir_all(&dir);
    let _ = fs::remove_dir_all(&state_dir);
}

#[test]
fn when_ticket_work_given_first_turn_end_then_issues_one_fresh_retry_with_original_kickoff() {
    let log_dir = super::scratch_dir();
    fs::create_dir_all(&log_dir).expect("the gate log directory is creatable");
    let log = log_dir.join("gate-runs.log");
    let gate = logging_gate(&log, "exit 0");
    let replies = vec![
        common::CodeReply::TurnEnd(Some("First attempt narration".to_string())),
        common::CodeReply::Complete(Some("Retry completed the work".to_string())),
    ];
    let repository = common::TempRepo::new();
    let (step, dir, repository, transport) = run_with(
        repository,
        replies,
        common::GateScript::Passing,
        Some(tickets_with_gate(&gate)),
        true,
    );

    assert!(matches!(step, Step::Complete), "{step:?}");
    let requests = code_requests(&transport);
    assert_eq!(requests.len(), 2, "exactly two attempts, never a third");
    let first_users = user_messages(&requests[0]);
    let second_users = user_messages(&requests[1]);
    assert_eq!(
        first_users[0], second_users[0],
        "the fresh retry starts from the same ticket kickoff"
    );
    assert_eq!(
        requests[0]["messages"][0], requests[1]["messages"][0],
        "the fresh retry starts from the same system prompt"
    );
    assert_eq!(
        transcript_roles(&requests[1]),
        vec!["system", "user", "user"],
        "the retry is a fresh transcript with only the kickoff and handover"
    );
    let handover = retry_handover(&requests);
    assert!(handover.contains("First attempt narration"), "{handover}");
    assert!(
        handover.contains("turn end without task_complete"),
        "{handover}"
    );
    assert_eq!(
        fs::read_to_string(&log)
            .expect("the gate log is readable")
            .lines()
            .count(),
        1,
        "a first attempt that did not complete runs no gate; only the final gate runs"
    );
    assert_eq!(snapshot(&dir)["1-verify-command-output"], "completed");
    assert!(report(&dir).contains("- Ended: declared"));

    drop(repository);
    let _ = fs::remove_dir_all(&dir);
    let _ = fs::remove_dir_all(&log_dir);
}

#[test]
fn when_ticket_work_given_first_completion_and_passing_gate_then_resolves_without_retry() {
    let log_dir = super::scratch_dir();
    fs::create_dir_all(&log_dir).expect("the gate log directory is creatable");
    let log = log_dir.join("gate-runs.log");
    let gate = logging_gate(&log, "exit 0");
    let replies = vec![common::CodeReply::Complete(Some(
        "The first attempt declared completion".to_string(),
    ))];
    let repository = common::TempRepo::new();
    let (step, dir, repository, transport) = run_with(
        repository,
        replies,
        common::GateScript::Passing,
        Some(tickets_with_gate(&gate)),
        true,
    );

    assert!(matches!(step, Step::Complete), "{step:?}");
    assert_eq!(
        code_requests(&transport).len(),
        1,
        "a passing first gate resolves the ticket without a retry"
    );
    assert_eq!(
        fs::read_to_string(&log)
            .expect("the gate log is readable")
            .lines()
            .count(),
        1,
        "the first verification gate runs exactly once"
    );
    assert_eq!(snapshot(&dir)["1-verify-command-output"], "completed");
    let report = report(&dir);
    assert!(report.contains("- Status: completed"), "{report}");
    assert!(report.contains("- Ended: declared"), "{report}");

    drop(repository);
    let _ = fs::remove_dir_all(&dir);
    let _ = fs::remove_dir_all(&log_dir);
}

// -- Edge-Case Tables -------------------------------------------------------

#[test]
fn when_retrying_given_empty_narration_and_diff_then_records_explicit_empty_fields() {
    let replies = vec![
        common::CodeReply::TurnEnd(None),
        common::CodeReply::TurnEnd(None),
    ];
    let repository = common::TempRepo::new();
    let (step, dir, repository, transport) = run_with(
        repository,
        replies,
        common::GateScript::Failing,
        None,
        false,
    );

    assert!(
        matches!(step, Step::Retry),
        "the failing final gate leaves the run retryable: {step:?}"
    );
    let handover = retry_handover(&code_requests(&transport));
    assert!(handover.contains("(empty narration)"), "{handover}");
    assert!(handover.contains("(empty diff)"), "{handover}");

    drop(repository);
    let _ = fs::remove_dir_all(&dir);
}

// -- Contract & Invariant Checks -------------------------------------------

#[test]
fn when_final_gate_given_second_attempt_without_declaration_then_accepts_only_passing_gate() {
    let log_dir = super::scratch_dir();
    fs::create_dir_all(&log_dir).expect("the gate log directory is creatable");
    let log = log_dir.join("gate-runs.log");
    let gate = logging_gate(&log, "exit 0");
    let replies = vec![
        common::CodeReply::TurnEnd(None),
        common::CodeReply::TurnEnd(None),
    ];
    let repository = common::TempRepo::new();
    let (step, dir, repository, _transport) = run_with(
        repository,
        replies,
        common::GateScript::Passing,
        Some(tickets_with_gate(&gate)),
        true,
    );

    assert!(
        matches!(step, Step::Complete),
        "the passing final gate resolves even without a declaration: {step:?}"
    );
    assert_eq!(
        fs::read_to_string(&log)
            .expect("the gate log is readable")
            .lines()
            .count(),
        1,
        "the final gate runs exactly once"
    );
    assert_eq!(snapshot(&dir)["1-verify-command-output"], "completed");
    let report = report(&dir);
    assert!(
        report.contains("- Ended: gate accepted without declaration"),
        "{report}"
    );

    drop(repository);
    let _ = fs::remove_dir_all(&dir);
    let _ = fs::remove_dir_all(&log_dir);
}

#[test]
fn when_final_gate_given_failure_then_never_persists_ticket_as_completed() {
    let log_dir = super::scratch_dir();
    fs::create_dir_all(&log_dir).expect("the gate log directory is creatable");
    let log = log_dir.join("gate-runs.log");
    let gate = logging_gate(&log, "printf 'FINAL-GATE-OUTPUT\\n' >&2; exit 1");
    let replies = vec![
        common::CodeReply::TurnEnd(None),
        common::CodeReply::TurnEnd(None),
    ];
    let repository = common::TempRepo::new();
    let (step, dir, repository, _transport) = run_with(
        repository,
        replies,
        common::GateScript::Passing,
        Some(tickets_with_gate(&gate)),
        true,
    );

    assert!(
        matches!(step, Step::Retry),
        "a failed final gate never resolves: {step:?}"
    );
    assert_eq!(
        fs::read_to_string(&log)
            .expect("the gate log is readable")
            .lines()
            .count(),
        1,
        "the final gate runs exactly once"
    );
    assert_eq!(snapshot(&dir)["1-verify-command-output"], "failed");
    let report = report(&dir);
    assert!(report.contains("- Status: failed"), "{report}");
    assert!(
        report.contains("retry exhausted without task_complete"),
        "{report}"
    );

    drop(repository);
    let _ = fs::remove_dir_all(&dir);
    let _ = fs::remove_dir_all(&log_dir);
}

// -- Maximum limit / Upper boundary ----------------------------------------

#[test]
fn when_ticket_work_given_second_attempt_stop_then_runs_one_final_gate_without_third_attempt() {
    let log_dir = super::scratch_dir();
    fs::create_dir_all(&log_dir).expect("the gate log directory is creatable");
    let log = log_dir.join("gate-runs.log");
    let gate = logging_gate(&log, "exit 0");
    let replies = vec![
        common::CodeReply::TurnEnd(None),
        common::CodeReply::TurnEnd(None),
    ];
    let repository = common::TempRepo::new();
    let (step, dir, repository, transport) = run_with(
        repository,
        replies,
        common::GateScript::Passing,
        Some(tickets_with_gate(&gate)),
        true,
    );

    assert!(matches!(step, Step::Complete), "{step:?}");
    assert_eq!(
        code_requests(&transport).len(),
        2,
        "the second attempt is the maximum; no third attempt exists"
    );
    assert_eq!(
        fs::read_to_string(&log)
            .expect("the gate log is readable")
            .lines()
            .count(),
        1,
        "the final gate is invoked exactly once"
    );
    assert_eq!(snapshot(&dir)["1-verify-command-output"], "completed");

    drop(repository);
    let _ = fs::remove_dir_all(&dir);
    let _ = fs::remove_dir_all(&log_dir);
}

#[test]
fn when_ticket_work_given_context_guard_then_runs_one_fresh_retry_and_authoritative_final_gate() {
    let log_dir = super::scratch_dir();
    fs::create_dir_all(&log_dir).expect("the gate log directory is creatable");
    let log = log_dir.join("gate-runs.log");
    let gate = logging_gate(&log, "exit 0");
    // The first attempt's scripted tool call carries an oversized argument.
    // The real decoder places it in the transcript, so the next prepared
    // request crosses the hard context guard before it can be sent. The
    // unknown tool is rejected cheaply; the huge call itself is the point.
    let oversized_arguments = format!("{{\"payload\":\"{}\"}}", "context".repeat(700_000));
    let replies = vec![
        common::CodeReply::RawToolCall {
            narration: Some("First attempt narration before the guard".to_string()),
            name: "unknown_tool".to_string(),
            arguments: oversized_arguments,
        },
        common::CodeReply::TurnEnd(None),
    ];
    let repository = common::TempRepo::new();
    let (step, dir, repository, transport) = run_with(
        repository,
        replies,
        common::GateScript::Passing,
        Some(tickets_with_gate(&gate)),
        true,
    );

    assert!(
        matches!(step, Step::Complete),
        "the authoritative final gate passes: {step:?}"
    );
    let requests = code_requests(&transport);
    assert_eq!(
        requests.len(),
        2,
        "the guarded first attempt and its one fresh retry, never a third"
    );
    let handover = retry_handover(&requests);
    assert!(handover.contains("[RETRY HANDOVER]"), "{handover}");
    assert!(
        handover.contains("First attempt narration before the guard"),
        "the guarded attempt's narration reaches the retry: {handover}"
    );
    assert!(
        handover.contains("context guard reached"),
        "the handover names the guarded stop: {handover}"
    );
    assert!(
        handover.contains("did not reach a failed verification gate"),
        "no gate ran before the retry: {handover}"
    );
    assert_eq!(
        fs::read_to_string(&log)
            .expect("the gate log is readable")
            .lines()
            .count(),
        1,
        "the authoritative final gate runs exactly once after the retry"
    );
    assert_eq!(snapshot(&dir)["1-verify-command-output"], "completed");
    let report = report(&dir);
    assert!(report.contains("- Status: completed"), "{report}");
    assert!(
        report.contains("- Ended: gate accepted without declaration"),
        "the retry only stopped; the gate resolved the ticket: {report}"
    );

    drop(repository);
    let _ = fs::remove_dir_all(&dir);
    let _ = fs::remove_dir_all(&log_dir);
}

// -- Malformed/Invalid payload ---------------------------------------------

#[test]
fn when_retrying_given_diff_collection_failure_then_reports_failure_and_still_runs_fresh_attempt() {
    let repository = common::TempRepo::new();
    let root = repository.path().to_path_buf();
    let log_dir = super::scratch_dir();
    fs::create_dir_all(&log_dir).expect("the gate log directory is creatable");
    let log = log_dir.join("gate-runs.log");
    let git_dir = root.join(".git");
    let hidden_git = root.join(".git-hidden-for-collection-failure");
    // The first attempt hides the repository metadata, so the retry's diff
    // collector fails for real. The authoritative final gate restores it before
    // the run finalises, so the pipeline can still complete.
    let gate = format!(
        "mv {hidden} {git}; printf 'gate run\n' >> {log}; exit 0",
        hidden = hidden_git.display(),
        git = git_dir.display(),
        log = log.display(),
    );
    let replies = vec![
        common::CodeReply::ToolCall {
            narration: Some("The first attempt hides the repository metadata".to_string()),
            command: format!("mv {} {}", git_dir.display(), hidden_git.display()),
        },
        common::CodeReply::TurnEnd(None),
        common::CodeReply::Complete(Some("The retry still ran".to_string())),
    ];
    let (step, dir, repository, transport) = run_with(
        repository,
        replies,
        common::GateScript::Passing,
        Some(tickets_with_gate(&gate)),
        true,
    );

    assert!(matches!(step, Step::Complete), "{step:?}");
    let requests = code_requests(&transport);
    assert_eq!(
        requests.len(),
        3,
        "the first attempt's tool turn and turn end, then exactly one fresh retry"
    );
    let retry_users = user_messages(&requests[2]);
    assert_eq!(
        retry_users.len(),
        2,
        "the retry carries the unchanged kickoff and a separate handover"
    );
    let handover = &retry_users[1];
    assert!(handover.contains("[RETRY HANDOVER]"), "{handover}");
    assert!(
        handover.contains("diff collection failed"),
        "the handover names the collection failure instead of claiming an empty tree: {handover}"
    );
    assert!(
        !handover.contains("(empty diff)"),
        "a collection failure is not reported as an empty diff: {handover}"
    );
    assert!(
        handover.contains("The first attempt hides the repository metadata"),
        "{handover}"
    );
    assert_eq!(
        fs::read_to_string(&log)
            .expect("the gate log is readable")
            .lines()
            .count(),
        1,
        "the authoritative final gate still runs exactly once"
    );
    assert_eq!(snapshot(&dir)["1-verify-command-output"], "completed");
    assert!(report(&dir).contains("- Ended: declared"));

    drop(repository);
    let _ = fs::remove_dir_all(&dir);
    let _ = fs::remove_dir_all(&log_dir);
}

// -- Malformed/Invalid payload ---------------------------------------------

#[test]
fn when_ticket_work_given_fatal_first_api_transport_or_malformed_response_then_skips_ticket_retry_and_failed_ticket_gate()
 {
    let cases: Vec<(&str, Vec<common::CodeReply>, usize, &str)> = vec![
        (
            "permanent api",
            vec![common::CodeReply::Api {
                status: 400,
                message: "the provider rejected the request permanently".to_string(),
            }],
            1,
            "- Ended: agent failure: API error",
        ),
        (
            "exhausted transport",
            vec![
                common::CodeReply::Transport("the connection reset".to_string()),
                common::CodeReply::Transport("the connection reset".to_string()),
                common::CodeReply::Transport("the connection reset".to_string()),
            ],
            3,
            "- Ended: agent failure: transport error",
        ),
        (
            "malformed",
            vec![common::CodeReply::Malformed],
            1,
            "- Ended: agent failure: malformed response",
        ),
    ];
    for (label, fatal_replies, fatal_http_attempts, ending) in cases {
        let log_dir = super::scratch_dir();
        fs::create_dir_all(&log_dir).expect("the gate log directory is creatable");
        let log = log_dir.join("gate-runs.log");
        let gate = logging_gate(&log, "exit 0");
        let mut replies = fatal_replies;
        replies.push(common::CodeReply::Complete(Some(
            "the independent branch finished".to_string(),
        )));
        let repository = common::TempRepo::new();
        let (step, dir, repository, transport) = run_with(
            repository,
            replies,
            common::GateScript::Passing,
            Some(two_independent_tickets(&gate)),
            true,
        );

        assert!(
            matches!(step, Step::Retry),
            "{label}: the failed ticket leaves the queue incomplete: {step:?}"
        );
        let requests = code_requests(&transport);
        assert_eq!(
            requests.len(),
            fatal_http_attempts + 1,
            "{label}: the fatal ticket's HTTP attempts plus the independent ticket's one request"
        );
        assert_eq!(
            distinct_requests(&requests).len(),
            2,
            "{label}: exactly one request per ticket attempt"
        );
        assert!(
            requests
                .iter()
                .all(|request| user_messages(request).len() == 1),
            "{label}: a fatal first attempt never earns a retry handover"
        );
        assert!(
            !requests.iter().any(carries_retry_handover),
            "{label}: no request carries a ticket-level handover"
        );
        if fatal_http_attempts > 1 {
            assert!(
                requests[..fatal_http_attempts]
                    .windows(2)
                    .all(|pair| pair[0] == pair[1]),
                "{label}: the transport policy replays the same HTTP request"
            );
        }
        assert_eq!(
            gate_runs(&log),
            1,
            "{label}: only the independent ticket reaches a gate"
        );
        let status = snapshot(&dir);
        assert_eq!(status["1-verify-command-output"], "failed", "{label}");
        assert_eq!(status["2-independent-branch"], "completed", "{label}");
        let report = report(&dir);
        assert!(report.contains(ending), "{label}: {report}");
        assert!(
            report.contains("1 completed, 1 failed, 0 blocked"),
            "{label}: {report}"
        );

        drop(repository);
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&log_dir);
    }
}

// -- Boundary unauthorized/Forbidden access --------------------------------

#[test]
fn when_ticket_work_given_first_unauthorized_or_forbidden_api_failure_then_skips_retry_and_gate_while_independent_ticket_runs()
 {
    for status_code in [401, 403] {
        let log_dir = super::scratch_dir();
        fs::create_dir_all(&log_dir).expect("the gate log directory is creatable");
        let log = log_dir.join("gate-runs.log");
        let gate = logging_gate(&log, "exit 0");
        let replies = vec![
            common::CodeReply::Api {
                status: status_code,
                message: format!("the first branch was refused with {status_code}"),
            },
            common::CodeReply::Complete(Some("the independent branch finished".to_string())),
        ];
        let repository = common::TempRepo::new();
        let (step, dir, repository, transport) = run_with(
            repository,
            replies,
            common::GateScript::Passing,
            Some(two_independent_tickets(&gate)),
            true,
        );

        assert!(
            matches!(step, Step::Retry),
            "{status_code}: the failed branch leaves the queue incomplete: {step:?}"
        );
        let requests = code_requests(&transport);
        assert_eq!(
            requests.len(),
            2,
            "{status_code}: the refused ticket is never retried; the other ticket runs once"
        );
        assert!(
            requests
                .iter()
                .all(|request| user_messages(request).len() == 1),
            "{status_code}: the refused ticket never earns a handover"
        );
        assert!(
            !requests.iter().any(carries_retry_handover),
            "{status_code}: no ticket-level handover exists"
        );
        assert_eq!(
            gate_runs(&log),
            1,
            "{status_code}: only the independent ticket reaches its gate"
        );
        let status = snapshot(&dir);
        assert_eq!(status["1-verify-command-output"], "failed", "{status_code}");
        assert_eq!(status["2-independent-branch"], "completed", "{status_code}");
        let report = report(&dir);
        assert!(
            report.contains("- Ended: agent failure: API error"),
            "{status_code}: {report}"
        );
        assert!(
            report.contains("1 completed, 1 failed, 0 blocked"),
            "{status_code}: {report}"
        );

        drop(repository);
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&log_dir);
    }
}

// -- Contract & Invariant Checks -------------------------------------------

#[test]
fn when_ticket_work_given_second_attempt_fatal_outcome_then_uses_real_final_gate_as_authority() {
    let cases: Vec<(&str, Vec<common::CodeReply>, usize)> = vec![
        (
            "turn end",
            vec![
                common::CodeReply::TurnEnd(None),
                common::CodeReply::TurnEnd(None),
            ],
            2,
        ),
        (
            "api",
            vec![
                common::CodeReply::TurnEnd(None),
                common::CodeReply::Api {
                    status: 400,
                    message: "the second attempt was refused".to_string(),
                },
            ],
            2,
        ),
        (
            "transport",
            vec![
                common::CodeReply::TurnEnd(None),
                common::CodeReply::Transport("the second attempt lost its connection".to_string()),
                common::CodeReply::Transport("the second attempt lost its connection".to_string()),
                common::CodeReply::Transport("the second attempt lost its connection".to_string()),
            ],
            4,
        ),
        (
            "malformed",
            vec![
                common::CodeReply::TurnEnd(None),
                common::CodeReply::Malformed,
            ],
            2,
        ),
    ];
    for (label, replies, expected_requests) in cases {
        // A passing final gate resolves the ticket after the second attempt
        // stopped or failed.
        let passing_log_dir = super::scratch_dir();
        fs::create_dir_all(&passing_log_dir).expect("the passing gate log directory is creatable");
        let passing_log = passing_log_dir.join("gate-runs.log");
        let passing_gate = logging_gate(&passing_log, "exit 0");
        let repository = common::TempRepo::new();
        let (step, dir, repository, transport) = run_with(
            repository,
            replies.clone(),
            common::GateScript::Passing,
            Some(tickets_with_gate(&passing_gate)),
            true,
        );

        assert!(
            matches!(step, Step::Complete),
            "{label}: the passing final gate resolves the ticket: {step:?}"
        );
        let requests = code_requests(&transport);
        assert_eq!(
            requests.len(),
            expected_requests,
            "{label}: the first attempt and every HTTP attempt of the second"
        );
        assert_eq!(
            distinct_requests(&requests).len(),
            2,
            "{label}: exactly two ticket attempts, never a third"
        );
        assert_eq!(
            gate_runs(&passing_log),
            1,
            "{label}: the passing final gate runs exactly once after the second attempt"
        );
        assert_eq!(snapshot(&dir)["1-verify-command-output"], "completed");
        let passing_report = report(&dir);
        assert!(
            passing_report.contains("- Status: completed"),
            "{label}: {passing_report}"
        );
        assert!(
            passing_report.contains("- Ended: gate accepted without declaration"),
            "{label}: {passing_report}"
        );

        drop(repository);
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&passing_log_dir);

        // A failing final gate leaves the ticket failed after the second and
        // maximum attempt; no third attempt may start.
        let failing_log_dir = super::scratch_dir();
        fs::create_dir_all(&failing_log_dir).expect("the failing gate log directory is creatable");
        let failing_log = failing_log_dir.join("gate-runs.log");
        let failing_gate = logging_gate(&failing_log, "printf 'FINAL-GATE-FAILED\\n' >&2; exit 1");
        let repository = common::TempRepo::new();
        let (step, dir, repository, transport) = run_with(
            repository,
            replies,
            common::GateScript::Passing,
            Some(tickets_with_gate(&failing_gate)),
            true,
        );

        assert!(
            matches!(step, Step::Retry),
            "{label}: the failing final gate leaves the run retryable: {step:?}"
        );
        let requests = code_requests(&transport);
        assert_eq!(
            requests.len(),
            expected_requests,
            "{label}: no third ticket attempt starts after the failing final gate"
        );
        assert_eq!(
            distinct_requests(&requests).len(),
            2,
            "{label}: exactly two ticket attempts"
        );
        assert_eq!(
            gate_runs(&failing_log),
            1,
            "{label}: the failing final gate runs exactly once after the second attempt"
        );
        assert_eq!(snapshot(&dir)["1-verify-command-output"], "failed");
        let failing_report = report(&dir);
        assert!(
            failing_report.contains("- Status: failed"),
            "{label}: {failing_report}"
        );
        assert!(
            failing_report.contains("retry exhausted without task_complete"),
            "{label}: {failing_report}"
        );

        drop(repository);
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&failing_log_dir);
    }
}

#[test]
fn when_ticket_work_given_second_attempt_declaration_and_failing_final_gate_then_keeps_ticket_failed()
 {
    let log_dir = super::scratch_dir();
    fs::create_dir_all(&log_dir).expect("the gate log directory is creatable");
    let log = log_dir.join("gate-runs.log");
    let gate = logging_gate(
        &log,
        "printf 'SECOND-DECLARATION-GATE-FAILED\\n' >&2; exit 1",
    );
    let replies = vec![
        common::CodeReply::TurnEnd(None),
        common::CodeReply::Complete(Some("the second attempt declared completion".to_string())),
    ];
    let repository = common::TempRepo::new();
    let (step, dir, repository, transport) = run_with(
        repository,
        replies,
        common::GateScript::Passing,
        Some(tickets_with_gate(&gate)),
        true,
    );

    assert!(
        matches!(step, Step::Retry),
        "the failing final gate overrules the declaration: {step:?}"
    );
    let requests = code_requests(&transport);
    assert_eq!(requests.len(), 2, "no third ticket attempt");
    assert_eq!(
        distinct_requests(&requests).len(),
        2,
        "exactly two ticket attempts"
    );
    assert_eq!(
        gate_runs(&log),
        1,
        "the final gate is the only gate the ticket ever reached"
    );
    assert_eq!(snapshot(&dir)["1-verify-command-output"], "failed");
    let report = report(&dir);
    assert!(report.contains("- Status: failed"), "{report}");
    assert!(report.contains("- Ended: gate failed"), "{report}");

    drop(repository);
    let _ = fs::remove_dir_all(&dir);
    let _ = fs::remove_dir_all(&log_dir);
}

// -- Regression Tests -------------------------------------------------------

#[test]
fn when_ticket_work_given_exhausted_transport_retries_then_keeps_model_retries_separate_from_ticket_attempts_and_persists_independent_results()
 {
    let log_dir = super::scratch_dir();
    fs::create_dir_all(&log_dir).expect("the gate log directory is creatable");
    let log = log_dir.join("gate-runs.log");
    let gate = logging_gate(&log, "exit 0");
    let replies = vec![
        common::CodeReply::Transport("the failed branch could not reach the endpoint".to_string()),
        common::CodeReply::Transport("the failed branch could not reach the endpoint".to_string()),
        common::CodeReply::Transport("the failed branch could not reach the endpoint".to_string()),
        common::CodeReply::Complete(Some("the independent branch finished".to_string())),
    ];
    let repository = common::TempRepo::new();
    let (step, dir, repository, transport) = run_with(
        repository,
        replies,
        common::GateScript::Passing,
        Some(two_independent_tickets(&gate)),
        true,
    );

    assert!(
        matches!(step, Step::Retry),
        "the exhausted branch leaves the queue incomplete: {step:?}"
    );
    let requests = code_requests(&transport);
    assert_eq!(
        requests.len(),
        4,
        "three model-level HTTP attempts for the failed ticket, then the independent ticket's one request"
    );
    assert!(
        requests[..3].windows(2).all(|pair| pair[0] == pair[1]),
        "the transport policy retries the same ticket request, not a new ticket attempt"
    );
    assert_eq!(
        distinct_requests(&requests).len(),
        2,
        "two ticket attempts: one failed, one independent"
    );
    assert!(
        requests
            .iter()
            .all(|request| user_messages(request).len() == 1),
        "the exhausted ticket never earns a handover"
    );
    assert!(
        !requests.iter().any(carries_retry_handover),
        "no ticket-level handover exists"
    );
    assert_eq!(
        gate_runs(&log),
        1,
        "the failed ticket's gate never runs; the independent ticket's does"
    );
    let status = snapshot(&dir);
    assert_eq!(status["1-verify-command-output"], "failed");
    assert_eq!(status["2-independent-branch"], "completed");
    let report = report(&dir);
    assert!(
        report.contains("- Ended: agent failure: transport error"),
        "{report}"
    );
    assert!(
        report.contains("1 completed, 1 failed, 0 blocked"),
        "{report}"
    );

    drop(repository);
    let _ = fs::remove_dir_all(&dir);
    let _ = fs::remove_dir_all(&log_dir);
}
