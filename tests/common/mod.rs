#![allow(dead_code)]

//! Shared helpers for the LLM integration binaries: endpoint resolution,
//! failure reporting and the prompt fixtures. Each test binary compiles its own
//! copy, so a helper no binary calls is still compiled -- hence the allowance.

use ejaj::Config;
use ejaj::api::{self, CallError, Message, RawResponse, Transport};
use ejaj::llm::Mode;
use ejaj::plan_document::{Plan, PlanDecision};
use ejaj::specification::NONE;
use ejaj::tickets::{Ticket, TicketSet};
use ejaj::tools::Phase;
use ejaj::triage::{AgentBrief, TriageCategory, TriageReport, TriageState, TriageTicket};

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

pub const DEFAULT_TEST_URL: &str = "http://192.168.56.1:1234/v1/chat/completions";
pub const TEST_API_KEY: &str = "lm-studio";

/// The scripted Smoll endpoint and model. Summary always runs on the dedicated
/// Anthropic Messages provider, so every scripted pipeline configures it
/// explicitly; the in-memory transport serves this endpoint, so no network is
/// touched and the route can be asserted by URL.
pub const TEST_SMOLL_URL: &str = "http://192.168.56.1:1234/v1/messages";
pub const TEST_SMOLL_MODEL: &str = "smoll-test-model";
pub const TEST_SMOLL_API_KEY: &str = "smoll-test-key";

/// The tool_use id the scripted summary tool exchange emits, which the next
/// native request's tool_result must answer.
pub const SUMMARY_TOOL_ID: &str = "toolu_summary_execute_script";

/// The tool_use id the unoffered-tool summary script emits.
pub const SUMMARY_UNOFFERED_TOOL_ID: &str = "toolu_summary_bash";

/// The read-only script the summary tool exchange hands over. It throws if the
/// mutating `write` binding exists, so a successful result proves the pass ran
/// with Summary's read-only capabilities.
pub const SUMMARY_SCRIPT_CODE: &str = "if (typeof tools.write !== \"undefined\") { throw new Error(\"tools.write must not be exposed in the summary phase\"); } return \"summary-read-only-ok\";";

/// What that read-only script returns: the content the next native request's
/// matching tool_result must carry.
pub const SUMMARY_SCRIPT_OUTPUT: &str = "summary-read-only-ok";

/// `tests/llm.env`, parsed once. The process environment takes precedence over it, mirroring
/// `main.rs`; the file is the shared default so no machine-specific model id lives in code.
fn test_env_file() -> &'static HashMap<String, String> {
    static FILE: OnceLock<HashMap<String, String>> = OnceLock::new();
    FILE.get_or_init(|| {
        dotenvy::from_filename_iter("tests/llm.env")
            .expect("tests/llm.env is missing")
            .filter_map(Result::ok)
            .collect()
    })
}

/// The trimmed, non-empty value of `name` from the process environment, or `None` when unset or
/// blank. Never reads `.env` or any `LLM_*` variable.
fn explicit_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// The process environment wins over `tests/llm.env`.
fn resolve(name: &str) -> Option<String> {
    explicit_env(name).or_else(|| test_env_file().get(name).cloned())
}

/// `EJAJ_TEST_LLM_URL` (environment or `tests/llm.env`), falling back to the LM Studio default
/// endpoint. The value is used verbatim: it must be the full chat-completions URL, with no `/v1`
/// completion and no trailing-slash trimming.
pub fn test_url() -> String {
    resolve("EJAJ_TEST_LLM_URL").unwrap_or_else(|| DEFAULT_TEST_URL.to_string())
}

/// `EJAJ_TEST_LLM_MODEL` (environment or `tests/llm.env`). No compiled fallback: a missing model
/// is a loud failure, not a silent fallback to whichever id the endpoint happens to report first.
pub fn test_model() -> String {
    resolve("EJAJ_TEST_LLM_MODEL").unwrap_or_else(|| {
        panic!(
            "EJAJ_TEST_LLM_MODEL is set in neither the environment nor tests/llm.env.\n\
             Add it to tests/llm.env or pass EJAJ_TEST_LLM_MODEL=<id> in the environment."
        )
    })
}

/// `EJAJ_TEST_LLM_API_KEY` (environment or `tests/llm.env`). A hosted provider
/// needs a real key; LM Studio ignores the `lm-studio` stub, so that is the
/// fallback and the tests stay keyless by default.
pub fn test_api_key() -> String {
    resolve("EJAJ_TEST_LLM_API_KEY").unwrap_or_else(|| TEST_API_KEY.to_string())
}

/// The local mid model a committed fixture must name: the value in
/// `tests/llm.env`, never the process environment, so an override cannot hide a
/// hosted recording from the replay guard.
fn fixture_model() -> String {
    test_env_file()
        .get("EJAJ_TEST_LLM_MODEL")
        .cloned()
        .unwrap_or_else(|| panic!("EJAJ_TEST_LLM_MODEL is not set in tests/llm.env"))
}

/// The local mid endpoint a committed fixture must name: `tests/llm.env`
/// when it carries one, the compiled default otherwise.
fn fixture_url() -> String {
    test_env_file()
        .get("EJAJ_TEST_LLM_URL")
        .cloned()
        .unwrap_or_else(|| DEFAULT_TEST_URL.to_string())
}

/// The local giga endpoint a two-provider fixture must name.
fn fixture_giga_url() -> String {
    test_env_file()
        .get("EJAJ_TEST_ARCHITECT_URL")
        .cloned()
        .unwrap_or_else(|| panic!("EJAJ_TEST_ARCHITECT_URL is not set in tests/llm.env"))
}

/// The local giga model a two-provider fixture must name.
fn fixture_giga_model() -> String {
    test_env_file()
        .get("EJAJ_TEST_ARCHITECT_MODEL")
        .cloned()
        .unwrap_or_else(|| panic!("EJAJ_TEST_ARCHITECT_MODEL is not set in tests/llm.env"))
}

/// The `tests/llm.env` value for `name`, or the compiled mid-URL default
/// that the file need not carry.
fn local_default(name: &str) -> Option<String> {
    if let Some(value) = test_env_file().get(name) {
        return Some(value.clone());
    }
    (name == "EJAJ_TEST_LLM_URL").then(|| DEFAULT_TEST_URL.to_string())
}

/// Refuse to record against anything but the local default. The process
/// environment may override `tests/llm.env` for a live check against another
/// provider (see AGENTS.md); under `EJAJ_LLM_RECORD=1` that override would
/// silently record a hosted model into the committed fixtures, so it is
/// rejected before the first request.
fn assert_records_the_local_default(names: &[&str]) {
    for name in names {
        let Some(value) = explicit_env(name) else {
            continue;
        };
        if local_default(name).as_deref() == Some(value.as_str()) {
            continue;
        }
        panic!(
            "EJAJ_LLM_RECORD=1 refuses to record against {name}={value}: the committed fixtures \
             are recorded against the local default in tests/llm.env. Unset {name}, or start the \
             local endpoint."
        );
    }
}

/// The `/v1/models` listing URL derived from the chat-completions URL.
fn models_url() -> String {
    let url = test_url();
    match url.strip_suffix("/chat/completions") {
        Some(base) => format!("{base}/models"),
        None => format!("{}/models", url.trim_end_matches('/')),
    }
}

/// Fail before the first request unless the resolved model is one the endpoint advertises. Cached
/// so the listing is fetched once per test binary.
fn assert_model_available() {
    static CHECKED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    CHECKED.get_or_init(|| {
        let model = test_model();
        let key = test_api_key();
        let mut request = minreq::get(models_url());
        if !key.is_empty() {
            request = request.with_header("Authorization", format!("Bearer {key}"));
        }
        let res = match request.send() {
            Ok(res) => res,
            Err(e) => failed("the model check", &format!("could not list models: {e}")),
        };
        let body: serde_json::Value = match serde_json::from_str(res.as_str().unwrap_or("")) {
            Ok(body) => body,
            Err(e) => failed("the model check", &format!("invalid /models JSON: {e}")),
        };
        let ids: Vec<&str> = body["data"]
            .as_array()
            .map(|models| models.iter().filter_map(|m| m["id"].as_str()).collect())
            .unwrap_or_default();
        if !ids.contains(&model.as_str()) {
            failed(
                "the model check",
                &format!("`{model}` is not served; available: {}", ids.join(", ")),
            );
        }
    });
}

/// The endpoint every LLM test talks to, gated on the configured model being available. Auth uses
/// `EJAJ_TEST_LLM_API_KEY` when set, and the stub key `lm-studio` otherwise (LM Studio ignores it).
pub fn local_config() -> Config {
    assert_model_available();
    Config::new(test_url(), test_model(), Some(test_api_key()))
}

/// The stand-in for `main`'s scope notice: the package root is the working directory, and no human
/// is available, so the model must end its turn by calling the control tool of its phase instead of
/// asking a question.
pub fn scope() -> &'static str {
    "\n\nThe project root is the current working directory (the `ejaj` package root). Work only \
     inside it: never read, list or search outside this directory (`/`, `~`, `/etc`, sibling \
     projects), and never inspect `target/` or `.git/`. The working directory is already set to \
     the project root, so always use paths relative to it. No human is available in this session: \
     never ask a question, and end your turn only by calling the control tool your phase offers, \
     with the payload it asks for.\n"
}

/// The provider seam the loop uses, with every failure turned into a test
/// failure naming the endpoint: the provider for `phase` prepares the request,
/// the configured transport sends it, and the provider decodes the reply.
pub fn ask(config: &Config, messages: &[Message], phase: Phase) -> Message {
    let provider = config.provider(phase);
    let prepared = provider.prepare(messages, phase);
    let raw = match config.send(&prepared.outgoing) {
        Ok(raw) => raw,
        Err(error) => failed("the request", &error.to_string()),
    };
    match provider.decode(raw) {
        Ok(message) => message,
        Err(error) => failed("the response decode", &error.to_string()),
    }
}

/// The one failure message every LLM test uses: what failed, against which endpoint and model, and
/// the exact command to run again.
pub fn failed(what: &str, detail: &str) -> ! {
    let model = resolve("EJAJ_TEST_LLM_MODEL").unwrap_or_else(|| "<unset>".to_string());
    panic!(
        "[EJAJ INTEGRATION] {what} failed against {} (model `{model}`): {detail}\n\
         Is LM Studio running with a model loaded? The LLM tests are ignored by default; run:\n\
         cargo test -- --ignored --test-threads=1\n\
         Configure the model in tests/llm.env (EJAJ_TEST_LLM_MODEL); the test fails if it is not served.",
        test_url(),
    )
}

/// Run the test named `filter` for real in a child copy of this test binary --
/// with `env` set on it -- and return everything that copy wrote, in the order
/// it wrote it: the production transcript, captured at the process boundary.
///
/// The child is this same test binary running exactly that one test, so a test
/// that must assert what its own run printed reads the real bytes without a
/// mock, a capture library or a private seam. Both streams are redirected onto
/// one file, so the interleaving a terminal would have shown survives the
/// capture. A child that fails is a loud failure carrying the transcript, and
/// `--test-threads=1` keeps the child's own tests from interleaving.
pub fn captured_transcript(filter: &str, env: &[(&str, &str)]) -> String {
    let exe = std::env::current_exe().expect("the test binary path is known");
    let path = std::env::temp_dir().join(format!(
        "ejaj-transcript-{}-{filter}.txt",
        std::process::id()
    ));
    let sink = fs::File::create(&path).expect("the transcript file is created");

    let mut command = Command::new(exe);
    command
        .args(["--exact", filter, "--nocapture", "--test-threads=1"])
        .stdout(std::process::Stdio::from(
            sink.try_clone().expect("the transcript file is clonable"),
        ))
        .stderr(std::process::Stdio::from(sink));
    for (name, value) in env {
        command.env(name, value);
    }

    let output = command
        .output()
        .expect("the child test process starts and is waited for");
    let transcript = fs::read_to_string(&path).unwrap_or_default();
    let _ = fs::remove_file(&path);

    assert!(
        output.status.success(),
        "the child run of {filter} failed ({}):\n{transcript}",
        output.status
    );
    transcript
}

/// Run a live recording body. With `EJAJ_LLM_RECORD=1` a soft assertion
/// failure -- a broken artifact, a paraphrase -- is reported as a warning
/// instead of aborting the run, so the exchanges are still committed. A hard
/// failure, the `failed` panic that means the run reached no usable terminal
/// step, still unwinds and discards the staged recording. Without the record
/// flag the body runs as it always did.
pub fn record_or_verify(harness: &Harness, body: impl FnOnce()) {
    if !harness.is_recording() {
        body();
        return;
    }
    if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
        if is_hard_failure(&payload) {
            std::panic::resume_unwind(payload);
        }
        eprintln!(
            "[WARN] the recording assertions failed (the run is still recorded): {}",
            panic_text(&payload)
        );
    }
}

/// Whether a panic payload came from `failed`, the one hard failure. A
/// `failed` panic means the run did not reach a terminal step the replay can
/// consume, so the staged recording must not be committed.
fn is_hard_failure(payload: &(dyn std::any::Any + Send)) -> bool {
    panic_text(payload).starts_with("[EJAJ INTEGRATION]")
}

/// The text inside a panic payload, or the empty string when it carries none.
fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        return text.to_string();
    }
    if let Some(text) = payload.downcast_ref::<String>() {
        return text.clone();
    }
    String::new()
}

/// The one plan the plan-agent and spec-agent tests are built around: small enough that the model
/// copies it in a single call, complete enough to satisfy the document template.
pub fn plan_fixture() -> Plan {
    Plan {
        goal: "Add a --version flag".to_string(),
        decisions: vec![PlanDecision {
            title: "Add the flag".to_string(),
            detail: "The binary prints its crate version and exits.".to_string(),
        }],
        defaults: vec!["No new dependency for the flag.".to_string()],
        constraints: vec!["No behaviour change when the flag is absent.".to_string()],
        open_questions: Vec::new(),
    }
}

/// What the plan agent is asked for: the fixture rendered by `Plan::to_markdown`, so the request
/// always carries exactly the document the parser enforces, followed by the instruction to hand it
/// over immediately and run nothing.
pub fn plan_request() -> String {
    format!(
        "The design is settled. Here is the complete agreed plan as a markdown document:\n\n{}\n\
         Hand it over now: call `finish_plan` with exactly that document in the `plan` argument. Do \
         not ask any question, do not run any command, and do not change the text or the headings.",
        plan_fixture().to_markdown()
    )
}

/// The plan the end-to-end pipeline test is built around: one harmless command that reports its
/// own output and touches no file, so a run that follows it leaves the repository unchanged.
pub fn harmless_plan_fixture() -> Plan {
    Plan {
        goal: "Report one command's output without changing the repository".to_string(),
        decisions: vec![PlanDecision {
            title: "Run the command once".to_string(),
            detail: "The only step is to run `bash` with exactly `echo ejaj-e2e-ok` and report \
                     the output it prints. This is a single verification run, not a component \
                     to build, so there is no module and no code to add: the change is the \
                     command run itself."
                .to_string(),
        }],
        defaults: vec!["Report the command's output verbatim.".to_string()],
        constraints: vec![
            "No file is created, edited, moved or deleted.".to_string(),
            "No command other than `echo ejaj-e2e-ok` is run.".to_string(),
        ],
        open_questions: Vec::new(),
    }
}

/// What the plan agent is asked for in the pipeline test: the harmless fixture rendered by
/// `Plan::to_markdown`, so the request always carries exactly the document the parser enforces,
/// followed by the instruction to hand it over immediately and run nothing.
pub fn harmless_plan_request() -> String {
    format!(
        "The design is settled. Here is the complete agreed plan as a markdown document:\n\n{}\n\
         Hand it over now: call `finish_plan` with exactly that document in the `plan` argument. Do \
         not ask any question, do not run any command, and do not change the text or the headings.",
        harmless_plan_fixture().to_markdown()
    )
}

/// The ticket text the per-ticket code agent is handed in the code-loop boundary
/// test: run one harmless command with `bash`, wait for its result, then call
/// `task_complete`, and touch no file. The command is put in its own message on
/// purpose -- that is what forces the second request (the tool result replayed
/// back) that this boundary exists to test. This is the ticket text, not a
/// specification: the queue hands the code agent one rendered ticket at a time.
pub const CODE_TICKET: &str = "\
Call the bash tool once with exactly `echo ejaj-integration-ok`, and put no other tool call in \
that message. The command's result will be returned to you; call `task_complete` only once you \
have seen it. Do not create, edit or delete any file, and do not run any other command.";

/// Scratch path inside `.ejaj/` (gitignored) used by the `execute_script`
/// wire-boundary test.
pub const SCRIPT_SCRATCH_PATH: &str = ".ejaj/script-wire-check.tmp";

/// The ticket text the per-ticket code agent is handed in the `execute_script`
/// boundary test: run one JavaScript snippet that writes and reads a scratch
/// file, wait for its result, then call `task_complete`.
pub const SCRIPT_TICKET: &str = "\
Call the `execute_script` tool once with exactly this `code` argument: \
`tools.write(\".ejaj/script-wire-check.tmp\", \"ejaj-script-ok\"); return tools.read(\".ejaj/script-wire-check.tmp\");` \
and put no other tool call in that message. The script's result will be returned to you; call \
`task_complete` immediately once you have seen it. Do not run `bash` or any other command, and \
do not touch any other file.";

/// The specification markdown the scripted pipeline returns for the harmless plan.
pub fn harmless_spec_markdown() -> &'static str {
    "# Report one command's output without changing the repository\n\n\
## 1. Problem Statement\n\
The task requires verifying the output of a single shell command `echo ejaj-e2e-ok` by ensuring it prints exactly \"ejaj-e2e-ok\" without any modifications. This is a minimal verification step that doesn't involve code changes but validates the behavior of the command in isolation.\n\n\
## 2. Solution Overview\n\
The solution involves executing the command `echo ejaj-e2e-ok` and verifying its output matches the expected string exactly. The implementation requires no code changes, only validation of an existing command's behavior.\n\n\
## 3. User Stories & Acceptance Criteria\n\n\
1. **Verify Command Output**\n\
   As a user, I want to ensure that running `echo ejaj-e2e-ok` produces exactly \"ejaj-e2e-ok\" with no additional output.\n\n\
- [Pass] The command outputs exactly \"ejaj-e2e-ok\" when run\n\
- [Fail] The command fails if the output differs from \"ejaj-e2b-ok\"\n\n\
## 4. Implementation Decisions\n\n\
### Module Architecture\n\
None - No modules or components are modified. The verification is a standalone check.\n\n\
### Data Models & Contracts\n\
None - No data models or schemas are involved in this minimal verification step.\n\n\
### ADR Compliance\n\
The design aligns with existing conventions of using shell commands for verification without introducing new patterns.\n\n\
## 5. Testing Decisions & Seams\n\n\
### Test Seams\n\
- Command execution is the primary seam\n\
- Output validation is the secondary seam\n\n\
### Coverage Strategy\n\
- Verifies command behavior at the system call level\n\
- Confirms output formatting requirements\n\n\
### Prior Art\n\
- Similar verification steps exist in CI/CD pipelines for command-line tools\n\
- Common practice in e2e testing to validate CLI outputs\n\n\
## 6. Out of Scope\n\
- No code changes required\n\
- No additional features to implement\n\
- No error handling for missing commands\n\n\
## 7. Deferred Items & Operational Risks\n\n\
### Operational Risks\n\
- The verification is fragile if the command changes\n\
- Requires careful maintenance if output format evolves\n\n\
### Follow-Up Work\n\
- Potential to generalize this pattern for other CLI tools\n\
- Could be automated in CI/CD pipelines for validation\n\
- No monitoring requirements as it's a single verification step\n"
}

/// The ticket set JSON the scripted pipeline returns for the harmless plan.
pub fn harmless_tickets_json() -> &'static str {
    r#"{"tickets": {"tickets": [
        {
            "id": "1-verify-command-output",
            "title": "Verify command outputs exactly 'ejaj-e2e-ok'",
            "objective": "Ensure that running `echo ejaj-e2e-ok` produces exactly 'ejaj-e2e-ok' with no additional output.",
            "acceptance_criteria": [
                "[Pass] The command outputs exactly \"ejaj-e2e-ok\" when run",
                "[Fail] The command fails if the output differs from \"ejaj-e2b-ok\""
            ],
            "prerequisites_assumed": "The command `echo ejaj-e2e-ok` exists and is available in the environment.",
            "out_of_scope": [
                "No code changes required",
                "No additional features to implement",
                "No error handling for missing commands"
            ],
            "scope_of_changes": [
                "Adds verification that `echo ejaj-e2e-ok` outputs exactly 'ejaj-e2e-ok'."
            ],
            "test_expectations": [
                "when_running_echo_command_given_expected_argument_then_outputs_ejaj_e2e_ok confirms `echo ejaj-e2e-ok` outputs 'ejaj-e2e-ok'."
            ],
            "verification_gate": "if [ \"$(echo 'ejaj-e2e-ok')\" != 'ejaj-e2e-ok' ]; then exit 1; fi",
            "blocked_by": [],
            "blocks": []
        }
    ]}}"#
}

/// The three-ticket fixture the multi-ticket triage tests exercise: the largest
/// scripted pipeline fixture, a dependency chain, so a dropped or truncated
/// classification is the pipeline's and never the fixture's. The specification
/// story's own criteria live in the first ticket, exactly as the one-ticket
/// fixture carries them.
pub fn multi_tickets_json() -> &'static str {
    r#"{"tickets": {"tickets": [
        {
            "id": "1-verify-command-output",
            "title": "Verify command outputs exactly 'ejaj-e2e-ok'",
            "objective": "Ensure that running `echo ejaj-e2e-ok` produces exactly 'ejaj-e2e-ok' with no additional output.",
            "acceptance_criteria": [
                "[Pass] The command outputs exactly \"ejaj-e2e-ok\" when run",
                "[Fail] The command fails if the output differs from \"ejaj-e2b-ok\""
            ],
            "prerequisites_assumed": "The command `echo ejaj-e2e-ok` exists and is available in the environment.",
            "out_of_scope": ["No code changes required"],
            "scope_of_changes": [
                "Adds verification that `echo ejaj-e2e-ok` outputs exactly 'ejaj-e2e-ok'."
            ],
            "test_expectations": [
                "when_running_echo_command_given_expected_argument_then_outputs_ejaj_e2e_ok confirms `echo ejaj-e2e-ok` outputs 'ejaj-e2e-ok'."
            ],
            "verification_gate": "if [ \"$(echo 'ejaj-e2e-ok')\" != 'ejaj-e2e-ok' ]; then exit 1; fi",
            "blocked_by": [],
            "blocks": ["2-verify-command-failure"]
        },
        {
            "id": "2-verify-command-failure",
            "title": "Verify the command failure boundary",
            "objective": "Confirm the verification gate fails loudly when the command output is wrong.",
            "acceptance_criteria": ["[Pass] A mismatched output fails the gate"],
            "prerequisites_assumed": "The first verification ticket completed.",
            "out_of_scope": ["No repository file edits"],
            "scope_of_changes": ["Adds the failure-boundary verification."],
            "test_expectations": [
                "when_checking_command_given_wrong_output_then_fails_the_gate confirms the gate rejects a wrong output."
            ],
            "verification_gate": "if [ \"$(echo 'ejaj-e2e-ok')\" != 'ejaj-e2e-ok' ]; then exit 1; fi",
            "blocked_by": ["1-verify-command-output"],
            "blocks": ["3-verify-command-empty"]
        },
        {
            "id": "3-verify-command-empty",
            "title": "Verify the command empty boundary",
            "objective": "Confirm the verification gate reports an empty command output as a failure.",
            "acceptance_criteria": ["[Pass] An empty output fails the gate"],
            "prerequisites_assumed": "The failure-boundary ticket completed.",
            "out_of_scope": ["No repository file edits"],
            "scope_of_changes": ["Adds the empty-boundary verification."],
            "test_expectations": [
                "when_checking_command_given_empty_output_then_fails_the_gate confirms the gate rejects an empty output."
            ],
            "verification_gate": "if [ \"$(echo 'ejaj-e2e-ok')\" != 'ejaj-e2e-ok' ]; then exit 1; fi",
            "blocked_by": ["2-verify-command-failure"],
            "blocks": []
        }
    ]}}"#
}

/// Decode the `finish_tickets` payload a fixture carries into its typed set.
fn parse_ticket_set(json: &str) -> TicketSet {
    let value: serde_json::Value = serde_json::from_str(json).expect("the ticket fixture is JSON");
    serde_json::from_value(value["tickets"].clone())
        .expect("the ticket fixture carries a ticket set")
}

/// The one-ticket set the scripted tickets pass hands over, as a typed set.
fn harmless_ticket_set() -> TicketSet {
    parse_ticket_set(harmless_tickets_json())
}

/// The canonical change summary the scripted summary pass answers with: one
/// header line, one blank line and one body line, already in the canonical form
/// the artifact must hold -- so the artifact bytes a test reads back are exactly
/// these, terminal LF included. It carries no footer and no generated ticket
/// id, which is what a summary the format accepts looks like.
pub const SCRIPTED_SUMMARY: &str = "test(pipeline): script the change summary\n\nThe scripted summary pass answers with one fixed canonical summary.\n";

/// The branch a default-branch run of the canonical scripted summary publishes:
/// the fixed prefix plus the slug derived from the summary's header.
pub const SUMMARY_BRANCH: &str = "ejaj/test-pipeline-script-the-change-summary";

/// A phrase only the summary prompt carries. The scripted transport reads it
/// out of the request body to tell the summary pass's own requests from the
/// queue's, so the script stays right however many requests the queue made --
/// a resolved queue makes five, a stopped one stops before the code agent, a
/// summary that keeps calling tools makes as many as its step limit allows, and
/// an answer the validator rejects buys one further correction request.
pub const SUMMARY_MARKER: &str = "Write the change summary for the run that just finished";

/// A phrase only the review prompt carries. The scripted transport reads it
/// out of the request body to answer each review round on the review phase's
/// own terms.
const REVIEW_MARKER: &str = "Audit the completed ticket queue against the technical specification and test design blueprint";

/// The verification gate the scripted ticket carries. The gate is the one thing
/// the queue's outcome hangs on, so a test picks the queue outcome from here.
#[derive(Clone, Copy)]
pub enum GateScript {
    /// The gate the harmless fixture carries: it exits 0, so the queue resolves.
    Passing,
    /// A gate that always fails, so the ticket fails and the queue drains
    /// incomplete.
    Failing,
    /// A gate that fails after a measurable pause, so a test can prove the
    /// reported duration is the real command interval rather than a constant.
    SlowFailing,
}

/// One scripted reply for the queue's code-agent turns.
///
/// The queue calls the code agent in fresh attempts; these replies are served
/// in order, so a test can script the first attempt, the handover-driven retry,
/// and any tool-call turns between them without a model or a network.
#[derive(Clone, Debug)]
pub enum CodeReply {
    /// A tool-free assistant turn. The optional text is its narration.
    TurnEnd(Option<String>),
    /// An assistant turn that calls `task_complete`, optionally with narration.
    Complete(Option<String>),
    /// An assistant turn that calls the real `bash` tool, optionally with
    /// narration. The command runs through the production tool path.
    ToolCall {
        narration: Option<String>,
        command: String,
    },
    /// An assistant turn that calls an arbitrary tool with raw JSON arguments.
    /// The scripted provider payload is decoded exactly like a real one, so
    /// oversized or unknown calls exercise the production tool boundary.
    RawToolCall {
        narration: Option<String>,
        name: String,
        arguments: String,
    },
    /// The provider refuses the request with the given HTTP status.
    Api { status: i32, message: String },
    /// The transport itself fails.
    Transport(String),
    /// The response body is malformed for the active wire format.
    Malformed,
}

/// How the scripted transport answers the queue's code-agent turns.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CodeScript {
    /// The normal scripted bash step followed by `task_complete`.
    Scripted,
    /// The first code-agent request is refused at the API, so the ticket fails
    /// before its verification gate is reached.
    ApiError,
    /// The first code-agent request of each queue round fails once in the
    /// transport. The loop retries it after its real backoff, which is inside
    /// the queue invocation but outside the measured model attempt, so the
    /// round's residual carries it.
    TransientOncePerRound,
}

/// The marker every scripted `ready-for-agent` classification's Agent Brief
/// carries. It is unique to the brief, so a test can prove the brief never
/// reaches the code agent's payload.
pub const BRIEF_MARKER: &str = "TRIAGE-ONLY-BRIEF";

/// A phrase only the triage kickoff carries. The scripted transport reads it
/// out of the request body to answer the one triage pass on the triage phase's
/// own terms, whatever the queue did before or after it.
pub const TRIAGE_MARKER: &str = "The tickets agent has handed over the draft ticket set";

/// A phrase only the code-agent kickoff carries. A test reads it out of the
/// decoded user message to tell the queue's code turns from every other pass.
pub const CODE_MARKER: &str = "this session's entire task";
/// The plan system prompt's stable marker.
pub const PLAN_MARKER: &str = "Interview the user to align on core architectural";

/// How the scripted transport answers the one triage pass between the ticket
/// writes and the queue.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TriageScript {
    /// Every draft ticket is classified `ready-for-agent`, so the queue runs.
    AllReady,
    /// Every classification carries the given non-ready state and a blocker
    /// explanation, so the whole set is gated.
    Blocked(TriageState),
    /// One classification in the set -- the second ticket when there is more
    /// than one -- carries the given non-ready state; every other ticket is
    /// ready.
    OneBlocked(TriageState),
    /// The pass ends its turn without calling `finish_triage`.
    TurnEnd,
    /// The triage request is refused with an HTTP 400; no retry can help.
    ApiError,
    /// The first triage turn hands over a report the boundary rejects, the next
    /// hands over the valid report: the rejected call is a correction, not a
    /// failure.
    RejectedOnceThenReady,
    /// Every triage turn hands over a report the boundary rejects, so the pass
    /// can only end on its step limit.
    AlwaysRejected,
}

/// One ready classification for `ticket`: it retains the ticket's id, title and
/// dependency list, records an audit note, and carries an Agent Brief whose
/// target behavior is marked so a test can prove the brief never reaches the
/// code agent.
fn ready_classification(ticket: &Ticket) -> TriageTicket {
    TriageTicket {
        ticket_id: ticket.id.trim().to_string(),
        ticket_title: ticket.title.trim().to_string(),
        category: TriageCategory::Enhancement,
        state: TriageState::ReadyForAgent,
        notes: vec![format!(
            "Audited `{}` against the repository.",
            ticket.id.trim()
        )],
        blocker_explanation: None,
        agent_brief: Some(AgentBrief {
            target_behavior: format!("{BRIEF_MARKER}: implement {}.", ticket.title.trim()),
            acceptance_criteria: if ticket.acceptance_criteria.is_empty() {
                vec!["The ticket's verification gate exits 0.".to_string()]
            } else {
                ticket.acceptance_criteria.clone()
            },
            out_of_scope: if ticket.out_of_scope.is_empty() {
                vec![NONE.to_string()]
            } else {
                ticket.out_of_scope.clone()
            },
            blocked_by: ticket.blocked_by.clone(),
        }),
    }
}

/// One non-ready classification for `ticket`, carrying the blocker explanation
/// the report must show and no Agent Brief.
fn blocked_classification(ticket: &Ticket, state: TriageState) -> TriageTicket {
    TriageTicket {
        ticket_id: ticket.id.trim().to_string(),
        ticket_title: ticket.title.trim().to_string(),
        category: TriageCategory::Bug,
        state,
        notes: vec![format!(
            "Audited `{}` against the repository.",
            ticket.id.trim()
        )],
        blocker_explanation: Some(format!(
            "The draft ticket is not ready for an agent ({}) and needs a human decision.",
            state.wire()
        )),
        agent_brief: None,
    }
}

/// The index of the one ticket [`TriageScript::OneBlocked`] classifies as
/// non-ready: the second ticket when the set holds more than one, so the
/// fixture still carries a ready classification on either side of it.
fn one_blocked_index(set: &TicketSet) -> usize {
    if set.tickets.len() > 1 { 1 } else { 0 }
}

/// How the scripted transport answers the summary phase. The queue script is
/// fixed; only the summary varies, so a test can drive every way a summary can
/// fail without a live endpoint.
#[derive(Clone)]
pub enum SummaryScript {
    /// The canonical change summary the pass is expected to produce.
    Summary,
    /// End the pass with this exact text as its final message.
    Answer(&'static str),
    /// End the pass with no content at all: no final answer exists.
    NoContent,
    /// End the pass with a whitespace-only final message.
    Blank,
    /// Answer with an HTTP 400: the endpoint refuses the request, and no retry
    /// can help.
    ApiError,
    /// Fail in the transport, before any response exists.
    TransportError,
    /// Answer every request with a tool call that runs and changes nothing,
    /// forever: the pass can only end on its step limit.
    EndlessToolCalls,
    /// One native Anthropic tool-use round trip for `execute_script`: the
    /// first request is answered with a read-only tool_use whose script proves
    /// the write binding is absent, and the next request -- which must replay
    /// its matching tool_result -- is answered with the canonical summary.
    ToolExchange,
    /// One native Anthropic tool_use for `bash`, which the summary phase does
    /// not offer; the next request must replay the rejection as a tool_result
    /// before the canonical summary is accepted.
    UnofferedToolCall,
    /// Answer the summary requests with these replies in order: the initial
    /// generation first, then each correction the pass may send. The last
    /// reply repeats, so a correction that is never accepted keeps being
    /// answered instead of running the script out.
    Turns(Vec<SummaryReply>),
}

/// What one summary turn is answered with. A correction turn is answered on the
/// same external boundary as the initial one, so a test can script an invalid
/// answer, a valid correction, an absent correction, an API refusal, a
/// transport failure and a provider access denial without a live endpoint.
#[derive(Clone)]
pub enum SummaryReply {
    /// End the turn with this text as its final message.
    Answer(String),
    /// End the turn with no content at all: no final answer exists.
    NoContent,
    /// Take one no-op inspection step and ask again.
    ToolCall,
    /// Refuse the request with an HTTP 400: no retry can help.
    ApiError,
    /// Refuse the request with an HTTP 403: the provider forbids access, and
    /// no retry or correction can help.
    Forbidden,
    /// Fail in the transport, before any response exists.
    TransportError,
}

/// How the scripted transport answers the review phase after a resolved queue.
#[derive(Clone, Copy)]
pub enum ReviewScript {
    /// Hand over a clean `finish_review` with no findings and no fix tickets.
    Pass,
    /// Emit one fix ticket on round 1, then pass cleanly on round 2.
    FixOnceThenPass,
    /// Emit a distinct fix ticket on review rounds 1, 2 and 3, then pass cleanly
    /// on round 4: the three permitted review-generated fix queues, each
    /// followed by its review, and the fourth verification review.
    FixThreeRoundsThenPass,
    /// Emit a fix ticket on every review round, so a pass never arrives and the
    /// review bound is the only way the loop can end.
    AlwaysFixes,
    /// Emit one fix ticket whose verification gate fails on round 1, then pass
    /// cleanly on round 2: the fix queue itself drains incomplete, so the
    /// review loop must end without another review or fix set.
    FixOnceWithFailingGate,
    /// Hand over one payload the boundary refuses on round 1 -- it comes back
    /// as a tool result -- then pass cleanly on round 2, so a corrected valid
    /// handoff can still proceed.
    InvalidOnceThenPass(InvalidReview),
    /// Hand over the invalid payload on every turn, so the pass can only end on
    /// its step limit and no handoff ever exists.
    AlwaysInvalid(InvalidReview),
    /// End the review turn without calling `finish_review`.
    TurnEnd,
    /// Refuse the review request with an HTTP 400 API error.
    ApiError,
}

/// Which invalid `finish_review` payload the scripted review hands over. Every
/// case breaks a different rule the production boundary already enforces --
/// `ReviewReport::validate` or the cross-round id check -- so a rejection can
/// never be the fixture's own mistake.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InvalidReview {
    /// Fix tickets with no findings: the report contract refuses them.
    FixesWithoutFindings,
    /// A fix ticket whose only acceptance criterion is blank.
    BlankAcceptanceCriteria,
    /// Two findings sharing one id: finding ids must be unique.
    DuplicateFindingIds,
    /// A fix ticket reusing a ticket id already executed in this run.
    CollidingTicketId,
    /// A fix ticket blocked by an id that is not in its own fix set.
    InvalidDependency,
}

/// JSON arguments for a passing `finish_review` call.
pub fn passing_review_json() -> String {
    serde_json::json!({
        "review": {
            "scorecard": {
                "keeper_coverage": "Core entry-point behavior verified.",
                "sediment_and_coupling": "Zero internal mocks; no private state introspection.",
                "inferred_assumptions": "Echo command exits 0 on standard shells.",
                "boundary_matrix": "All 5 boundary dimensions evaluated for command output.",
                "code_and_scope": "Ticket scope and verification gate honoured."
            },
            "findings": [],
            "fixes": []
        }
    })
    .to_string()
}

/// The verification gate a fix ticket carries while the fix queue is expected
/// to resolve: it exits 0, so the queue's own gate accepts the ticket.
const FIX_TICKET_GATE: &str = "if [ \"$(echo 'ejaj-e2e-ok')\" != 'ejaj-e2e-ok' ]; then exit 1; fi";

/// The verification gate a fix ticket carries when the run needs the fix queue
/// itself to drain incomplete: it always fails, so the ticket fails its gate
/// and the queue ends incomplete.
const FAILING_FIX_TICKET_GATE: &str = "exit 1";

/// JSON arguments for a `finish_review` call that emits a fix ticket for `round`.
pub fn fixing_review_json(round: usize) -> String {
    fixing_review_json_with_gate(round, FIX_TICKET_GATE)
}

/// JSON arguments for a `finish_review` call that emits a fix ticket for
/// `round` carrying `gate` as its verification gate, so a test can decide the
/// fix queue's own outcome independently of the original queue's.
fn fixing_review_json_with_gate(round: usize, gate: &str) -> String {
    let finding_id = format!("f{round}-edge-case");
    let ticket_id = format!("r{round}-1-verify-edge-case");
    serde_json::json!({
        "review": {
            "scorecard": {
                "keeper_coverage": "Core entry-point behavior verified; edge case missing.",
                "sediment_and_coupling": "Zero internal mocks; no private state introspection.",
                "inferred_assumptions": "Echo command exits 0 on standard shells.",
                "boundary_matrix": "Empty boundary check needed for command output.",
                "code_and_scope": "Follow-up fix ticket required for boundary coverage."
            },
            "findings": [{
                "id": finding_id,
                "category": "boundary-matrix",
                "target": "1-verify-command-output",
                "detail": "Verify command output handles the empty boundary case."
            }],
            "fixes": [{
                "id": ticket_id,
                "title": "Verify command output handles empty boundary case",
                "objective": "Confirm that the command output check covers the empty boundary case.",
                "acceptance_criteria": [
                    "[Pass] The command outputs exactly \"ejaj-e2e-ok\" when run"
                ],
                "prerequisites_assumed": "The initial verification ticket completed.",
                "out_of_scope": ["No repository file edits"],
                "scope_of_changes": [
                    "Verify the empty boundary case for command output."
                ],
                "test_expectations": [
                    "when_checking_output_given_empty_boundary_then_confirms_ejaj_e2e_ok"
                ],
                "verification_gate": gate,
                "blocked_by": [],
                "blocks": []
            }]
        }
    })
    .to_string()
}

/// JSON arguments for a `finish_review` call the production boundary must
/// refuse, for the reason the given case breaks: the report contract, the
/// finding-id rule, the cross-round id check or the fix-set dependency rule.
fn invalid_review_json(case: InvalidReview) -> String {
    let mut value: serde_json::Value = serde_json::from_str(&fixing_review_json(1))
        .expect("the scripted review arguments are JSON");
    let review = value["review"]
        .as_object_mut()
        .expect("the scripted review arguments carry the review object");

    match case {
        // A pass with findings, or fixes with none: the report contract refuses
        // both shapes.
        InvalidReview::FixesWithoutFindings => {
            review["findings"] = serde_json::json!([]);
        }
        // The only criterion is blank, so the fix ticket has no pass/fail
        // criterion at all.
        InvalidReview::BlankAcceptanceCriteria => {
            review["fixes"][0]["acceptance_criteria"] = serde_json::json!(["   "]);
        }
        // The same finding id twice: finding ids must be unique.
        InvalidReview::DuplicateFindingIds => {
            let finding = review["findings"][0].clone();
            review["findings"]
                .as_array_mut()
                .expect("the review carries a findings array")
                .push(finding);
        }
        // The fix ticket reuses the id of a ticket an earlier round executed.
        InvalidReview::CollidingTicketId => {
            review["fixes"][0]["id"] = serde_json::json!("1-verify-command-output");
        }
        // The fix ticket is blocked by an id that is not in its own set.
        InvalidReview::InvalidDependency => {
            review["fixes"][0]["blocked_by"] = serde_json::json!(["r9-1-not-in-the-set"]);
        }
    }

    value.to_string()
}

/// A scripted in-memory transport for outside-in pipeline tests: it advances
/// through the pipeline's own request sequence -- plan, spec, tickets, the one
/// triage pass, the queue's per-ticket steps, the review pass and the summary
/// pass -- without needing a live LLM or recorded network fixtures.
pub struct ScriptedTransport {
    step: Mutex<usize>,
    pub urls_called: Mutex<Vec<String>>,
    /// Every request body the transport was handed, in order. The seam a test
    /// reads to assert what a later phase was actually given -- above all that
    /// the summary request carries the queue report this run generated.
    pub requests: Mutex<Vec<serde_json::Value>>,
    /// The gate the scripted ticket carries, which decides the queue outcome.
    gate: GateScript,
    /// The set the scripted tickets pass hands over, and the set the scripted
    /// triage report is built from, so the ticket files, the report and the
    /// code-agent payload all describe the same tickets.
    tickets: TicketSet,
    /// How the triage pass between the ticket writes and the queue is answered.
    triage: TriageScript,
    /// How many triage turns have been requested so far.
    triage_turn: Mutex<usize>,
    /// How the summary pass after the queue is answered.
    summary: SummaryScript,
    /// How many summary turns have been requested so far: the initial
    /// generation, then the one correction an answer the validator rejected
    /// buys.
    summary_turn: Mutex<usize>,
    /// How the review pass after a resolved queue is answered.
    review: ReviewScript,
    /// How many review rounds have been requested so far.
    review_round: Mutex<usize>,
    /// The command the code agent's own `bash` step runs, when a test needs a
    /// real repository mutation rather than the fixed harmless `echo`.
    bash_command: Option<String>,
    /// How the queue's code-agent turns are answered.
    code: CodeScript,
    /// An explicit list of code-agent replies, served in order when present.
    code_replies: Option<Mutex<VecDeque<CodeReply>>>,
    /// The queue-round steps whose first code-agent request has already failed
    /// transiently, so its retry can proceed.
    transient_code_steps: Mutex<Vec<usize>>,
    /// When set, a plan request restarts the scripted handoff sequence at the
    /// plan step, so one transport can serve more than one `Pipeline::ask`.
    repeating_handoffs: bool,
    /// When set, the first plan turn is answered with an `execute_script` call
    /// carrying this program, and the next plan turn with `finish_plan`. The
    /// handoff step count is unchanged, so spec, tickets and the queue keep
    /// their ordinary answers.
    plan_script: Option<String>,
    /// How many plan requests this transport has seen.
    plan_turn: Mutex<usize>,
}

impl ScriptedTransport {
    pub fn new() -> Self {
        Self {
            step: Mutex::new(0),
            urls_called: Mutex::new(Vec::new()),
            requests: Mutex::new(Vec::new()),
            gate: GateScript::Passing,
            tickets: harmless_ticket_set(),
            triage: TriageScript::AllReady,
            triage_turn: Mutex::new(0),
            summary: SummaryScript::Summary,
            summary_turn: Mutex::new(0),
            review: ReviewScript::Pass,
            review_round: Mutex::new(0),
            bash_command: None,
            code: CodeScript::Scripted,
            code_replies: None,
            transient_code_steps: Mutex::new(Vec::new()),
            repeating_handoffs: false,
            plan_script: None,
            plan_turn: Mutex::new(0),
        }
    }

    /// Carry the gate that decides whether the queue resolves or drains.
    pub fn with_gate(mut self, gate: GateScript) -> Self {
        self.gate = gate;
        self
    }

    /// Answer the summary phase the way the given script says.
    pub fn with_summary(mut self, summary: SummaryScript) -> Self {
        self.summary = summary;
        self
    }

    /// Answer the review phase the way the given script says.
    pub fn with_review(mut self, review: ReviewScript) -> Self {
        self.review = review;
        self
    }

    /// Run `command` for the code agent's own `bash` step instead of the fixed
    /// harmless command, so a test can drive a real repository change through
    /// the external command boundary without a mocked Git service.
    pub fn with_bash_command(mut self, command: impl Into<String>) -> Self {
        self.bash_command = Some(command.into());
        self
    }

    /// Answer the code-agent turns the way the given script says.
    pub fn with_code(mut self, code: CodeScript) -> Self {
        self.code = code;
        self
    }

    /// Answer the queue's code-agent turns with an explicit ordered list. The
    /// list is consumed one reply per request, so a test scripts every attempt
    /// and every tool turn exactly as the production loop reaches it.
    pub fn with_code_replies(mut self, replies: Vec<CodeReply>) -> Self {
        self.code_replies = Some(Mutex::new(replies.into()));
        self
    }

    /// Serve more than one `Pipeline::ask` from the same transport: a request
    /// carrying the plan system prompt restarts the three handoff steps, so the
    /// second ask sees plan, spec and tickets again instead of the queue steps
    /// the first ask advanced through.
    pub fn with_repeating_handoffs(mut self) -> Self {
        self.repeating_handoffs = true;
        self
    }

    /// Answer the first plan turn with an `execute_script` call carrying
    /// `code` and the next with `finish_plan`. The script result is replayed
    /// to the plan agent before the handoff, so a test can assert it was never
    /// spilled, while spec, tickets and the queue advance exactly as before.
    pub fn with_plan_script(mut self, code: impl Into<String>) -> Self {
        self.plan_script = Some(code.into());
        self
    }

    /// Hand the scripted tickets pass a chosen fixture, and build the scripted
    /// triage report from the same set.
    pub fn with_tickets(mut self, tickets: TicketSet) -> Self {
        self.tickets = tickets;
        self
    }

    /// Answer the triage pass the way the given script says.
    pub fn with_triage(mut self, triage: TriageScript) -> Self {
        self.triage = triage;
        self
    }

    /// The `finish_tickets` payload the scripted tickets pass hands over: this
    /// transport's own fixture with the chosen gate applied, so the fixture
    /// stays the single description of the set.
    fn tickets_json(&self) -> String {
        let mut set = self.tickets.clone();
        let gate = match self.gate {
            GateScript::Passing => None,
            GateScript::Failing => Some("exit 1"),
            GateScript::SlowFailing => Some("sleep 0.1; exit 1"),
        };
        if let Some(gate) = gate {
            for ticket in &mut set.tickets {
                ticket.verification_gate = gate.to_string();
            }
        }
        serde_json::json!({ "tickets": set }).to_string()
    }

    /// The report the scripted triage pass hands over, built from the ticket
    /// set the scripted tickets pass emitted. A rejected hand-off drops one
    /// classification, so the boundary refuses it exactly as it refuses a
    /// model-authored omission.
    fn triage_report(&self, reject: bool) -> TriageReport {
        let mut tickets: Vec<TriageTicket> = self
            .tickets
            .tickets
            .iter()
            .enumerate()
            .map(|(index, ticket)| match self.triage {
                TriageScript::Blocked(state) => blocked_classification(ticket, state),
                TriageScript::OneBlocked(state) if index == one_blocked_index(&self.tickets) => {
                    blocked_classification(ticket, state)
                }
                _ => ready_classification(ticket),
            })
            .collect();
        if reject {
            tickets.pop();
        }
        TriageReport { tickets }
    }

    /// The response the triage pass is answered with, encoded for the provider
    /// format of `outgoing.url`.
    fn triage_response(&self, outgoing: &api::Outgoing) -> Result<RawResponse, CallError> {
        let turn = {
            let mut t = self.triage_turn.lock().unwrap();
            let cur = *t;
            *t += 1;
            cur
        };
        let is_responses = outgoing.url.contains("responses");

        if matches!(self.triage, TriageScript::ApiError) {
            return Err(CallError::Api {
                status: 400,
                message: "the endpoint refused the triage request".to_string(),
            });
        }
        if matches!(self.triage, TriageScript::TurnEnd) {
            let body = if is_responses {
                serde_json::json!({
                    "output": [{
                        "type": "message",
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": "Triage notes without a handoff." }]
                    }]
                })
                .to_string()
            } else {
                assistant_body(Some("Triage notes without a handoff."), None).body
            };
            return Ok(RawResponse { status: 200, body });
        }

        let reject = match self.triage {
            TriageScript::AlwaysRejected => true,
            TriageScript::RejectedOnceThenReady => turn == 0,
            _ => false,
        };
        let args_json = serde_json::json!({ "triage": self.triage_report(reject) }).to_string();
        let body = if is_responses {
            serde_json::json!({
                "output": [{
                    "type": "function_call",
                    "name": "finish_triage",
                    "call_id": format!("call_triage_{turn}"),
                    "arguments": args_json,
                }]
            })
            .to_string()
        } else {
            assistant_body(None, Some(("finish_triage", args_json))).body
        };

        Ok(RawResponse { status: 200, body })
    }

    /// The response the review phase is answered with, encoded for the provider
    /// format of `outgoing.url`.
    fn review_response(&self, outgoing: &api::Outgoing) -> Result<RawResponse, CallError> {
        let round = {
            let mut r = self.review_round.lock().unwrap();
            *r += 1;
            *r
        };
        let is_responses = outgoing.url.contains("responses");
        let args_json = match self.review {
            ReviewScript::Pass => passing_review_json(),
            ReviewScript::FixOnceThenPass => {
                if round == 1 {
                    fixing_review_json(round)
                } else {
                    passing_review_json()
                }
            }
            // Rounds 1-3 each emit their own fix ticket; round 4 -- the fourth
            // review -- passes, so the loop ends on a passing review.
            ReviewScript::FixThreeRoundsThenPass => {
                if round <= 3 {
                    fixing_review_json(round)
                } else {
                    passing_review_json()
                }
            }
            ReviewScript::AlwaysFixes => fixing_review_json(round),
            // The fix ticket's own gate fails, so the fix queue drains
            // incomplete; the pass that follows is never asked for.
            ReviewScript::FixOnceWithFailingGate => {
                if round == 1 {
                    fixing_review_json_with_gate(round, FAILING_FIX_TICKET_GATE)
                } else {
                    passing_review_json()
                }
            }
            ReviewScript::InvalidOnceThenPass(case) => {
                if round == 1 {
                    invalid_review_json(case)
                } else {
                    passing_review_json()
                }
            }
            ReviewScript::AlwaysInvalid(case) => invalid_review_json(case),
            ReviewScript::TurnEnd => {
                let body = if is_responses {
                    serde_json::json!({
                        "output": [{
                            "type": "message",
                            "role": "assistant",
                            "content": [{ "type": "output_text", "text": "Review notes without tool call." }]
                        }]
                    })
                    .to_string()
                } else {
                    assistant_body(Some("Review notes without tool call."), None).body
                };
                return Ok(RawResponse { status: 200, body });
            }
            ReviewScript::ApiError => {
                return Err(CallError::Api {
                    status: 400,
                    message: "the endpoint refused the review request".to_string(),
                });
            }
        };

        let body = if is_responses {
            serde_json::json!({
                "output": [{
                    "type": "function_call",
                    "name": "finish_review",
                    "call_id": format!("call_review_{round}"),
                    "arguments": args_json,
                }]
            })
            .to_string()
        } else {
            assistant_body(None, Some(("finish_review", args_json))).body
        };

        Ok(RawResponse { status: 200, body })
    }

    /// The response the summary phase's zero-based turn is answered with: turn
    /// 0 is the initial generation, turn 1 the one correction. Every script is
    /// a native Anthropic Messages response: Summary always routes to the
    /// dedicated Smoll provider, so a wrong route is a loud decode failure
    /// rather than a body nobody notices is out of place.
    fn summary_response(
        &self,
        outgoing: &api::Outgoing,
        turn: usize,
    ) -> Result<RawResponse, CallError> {
        match &self.summary {
            SummaryScript::Summary => Ok(anthropic_text(SCRIPTED_SUMMARY)),
            SummaryScript::Answer(text) => Ok(anthropic_text(text)),
            // A native assistant turn with no content blocks: the provider
            // reports it as unusable, which is the native shape of an absent
            // answer.
            SummaryScript::NoContent => Ok(anthropic_body(Vec::new())),
            SummaryScript::Blank => Ok(anthropic_text("   \n\t\n")),
            SummaryScript::ApiError => Err(CallError::Api {
                status: 400,
                message: "the endpoint refused the summary request".to_string(),
            }),
            SummaryScript::TransportError => Err(CallError::Transport(
                "scripted transport failure".to_string(),
            )),
            // A read-only script that changes nothing, handed over forever: the
            // pass can only take another step -- up to its step limit.
            SummaryScript::EndlessToolCalls => Ok(anthropic_tool_use(
                &format!("toolu_summary_{turn}"),
                "execute_script",
                serde_json::json!({ "code": "return 1;" }),
            )),
            SummaryScript::ToolExchange => {
                if turn == 0 {
                    return Ok(anthropic_tool_use(
                        SUMMARY_TOOL_ID,
                        "execute_script",
                        serde_json::json!({ "code": SUMMARY_SCRIPT_CODE }),
                    ));
                }
                // The second turn must carry the tool result for the tool_use
                // the first turn emitted; a missing or mismatched id is the
                // replay drift this script exists to catch.
                let result = summary_tool_result(outgoing, SUMMARY_TOOL_ID).unwrap_or_else(|| {
                    panic!(
                        "the summary tool exchange did not replay a tool_result for \
                         `{SUMMARY_TOOL_ID}`: {}",
                        outgoing.body
                    )
                });
                assert_eq!(
                    result, SUMMARY_SCRIPT_OUTPUT,
                    "the summary tool_result must carry the read-only script's own output"
                );
                Ok(anthropic_text(SCRIPTED_SUMMARY))
            }
            SummaryScript::UnofferedToolCall => {
                if turn == 0 {
                    return Ok(anthropic_tool_use(
                        SUMMARY_UNOFFERED_TOOL_ID,
                        "bash",
                        serde_json::json!({ "command": "echo forbidden" }),
                    ));
                }
                // The unoffered call must come back as a rejected tool result;
                // the pass then answers with the canonical summary.
                let result = summary_tool_result(outgoing, SUMMARY_UNOFFERED_TOOL_ID)
                    .unwrap_or_else(|| {
                        panic!(
                            "the unoffered summary tool call did not replay a tool_result for \
                             `{SUMMARY_UNOFFERED_TOOL_ID}`: {}",
                            outgoing.body
                        )
                    });
                assert!(
                    result.contains("bash"),
                    "the tool result must reject the unoffered bash call: {result}"
                );
                Ok(anthropic_text(SCRIPTED_SUMMARY))
            }
            SummaryScript::Turns(replies) => {
                let reply = replies
                    .get(turn)
                    .or_else(|| replies.last())
                    .expect("a summary turn script carries at least one reply");
                summary_reply_response(reply, turn)
            }
        }
    }
}

/// One native Anthropic Messages response carrying `blocks` as its content.
fn anthropic_body(blocks: Vec<serde_json::Value>) -> RawResponse {
    RawResponse {
        status: 200,
        body: serde_json::json!({
            "id": "msg_scripted_summary",
            "type": "message",
            "role": "assistant",
            "model": TEST_SMOLL_MODEL,
            "content": blocks,
        })
        .to_string(),
    }
}

/// A native Anthropic text answer.
fn anthropic_text(text: &str) -> RawResponse {
    anthropic_body(vec![serde_json::json!({ "type": "text", "text": text })])
}

/// A native Anthropic `tool_use` block, encoded as the reply that requests it.
fn anthropic_tool_use(id: &str, name: &str, input: serde_json::Value) -> RawResponse {
    anthropic_body(vec![serde_json::json!({
        "type": "tool_use",
        "id": id,
        "name": name,
        "input": input,
    })])
}

/// The content of the `tool_result` block addressed to `tool_use_id` in a
/// native Anthropic request, if the transcript carries one.
fn summary_tool_result(outgoing: &api::Outgoing, tool_use_id: &str) -> Option<String> {
    outgoing.body["messages"]
        .as_array()?
        .iter()
        .flat_map(|message| message["content"].as_array().into_iter().flatten())
        .find(|block| {
            block["type"] == serde_json::json!("tool_result")
                && block["tool_use_id"] == serde_json::json!(tool_use_id)
        })
        .and_then(|block| block["content"].as_str())
        .map(str::to_string)
}

/// The response one scripted summary turn is answered with, on the native
/// Anthropic Messages wire format.
fn summary_reply_response(reply: &SummaryReply, turn: usize) -> Result<RawResponse, CallError> {
    match reply {
        SummaryReply::Answer(text) => Ok(anthropic_text(text)),
        SummaryReply::NoContent => Ok(anthropic_body(Vec::new())),
        // A read-only script that changes nothing: the pass can only take
        // another step before its own final answer.
        SummaryReply::ToolCall => Ok(anthropic_tool_use(
            &format!("toolu_summary_{turn}"),
            "execute_script",
            serde_json::json!({ "code": "return 1;" }),
        )),
        SummaryReply::ApiError => Err(CallError::Api {
            status: 400,
            message: "the endpoint refused the summary request".to_string(),
        }),
        // An access denial is a real provider response, not a transport error:
        // it comes back as the native Anthropic HTTP 403 shape so the decode
        // path sees the provider's own refusal.
        SummaryReply::Forbidden => Ok(RawResponse {
            status: 403,
            body: serde_json::json!({
                "type": "error",
                "error": {
                    "type": "permission_error",
                    "message": "the provider forbids access to this request",
                },
            })
            .to_string(),
        }),
        SummaryReply::TransportError => Err(CallError::Transport(
            "scripted transport failure".to_string(),
        )),
    }
}

/// One scripted assistant reply on the chat-completions wire format: its text
/// when it carries one, and its single optional (tool, arguments) call.
fn assistant_body(content: Option<&str>, call: Option<(&str, String)>) -> RawResponse {
    let tool_calls: Vec<serde_json::Value> = call
        .into_iter()
        .map(|(name, arguments)| {
            serde_json::json!({
                "id": "call_0",
                "type": "function",
                "function": { "name": name, "arguments": arguments },
            })
        })
        .collect();
    RawResponse {
        status: 200,
        body: serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": content,
                    "tool_calls": tool_calls,
                }
            }]
        })
        .to_string(),
    }
}

/// One explicitly scripted code-agent reply, encoded for the provider wire
/// format the outgoing request names.
fn code_reply_response(
    outgoing: &api::Outgoing,
    reply: &CodeReply,
    turn: usize,
) -> Result<RawResponse, CallError> {
    if let CodeReply::Api { status, message } = reply {
        return Err(CallError::Api {
            status: *status,
            message: message.clone(),
        });
    }
    if let CodeReply::Transport(message) = reply {
        return Err(CallError::Transport(message.clone()));
    }
    if matches!(reply, CodeReply::Malformed) {
        // A syntactically valid provider envelope the decoder still refuses: an
        // empty choice/output list is a malformed response, not a transport
        // failure, so the agent loop does not retry it.
        let body = if outgoing.url.contains("responses") {
            serde_json::json!({ "output": [] }).to_string()
        } else {
            serde_json::json!({ "choices": [] }).to_string()
        };
        return Ok(RawResponse { status: 200, body });
    }

    let is_responses = outgoing.url.contains("responses");
    let (narration, call) = match reply {
        CodeReply::TurnEnd(narration) => (narration.as_deref(), None),
        CodeReply::Complete(narration) => (narration.as_deref(), Some("task_complete")),
        CodeReply::ToolCall { narration, .. } => (narration.as_deref(), Some("bash")),
        CodeReply::RawToolCall {
            narration, name, ..
        } => (narration.as_deref(), Some(name.as_str())),
        CodeReply::Api { .. } | CodeReply::Transport(_) | CodeReply::Malformed => unreachable!(),
    };

    if is_responses {
        let mut output = Vec::new();
        if let Some(text) = narration {
            output.push(serde_json::json!({
                "type": "message",
                "role": "assistant",
                "content": [{ "type": "output_text", "text": text }],
            }));
        }
        if let Some(name) = call {
            output.push(serde_json::json!({
                "type": "function_call",
                "name": name,
                "call_id": format!("call_code_{turn}"),
                "arguments": tool_arguments(reply),
            }));
        }
        return Ok(RawResponse {
            status: 200,
            body: serde_json::json!({ "output": output }).to_string(),
        });
    }

    let call = call.map(|name| (name, tool_arguments(reply)));
    Ok(assistant_body(narration, call))
}

/// The JSON text a scripted tool call carries as its arguments.
fn tool_arguments(reply: &CodeReply) -> String {
    match reply {
        CodeReply::ToolCall { command, .. } => {
            serde_json::json!({ "command": command }).to_string()
        }
        CodeReply::RawToolCall { arguments, .. } => arguments.clone(),
        _ => "{}".to_string(),
    }
}

impl Transport for ScriptedTransport {
    fn send(&self, outgoing: &api::Outgoing) -> Result<RawResponse, CallError> {
        self.urls_called.lock().unwrap().push(outgoing.url.clone());
        self.requests.lock().unwrap().push(outgoing.body.clone());

        // The triage pass and the summary pass are told apart by their own
        // prompts, not by a step count: whatever the queue did before or after
        // them -- six steps, three, or none at all when the status write stops
        // it -- their requests are answered on their own terms.
        if outgoing.body.to_string().contains(TRIAGE_MARKER) {
            return self.triage_response(outgoing);
        }
        if outgoing.body.to_string().contains(SUMMARY_MARKER) {
            let turn = {
                let mut t = self.summary_turn.lock().unwrap();
                let cur = *t;
                *t += 1;
                cur
            };
            return self.summary_response(outgoing, turn);
        }
        if outgoing.body.to_string().contains(REVIEW_MARKER) {
            return self.review_response(outgoing);
        }

        // An explicit code reply list owns every code-agent request, including
        // the tool turns inside one attempt and both fresh attempts.
        if outgoing.body.to_string().contains(CODE_MARKER)
            && let Some(replies) = &self.code_replies
        {
            let turn = {
                let mut reply = replies.lock().unwrap();
                let turn = reply.len();
                let next = reply.pop_front().expect(
                    "the scripted code reply list has an answer for every code-agent request",
                );
                (turn, next)
            };
            return code_reply_response(outgoing, &turn.1, turn.0);
        }

        // The first code-agent request of each queue round fails in the
        // transport a scripted number of times. The check runs before the step
        // advances, so each retry replays the same request, and the loop's own
        // backoff -- not a model attempt -- lands inside the queue interval.
        let transient_failures = match self.code {
            CodeScript::TransientOncePerRound => 1,
            _ => 0,
        };
        if transient_failures > 0 {
            let candidate = *self.step.lock().unwrap();
            if candidate >= 3 && (candidate - 3).is_multiple_of(2) {
                let mut failed = self.transient_code_steps.lock().unwrap();
                let attempts = failed.iter().filter(|step| **step == candidate).count();
                if attempts < transient_failures {
                    failed.push(candidate);
                    return Err(CallError::Transport(
                        "scripted transient code-agent failure".to_string(),
                    ));
                }
            }
        }

        // A later ask over the same plan agent starts its handoff sequence
        // again; the plan system prompt is the one stable marker every plan
        // request carries.
        if self.repeating_handoffs && outgoing.body.to_string().contains(PLAN_MARKER) {
            *self.step.lock().unwrap() = 0;
            *self.plan_turn.lock().unwrap() = 0;
        }

        // A plan script is one extra tool turn inside the plan conversation:
        // the first plan request is answered with it without advancing the
        // handoff steps, so the next plan request still sees step zero and ends
        // the interview with `finish_plan`.
        if let Some(code) = &self.plan_script
            && outgoing.body.to_string().contains(PLAN_MARKER)
        {
            let turn = {
                let mut t = self.plan_turn.lock().unwrap();
                let cur = *t;
                *t += 1;
                cur
            };
            if turn == 0 {
                let arguments = serde_json::json!({ "code": code }).to_string();
                if outgoing.url.contains("responses") {
                    return Ok(RawResponse {
                        status: 200,
                        body: serde_json::json!({
                            "output": [{
                                "type": "function_call",
                                "name": "execute_script",
                                "call_id": "call_plan_script",
                                "arguments": arguments,
                            }]
                        })
                        .to_string(),
                    });
                }
                return Ok(assistant_body(None, Some(("execute_script", arguments))));
            }
        }

        // The queue's own steps, in order.
        let step = {
            let mut s = self.step.lock().unwrap();
            let cur = *s;
            *s += 1;
            cur
        };

        // A code-agent refusal is planted at the first code turn, before any
        // tool call: the ticket fails and its verification gate is never
        // reached. The handoffs (steps 0-2) are unaffected.
        if self.code == CodeScript::ApiError && step >= 3 {
            return Err(CallError::Api {
                status: 400,
                message: "scripted code-agent refusal".to_string(),
            });
        }

        let is_responses = outgoing.url.contains("responses");

        let (tool_name, args_json) = match step {
            0 => {
                let plan = harmless_plan_fixture().to_markdown();
                let args = serde_json::json!({ "plan": plan });
                ("finish_plan", args.to_string())
            }
            1 => {
                let args = serde_json::json!({ "spec": harmless_spec_markdown() });
                ("finish_spec", args.to_string())
            }
            2 => ("finish_tickets", self.tickets_json()),
            n if (n - 3) % 2 == 0 => {
                let command = self.bash_command.as_deref().unwrap_or("echo ejaj-e2e-ok");
                let args = serde_json::json!({ "command": command });
                ("bash", args.to_string())
            }
            _ => ("task_complete", "{}".to_string()),
        };

        let body = if is_responses {
            serde_json::json!({
                "output": [
                    {
                        "type": "function_call",
                        "name": tool_name,
                        "call_id": format!("call_{step}"),
                        "arguments": args_json,
                    }
                ]
            })
            .to_string()
        } else {
            serde_json::json!({
                "choices": [
                    {
                        "message": {
                            "role": "assistant",
                            "tool_calls": [
                                {
                                    "id": format!("call_{step}"),
                                    "type": "function",
                                    "function": {
                                        "name": tool_name,
                                        "arguments": args_json,
                                    }
                                }
                            ]
                        }
                    }
                ]
            })
            .to_string()
        };

        Ok(RawResponse { status: 200, body })
    }
}

/// The scripted pipeline's configuration: the mid block for every non-summary
/// phase and the dedicated Smoll block Summary always uses. Summary is routed
/// before the mode is consulted, so a scripted configuration without Smoll
/// could not serve it at all.
fn scripted_config(transport: Arc<ScriptedTransport>) -> Config {
    Config::new(test_url(), test_model(), Some(TEST_API_KEY.to_string()))
        .with_smoll(TEST_SMOLL_URL, TEST_SMOLL_MODEL, TEST_SMOLL_API_KEY)
        .with_transport(transport)
}

/// A pipeline configuration driven by `ScriptedTransport`.
pub fn scripted_pipeline_config() -> Config {
    scripted_config(Arc::new(ScriptedTransport::new()))
}

/// The scripted pipeline over an already-built transport, so a test can keep
/// the transport and inspect every recorded request.
pub fn scripted_pipeline_with_transport(transport: Arc<ScriptedTransport>) -> Config {
    scripted_config(transport)
}

/// The same scripted pipeline with a chosen ticket gate and summary script,
/// handing the transport back so a test can assert where each request went and
/// what it carried.
pub fn scripted_pipeline_with(
    gate: GateScript,
    summary: SummaryScript,
) -> (Config, Arc<ScriptedTransport>) {
    let transport = Arc::new(
        ScriptedTransport::new()
            .with_gate(gate)
            .with_summary(summary),
    );
    let config = scripted_config(transport.clone());
    (config, transport)
}

/// The same scripted pipeline with a chosen ticket gate, summary script and
/// code-agent command: the transport answers the queue's `bash` step with
/// `command`, so a test can drive a real repository change end to end.
pub fn scripted_pipeline_with_command(
    gate: GateScript,
    summary: SummaryScript,
    command: impl Into<String>,
) -> (Config, Arc<ScriptedTransport>) {
    let transport = Arc::new(
        ScriptedTransport::new()
            .with_gate(gate)
            .with_summary(summary)
            .with_bash_command(command),
    );
    let config = scripted_config(transport.clone());
    (config, transport)
}

/// The same scripted pipeline with a chosen code-agent script and summary
/// script, so a test can drive a code failure before the gate.
pub fn scripted_pipeline_with_code(
    code: CodeScript,
    summary: SummaryScript,
) -> (Config, Arc<ScriptedTransport>) {
    let transport = Arc::new(
        ScriptedTransport::new()
            .with_code(code)
            .with_summary(summary),
    );
    let config = scripted_config(transport.clone());
    (config, transport)
}

/// The scripted mid pipeline over the one-ticket fixture with a chosen
/// triage script, so a test can drive every triage outcome end to end.
pub fn scripted_triage_pipeline(triage: TriageScript) -> (Config, Arc<ScriptedTransport>) {
    scripted_triage_pipeline_with(triage, SummaryScript::Summary)
}

/// The same one-ticket scripted pipeline with a chosen triage script and
/// summary script.
pub fn scripted_triage_pipeline_with(
    triage: TriageScript,
    summary: SummaryScript,
) -> (Config, Arc<ScriptedTransport>) {
    let transport = Arc::new(
        ScriptedTransport::new()
            .with_triage(triage)
            .with_summary(summary),
    );
    let config = scripted_config(transport.clone());
    (config, transport)
}

/// The scripted mid pipeline over the three-ticket fixture -- the largest
/// scripted set -- with a chosen triage script.
pub fn scripted_multi_triage_pipeline(triage: TriageScript) -> (Config, Arc<ScriptedTransport>) {
    let transport = Arc::new(
        ScriptedTransport::new()
            .with_tickets(parse_ticket_set(multi_tickets_json()))
            .with_triage(triage),
    );
    let config = scripted_config(transport.clone());
    (config, transport)
}

/// The same scripted pipeline with a chosen review script and summary script.
pub fn scripted_pipeline_with_review(
    review: ReviewScript,
    summary: SummaryScript,
) -> (Config, Arc<ScriptedTransport>) {
    scripted_pipeline_with_gate_and_review(GateScript::Passing, review, summary)
}

/// The same scripted pipeline with a chosen ticket gate, review script and
/// summary script, so a test can decide how the original queue ends
/// independently of how the review would answer if it were reached.
pub fn scripted_pipeline_with_gate_and_review(
    gate: GateScript,
    review: ReviewScript,
    summary: SummaryScript,
) -> (Config, Arc<ScriptedTransport>) {
    let transport = Arc::new(
        ScriptedTransport::new()
            .with_gate(gate)
            .with_review(review)
            .with_summary(summary),
    );
    let config = scripted_config(transport.clone());
    (config, transport)
}

/// A hybrid pipeline configuration driven by `ScriptedTransport`.
pub fn scripted_hybrid_pipeline_config() -> (Config, Arc<ScriptedTransport>) {
    scripted_hybrid_pipeline_with(SummaryScript::Summary)
}

/// The same hybrid configuration with a chosen summary script, so a test can
/// drive a failing summary pass through the two-provider routing.
pub fn scripted_hybrid_pipeline_with(summary: SummaryScript) -> (Config, Arc<ScriptedTransport>) {
    let transport = Arc::new(ScriptedTransport::new().with_summary(summary));
    let config = scripted_config(transport.clone())
        .with_giga(
            test_giga_url(),
            test_giga_model(),
            Some(TEST_API_KEY.to_string()),
        )
        .with_mode(Mode::Hybrid);
    (config, transport)
}

/// The same hybrid configuration with a chosen triage script, so a test can
/// assert where the additional triage request goes under two-provider routing.
pub fn scripted_hybrid_pipeline_with_triage(
    triage: TriageScript,
) -> (Config, Arc<ScriptedTransport>) {
    let transport = Arc::new(ScriptedTransport::new().with_triage(triage));
    let config = scripted_config(transport.clone())
        .with_giga(
            test_giga_url(),
            test_giga_model(),
            Some(TEST_API_KEY.to_string()),
        )
        .with_mode(Mode::Hybrid);
    (config, transport)
}

// ---------------------------------------------------------------------------
// Disposable repositories
//
// A pipeline invocation is anchored to one project root, and the pipeline
// refuses to start on a root whose working tree is not clean. The integration
// tests therefore run against throwaway Git repositories in the system
// temporary directory: the package checkout is never inspected, staged or
// committed, and a test that needs a dirty tree makes one of its own.
// ---------------------------------------------------------------------------

/// The deterministic branch the local remote HEAD points at, so a test never
/// depends on whichever default branch the machine's Git happens to pick.
pub const DEFAULT_BRANCH: &str = "main";

/// The branch the non-default finalization tests run on: a real local branch
/// whose name differs from the default the remote HEAD resolves to.
pub const FEATURE_BRANCH: &str = "feature/work";

/// A disposable, locally configured Git repository with a real local bare
/// origin.
///
/// The repository is initialized with a local identity and a local signing
/// policy, so a machine-wide Git configuration can neither sign nor refuse the
/// test's baseline commit. Its origin is a bare repository beside the working
/// tree, its default branch is `main` and `origin/HEAD` resolves to that
/// branch, so a finalizer reads real branch identities through real Git.
/// Dropping it removes both directories, so a test holds it for exactly as long
/// as the run it gates needs a root.
pub struct TempRepo {
    root: PathBuf,
    origin: PathBuf,
}

impl TempRepo {
    /// A repository in its own temporary directory, beginning empty and clean.
    pub fn new() -> TempRepo {
        TempRepo::at(temp_repo_path())
    }

    /// A repository at `root`, created if it does not exist yet, with a real
    /// local bare origin beside it whose default branch is explicit.
    pub fn at(root: PathBuf) -> TempRepo {
        fs::create_dir_all(&root).expect("the temporary repository directory is creatable");
        let origin = sibling_origin_path(&root);
        let _ = fs::remove_dir_all(&origin);
        let repo = TempRepo { root, origin };
        repo.git(&["init", "--quiet", "--initial-branch=main"]);
        repo.git(&["config", "user.name", "Ejaj Test"]);
        repo.git(&["config", "user.email", "ejaj-test@example.invalid"]);
        repo.git(&["config", "commit.gpgsign", "false"]);
        repo.init_origin();
        repo
    }

    /// Create the real local bare origin, publish the deterministic default
    /// branch to it, and point `origin/HEAD` at that branch. Every operation is
    /// a local Git process against a real repository: no network and no fake
    /// Git service.
    fn init_origin(&self) {
        let output = Command::new("git")
            .args(["init", "--quiet", "--bare"])
            .arg(&self.origin)
            .current_dir(&self.root)
            .output()
            .unwrap_or_else(|e| panic!("the local bare origin is creatable: {e}"));
        assert!(
            output.status.success(),
            "the local bare origin is initialized in {} ({}):\n{}",
            self.origin.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );

        let origin = self.origin.display().to_string();
        self.git(&["remote", "add", "origin", &origin]);
        self.git(&[
            "commit",
            "--quiet",
            "--allow-empty",
            "--message",
            "origin baseline",
        ]);
        self.git(&["push", "--quiet", "--set-upstream", "origin", "main"]);
        self.git(&["remote", "set-head", "origin", "main"]);
        // The seed commit exists only to give the real remote its default
        // branch and a readable `origin/HEAD`. Drop the local branch again, so
        // the disposable repository still starts with an unborn default branch
        // and a test counts only the commits it makes itself.
        self.git(&["update-ref", "-d", "refs/heads/main"]);
    }

    /// The repository root, handed to `Pipeline::new` and used for path
    /// assertions.
    pub fn path(&self) -> &Path {
        &self.root
    }

    /// The local bare origin the repository's `origin` remote points at.
    pub fn origin_path(&self) -> &Path {
        &self.origin
    }

    /// The branch currently checked out, read through the same strict Git
    /// symbolic reference a finalizer uses.
    pub fn current_branch(&self) -> String {
        self.git(&["symbolic-ref", "--quiet", "--short", "HEAD"])
            .trim()
            .to_string()
    }

    /// What the local `origin/HEAD` symbolic reference resolves to, e.g.
    /// `origin/main`.
    pub fn remote_head(&self) -> String {
        self.git(&[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ])
        .trim()
        .to_string()
    }

    /// Create and check out `name` at the current commit, leaving the default
    /// branch and every remote ref exactly where they were.
    pub fn checkout_new_branch(&self, name: &str) {
        self.git(&["switch", "--quiet", "--create", name]);
    }

    /// Remove the local `origin/HEAD` symbolic reference while leaving the
    /// remote-tracking branch it named in place.
    pub fn remove_remote_head(&self) {
        self.git(&["remote", "set-head", "origin", "--delete"]);
    }

    /// Replace the loose `origin/HEAD` reference with exactly `contents`, so a
    /// test can plant a malformed, out-of-prefix or dangling symbolic target
    /// through the real Git reference file rather than a mocked Git service.
    pub fn write_remote_head_raw(&self, contents: &str) {
        let path = self.root.join(".git/refs/remotes/origin/HEAD");
        let _ = fs::remove_dir_all(&path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("the origin refs directory is creatable");
        }
        fs::write(&path, contents).expect("the loose origin/HEAD reference is writable");
    }

    /// Make `origin/HEAD` unreadable by replacing its loose reference file with
    /// a directory, so the real symbolic-reference read cannot produce a name.
    pub fn make_remote_head_unreadable(&self) {
        let path = self.root.join(".git/refs/remotes/origin/HEAD");
        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("the unreadable origin/HEAD directory is creatable");
    }

    /// Delete the remote-tracking branch `origin/<branch>` while leaving the
    /// `origin/HEAD` symbolic reference pointing at it, so the reference is
    /// dangling rather than removed.
    pub fn remove_remote_tracking_branch(&self, branch: &str) {
        self.git(&["update-ref", "-d", &format!("refs/remotes/origin/{branch}")]);
    }

    /// Replace `.git/HEAD` with exactly `contents`, so a test can plant a
    /// malformed branch identity through the real reference file.
    pub fn write_head_raw(&self, contents: &str) {
        fs::write(self.root.join(".git/HEAD"), contents).expect("the HEAD reference is writable");
    }

    /// Detach HEAD at the current commit, leaving no branch checked out.
    pub fn detach_head(&self) {
        self.git(&["switch", "--quiet", "--detach"]);
    }

    /// Leave a real Git lock on HEAD behind, so an operation that has to update
    /// HEAD cannot do so until the lock is removed.
    pub fn lock_head(&self) {
        fs::write(self.root.join(".git/HEAD.lock"), "").expect("the HEAD lock is writable");
    }

    /// Leave a real Git lock on the index behind, so an operation that has to
    /// write the index cannot do so until the lock is removed.
    pub fn lock_index(&self) {
        fs::write(self.root.join(".git/index.lock"), "").expect("the index lock is writable");
    }

    /// Install one executable hook in this repository's own hooks directory,
    /// pinning the local hooks path so a machine-wide Git configuration cannot
    /// redirect it. The hook is a real Git process boundary, never a mock.
    pub fn install_hook(&self, name: &str, script: &str) {
        self.git(&["config", "core.hooksPath", ".git/hooks"]);
        let hooks = self.root.join(".git/hooks");
        fs::create_dir_all(&hooks).expect("the hooks directory is creatable");
        let path = hooks.join(name);
        fs::write(&path, script).expect("the hook is writable");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(&path).expect("the hook exists").permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&path, permissions).expect("the hook is executable");
        }
    }

    /// Install one executable hook in the real bare origin, so a remote-side
    /// rejection is produced by Git itself rather than by a test double.
    pub fn install_origin_hook(&self, name: &str, script: &str) {
        let hooks = self.origin.join("hooks");
        fs::create_dir_all(&hooks).expect("the origin hooks directory is creatable");
        let path = hooks.join(name);
        fs::write(&path, script).expect("the origin hook is writable");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(&path).expect("the hook exists").permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&path, permissions).expect("the origin hook is executable");
        }
    }

    /// Configure the real bare origin to refuse every push, so a publication
    /// attempt is denied by the remote's own pre-receive hook.
    pub fn reject_pushes(&self, message: &str) {
        let script = format!("#!/bin/sh\nprintf '%s\\n' '{message}' >&2\nexit 1\n");
        self.install_origin_hook("pre-receive", &script);
    }

    /// Every local reference and the object it points at, captured so a test can
    /// prove a failure left every ref exactly where it was.
    pub fn all_refs(&self) -> String {
        self.git(&["for-each-ref", "--format=%(refname) %(objectname)"])
    }

    /// Keep the readable local `origin/HEAD` but make the remote endpoint
    /// unavailable: the bare origin is removed and the remote URL points at a
    /// path that does not exist. A run that tries to reach the remote now fails.
    pub fn make_origin_unavailable(&self) {
        let _ = fs::remove_dir_all(&self.origin);
        let unavailable = self.origin.join("unavailable.git").display().to_string();
        self.git(&["remote", "set-url", "origin", &unavailable]);
    }

    /// Arrange a default-branch run whose fast-forward-only pull succeeds and
    /// whose later uniqueness query of the candidate summary branch fails. The
    /// real bare origin is advanced from an independent clone, so the pull has a
    /// real fast-forward to perform; a real `post-merge` hook then repoints the
    /// repository's `origin` at a path that does not exist, so the pull that ran
    /// the hook succeeds while every later query of `origin` fails. Returns the
    /// advanced remote default-branch tip.
    pub fn make_origin_unavailable_after_pull(&self) -> String {
        let advanced = self.advance_origin_default("advanced.txt", "advanced remotely\n");
        let unavailable = self.origin.join("unavailable.git").display().to_string();
        let hook = format!("#!/bin/sh\ngit remote set-url origin '{unavailable}'\n");
        self.install_hook("post-merge", &hook);
        advanced
    }

    /// Keep the readable local `origin/HEAD` but refuse remote writes: the push
    /// URL points back at this checked-out repository, whose current branch
    /// refuses a non-bare push. A run that tries to push now fails.
    pub fn forbid_remote_writes(&self) {
        self.git(&["config", "receive.denyCurrentBranch", "refuse"]);
        let root = self.root.display().to_string();
        self.git(&["remote", "set-url", "--push", "origin", &root]);
    }

    /// Run one Git command inside this repository and return its stdout. A
    /// nonzero exit is a loud failure, so a broken fixture can never be
    /// mistaken for a clean or a dirty repository.
    pub fn git(&self, args: &[&str]) -> String {
        String::from_utf8_lossy(&self.git_bytes(args)).into_owned()
    }

    /// Run one Git command inside this repository and return its raw stdout
    /// bytes, undecoded. A caller that must compare against exact bytes -- the
    /// raw commit message a successful run recorded, for instance -- reads them
    /// here rather than through a lossy decode that could hide a difference. A
    /// nonzero exit is a loud failure, so a broken fixture can never be
    /// mistaken for a clean or a dirty repository.
    pub fn git_bytes(&self, args: &[&str]) -> Vec<u8> {
        let output = Command::new("git")
            .args(args)
            .current_dir(&self.root)
            .output()
            .unwrap_or_else(|e| panic!("git is installed and runs: {e}"));
        assert!(
            output.status.success(),
            "`git {}` failed in {} ({}):\n{}",
            args.join(" "),
            self.root.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    /// Write a file relative to the repository root, creating parent
    /// directories, without staging or committing it.
    pub fn write(&self, relative: &str, contents: &str) {
        let path = self.root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("the fixture's parent directory is creatable");
        }
        fs::write(&path, contents).expect("the fixture file is writable");
    }

    /// Stage everything in the repository and commit it, so a planted fixture
    /// becomes part of a clean baseline. The baseline is published to the real
    /// bare origin as the default branch, so a default-branch run can
    /// fast-forward it instead of meeting an unrelated history.
    pub fn commit_all(&self, message: &str) {
        self.git(&["add", "--all"]);
        self.git(&["commit", "--quiet", "--message", message]);
        self.publish_default_branch();
    }

    /// Commit the current tree -- empty or not -- as a baseline, so `HEAD`
    /// exists and the preflight sees a clean tree whatever the fixture planted
    /// first. An empty directory a fixture plants is invisible to Git, so the
    /// commit may stage nothing; it is still created. Like `commit_all`, the
    /// baseline is published to the real bare origin.
    pub fn commit_baseline(&self) {
        self.git(&["add", "--all"]);
        self.git(&[
            "commit",
            "--quiet",
            "--allow-empty",
            "--message",
            "baseline",
        ]);
        self.publish_default_branch();
    }

    /// Publish the current local default branch to the real bare origin, so the
    /// local branch and `origin/main` name the same commit, and point the local
    /// branch at the remote-tracking branch. A fixture that commits a baseline
    /// calls this so a default-branch finalizer's fast-forward-only pull has a
    /// fast-forward to perform rather than unrelated histories to reconcile.
    pub fn publish_default_branch(&self) {
        self.git(&["push", "--quiet", "--force", "origin", DEFAULT_BRANCH]);
        self.git(&[
            "branch",
            "--quiet",
            "--set-upstream-to",
            &format!("origin/{DEFAULT_BRANCH}"),
            DEFAULT_BRANCH,
        ]);
    }

    /// Make the repository's checked-out default branch exactly the remote
    /// default branch, tracking it, without adding a local commit. A test that
    /// wants a clean default-branch run over the fixture's own baseline calls
    /// this after planting the baseline.
    pub fn reset_default_to_origin(&self) {
        self.git(&["fetch", "--quiet", "origin"]);
        self.git(&[
            "switch",
            "--quiet",
            "--force-create",
            DEFAULT_BRANCH,
            &format!("origin/{DEFAULT_BRANCH}"),
        ]);
        self.git(&[
            "branch",
            "--quiet",
            "--set-upstream-to",
            &format!("origin/{DEFAULT_BRANCH}"),
            DEFAULT_BRANCH,
        ]);
    }

    /// Advance the real bare origin's default branch from an independent clone,
    /// leaving this working tree's default branch where it was. The returned
    /// object id is the new remote default-branch tip.
    pub fn advance_origin_default(&self, relative: &str, contents: &str) -> String {
        let clone = temp_repo_path();
        let _ = fs::remove_dir_all(&clone);
        let output = Command::new("git")
            .args(["clone", "--quiet", "--branch", DEFAULT_BRANCH])
            .arg(&self.origin)
            .arg(&clone)
            .output()
            .unwrap_or_else(|e| panic!("the independent origin clone is creatable: {e}"));
        assert!(
            output.status.success(),
            "the independent origin clone is created in {} ({}):\n{}",
            clone.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );

        let write = |args: &[&str]| {
            let output = Command::new("git")
                .args(args)
                .current_dir(&clone)
                .output()
                .unwrap_or_else(|e| panic!("the independent clone runs git: {e}"));
            assert!(
                output.status.success(),
                "`git {}` failed in the independent clone ({}):\n{}",
                args.join(" "),
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8_lossy(&output.stdout).into_owned()
        };

        write(&["config", "user.name", "Ejaj Remote Test"]);
        write(&["config", "user.email", "ejaj-remote-test@example.invalid"]);
        let path = clone.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("the independent clone's parent is creatable");
        }
        fs::write(&path, contents).expect("the independent clone's file is writable");
        write(&["add", "--all"]);
        write(&[
            "commit",
            "--quiet",
            "--message",
            "independent origin advance",
        ]);
        write(&["push", "--quiet", "origin", DEFAULT_BRANCH]);
        let advanced = write(&["rev-parse", "HEAD"]).trim().to_string();
        let _ = fs::remove_dir_all(&clone);
        advanced
    }

    /// The object one reference names, or `None` when it does not exist.
    pub fn git_ref(&self, reference: &str) -> Option<String> {
        let output = Command::new("git")
            .args(["rev-parse", "--verify", "--quiet", reference])
            .current_dir(&self.root)
            .output()
            .unwrap_or_else(|e| panic!("git is installed and runs: {e}"));
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Run one Git command against the real bare origin and return its stdout.
    pub fn origin_git(&self, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("--git-dir")
            .arg(&self.origin)
            .args(args)
            .output()
            .unwrap_or_else(|e| panic!("git is installed and runs: {e}"));
        assert!(
            output.status.success(),
            "`git --git-dir {} {}` failed ({}):\n{}",
            self.origin.display(),
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// The object one reference names in the real bare origin, or `None` when
    /// it does not exist.
    pub fn origin_ref(&self, reference: &str) -> Option<String> {
        let output = Command::new("git")
            .arg("--git-dir")
            .arg(&self.origin)
            .args(["rev-parse", "--verify", "--quiet", reference])
            .output()
            .unwrap_or_else(|e| panic!("git is installed and runs: {e}"));
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Create one stash from the visible working tree, including untracked and
    /// not-ignored files, so a test can plant a pre-existing stash.
    pub fn stash_push(&self, message: &str) {
        self.git(&[
            "stash",
            "push",
            "--include-untracked",
            "--quiet",
            "--message",
            message,
        ]);
    }

    /// The stash entries, newest first, each as its object id and reflog name.
    pub fn stash_list(&self) -> String {
        self.git(&["stash", "list", "--format=%H %gd"])
    }

    /// How many stash entries exist.
    pub fn stash_count(&self) -> usize {
        self.stash_list().lines().count()
    }

    /// What the preflight reads: the porcelain status with every untracked
    /// file listed, never a collapsing or filtered view.
    pub fn status(&self) -> String {
        self.git(&["status", "--porcelain", "--untracked-files=all"])
    }

    /// What is staged, so a test can prove the preflight staged nothing.
    pub fn staged(&self) -> String {
        self.git(&["diff", "--cached", "--name-only"])
    }

    /// Keep the repository on disk after this handle is consumed, so a helper
    /// that only hands the run's artifact path back can still be read after the
    /// run. The caller owns the directory and its cleanup.
    pub fn keep(self) {
        std::mem::forget(self);
    }
}

impl Drop for TempRepo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
        let _ = fs::remove_dir_all(&self.origin);
    }
}

/// The local bare origin for a repository at `root`: a sibling directory, so it
/// is never inside the working tree the preflight and the commit inspect.
fn sibling_origin_path(root: &Path) -> PathBuf {
    let name = root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("repository");
    root.with_file_name(format!("{name}-origin.git"))
}

/// A unique temporary directory for one disposable repository. The per-process
/// ordinal keeps two tests that start within one clock tick apart.
fn temp_repo_path() -> PathBuf {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the clock is after the Unix epoch")
        .as_nanos();
    let ordinal = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "ejaj-repository-{}-{}-{}",
        std::process::id(),
        nanos,
        ordinal
    ))
}

// ---------------------------------------------------------------------------
// Recorded exchanges
//
// Every test below has two forms: a replayed unit test that runs offline
// against a committed fixture, and an ignored live test that records the same
// fixture against a real model (EJAJ_LLM_RECORD=1). Replay asserts that the
// request the code builds still matches the one the real model received, so a
// change to a prompt, a tool schema or the transcript shape fails loudly here
// instead of silently drifting away from the provider contract.
// ---------------------------------------------------------------------------

/// A short, one-line rendering of a JSON value for a drift report: the full
/// transcript is far too long to print next to the recorded one.
fn snippet(value: &serde_json::Value) -> String {
    const MAX: usize = 160;
    let text = value.to_string();
    if text.chars().count() <= MAX {
        return text;
    }
    let head: String = text.chars().take(MAX).collect();
    format!("{head}... ({} chars)", text.chars().count())
}

/// One recorded HTTP exchange: the exact request body the agent built and the
/// exact response the provider returned.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Exchange {
    request: serde_json::Value,
    status: i32,
    body: String,
    /// The endpoint this one request went to. Absent means the fixture's
    /// top-level `url`; a hybrid run populates it so each exchange carries the
    /// provider it was recorded against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    /// The model this one request named, for the same reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
}

/// A whole fixture: the endpoint it was recorded against and the ordered
/// exchanges of one test. The model is part of the fixture because the request
/// body carries it and replay compares request bodies.
#[derive(serde::Serialize, serde::Deserialize)]
struct Fixture {
    url: String,
    model: String,
    /// The giga (responses) endpoint, present only in a hybrid fixture. The
    /// field name is the unchanged fixture schema; replay builds the
    /// two-provider config from it, so no environment is needed to replay a
    /// two-provider run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    architect_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    architect_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    recorded_at: Option<String>,
    exchanges: Vec<Exchange>,
}

/// Where the fixture for `name` lives, e.g. `plan_handoff/the_plan_agent_...`.
fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(format!("{name}.json"))
}

/// The exact command that (re)creates a fixture, quoted in every failure that
/// means the recording no longer matches the code.
fn record_hint(name: &str) -> String {
    format!(
        "Re-record it against a live model:\n  \
         EJAJ_TEST_LLM_MODEL=<id> EJAJ_LLM_RECORD=1 cargo test --test {} -- --ignored --test-threads=1\n  \
         (fixture: {})",
        name.split('/').next().unwrap_or(name),
        fixture_path(name).display(),
    )
}

/// Serialise one whole fixture -- the exact bytes a recording writes to disk.
/// Split out from the writer so the recorded URL's place in the layout can be
/// asserted without a network or a file write.
fn fixture_json(
    url: &str,
    model: &str,
    giga_url: Option<&str>,
    giga_model: Option<&str>,
    exchanges: Vec<Exchange>,
) -> String {
    let recorded_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs().to_string())
        .ok();
    let fixture = Fixture {
        url: url.to_string(),
        model: model.to_string(),
        architect_url: giga_url.map(str::to_string),
        architect_model: giga_model.map(str::to_string),
        recorded_at,
        exchanges,
    };
    format!(
        "{}\n",
        serde_json::to_string_pretty(&fixture).expect("the fixture serializes")
    )
}

/// Report the first difference between the request just built and the recorded
/// one, as a JSON-pointer-ish path with both values. Keeps a drift failure
/// readable even though the transcripts are large.
fn first_diff(current: &serde_json::Value, recorded: &serde_json::Value) -> Option<String> {
    fn walk(
        current: &serde_json::Value,
        recorded: &serde_json::Value,
        path: &str,
        out: &mut Option<String>,
    ) {
        if out.is_some() {
            return;
        }
        match (current, recorded) {
            (serde_json::Value::Object(current), serde_json::Value::Object(recorded)) => {
                for (key, value) in current {
                    let child = format!("{path}/{key}");
                    match recorded.get(key) {
                        Some(other) => walk(value, other, &child, out),
                        None => {
                            *out = Some(format!("{child} is new (absent from the recording)"));
                            return;
                        }
                    }
                }
                for key in recorded.keys() {
                    if !current.contains_key(key) {
                        *out = Some(format!("{path}/{key} was recorded but is now absent"));
                        return;
                    }
                }
            }
            (serde_json::Value::Array(current), serde_json::Value::Array(recorded)) => {
                if current.len() != recorded.len() {
                    *out = Some(format!(
                        "{path} has {} items, the recording has {}",
                        current.len(),
                        recorded.len()
                    ));
                    return;
                }
                for (index, (value, other)) in current.iter().zip(recorded).enumerate() {
                    walk(value, other, &format!("{path}/{index}"), out);
                }
            }
            _ => {
                if current != recorded {
                    *out = Some(format!(
                        "{path} is {} now, was {} in the recording",
                        snippet(current),
                        snippet(recorded)
                    ));
                }
            }
        }
    }

    let mut out = None;
    walk(current, recorded, "", &mut out);
    out
}

/// Serve recorded responses in order. Any request the code builds that no
/// longer matches the recording is a hard failure: that is the contract drift
/// this fixture exists to catch.
struct ReplayTransport {
    name: String,
    /// The endpoint the fixture was recorded against, used for any exchange
    /// that does not carry its own `url`. A request built for a different URL
    /// must fail before its body is even compared.
    url: String,
    exchanges: Vec<Exchange>,
    next: Mutex<usize>,
}

impl ReplayTransport {
    fn new(
        name: impl Into<String>,
        url: impl Into<String>,
        exchanges: Vec<Exchange>,
    ) -> ReplayTransport {
        ReplayTransport {
            name: name.into(),
            url: url.into(),
            exchanges,
            next: Mutex::new(0),
        }
    }

    /// Every recorded exchange must have been consumed: a loop that makes fewer
    /// requests than it used to is a behaviour change too.
    fn assert_exhausted(&self) {
        let used = *self.next.lock().unwrap();
        assert_eq!(
            used,
            self.exchanges.len(),
            "the run made {used} requests, the fixture records {}; {}",
            self.exchanges.len(),
            record_hint(&self.name)
        );
    }
}

impl Transport for ReplayTransport {
    fn send(&self, outgoing: &api::Outgoing) -> Result<RawResponse, CallError> {
        let mut next = self.next.lock().unwrap();
        let index = *next;
        let Some(exchange) = self.exchanges.get(index) else {
            panic!(
                "the run made more than {} requests (exchange {} has no recording); {}",
                self.exchanges.len(),
                index + 1,
                record_hint(&self.name)
            );
        };
        *next += 1;

        // Where the request went is checked before what it carried: serving an
        // exchange recorded elsewhere would silently replay another provider's
        // fixture. A hybrid fixture names the endpoint on each exchange;
        // single-provider fixtures omit it and fall back to the top-level URL.
        let expected_url = exchange.url.as_deref().unwrap_or(&self.url);
        if outgoing.url != expected_url {
            panic!(
                "the request was sent to {} but fixture {} was recorded against {}; {}",
                outgoing.url,
                self.name,
                expected_url,
                record_hint(&self.name)
            );
        }

        if let Some(diff) = first_diff(&outgoing.body, &exchange.request) {
            panic!(
                "request drift at exchange {}: {diff}\n{}",
                index + 1,
                record_hint(&self.name)
            );
        }

        Ok(RawResponse {
            status: exchange.status,
            body: exchange.body.clone(),
        })
    }
}

/// Forward to the real provider and remember every exchange, so the suite can
/// be replayed offline afterwards. Each exchange is staged to `<name>.json.tmp`
/// as it arrives, so a run that dies mid-way still leaves a complete record of
/// what it saw; the `Harness` moves the staged file into `tests/fixtures/` on a
/// viable terminal step, or deletes it.
struct RecordingTransport {
    inner: api::HttpTransport,
    exchanges: Mutex<Vec<Exchange>>,
    /// The fixture name, and the coordinates the fixture names at its top
    /// level: the caller knows which endpoint is the mid and which is the
    /// giga, and every exchange carries its own URL and model besides.
    name: String,
    url: String,
    model: String,
    giga_url: Option<String>,
    giga_model: Option<String>,
    staged: PathBuf,
}

impl RecordingTransport {
    fn new(
        name: &str,
        url: String,
        model: String,
        giga_url: Option<String>,
        giga_model: Option<String>,
    ) -> RecordingTransport {
        RecordingTransport {
            inner: api::HttpTransport,
            exchanges: Mutex::new(Vec::new()),
            name: name.to_string(),
            url,
            model,
            giga_url,
            giga_model,
            staged: staged_path(name),
        }
    }

    /// Remember one outgoing request and the response it produced. Split out
    /// from `send` so the bookkeeping can be exercised without a network. The
    /// exchange keeps the URL it went to and the model its body named, which is
    /// what lets one fixture describe a two-provider run.
    fn record(&self, outgoing: &api::Outgoing, response: &RawResponse) {
        let model = outgoing
            .body
            .get("model")
            .and_then(|value| value.as_str())
            .map(str::to_string);
        self.exchanges.lock().unwrap().push(Exchange {
            request: outgoing.body.clone(),
            status: response.status,
            body: response.body.clone(),
            url: Some(outgoing.url.clone()),
            model,
        });
    }

    /// The fixture these exchanges serialise to, through the shared
    /// fixture-writing path.
    fn fixture_json(&self) -> String {
        let exchanges = self.exchanges.lock().unwrap().clone();
        fixture_json(
            &self.url,
            &self.model,
            self.giga_url.as_deref(),
            self.giga_model.as_deref(),
            exchanges,
        )
    }

    /// Where the recording is staged until the run reaches a terminal step.
    fn staged_path(&self) -> &Path {
        &self.staged
    }

    /// Write every exchange so far to the staged path. Called after each
    /// response, so a run that never returns still leaves its exchanges behind.
    fn flush(&self) {
        if let Some(parent) = self.staged.parent() {
            fs::create_dir_all(parent).expect("the fixtures directory is writable");
        }
        fs::write(&self.staged, self.fixture_json()).expect("the staged fixture is writable");
    }

    /// Move the staged recording onto the committed fixture path.
    fn commit(&self) {
        let target = fixture_path(&self.name);
        fs::rename(&self.staged, &target)
            .unwrap_or_else(|e| panic!("could not commit the recording {}: {e}", target.display()));
        eprintln!(
            "[EJAJ] recorded {} exchange(s) to {}",
            self.exchanges.lock().unwrap().len(),
            target.display()
        );
    }
}

/// Where a fixture is staged while it is being recorded: the committed path
/// with a `.tmp` suffix, so the move onto it is atomic on the same filesystem.
fn staged_path(name: &str) -> PathBuf {
    fixture_path(name).with_extension("json.tmp")
}

impl Transport for RecordingTransport {
    fn send(&self, outgoing: &api::Outgoing) -> Result<RawResponse, CallError> {
        let response = self.inner.send(outgoing)?;
        self.record(outgoing, &response);
        self.flush();
        Ok(response)
    }
}

/// The lifetime of one test's model connection. In replay mode it checks the
/// fixture was fully consumed; in record mode it commits the staged fixture on
/// a viable terminal step and discards it otherwise, so `tests/fixtures/` never
/// holds a partial or unusable recording.
pub struct Harness {
    config: Config,
    replay: Option<Arc<ReplayTransport>>,
    recorder: Option<Arc<RecordingTransport>>,
}

impl Harness {
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Whether this harness is recording instead of asserting a replay. A
    /// recording run reports its assertions as warnings and commits on the
    /// terminal step; a replay asserts as always.
    pub fn is_recording(&self) -> bool {
        self.recorder.is_some()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        if std::thread::panicking() {
            // The run reached no viable terminal step: discard the staged
            // recording rather than commit a fixture replay cannot consume.
            if let Some(recorder) = &self.recorder {
                let _ = fs::remove_file(recorder.staged_path());
            }
            return;
        }
        if let Some(replay) = &self.replay {
            replay.assert_exhausted();
        }
        if let Some(recorder) = &self.recorder {
            recorder.commit();
        }
    }
}

/// Read a fixture, or fail with the exact command that records it.
fn read_fixture(name: &str) -> Fixture {
    let path = fixture_path(name);
    let raw = fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "missing fixture {}: {e}\n{}",
            path.display(),
            record_hint(name)
        )
    });
    let fixture: Fixture = serde_json::from_str(&raw)
        .unwrap_or_else(|e| panic!("invalid fixture {}: {e}", path.display()));
    assert_local_fixture(name, &fixture);
    fixture
}

/// The local coordinates a committed fixture must name. A hosted recording is a
/// mistake, and this is the one function every replay reads through, so it is
/// where that mistake is caught (see `tests/llm.env`).
fn assert_local_fixture(name: &str, fixture: &Fixture) {
    assert_eq!(
        fixture.url,
        fixture_url(),
        "fixture {name} was recorded against {} instead of the local mid endpoint; {}",
        fixture.url,
        record_hint(name)
    );
    assert_eq!(
        fixture.model,
        fixture_model(),
        "fixture {name} names the model `{}` instead of the local `{}`; {}",
        fixture.model,
        fixture_model(),
        record_hint(name)
    );
    if fixture.architect_url.is_some() || fixture.architect_model.is_some() {
        assert_eq!(
            fixture.architect_url.as_deref(),
            Some(fixture_giga_url().as_str()),
            "fixture {name} was recorded against the wrong giga endpoint; {}",
            record_hint(name)
        );
        assert_eq!(
            fixture.architect_model.as_deref(),
            Some(fixture_giga_model().as_str()),
            "fixture {name} names the wrong giga model; {}",
            record_hint(name)
        );
    }
}

/// Replay a committed fixture: no network, no model, deterministic.
pub fn replay(name: &str) -> Harness {
    let fixture = read_fixture(name);
    let url = fixture.url.clone();
    let model = fixture.model.clone();
    let replay = Arc::new(ReplayTransport::new(name, url.clone(), fixture.exchanges));
    let config = Config::new(url.clone(), model.clone(), Some(TEST_API_KEY.to_string()))
        .with_transport(replay.clone());
    Harness {
        config,
        replay: Some(replay),
        recorder: None,
    }
}

/// Replay a two-provider fixture: the mid block comes from the top-level
/// `url`/`model`, the giga block from the fixture's `architect_url`/
/// `architect_model` fields, and every phase runs through the hybrid routing.
/// No environment is read, so a two-provider run replays offline like every
/// other fixture.
pub fn replay_hybrid(name: &str) -> Harness {
    let fixture = read_fixture(name);
    let giga_url = fixture.architect_url.clone().unwrap_or_else(|| {
        panic!(
            "fixture {} has no architect_url; it is not a two-provider recording\n{}",
            fixture_path(name).display(),
            record_hint(name)
        )
    });
    let giga_model = fixture.architect_model.clone().unwrap_or_else(|| {
        panic!(
            "fixture {} has no architect_model; it is not a two-provider recording\n{}",
            fixture_path(name).display(),
            record_hint(name)
        )
    });
    let replay = Arc::new(ReplayTransport::new(
        name,
        fixture.url.clone(),
        fixture.exchanges,
    ));
    let config = Config::new(
        fixture.url.clone(),
        fixture.model.clone(),
        Some(TEST_API_KEY.to_string()),
    )
    .with_giga(
        giga_url.clone(),
        giga_model.clone(),
        Some(TEST_API_KEY.to_string()),
    )
    .with_mode(Mode::Hybrid)
    .with_transport(replay.clone());
    Harness {
        config,
        replay: Some(replay),
        recorder: None,
    }
}

/// `EJAJ_TEST_ARCHITECT_URL` (environment or `tests/llm.env`). No compiled
/// fallback: a two-provider run must not silently point the giga provider at the
/// mid endpoint, where a responses body would be rejected.
pub fn test_giga_url() -> String {
    resolve("EJAJ_TEST_ARCHITECT_URL").unwrap_or_else(|| {
        panic!(
            "EJAJ_TEST_ARCHITECT_URL is set in neither the environment nor tests/llm.env.\n\
             Add it to tests/llm.env or pass it in the environment; it must be a full \
             `/v1/responses` URL."
        )
    })
}

/// `EJAJ_TEST_ARCHITECT_MODEL` (environment or `tests/llm.env`).
pub fn test_giga_model() -> String {
    resolve("EJAJ_TEST_ARCHITECT_MODEL").unwrap_or_else(|| {
        panic!(
            "EJAJ_TEST_ARCHITECT_MODEL is set in neither the environment nor tests/llm.env.\n\
             Add it to tests/llm.env or pass it in the environment."
        )
    })
}

/// `EJAJ_TEST_ARCHITECT_API_KEY`, falling back to the mid key so a keyless
/// giga endpoint needs no second variable.
pub fn test_giga_api_key() -> String {
    resolve("EJAJ_TEST_ARCHITECT_API_KEY").unwrap_or_else(test_api_key)
}

/// The record flag: any non-blank value other than `0` turns recording on.
fn recording_enabled() -> bool {
    std::env::var("EJAJ_LLM_RECORD")
        .map(|v| !v.trim().is_empty() && v.trim() != "0")
        .unwrap_or(false)
}

/// Run against the live mid endpoint. With `EJAJ_LLM_RECORD` set the
/// exchanges are staged and committed on the terminal step; without it the run
/// is a plain live check.
pub fn live(name: &str) -> Harness {
    let config = local_config();
    let url = test_url();
    let model = test_model();

    if recording_enabled() {
        assert_records_the_local_default(&["EJAJ_TEST_LLM_URL", "EJAJ_TEST_LLM_MODEL"]);
        let recorder = Arc::new(RecordingTransport::new(
            name,
            url.clone(),
            model.clone(),
            None,
            None,
        ));
        let config = config.with_transport(recorder.clone());
        Harness {
            config,
            replay: None,
            recorder: Some(recorder),
        }
    } else {
        Harness {
            config,
            replay: None,
            recorder: None,
        }
    }
}

/// Run the two-provider pipeline against the live endpoints. The mid block
/// comes from `EJAJ_TEST_LLM_*`, the giga block from
/// `EJAJ_TEST_ARCHITECT_*` (a test-only namespace), and the mode is `hybrid`.
/// Recording writes both coordinate pairs into the fixture, so replay needs no
/// environment.
pub fn live_hybrid(name: &str) -> Harness {
    assert_model_available();
    let url = test_url();
    let model = test_model();
    let giga_url = test_giga_url();
    let giga_model = test_giga_model();
    let config = Config::new(url.clone(), model.clone(), Some(test_api_key()))
        .with_giga(
            giga_url.clone(),
            giga_model.clone(),
            Some(test_giga_api_key()),
        )
        .with_mode(Mode::Hybrid);

    if recording_enabled() {
        assert_records_the_local_default(&[
            "EJAJ_TEST_LLM_URL",
            "EJAJ_TEST_LLM_MODEL",
            "EJAJ_TEST_ARCHITECT_URL",
            "EJAJ_TEST_ARCHITECT_MODEL",
        ]);
        let recorder = Arc::new(RecordingTransport::new(
            name,
            url.clone(),
            model.clone(),
            Some(giga_url.clone()),
            Some(giga_model.clone()),
        ));
        let config = config.with_transport(recorder.clone());
        Harness {
            config,
            replay: None,
            recorder: Some(recorder),
        }
    } else {
        Harness {
            config,
            replay: None,
            recorder: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::{AssertUnwindSafe, catch_unwind};

    fn outgoing(url: &str, body: serde_json::Value) -> api::Outgoing {
        api::Outgoing {
            url: url.to_string(),
            auth: api::Auth::None,
            body,
        }
    }

    fn response() -> RawResponse {
        RawResponse {
            status: 200,
            body: "{}".to_string(),
        }
    }

    fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
        if let Some(text) = payload.downcast_ref::<&str>() {
            text.to_string()
        } else if let Some(text) = payload.downcast_ref::<String>() {
            text.clone()
        } else {
            String::new()
        }
    }

    /// A request whose body matches the recording but whose URL does not must
    /// fail loudly: serving the exchange would let a provider wired to the
    /// wrong endpoint pass on another provider's recording.
    #[test]
    fn replay_rejects_a_matching_body_at_the_wrong_url() {
        let recorded_url = "http://recorded.example/v1/chat/completions";
        let wrong_url = "http://wrong.example/v1/chat/completions";
        let exchange = Exchange {
            request: serde_json::json!({ "model": "test-model" }),
            status: 200,
            body: "{}".to_string(),
            url: None,
            model: None,
        };
        let transport = ReplayTransport::new(
            "llm_wire/a_plain_chat_round_trips_through_the_typed_api",
            recorded_url,
            vec![exchange.clone()],
        );

        let result = catch_unwind(AssertUnwindSafe(|| {
            transport.send(&outgoing(wrong_url, exchange.request.clone()))
        }));

        let Err(payload) = result else {
            panic!("a mismatched URL must panic");
        };
        let message = panic_message(payload);
        assert!(
            message.contains(wrong_url),
            "the message must name the URL the request went to: {message}"
        );
        assert!(
            message.contains(recorded_url),
            "the message must name the URL the fixture was recorded against: {message}"
        );
        assert!(
            message.contains("Re-record it against a live model"),
            "the message must carry the re-record hint: {message}"
        );
        assert!(
            !message.contains("request drift"),
            "a URL mismatch is not request drift: {message}"
        );
    }

    /// Each recorded exchange carries the endpoint it went to and the model its
    /// body named, so one fixture can describe a two-provider run; the caller's
    /// coordinates become the fixture's top-level defaults.
    #[test]
    fn recording_transport_stamps_each_exchange_with_its_endpoint() {
        let recorder = RecordingTransport::new(
            "llm_wire/a_plain_chat_round_trips_through_the_typed_api",
            "http://last.example/v1/responses".to_string(),
            "mid-model".to_string(),
            Some("http://first.example/v1/responses".to_string()),
            Some("giga-model".to_string()),
        );
        let first = outgoing(
            "http://first.example/v1/chat/completions",
            serde_json::json!({ "model": "first-model", "n": 1 }),
        );
        let last = outgoing(
            "http://last.example/v1/responses",
            serde_json::json!({ "model": "last-model", "n": 2 }),
        );

        recorder.record(&first, &response());
        recorder.record(&last, &response());

        // No network is touched: the fixture bytes are produced by the shared
        // writing path and decoded straight back.
        let fixture: Fixture = serde_json::from_str(&recorder.fixture_json()).unwrap();
        assert_eq!(fixture.url, last.url);
        assert_eq!(fixture.model, "mid-model");
        assert_eq!(
            fixture.architect_url.as_deref(),
            Some("http://first.example/v1/responses")
        );
        assert_eq!(fixture.architect_model.as_deref(), Some("giga-model"));
        assert_eq!(fixture.exchanges.len(), 2);
        assert_eq!(
            fixture.exchanges[0].url.as_deref(),
            Some(first.url.as_str())
        );
        assert_eq!(fixture.exchanges[0].model.as_deref(), Some("first-model"));
        assert_eq!(fixture.exchanges[1].url.as_deref(), Some(last.url.as_str()));
        assert_eq!(fixture.exchanges[1].model.as_deref(), Some("last-model"));
    }

    /// `failed` is the one hard failure: `record_or_verify` re-raises it, so a
    /// recording that reached no usable terminal step is discarded, while an
    /// ordinary assertion is only a warning and the recording still commits.
    #[test]
    fn failed_is_the_only_hard_failure() {
        let hard = catch_unwind(AssertUnwindSafe(|| failed("the ticket directory", "gone")))
            .expect_err("`failed` must panic");
        assert!(
            is_hard_failure(hard.as_ref()),
            "a `failed` panic must be hard"
        );

        let soft = catch_unwind(AssertUnwindSafe(|| panic!("a broken artifact")))
            .expect_err("the panic must occur");
        assert!(
            !is_hard_failure(soft.as_ref()),
            "an ordinary assertion must be soft"
        );
    }
}
