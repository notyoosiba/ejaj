#![allow(clippy::cognitive_complexity, clippy::manual_let_else)]

//! The two-provider pipeline: one `Pipeline::ask` must route the planning
//! phases -- plan, spec and tickets -- to the giga endpoint and the code
//! phase to the mid provider, through the production types, anchored to a disposable
//! repository that begins clean and is left unchanged.
//!
//! Every test comes in two forms: the default replays a committed fixture with
//! no network, and the `live_` twin (ignored) runs against the real endpoints --
//! recording the fixture when `EJAJ_LLM_RECORD=1` is set. See `tests/common/mod.rs`.

mod common;

use ejaj::Config;
use ejaj::llm::Mode;
use ejaj::pipeline::{Pipeline, Step};
use ejaj::plan_document::Plan;
use ejaj::specification::Specification;
use ejaj::tools::Phase;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

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
        "ejaj-hybrid-{}-{}-{}",
        std::process::id(),
        nanos,
        ordinal
    ))
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

/// The routing the mode promises: the giga provider makes the plan, the
/// specification, the tickets, the triage audit and the review; the mid
/// provider runs the code phase. A wrong routing would also make replay fail,
/// because the recorded URL would not match, but asserting it here names the
/// contract directly.
fn assert_hybrid_routing(config: &Config) {
    for phase in [
        Phase::Plan,
        Phase::Spec,
        Phase::Tickets,
        Phase::Triage,
        Phase::Review,
    ] {
        assert_eq!(
            config.provider(phase).name(),
            "responses",
            "phase {phase:?}"
        );
    }
    assert_eq!(
        config.provider(Phase::Code).name(),
        "chat-completions",
        "the code phase must run on the mid provider"
    );
    assert_eq!(
        config.provider(Phase::Summary).name(),
        "anthropic-messages",
        "the summary phase always runs on the Smoll provider"
    );
}

/// The whole run: one `Pipeline::ask` over the harmless plan, then the `Step`,
/// every artifact, the ticket files, and the repository's before/after status.
fn run_the_pipeline(config: &Config) {
    assert_hybrid_routing(config);

    let dir = scratch_dir();
    // The pipeline's root is the run's own disposable repository, and the
    // artifacts land inside it, so a successful run commits the changes it
    // made. The baseline commit makes the preflight see a clean tree.
    let repository = common::TempRepo::at(dir.clone());
    repository.commit_baseline();
    let before = repository.status();

    let mut pipeline = Pipeline::new(config, common::scope(), repository.path());
    match pipeline.ask(&common::harmless_plan_request(), &dir) {
        // The queue resolved every ticket, or stopped on one: both terminal
        // steps come from the recorded model, so either is acceptable.
        Step::Complete | Step::Retry => {}
        Step::WriteFailed { artifact, error } => common::failed(
            "the hybrid pipeline run",
            &format!("could not write the {artifact} artifact: {error}"),
        ),
        Step::SummaryFailed { reason } => common::failed(
            "the hybrid summary phase",
            &format!("the run could not be summarised: {reason}"),
        ),
        // The one staged Git failure: the preflight of the disposable
        // repository, the staging of its changes and the commit that follows
        // are all stages this hybrid run must pass.
        Step::GitFailed { stage, reason } => {
            panic!("the disposable repository must pass the {stage} stage: {reason}")
        }
    }

    // The plan artifact is the harness re-render of the typed plan the boundary
    // exchanged, so it must parse, carry the mandatory content and render back
    // to the same bytes. The giga provider's wording is its own: the hybrid
    // recording does not require a verbatim copy.
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

    // The specification artifact must parse into a titled specification.
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

    // One markdown file per ticket, each holding a non-empty document.
    let files = ticket_files(&dir);
    assert!(!files.is_empty(), "the pipeline wrote no ticket files");
    for path in &files {
        let text = fs::read_to_string(path)
            .unwrap_or_else(|e| common::failed("a ticket artifact", &e.to_string()));
        assert!(!text.is_empty(), "{} is empty", path.display());
    }

    let triage_text = fs::read_to_string(dir.join("ticket-triage.md"))
        .unwrap_or_else(|e| common::failed("the triage artifact", &e.to_string()));
    assert!(
        triage_text.starts_with("# Ticket triage report\n"),
        "the triage report has no heading: {triage_text}"
    );

    let review_text = fs::read_to_string(dir.join("review.md"))
        .unwrap_or_else(|e| common::failed("the review artifact", &e.to_string()));
    assert!(!review_text.is_empty(), "review.md is empty");
    // The change summary lands in the same run directory, holding the
    // canonical text the scripted summary pass answered with.
    let summary_text = fs::read_to_string(dir.join("change-summary.md"))
        .unwrap_or_else(|e| common::failed("the change summary artifact", &e.to_string()));
    assert_eq!(summary_text, common::SCRIPTED_SUMMARY);

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
fn when_running_given_recorded_hybrid_pipeline_then_preserves_giga_mid_and_smoll_routes() {
    let (config, transport) = common::scripted_hybrid_pipeline_config();
    run_the_pipeline(&config);
    let urls = transport.urls_called.lock().unwrap().clone();
    assert_eq!(
        urls.len(),
        8,
        "expected the 6 queue-driven steps (3 handoffs, the triage pass and the 2 code steps), the review pass and the summary pass"
    );
    // Giga still plans, specifies, slices and audits.
    for (i, url) in urls[..4].iter().enumerate() {
        assert_eq!(
            url,
            &common::test_giga_url(),
            "step {i} (planning phase) must route to the giga endpoint"
        );
    }
    // Mid still runs the code phase.
    for (i, url) in urls[4..6].iter().enumerate() {
        assert_eq!(
            url,
            &common::test_url(),
            "step {} (code phase) must route to the mid endpoint",
            i + 4
        );
    }
    assert_eq!(
        urls[6],
        common::test_giga_url(),
        "the review phase must run on the giga provider"
    );
    // Summary is not a mode-routed phase: it always runs on the dedicated Smoll
    // Anthropic Messages provider, after the review pass, and its request
    // carries the report the queue generated.
    assert_eq!(
        urls[7],
        common::TEST_SMOLL_URL,
        "the summary phase must run on the Smoll provider"
    );
    let summary = transport
        .requests
        .lock()
        .unwrap()
        .last()
        .expect("the run made a summary request")
        .clone();
    assert_eq!(
        summary["max_tokens"],
        serde_json::json!(16384),
        "the summary request is a native Anthropic Messages body: {summary}"
    );
    let tools: Vec<&str> = summary["tools"]
        .as_array()
        .expect("the native summary request advertises tools")
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert_eq!(
        tools,
        ["execute_script"],
        "the summary request advertises only its read-only tool"
    );
    let body = summary.to_string();
    assert!(
        body.contains("## 1-verify-command-output"),
        "the summary request must carry the queue report: {body}"
    );
}

/// Edge-case table: every hybrid phase has exactly one established endpoint,
/// whichever phase comes before or after it in a run.
#[test]
fn when_routing_given_each_hybrid_phase_then_uses_its_established_endpoint() {
    let (config, _transport) = common::scripted_hybrid_pipeline_config();

    let cases = [
        (Phase::Plan, "responses", common::test_giga_url()),
        (Phase::Spec, "responses", common::test_giga_url()),
        (Phase::Tickets, "responses", common::test_giga_url()),
        (Phase::Triage, "responses", common::test_giga_url()),
        (Phase::Review, "responses", common::test_giga_url()),
        (Phase::Code, "chat-completions", common::test_url()),
        (
            Phase::Summary,
            "anthropic-messages",
            common::TEST_SMOLL_URL.to_string(),
        ),
    ];

    for (phase, protocol, endpoint) in cases {
        let provider = config.provider(phase);
        assert_eq!(provider.name(), protocol, "{phase:?} wire format");
        let prepared = provider.prepare(&[], phase);
        assert_eq!(prepared.outgoing.url, endpoint, "{phase:?} endpoint");
    }
}

/// Contract: explicit giga coordinates go through the canonical builder, and
/// the hybrid route uses them for the design phases while the mid block keeps
/// the code phase. Summary is not mode-routed: it uses Smoll in every mode.
#[test]
fn when_building_given_explicit_giga_settings_then_uses_canonical_builder() {
    let giga_url = "https://giga.example/v1/responses";
    let config = Config::new(
        common::test_url(),
        common::test_model(),
        Some(common::TEST_API_KEY.to_string()),
    )
    .with_giga(
        giga_url,
        "explicit-giga-model",
        Some("giga-key".to_string()),
    )
    .with_smoll(
        common::TEST_SMOLL_URL,
        common::TEST_SMOLL_MODEL,
        common::TEST_SMOLL_API_KEY,
    )
    .with_mode(Mode::Hybrid);

    let plan = config.provider(Phase::Plan).prepare(&[], Phase::Plan);
    assert_eq!(plan.outgoing.url, giga_url);
    assert_eq!(
        plan.outgoing.body["model"].as_str(),
        Some("explicit-giga-model"),
        "the explicit giga model must be the one requested"
    );

    let code = config.provider(Phase::Code).prepare(&[], Phase::Code);
    assert_eq!(code.outgoing.url, common::test_url());
    assert_eq!(
        code.outgoing.body["model"].as_str(),
        Some(common::test_model().as_str()),
        "the mid block still serves the code phase"
    );
    assert!(
        config
            .describe()
            .contains("responses (explicit-giga-model)"),
        "the canonical builder's giga coordinates must show in the description: {}",
        config.describe()
    );
}

/// A summary failure is fatal in the two-provider mode as well, and it happens
/// on the Smoll provider: the giga provider plans, specifies and slices, the
/// mid provider codes, and neither is asked to summarise the run.
#[test]
fn a_summary_failure_is_fatal_in_hybrid_mode() {
    let (config, transport) =
        common::scripted_hybrid_pipeline_with(common::SummaryScript::ApiError);
    let dir = scratch_dir();
    let repository = common::TempRepo::at(dir.join("repository"));

    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    match pipeline.ask(&common::harmless_plan_request(), &dir) {
        Step::SummaryFailed { reason } => assert!(
            reason.contains("API error"),
            "the reason must name the failure: {reason}"
        ),
        other => panic!("expected a fatal summary failure, got {other:?}"),
    }
    assert!(
        !dir.join("change-summary.md").exists(),
        "a failed summary leaves no artifact"
    );

    // The failing request is the summary's own, and it went to Smoll.
    let urls = transport.urls_called.lock().unwrap().clone();
    assert_eq!(
        urls.len(),
        8,
        "six queue-driven steps, one review pass and the one summary pass"
    );
    assert_eq!(
        urls[6],
        common::test_giga_url(),
        "the review phase must run on the giga provider"
    );
    assert_eq!(
        urls[7],
        common::TEST_SMOLL_URL,
        "the summary phase must run on the Smoll provider even when it fails"
    );

    let _ = fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Triage routing
// ---------------------------------------------------------------------------

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

/// The mode still splits by what a phase does: the additional triage audit
/// inspects and designs, so hybrid routes it to the giga beside plan, spec,
/// tickets and review, while the code phase stays on the mid. Summary always
/// runs on Smoll. Mid mode, by contrast, serves the triage pass from its
/// configured provider.
#[test]
fn when_triaging_tickets_given_hybrid_mode_then_routes_the_audit_to_giga() {
    let mid_config = Config::new(
        common::test_url(),
        common::test_model(),
        Some(common::TEST_API_KEY.to_string()),
    );
    assert_eq!(
        mid_config.provider(Phase::Triage).name(),
        "chat-completions",
        "mid mode runs the triage pass on its configured provider"
    );

    let (config, transport) =
        common::scripted_hybrid_pipeline_with_triage(common::TriageScript::AllReady);
    let dir = scratch_dir();
    let repository = common::TempRepo::at(dir.clone());
    repository.commit_baseline();

    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), &dir);
    assert!(
        matches!(step, Step::Complete),
        "an all-ready triage completes: {step:?}"
    );

    let urls = transport.urls_called.lock().unwrap().clone();
    assert_eq!(
        urls.len(),
        8,
        "plan, spec, tickets, triage, the ticket's two steps, the review pass and the summary"
    );
    let index = triage_request_index(&transport);
    assert_eq!(
        index, 3,
        "the triage request follows the three handoff writes and precedes the queue"
    );
    assert_eq!(
        urls[index],
        common::test_giga_url(),
        "the triage audit must run on the giga provider"
    );
    assert_eq!(
        urls[4],
        common::test_url(),
        "the code phase must run on the mid provider"
    );
    assert_eq!(
        urls[5],
        common::test_url(),
        "the code phase must run on the mid provider"
    );
    assert_eq!(
        urls[6],
        common::test_giga_url(),
        "the review phase must run on the giga provider"
    );
    assert_eq!(
        urls[7],
        common::TEST_SMOLL_URL,
        "the summary phase must run on the Smoll provider"
    );

    // The report the audit produced landed beside the plan and the spec.
    assert!(dir.join("ticket-triage.md").is_file());
    assert_eq!(config.provider(Phase::Triage).name(), "responses");

    let _ = fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Phase timing in the two-provider mode
//
// The elapsed lines are mode-independent: one line per invoked phase invocation,
// whichever provider served the request. The transcript is read back at the
// process boundary, exactly as the single-provider phase-timing tests do, and
// the run that produced it is a whole production Pipeline::ask over both
// scripted providers.
// ---------------------------------------------------------------------------

/// The environment variable a child copy of this test sets on itself: a copy
/// that sees it performs the run for real -- letting the pipeline's own output
/// reach the streams the parent captured -- and returns.
const HYBRID_TIMING_CHILD: &str = "EJAJ_HYBRID_PHASE_TIMING_CHILD";

/// The phase labels of one complete run, in invocation order.
const HYBRID_PHASE_LABELS: [&str; 7] = [
    "plan-agent ask",
    "spec pass",
    "tickets pass",
    "triage pass",
    "ticket-queue round 1",
    "review round 1",
    "summary pass",
];

/// The label of every elapsed line in transcript, in order.
fn elapsed_labels(transcript: &str) -> Vec<String> {
    transcript
        .lines()
        .map(str::trim_end)
        .filter(|line| line.starts_with("[INFO] ") && line.contains(" elapsed: "))
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

/// The routing the two-provider run really used, and every artifact the run
/// wrote, plus the timed phase labels the run reported: the same ones the
/// single-provider mode reports, because the phases are the same.
#[test]
fn when_hybrid_pipeline_runs_given_scripted_providers_then_preserves_routing_and_artifacts() {
    // The child copy: perform the run for real and return, so the parent reads
    // the production transcript at the process boundary.
    if std::env::var_os(HYBRID_TIMING_CHILD).is_some() {
        // The test harness prints its own progress line with no terminator, so
        // the first line the pipeline writes needs one of its own.
        println!();
        let (config, _transport) = common::scripted_hybrid_pipeline_config();
        let dir = scratch_dir();
        let repository = common::TempRepo::at(dir.clone());
        repository.commit_baseline();

        let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
        let step = pipeline.ask(&common::harmless_plan_request(), &dir);
        assert!(matches!(step, Step::Complete), "{step:?}");
        let _ = fs::remove_dir_all(&dir);
        return;
    }

    // The artifacts, and the routing the mode promises: the giga plans,
    // specifies, slices, audits and reviews; the mid codes; the Smoll provider
    // summarises.
    let (config, transport) = common::scripted_hybrid_pipeline_config();
    run_the_pipeline(&config);
    let urls = transport.urls_called.lock().unwrap().clone();
    assert_eq!(
        urls.len(),
        8,
        "three handoffs, the triage pass, two code steps, the review and the summary"
    );
    for (index, url) in urls[..4].iter().enumerate() {
        assert_eq!(
            url,
            &common::test_giga_url(),
            "planning step {index} must route to the giga provider"
        );
    }
    assert_eq!(
        urls[4],
        common::test_url(),
        "the first code step must route to the mid provider"
    );
    assert_eq!(
        urls[5],
        common::test_url(),
        "the second code step must route to the mid provider"
    );
    assert_eq!(
        urls[6],
        common::test_giga_url(),
        "the review phase must route to the giga provider"
    );
    assert_eq!(
        urls[7],
        common::TEST_SMOLL_URL,
        "the summary phase must route to the Smoll provider"
    );

    // And the additive timing is unchanged by the mode: one line per invoked
    // phase, with the same exact labels.
    let transcript = common::captured_transcript(
        "when_hybrid_pipeline_runs_given_scripted_providers_then_preserves_routing_and_artifacts",
        &[(HYBRID_TIMING_CHILD, "1")],
    );
    let labels: Vec<String> = elapsed_labels(&transcript)
        .into_iter()
        .filter(|label| {
            label != "bash"
                && label != "execute_script"
                && label != "thinking"
                && !label.ends_with(" verification gate")
                && !label.ends_with(" overhead")
        })
        .collect();
    assert_eq!(labels, HYBRID_PHASE_LABELS, "{transcript}");
}
