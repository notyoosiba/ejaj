//! The wire boundary: the provider layer's request for a phase must be accepted
//! by a real OpenAI-compatible server, and its reply must decode into the typed
//! `Message` the agents consume.
//!
//! Every test comes in two forms: the default replays a committed fixture with
//! no network, and the `live_` twin (ignored) runs against a real model --
//! recording the fixture when `EJAJ_LLM_RECORD=1` is set. See `tests/common/mod.rs`.

mod common;

use ejaj::Config;
use ejaj::api::{Message, Role};
use ejaj::tools::Phase;

/// The binary's own request path: the provider selected for the phase prepares
/// the body, the configured transport sends it and decodes the reply into a
/// typed `Message`.
fn run_a_plain_chat_round_trip(config: &Config) {
    let messages = vec![
        Message::system("You are a terse assistant."),
        Message::user("Reply with exactly one word: pong"),
    ];
    let message = common::ask(config, &messages, Phase::Plan);
    assert_eq!(message.role, Role::Assistant);
    let content = message.content.unwrap_or_default().to_lowercase();
    assert!(content.contains("pong"), "unexpected reply: {content}");
}

#[test]
fn a_plain_chat_round_trips_through_the_typed_api() {
    let harness = common::replay("llm_wire/a_plain_chat_round_trips_through_the_typed_api");
    run_a_plain_chat_round_trip(harness.config());
}

#[test]
#[ignore = "requires a local LM Studio model; EJAJ_LLM_RECORD=1 refreshes the fixture"]
fn live_a_plain_chat_round_trips_through_the_typed_api() {
    let harness = common::live("llm_wire/a_plain_chat_round_trips_through_the_typed_api");
    common::record_or_verify(&harness, || run_a_plain_chat_round_trip(harness.config()));
}

/// The tool-calling wire boundary: tool schemas generated from Rust argument
/// types must be accepted by the provider, the model's tool call must decode
/// into a typed tool call, the tool result must be accepted by the provider, and
/// the turn must complete.
fn run_a_tool_call_round_trip(config: &Config) {
    let session = ejaj::loops::code::Session::new(config, common::scope());
    match session.run_ticket(common::CODE_TICKET) {
        ejaj::loops::code::Outcome::Complete => {}
        ejaj::loops::code::Outcome::TurnEnd => common::failed(
            "the tool-call round trip",
            "the model ended its turn without calling `task_complete`",
        ),
        ejaj::loops::code::Outcome::Failed(kind) => {
            common::failed("the tool-call round trip", kind.label())
        }
    }
}

#[test]
fn a_tool_call_round_trips_through_the_typed_api() {
    let harness = common::replay("llm_wire/a_tool_call_round_trips_through_the_typed_api");
    run_a_tool_call_round_trip(harness.config());
}

#[test]
#[ignore = "requires a local LM Studio model; EJAJ_LLM_RECORD=1 refreshes the fixture"]
fn live_a_tool_call_round_trips_through_the_typed_api() {
    let harness = common::live("llm_wire/a_tool_call_round_trips_through_the_typed_api");
    common::record_or_verify(&harness, || run_a_tool_call_round_trip(harness.config()));
}

/// The `execute_script` wire boundary: the code agent runs a JavaScript program
/// that writes and reads a scratch file inside the project root, receives the
/// JSON-serialized return value, and finishes the ticket with `task_complete`.
fn run_an_execute_script_round_trip(config: &Config) {
    let _ = std::fs::remove_file(common::SCRIPT_SCRATCH_PATH);
    let session = ejaj::loops::code::Session::new(config, common::scope());
    let outcome = session.run_ticket(common::SCRIPT_TICKET);
    let written = std::fs::read_to_string(common::SCRIPT_SCRATCH_PATH);
    let _ = std::fs::remove_file(common::SCRIPT_SCRATCH_PATH);

    match outcome {
        ejaj::loops::code::Outcome::Complete => {}
        ejaj::loops::code::Outcome::TurnEnd => common::failed(
            "the execute_script round trip",
            "the model ended its turn without calling `task_complete`",
        ),
        ejaj::loops::code::Outcome::Failed(kind) => {
            common::failed("the execute_script round trip", kind.label())
        }
    }

    assert_eq!(
        written.as_deref().ok(),
        Some("ejaj-script-ok"),
        "the execute_script round trip did not write the expected scratch file"
    );
}

#[test]
fn an_execute_script_call_round_trips_through_the_typed_api() {
    let harness =
        common::replay("llm_wire/an_execute_script_call_round_trips_through_the_typed_api");
    run_an_execute_script_round_trip(harness.config());
}

#[test]
#[ignore = "requires a local LM Studio model; EJAJ_LLM_RECORD=1 refreshes the fixture"]
fn live_an_execute_script_call_round_trips_through_the_typed_api() {
    let harness = common::live("llm_wire/an_execute_script_call_round_trips_through_the_typed_api");
    common::record_or_verify(&harness, || {
        run_an_execute_script_round_trip(harness.config())
    });
}
