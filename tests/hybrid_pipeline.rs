#![allow(clippy::cognitive_complexity, clippy::manual_let_else)]

//! The two-provider pipeline: one `Pipeline::ask` must route the planning
//! phases -- plan, spec and tickets -- to the giga endpoint and the code
//! phase to the mid provider, through the production types, anchored to a disposable
//! repository that begins clean and is left unchanged.
//!
//! Every test runs against the deterministic in-memory `ScriptedTransport` with
//! no network access and no live model dependency. See `tests/common/mod.rs`
//! for the scripted provider responses and disposable repository helpers.

mod common;

use ejaj::Config;
use ejaj::llm::Mode;
use ejaj::pipeline::{Pipeline, Step};
use ejaj::plan_document::Plan;
use ejaj::specification::Specification;
use ejaj::tickets::TicketSet;
use ejaj::tools::Phase;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
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

// ---------------------------------------------------------------------------
// T26: the one Giga handover retry
// ---------------------------------------------------------------------------

/// The environment variable a child copy of the retry test sets on itself: a
/// copy that sees it performs the run for real, so the parent reads the
/// production status output at the process boundary.
const HYBRID_RETRY_STATUS_CHILD: &str = "EJAJ_HYBRID_RETRY_STATUS_CHILD";

/// The configured input rates the production configuration carries. The code
/// phase starts on Mid; only the derived Giga retry selects the Giga rate.
const MID_INPUT_PRICE: f64 = 1.25;
const GIGA_INPUT_PRICE: f64 = 2.5;
const SMOLL_INPUT_PRICE: f64 = 3.75;

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

/// A shell gate that logs every invocation and then does the given thing.
fn logging_gate(log: &Path, result: &str) -> String {
    format!("printf 'gate run\n' >> {}; {result}", log.display())
}

/// How many times a logging shell gate really ran. A missing log means zero.
fn gate_runs(log: &Path) -> usize {
    fs::read_to_string(log)
        .map(|text| text.lines().count())
        .unwrap_or(0)
}

/// A hybrid configuration resolved from explicit environment values, so the
/// per-role configured input rates are part of the same configuration the
/// production lifecycle reads. The transport is the only scripted boundary.
fn priced_config(mode: Mode, transport: Arc<common::ScriptedTransport>) -> Config {
    let values: Vec<(&str, String)> = vec![
        ("AGENT_MODE", mode.label().to_string()),
        ("LLM_MID_URL", common::DEFAULT_TEST_URL.to_string()),
        ("LLM_MID_MODEL", common::TEST_MODEL.to_string()),
        ("LLM_MID_API_KEY", common::TEST_API_KEY.to_string()),
        (
            "LLM_MID_INPUT_PRICE_PER_MILLION_USD",
            MID_INPUT_PRICE.to_string(),
        ),
        ("LLM_GIGA_URL", common::TEST_GIGA_URL.to_string()),
        ("LLM_GIGA_MODEL", common::TEST_GIGA_MODEL.to_string()),
        ("LLM_GIGA_API_KEY", common::TEST_API_KEY.to_string()),
        (
            "LLM_GIGA_INPUT_PRICE_PER_MILLION_USD",
            GIGA_INPUT_PRICE.to_string(),
        ),
        ("LLM_SMOLL_URL", common::TEST_SMOLL_URL.to_string()),
        ("LLM_SMOLL_MODEL", common::TEST_SMOLL_MODEL.to_string()),
        ("LLM_SMOLL_API_KEY", common::TEST_SMOLL_API_KEY.to_string()),
        (
            "LLM_SMOLL_INPUT_PRICE_PER_MILLION_USD",
            SMOLL_INPUT_PRICE.to_string(),
        ),
    ];
    let environment = |name: &str| {
        values
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.clone())
    };
    Config::from_environment(&environment)
        .unwrap_or_else(|message| panic!("the priced hybrid configuration resolves: {message}"))
        .with_transport(transport)
}

/// Run one production hybrid pipeline over a disposable repository, keeping
/// the run's session artifacts under its real .ejaj directory so the
/// trajectories persist exactly as they do in production.
fn run_hybrid(
    mode: Mode,
    replies: Vec<common::CodeReply>,
    tickets: TicketSet,
) -> (
    Step,
    PathBuf,
    Arc<common::ScriptedTransport>,
    common::TempRepo,
) {
    let transport = Arc::new(
        common::ScriptedTransport::new()
            .with_tickets(tickets)
            .with_code_replies(replies),
    );
    let config = priced_config(mode, transport.clone());
    let work = scratch_dir();
    let repository = common::TempRepo::at(work);
    repository.commit_baseline();
    let dir = repository.path().join(".ejaj/run");
    let mut pipeline = Pipeline::new(&config, common::scope(), repository.path());
    let step = pipeline.ask(&common::harmless_plan_request(), &dir);
    (step, dir, transport, repository)
}

/// Every code-agent request the run made, with its outgoing URL, in order.
fn code_exchanges(transport: &common::ScriptedTransport) -> Vec<(String, serde_json::Value)> {
    let urls = transport.urls_called.lock().unwrap();
    let requests = transport.requests.lock().unwrap();
    urls.iter()
        .cloned()
        .zip(requests.iter().cloned())
        .filter(|(_, body)| body.to_string().contains(common::CODE_MARKER))
        .collect()
}

/// Every user message one request carried, in order, on either wire format:
/// the chat-completions messages array or the Responses API input array.
fn user_messages(body: &serde_json::Value) -> Vec<String> {
    let items = body["messages"]
        .as_array()
        .or_else(|| body["input"].as_array());
    items
        .into_iter()
        .flatten()
        .filter(|message| message["role"] == "user")
        .filter_map(|message| message["content"].as_str())
        .map(str::to_string)
        .collect()
}

/// Whether one request carried the retry handover.
fn carries_retry_handover(body: &serde_json::Value) -> bool {
    body.to_string().contains("[RETRY HANDOVER]")
}

/// Read one persisted trajectory snapshot.
fn trajectory(dir: &Path, name: &str) -> serde_json::Value {
    let path = dir.join("trajectories").join(name);
    let text =
        fs::read_to_string(&path).unwrap_or_else(|e| panic!("{} is readable: {e}", path.display()));
    serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("{} is trajectory JSON: {e}", path.display()))
}

/// Every status line the production transcript carried for the code phase, as
/// its (provider role, configured model) pair, in order.
fn code_thinking_reports(transcript: &str) -> Vec<(String, String)> {
    transcript
        .lines()
        .filter_map(|line| {
            let (_, rest) = line.split_once("#thinking (")?;
            let (_, rest) = rest.split_once("s) ")?;
            let mut fields = rest.split_whitespace();
            let provider = fields.next()?.to_string();
            let model = fields.next()?.strip_prefix("model=")?.to_string();
            let phase = fields.next()?.strip_prefix("phase=")?;
            (phase == "code").then_some((provider, model))
        })
        .collect()
}

/// The production run at the retry seam: a Mid tool call followed by a
/// tool-free turn end, then a Giga handover retry whose tool-free Responses API
/// turn is an explicit empty assistant message. A passing gate makes the queue
/// resolve without that retry ever declaring completion.
#[test]
fn when_hybrid_code_attempt_ends_given_retryable_turn_end_then_escalates_once_and_preserves_other_routes()
 {
    let state = scratch_dir();
    fs::create_dir_all(&state).expect("the retry state directory is creatable");
    let gate_log = state.join("gate-runs.log");
    let gate = logging_gate(&gate_log, "exit 0");
    let tickets = tickets_with_gate(&gate);
    let replies = vec![
        common::CodeReply::ToolCall {
            narration: None,
            command: "printf 'first attempt change\n' > retry-change.txt".to_string(),
        },
        common::CodeReply::TurnEnd(None),
        common::CodeReply::TurnEnd(None),
    ];

    if std::env::var_os(HYBRID_RETRY_STATUS_CHILD).is_some() {
        println!();
        let (step, _dir, _transport, _repository) = run_hybrid(Mode::Hybrid, replies, tickets);
        assert!(matches!(&step, Step::Complete), "{step:?}");
        let _ = fs::remove_dir_all(&state);
        return;
    }

    // The configured rates live on the very configuration the production
    // lifecycle reads: the initial code attempt selects the Mid rate, while the
    // Giga block (which the handover retry inherits) carries the Giga rate.
    let hybrid = priced_config(Mode::Hybrid, Arc::new(common::ScriptedTransport::new()));
    assert_eq!(
        hybrid.input_price(Phase::Code),
        Some(MID_INPUT_PRICE),
        "the initial code attempt is priced at the configured mid rate"
    );
    assert_eq!(
        hybrid.input_price(Phase::Review),
        Some(GIGA_INPUT_PRICE),
        "the hybrid giga block carries the configured giga rate"
    );
    let giga = priced_config(Mode::Giga, Arc::new(common::ScriptedTransport::new()));
    assert_eq!(
        giga.input_price(Phase::Code),
        Some(GIGA_INPUT_PRICE),
        "a giga code route is priced at the configured giga rate"
    );
    assert_hybrid_routing(&hybrid);

    let (step, dir, transport, _repository) = run_hybrid(Mode::Hybrid, replies, tickets);
    assert!(matches!(&step, Step::Complete), "{step:?}");

    // Attempt 1 makes both of its code requests to the mid endpoint and only
    // stops after the real tool call (so a follow-up turn was needed). Attempt
    // 2 is the one Giga Responses API request, carrying a separate handover.
    let exchanges = code_exchanges(&transport);
    assert_eq!(
        exchanges.len(),
        3,
        "two mid turns for the first attempt and one giga turn for the retry"
    );
    assert_eq!(exchanges[0].0, common::test_url(), "first code turn is mid");
    assert_eq!(exchanges[1].0, common::test_url(), "tool follow-up is mid");
    assert_eq!(
        exchanges[2].0,
        common::test_giga_url(),
        "the handover retry must use the giga Responses API endpoint"
    );
    for (index, (_, body)) in exchanges[..2].iter().enumerate() {
        assert_eq!(
            body["model"],
            serde_json::json!(common::TEST_MODEL),
            "mid code turn {index} must request the configured mid model"
        );
    }
    let retry = &exchanges[2].1;
    assert_eq!(
        retry["model"],
        serde_json::json!(common::TEST_GIGA_MODEL),
        "the retry must request the configured giga model"
    );
    assert!(
        retry.get("input").is_some(),
        "the retry must be a Responses API body: {retry}"
    );
    let first_users = user_messages(&exchanges[0].1);
    let retry_users = user_messages(retry);
    assert_eq!(
        first_users.len(),
        1,
        "the first attempt carries only the ticket kickoff"
    );
    assert_eq!(
        retry_users.len(),
        2,
        "the retry adds one separate handover user message"
    );
    assert_eq!(
        retry_users[0], first_users[0],
        "the retry keeps the unchanged ticket kickoff"
    );
    assert!(
        retry_users[1].contains("[RETRY HANDOVER]"),
        "the separate retry message must be the handover: {}",
        retry_users[1]
    );
    assert!(
        retry_users[1].contains("(empty narration)"),
        "the first attempt narrated nothing, so the handover says so: {}",
        retry_users[1]
    );
    assert!(
        retry_users[1].contains("turn end without task_complete"),
        "the handover names the retryable stop: {}",
        retry_users[1]
    );
    assert!(
        !carries_retry_handover(&exchanges[0].1) && !carries_retry_handover(&exchanges[1].1),
        "the first attempt must not carry a retry handover"
    );

    // The first attempt really called the bash tool and then ended its turn;
    // the retry's tool-free Responses API turn decoded as an ordinary turn end
    // through the explicit empty assistant message, not as malformed output.
    let first_trajectory = trajectory(&dir, "code.traj.json");
    assert_eq!(first_trajectory["role"], serde_json::json!("mid"));
    assert_eq!(
        first_trajectory["model"],
        serde_json::json!(common::TEST_MODEL)
    );
    assert_eq!(
        first_trajectory["exit_status"],
        serde_json::json!("turn_end")
    );
    let first_messages = first_trajectory["messages"]
        .as_array()
        .expect("the trajectory carries its messages");
    assert!(
        first_messages
            .iter()
            .any(|message| message["role"] == serde_json::json!("tool")),
        "the first attempt must have run a real tool: {first_trajectory}"
    );
    let retry_trajectory = trajectory(&dir, "code-1.traj.json");
    assert_eq!(retry_trajectory["role"], serde_json::json!("giga"));
    assert_eq!(
        retry_trajectory["model"],
        serde_json::json!(common::TEST_GIGA_MODEL)
    );
    assert_eq!(
        retry_trajectory["exit_status"],
        serde_json::json!("turn_end")
    );
    let retry_messages = retry_trajectory["messages"]
        .as_array()
        .expect("the retry trajectory carries its messages");
    assert!(
        !retry_messages
            .iter()
            .any(|message| message["tool_calls"].is_array()),
        "an empty assistant turn must call no tool: {retry_trajectory}"
    );

    // Only the final gate ran: the first attempt never declared completion, so
    // it never reached the first gate, and the passing final gate resolved the
    // ticket without a declaration.
    assert_eq!(
        gate_runs(&gate_log),
        1,
        "only the second attempt's final gate runs"
    );
    let report = fs::read_to_string(dir.join("queue-report.md"))
        .expect("the queue report artifact is readable");
    assert!(
        report.contains("- Ended: gate accepted without declaration"),
        "the passing final gate is authoritative: {report}"
    );

    // The normal hybrid routes survive the retry: plan/spec/tickets/triage and
    // the review stay on giga, and summary stays on Smoll.
    let urls = transport.urls_called.lock().unwrap().clone();
    assert_eq!(
        urls.len(),
        9,
        "plan, spec, tickets, triage, three code turns, review and summary"
    );
    for (index, url) in urls[..4].iter().enumerate() {
        assert_eq!(url, &common::test_giga_url(), "planning step {index}");
    }
    assert_eq!(urls[6], common::test_giga_url(), "the review is giga");
    assert_eq!(urls[7], common::test_giga_url(), "the review is giga");
    assert_eq!(urls[8], common::TEST_SMOLL_URL, "the summary is Smoll");

    // The production status output names the retry's provider identity and
    // model: two mid code turns, then one giga code turn. The same giga
    // identity and model are what the persisted trajectory recorded above.
    let transcript = common::captured_transcript(
        "when_hybrid_code_attempt_ends_given_retryable_turn_end_then_escalates_once_and_preserves_other_routes",
        &[(HYBRID_RETRY_STATUS_CHILD, "1")],
    );
    let reports = code_thinking_reports(&transcript);
    assert_eq!(
        reports.len(),
        3,
        "one status line per code request: {transcript}"
    );
    assert_eq!(
        reports[0],
        ("mid".to_string(), common::TEST_MODEL.to_string()),
        "{transcript}"
    );
    assert_eq!(
        reports[1],
        ("mid".to_string(), common::TEST_MODEL.to_string()),
        "{transcript}"
    );
    assert_eq!(
        reports[2],
        ("giga".to_string(), common::TEST_GIGA_MODEL.to_string()),
        "{transcript}"
    );

    let _ = fs::remove_dir_all(&state);
}

/// Contract: a failed first verification gate buys the same one fresh retry as
/// an eligible stop, and that retry runs the derived Giga configuration. The
/// second attempt here ends without declaring completion, so only the passing
/// final gate resolves the ticket.
#[test]
fn when_hybrid_code_attempt_retries_given_first_gate_failure_then_uses_giga_for_single_handover() {
    let state = scratch_dir();
    fs::create_dir_all(&state).expect("the retry state directory is creatable");
    let gate_log = state.join("gate-runs.log");
    let failed_once = state.join("first-gate-failed");
    let gate = format!(
        "printf 'gate run\n' >> {log}; if [ ! -e {state} ]; then : > {state}; \
         printf 'FIRST-GATE-DIAGNOSTIC\n' >&2; \
         printf 'first gate change\n' > first-gate-change.txt; exit 1; else exit 0; fi",
        log = gate_log.display(),
        state = failed_once.display(),
    );
    let tickets = tickets_with_gate(&gate);
    let replies = vec![
        common::CodeReply::Complete(None),
        common::CodeReply::TurnEnd(None),
    ];

    let (step, dir, transport, _repository) = run_hybrid(Mode::Hybrid, replies, tickets);
    assert!(matches!(&step, Step::Complete), "{step:?}");

    let exchanges = code_exchanges(&transport);
    assert_eq!(
        exchanges.len(),
        2,
        "exactly the first attempt and its one fresh handover retry"
    );
    assert_eq!(
        exchanges[0].0,
        common::test_url(),
        "the first attempt keeps the mid code route"
    );
    assert_eq!(
        exchanges[1].0,
        common::test_giga_url(),
        "the first-gate retry must use the giga Responses API route"
    );
    assert!(
        !carries_retry_handover(&exchanges[0].1),
        "the first attempt must not carry a retry handover"
    );
    let retry_users = user_messages(&exchanges[1].1);
    assert_eq!(
        retry_users.len(),
        2,
        "the retry carries the unchanged kickoff plus one separate handover"
    );
    assert!(
        retry_users[1].contains("[RETRY HANDOVER]"),
        "the second attempt's separate message is the handover: {}",
        retry_users[1]
    );
    assert!(
        retry_users[1].contains("task_complete, but the first verification gate failed"),
        "the handover names the failed first gate: {}",
        retry_users[1]
    );
    assert!(
        retry_users[1].contains("FIRST-GATE-DIAGNOSTIC"),
        "the handover carries the first gate's own output: {}",
        retry_users[1]
    );

    assert_eq!(
        gate_runs(&gate_log),
        2,
        "the failed first gate and the authoritative final gate"
    );
    let first_trajectory = trajectory(&dir, "code.traj.json");
    assert_eq!(first_trajectory["role"], serde_json::json!("mid"));
    assert_eq!(
        first_trajectory["exit_status"],
        serde_json::json!("complete"),
        "the first attempt declared completion before its gate failed"
    );
    let retry_trajectory = trajectory(&dir, "code-1.traj.json");
    assert_eq!(retry_trajectory["role"], serde_json::json!("giga"));
    assert_eq!(
        retry_trajectory["model"],
        serde_json::json!(common::TEST_GIGA_MODEL)
    );
    assert_eq!(
        retry_trajectory["exit_status"],
        serde_json::json!("turn_end")
    );

    let report = fs::read_to_string(dir.join("queue-report.md"))
        .expect("the queue report artifact is readable");
    assert!(
        report.contains("- Status: completed")
            && report.contains("- Ended: gate accepted without declaration"),
        "the passing final gate controls completion: {report}"
    );

    let urls = transport.urls_called.lock().unwrap().clone();
    assert_eq!(
        urls.len(),
        8,
        "four planning steps, the two code turns, review and summary"
    );
    assert_eq!(urls[4], common::test_url(), "the first code turn is mid");
    assert_eq!(urls[5], common::test_giga_url(), "the retry is giga");
    assert_eq!(urls[6], common::test_giga_url(), "review stays on giga");
    assert_eq!(urls[7], common::TEST_SMOLL_URL, "summary stays on Smoll");

    let _ = fs::remove_dir_all(&state);
}

/// Regression and boundary table: an unauthorized or forbidden API refusal, a
/// terminal transport exhaustion, and a malformed response are all fatal on the
/// first code attempt. None earns a ticket-level handover retry or reaches the
/// first verification gate.
#[test]
fn when_hybrid_first_code_attempt_fails_given_fatal_api_transport_or_malformed_response_then_skips_retry_and_gate()
 {
    struct Case {
        label: &'static str,
        replies: Vec<common::CodeReply>,
        code_requests: usize,
        ended: &'static str,
    }

    let cases = vec![
        Case {
            label: "HTTP 401 unauthorized",
            replies: vec![common::CodeReply::Api {
                status: 401,
                message: "unauthorized".to_string(),
            }],
            code_requests: 1,
            ended: "agent failure: API error",
        },
        Case {
            label: "HTTP 403 forbidden",
            replies: vec![common::CodeReply::Api {
                status: 403,
                message: "forbidden".to_string(),
            }],
            code_requests: 1,
            ended: "agent failure: API error",
        },
        Case {
            label: "terminal transport exhaustion",
            replies: vec![
                common::CodeReply::Transport("connection reset".to_string()),
                common::CodeReply::Transport("connection reset".to_string()),
                common::CodeReply::Transport("connection reset".to_string()),
            ],
            code_requests: 3,
            ended: "agent failure: transport error",
        },
        Case {
            label: "malformed response",
            replies: vec![common::CodeReply::Malformed],
            code_requests: 1,
            ended: "agent failure: malformed response",
        },
    ];

    for case in cases {
        let state = scratch_dir();
        fs::create_dir_all(&state).expect("the fatal case state directory is creatable");
        let gate_log = state.join("gate-runs.log");
        let gate = logging_gate(&gate_log, "exit 0");
        let tickets = tickets_with_gate(&gate);

        let (step, dir, transport, _repository) = run_hybrid(Mode::Hybrid, case.replies, tickets);
        assert!(
            matches!(&step, Step::Retry),
            "{}: a fatal first attempt leaves the queue incomplete: {step:?}",
            case.label
        );

        let exchanges = code_exchanges(&transport);
        assert_eq!(
            exchanges.len(),
            case.code_requests,
            "{}: the fatal first attempt must not earn a ticket-level retry",
            case.label
        );
        for (url, body) in &exchanges {
            assert_eq!(
                url,
                &common::test_url(),
                "{}: every fatal code request stays on mid",
                case.label
            );
            assert!(
                !carries_retry_handover(body),
                "{}: no handover may be sent on a fatal first attempt",
                case.label
            );
        }
        assert_eq!(
            gate_runs(&gate_log),
            0,
            "{}: a fatal first attempt never reaches a verification gate",
            case.label
        );

        let report = fs::read_to_string(dir.join("queue-report.md"))
            .expect("the queue report artifact is readable");
        assert!(
            report.contains("- Status: failed"),
            "{}: the ticket is failed: {report}",
            case.label
        );
        assert!(
            report.contains(&format!("- Ended: {}", case.ended)),
            "{}: the report names the fatal failure: {report}",
            case.label
        );

        let urls = transport.urls_called.lock().unwrap().clone();
        assert_eq!(
            urls.len(),
            5 + case.code_requests,
            "{}: four planning steps, the fatal code requests and the summary",
            case.label
        );
        assert!(
            urls[4..4 + case.code_requests]
                .iter()
                .all(|url| url == &common::test_url()),
            "{}: no giga retry follows the fatal code failure: {urls:?}",
            case.label
        );
        assert_eq!(
            urls[4 + case.code_requests],
            common::TEST_SMOLL_URL,
            "{}: the run skips review and finishes with the Smoll summary: {urls:?}",
            case.label
        );

        let _ = fs::remove_dir_all(&state);
    }
}
