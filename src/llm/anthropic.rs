//! The native Anthropic Messages provider.
//!
//! The loop's transcript is chat-shaped; [`AnthropicProvider::prepare`]
//! projects it into the Messages API's native shape -- system text in the
//! top-level `system` field, user/assistant turns with content blocks, tool
//! calls as `tool_use` blocks and tool results as user `tool_result` blocks --
//! and [`AnthropicProvider::decode`] turns the wire's content blocks back into
//! the canonical [`Message`] the loop replays. Both operations are pure: no
//! network, environment or clock enters this module.

use serde_json::{Map, Value, json};

use crate::api::{CallError, FunctionCall, Message, Outgoing, RawResponse, Role, ToolCall};
use crate::llm::{LlmProvider, Prepared, factory};
use crate::tools::{Phase, Tool};

/// The fixed output budget every non-Summary request declares. Anthropic
/// requires an explicit integer, and the agent pins the native default rather
/// than making it configurable.
const MAX_TOKENS: u64 = 4096;

/// The fixed output budget every Summary request declares. The summary pass
/// reports on a whole run in one response, so its budget is larger than every
/// other phase's. Like the other budget it is a fixed code constant, never
/// configuration.
const SUMMARY_MAX_TOKENS: u64 = 16384;

/// The reasoning effort every Summary request asks for. It is fixed: the
/// summary pass is the one read-only pass that reports on the whole run, so it
/// is worth the deeper reasoning, and nothing configures it. Non-Summary
/// requests never carry an effort.
const SUMMARY_EFFORT: &str = "high";

/// The fixed top-level output budget a phase declares. Both preparation
/// paths select it through this one rule, so an ordinary request and its
/// tool-free twin always agree on the budget.
fn max_tokens(phase: Phase) -> u64 {
    match phase {
        Phase::Summary => SUMMARY_MAX_TOKENS,
        _ => MAX_TOKENS,
    }
}

/// A provider that speaks Anthropic's native Messages wire format.
pub struct AnthropicProvider {
    settings: factory::SmollSettings,
}

impl AnthropicProvider {
    pub fn new(settings: factory::SmollSettings) -> AnthropicProvider {
        AnthropicProvider { settings }
    }

    /// Build one native Messages request with exactly `tools` advertised. The
    /// ordinary `prepare` passes the phase's native tool list; the
    /// compaction-only `prepare_tool_free` passes an empty list, which is this
    /// wire's own "advertise no tools" form.
    fn prepare_with_tools(
        &self,
        messages: &[Message],
        phase: Phase,
        tools: Vec<Value>,
    ) -> Prepared {
        let system = system_text(messages);
        let native_messages = project(messages);
        let context_bytes = context_bytes(&system, &native_messages);

        let mut body = Map::new();
        body.insert(
            "model".to_string(),
            Value::String(self.settings.model.clone()),
        );
        body.insert("max_tokens".to_string(), json!(max_tokens(phase)));
        if let Some(system) = &system {
            body.insert("system".to_string(), Value::String(system.clone()));
        }
        body.insert("messages".to_string(), Value::Array(native_messages));
        body.insert("tools".to_string(), Value::Array(tools));
        // Only the Summary pass asks for reasoning effort, and it asks for the
        // fixed high effort as a nested `output_config.effort` string. Every
        // other phase keeps the unchanged body shape.
        if phase == Phase::Summary {
            body.insert(
                "output_config".to_string(),
                json!({ "effort": SUMMARY_EFFORT }),
            );
        }

        Prepared {
            outgoing: Outgoing {
                url: self.settings.url.clone(),
                auth: self.settings.auth(),
                body: Value::Object(body),
            },
            context_bytes,
        }
    }
}

impl LlmProvider for AnthropicProvider {
    fn name(&self) -> &'static str {
        "anthropic-messages"
    }

    fn prepare(&self, messages: &[Message], phase: Phase) -> Prepared {
        self.prepare_with_tools(messages, phase, native_tools(phase))
    }

    fn prepare_tool_free(&self, messages: &[Message], phase: Phase) -> Prepared {
        self.prepare_with_tools(messages, phase, Vec::new())
    }

    fn decode(&self, raw: RawResponse) -> Result<Message, CallError> {
        let status = raw.status;
        let success = (200..300).contains(&status);

        // Parse to a value first: a body that is not a JSON object is an
        // invalid response on any status. Only a JSON object can carry the
        // nested error or the content blocks the provider decodes.
        let value: Value = match serde_json::from_str(&raw.body) {
            Ok(value) => value,
            Err(error) => return Err(invalid_response(status, &error)),
        };
        let Some(object) = value.as_object() else {
            let error: serde_json::Error =
                serde::de::Error::custom("invalid type: expected a JSON object");
            return Err(invalid_response(status, &error));
        };

        // A nested error object is an API error on either status: Anthropic
        // can report an overload inside a 2xx body.
        if let Some(message) = nested_error_message(object) {
            return Err(CallError::Api { status, message });
        }
        if !success {
            return Err(CallError::Api {
                status,
                message: raw.body,
            });
        }

        let blocks = match object.get("content") {
            Some(Value::Array(blocks)) => blocks,
            Some(_) => return Err(malformed("the response content was not an array")),
            None => return Err(no_usable_content(object)),
        };

        let mut text = String::new();
        let mut calls = Vec::new();
        for block in blocks {
            let Some(block) = block.as_object() else {
                continue;
            };
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(chunk) = block.get("text").and_then(Value::as_str) {
                        text.push_str(chunk);
                    }
                }
                Some("tool_use") => {
                    if let Some(call) = tool_call_from_block(block) {
                        calls.push(call);
                    }
                }
                // Thinking, redacted thinking and block types the loop does
                // not model carry nothing it can replay.
                _ => {}
            }
        }

        let content = (!text.is_empty()).then_some(text);
        let tool_calls = (!calls.is_empty()).then_some(calls);
        if content.is_none() && tool_calls.is_none() {
            return Err(no_usable_content(object));
        }

        Ok(Message {
            role: Role::Assistant,
            content,
            tool_calls,
            tool_call_id: None,
            reasoning_content: None,
            extra: Map::new(),
        })
    }
}

/// Concatenate every system turn into the one native `system` field. The
/// field is absent when the transcript carries no system turn.
fn system_text(messages: &[Message]) -> Option<String> {
    let parts: Vec<&str> = messages
        .iter()
        .filter(|message| message.role == Role::System)
        .map(|message| message.content.as_deref().unwrap_or(""))
        .collect();
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n\n"))
    }
}

/// Project the chat-shaped transcript into native messages, in order. System
/// turns leave this array for the top-level `system` field; assistant tool
/// calls become `tool_use` blocks and tool results become user `tool_result`
/// blocks. Adjacent turns with the same native role are coalesced so the wire
/// never carries consecutive identical roles.
fn project(messages: &[Message]) -> Vec<Value> {
    let mut native = Vec::new();

    for message in messages {
        match message.role {
            Role::System => {}
            Role::User => push_blocks(
                &mut native,
                "user",
                vec![text_block(message.content.as_deref().unwrap_or(""))],
            ),
            Role::Assistant => {
                let mut blocks = Vec::new();
                if let Some(text) = message.content.as_deref()
                    && !text.is_empty()
                {
                    blocks.push(text_block(text));
                }
                if let Some(calls) = &message.tool_calls {
                    for call in calls {
                        blocks.push(tool_use_block(call));
                    }
                }
                if !blocks.is_empty() {
                    push_blocks(&mut native, "assistant", blocks);
                }
            }
            Role::Tool => push_blocks(
                &mut native,
                "user",
                vec![json!({
                    "type": "tool_result",
                    "tool_use_id": message.tool_call_id.as_deref().unwrap_or(""),
                    "content": message.content.as_deref().unwrap_or(""),
                })],
            ),
        }
    }

    native
}

/// Append blocks to the last native message when it already has `role`,
/// otherwise start a new native message. This is the one coalescing rule, so
/// its invariant holds everywhere a source turn projects.
fn push_blocks(native: &mut Vec<Value>, role: &str, blocks: Vec<Value>) {
    if let Some(last) = native.last_mut()
        && last.get("role").and_then(Value::as_str) == Some(role)
        && let Some(content) = last.get_mut("content").and_then(Value::as_array_mut)
    {
        content.extend(blocks);
        return;
    }
    native.push(json!({ "role": role, "content": blocks }));
}

fn text_block(text: &str) -> Value {
    json!({ "type": "text", "text": text })
}

/// A tool call becomes a `tool_use` block whose `input` is the call's JSON
/// arguments parsed back into an object. Arguments that are not JSON degrade
/// to an empty object rather than panicking.
fn tool_use_block(call: &ToolCall) -> Value {
    let input = serde_json::from_str(&call.function.arguments).unwrap_or_else(|_| json!({}));
    json!({
        "type": "tool_use",
        "id": call.id,
        "name": call.function.name,
        "input": input,
    })
}

/// The phase's tool definitions in Anthropic's native shape:
/// `name`, `description` and the generated `input_schema`, with no OpenAI
/// `function` wrapper.
fn native_tools(phase: Phase) -> Vec<Value> {
    Tool::definitions(phase)
        .into_iter()
        .map(|definition| {
            json!({
                "name": definition.function.name,
                "description": definition.function.description,
                "input_schema": serde_json::to_value(&definition.function.parameters)
                    .unwrap_or(Value::Null),
            })
        })
        .collect()
}

/// The encoded context the request carries: the system field and the native
/// messages, excluding the model name and the tool schemas.
fn context_bytes(system: &Option<String>, messages: &[Value]) -> usize {
    let mut context = Map::new();
    if let Some(system) = system {
        context.insert("system".to_string(), Value::String(system.clone()));
    }
    context.insert("messages".to_string(), Value::Array(messages.to_vec()));
    serde_json::to_string(&Value::Object(context))
        .map(|encoded| encoded.len())
        .unwrap_or(0)
}

/// The nested `error.message` of an Anthropic error body, if it has one.
fn nested_error_message(object: &Map<String, Value>) -> Option<String> {
    match object.get("error")? {
        Value::Object(error) => error
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_string),
        Value::String(text) => Some(text.clone()),
        _ => None,
    }
}

/// A usable `tool_use` block has a non-empty id and name; anything else is a
/// degenerate block the loop must ignore rather than replay with blank fields.
fn tool_call_from_block(block: &Map<String, Value>) -> Option<ToolCall> {
    let id = block.get("id").and_then(Value::as_str)?;
    let name = block.get("name").and_then(Value::as_str)?;
    if id.is_empty() || name.is_empty() {
        return None;
    }
    let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
    let arguments = serde_json::to_string(&input).unwrap_or_else(|_| "{}".to_string());
    Some(ToolCall {
        id: id.to_string(),
        kind: "function".to_string(),
        function: FunctionCall {
            name: name.to_string(),
            arguments,
        },
    })
}

/// The invalid-response shape, matching the other providers' wording.
fn invalid_response(status: i32, error: &serde_json::Error) -> CallError {
    CallError::Transport(format!(
        "invalid JSON response (HTTP {}): {}",
        status, error
    ))
}

fn malformed(detail: &str) -> CallError {
    CallError::Malformed(detail.to_string())
}

/// The one malformed-response message shared by a missing content array and an
/// array that yields neither text nor tool calls.
///
/// When the parsed response carries safe structural metadata -- a usable stop
/// reason and/or an array of content blocks -- the base detail gains a compact,
/// JSON-escaped suffix in fixed `stop_reason` then `content_types` order.
/// Only the already-validated scalar values are echoed; response text,
/// reasoning, tool inputs, identifiers and the raw body never enter it.
fn no_usable_content(object: &Map<String, Value>) -> CallError {
    let mut metadata = Vec::new();
    if let Some(stop_reason) = usable_metadata_string(object.get("stop_reason"))
        && let Ok(encoded) = serde_json::to_string(stop_reason)
    {
        metadata.push(format!("stop_reason={encoded}"));
    }
    if let Some(blocks) = object.get("content").and_then(Value::as_array) {
        let types: Vec<&str> = blocks
            .iter()
            .filter_map(Value::as_object)
            .filter_map(|block| usable_metadata_string(block.get("type")))
            .collect();
        if let Ok(encoded) = serde_json::to_string(&types) {
            metadata.push(format!("content_types={encoded}"));
        }
    }

    let base = "the response carried no usable content";
    if metadata.is_empty() {
        malformed(base)
    } else {
        malformed(&format!("{base} ({})", metadata.join("; ")))
    }
}

/// A metadata value is emitted only when it is a string with some non-whitespace
/// content. The value itself is preserved verbatim, never trimmed.
fn usable_metadata_string(value: Option<&Value>) -> Option<&str> {
    value
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Auth, CallError, FunctionCall, Message, RawResponse, Role, ToolCall};
    use crate::llm::factory::SmollSettings;
    use crate::retry::should_retry;
    use crate::tools::{Phase, Tool};
    use serde_json::{Map, Value, json};
    use std::time::Duration;

    /// A provider whose transport coordinates are fixed for the assertion.
    fn provider() -> AnthropicProvider {
        AnthropicProvider::new(
            SmollSettings::new("http://smoll.test/v1/messages", "smoll-model", "smoll-key")
                .expect("a non-blank key configures the provider"),
        )
    }

    fn raw(status: i32, body: &str) -> RawResponse {
        RawResponse {
            status,
            body: body.to_string(),
        }
    }

    fn decoded(provider: &AnthropicProvider, raw: RawResponse) -> Message {
        match provider.decode(raw) {
            Ok(message) => message,
            Err(error) => panic!("expected a decoded message, got {}", error),
        }
    }

    fn decode_error(provider: &AnthropicProvider, raw: RawResponse) -> CallError {
        match provider.decode(raw) {
            Ok(_) => panic!("expected a decode error"),
            Err(error) => error,
        }
    }

    /// The request body's keys, sorted, for the no-other-key assertion.
    fn body_keys(body: &Value) -> Vec<String> {
        let mut keys: Vec<String> = body
            .as_object()
            .expect("the prepared body is a JSON object")
            .keys()
            .cloned()
            .collect();
        keys.sort();
        keys
    }

    /// The one nested effort object every Summary request must carry: an
    /// object with exactly one member whose `effort` is the string `high`,
    /// never a top-level field and never any other value.
    fn assert_high_effort(body: &Value) {
        assert!(
            body.get("effort").is_none(),
            "effort must be nested under output_config, not top-level"
        );
        let config = body
            .get("output_config")
            .expect("the summary body carries an output_config object")
            .as_object()
            .expect("output_config is a JSON object");
        assert_eq!(
            config.len(),
            1,
            "output_config carries exactly the effort member"
        );
        assert_eq!(config.get("effort"), Some(&json!("high")));
        assert_eq!(
            config.get("effort").and_then(Value::as_str),
            Some("high"),
            "effort is the JSON string high"
        );
    }

    /// Assert a prepared body declares the given expected value as its top-level output
    /// budget and that the value is a bare JSON integer -- never a string, a
    /// float, null, an array or an object.
    fn assert_integer_budget(body: &Value, expected: u64) {
        let budget = body
            .get("max_tokens")
            .expect("the prepared body declares a top-level max_tokens");
        assert_eq!(budget, &json!(expected));
        assert!(budget.is_number(), "max_tokens must be a JSON number");
        assert!(
            !budget.is_f64(),
            "max_tokens must be an integer, not a float: {budget}"
        );
        assert_eq!(budget.as_u64(), Some(expected));
        assert_eq!(budget.as_i64(), i64::try_from(expected).ok());
        assert_eq!(
            budget.to_string(),
            expected.to_string(),
            "max_tokens must serialize as a bare integer"
        );
        assert!(!budget.is_string(), "max_tokens must not be a string");
        assert!(!budget.is_null(), "max_tokens must not be null");
        assert!(!budget.is_boolean(), "max_tokens must not be a boolean");
        assert!(!budget.is_array(), "max_tokens must not be an array");
        assert!(!budget.is_object(), "max_tokens must not be an object");
    }

    /// Every phase whose output budget stays at the retained constant ceiling.
    fn non_summary_phases() -> [Phase; 6] {
        [
            Phase::Plan,
            Phase::Spec,
            Phase::Tickets,
            Phase::Triage,
            Phase::Code,
            Phase::Review,
        ]
    }

    /// A long populated summary transcript: system text, a long user request,
    /// a narrated tool call and its result.
    fn long_summary_transcript() -> Vec<Message> {
        vec![
            Message::system("You summarize the change."),
            Message::user("inspect the repository ".repeat(128)),
            assistant(
                Some("checking the diff"),
                Some(vec![call(
                    "toolu_1",
                    "execute_script",
                    "{\"code\":\"return tools.diff();\"}",
                )]),
            ),
            Message::tool("toolu_1", "a very long diff"),
            Message::user("write the summary"),
        ]
    }

    /// A later summary turn: the correction turn of the same pass, after the
    /// first summary was rejected as non-canonical.
    fn correction_summary_transcript() -> Vec<Message> {
        vec![
            Message::user("write the summary"),
            assistant(Some("feat: add the exporter"), None),
            Message::user("that header is not canonical; write it again"),
        ]
    }

    /// A tool call as the loop builds one.
    fn call(id: &str, name: &str, arguments: &str) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            kind: "function".to_string(),
            function: FunctionCall {
                name: name.to_string(),
                arguments: arguments.to_string(),
            },
        }
    }

    /// An assistant message, optionally narrating and optionally calling tools.
    fn assistant(content: Option<&str>, calls: Option<Vec<ToolCall>>) -> Message {
        Message {
            role: Role::Assistant,
            content: content.map(str::to_string),
            tool_calls: calls,
            tool_call_id: None,
            reasoning_content: None,
            extra: Map::new(),
        }
    }

    /// The body's native messages, which every projection assertion inspects.
    fn native_messages(prepared: &crate::llm::Prepared) -> &Vec<Value> {
        prepared.outgoing.body["messages"]
            .as_array()
            .expect("the prepared body carries a native messages array")
    }

    /// Regression: a summary transcript still becomes the native Messages
    /// request -- configured model, fixed integer budget, a system field,
    /// native messages and native tools that advertise exactly the summary
    /// inspection tool -- and `output_config` is the only added body key.
    #[test]
    fn when_preparing_given_a_summary_transcript_then_preserves_native_request_contract() {
        let messages = vec![
            Message::system("You summarize the change."),
            Message::user("summarize the diff"),
        ];
        let prepared = provider().prepare(&messages, Phase::Summary);
        let body = &prepared.outgoing.body;

        assert_eq!(body["model"], json!("smoll-model"));
        assert_integer_budget(body, 16384);
        assert_eq!(body["system"], json!("You summarize the change."));
        assert_eq!(
            body["messages"],
            json!([{
                "role": "user",
                "content": [{"type": "text", "text": "summarize the diff"}],
            }])
        );

        let tools = body["tools"].as_array().expect("tools is an array");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], json!("execute_script"));
        assert_eq!(
            tools[0]["input_schema"],
            serde_json::to_value(&Tool::definitions(Phase::Summary)[0].function.parameters)
                .unwrap()
        );
        assert_eq!(tools[0].as_object().unwrap().len(), 3);
        assert!(tools[0].get("function").is_none());
        assert!(tools[0].get("parameters").is_none());
        assert!(tools[0].get("type").is_none());

        assert_high_effort(body);
        assert_eq!(
            body_keys(body),
            vec![
                "max_tokens",
                "messages",
                "model",
                "output_config",
                "system",
                "tools"
            ]
        );
        for key in ["choices", "input", "store", "temperature"] {
            assert!(body.get(key).is_none(), "unexpected OpenAI key {key}");
        }
    }

    /// Core entry-point behavior and contract: a Summary request prepared for
    /// the configured endpoint asks for the fixed high reasoning effort as a
    /// nested `output_config.effort` string, alongside the unchanged native
    /// request -- configured model, integer budget, native messages, native
    /// tools, configured URL and Anthropic authentication.
    #[test]
    fn when_preparing_given_a_summary_transcript_then_requests_high_effort() {
        let messages = vec![
            Message::system("You summarize the change."),
            Message::user("summarize the diff"),
        ];
        let prepared = provider().prepare(&messages, Phase::Summary);
        let body = &prepared.outgoing.body;

        assert_eq!(body["output_config"], json!({"effort": "high"}));
        assert_high_effort(body);

        assert_eq!(body["model"], json!("smoll-model"));
        assert_integer_budget(body, 16384);
        assert_eq!(body["system"], json!("You summarize the change."));
        assert_eq!(
            body["messages"],
            json!([{
                "role": "user",
                "content": [{"type": "text", "text": "summarize the diff"}],
            }])
        );
        assert_eq!(body["tools"].as_array().map(Vec::len), Some(1));
        assert_eq!(prepared.outgoing.url, "http://smoll.test/v1/messages");
        assert_eq!(
            prepared.outgoing.auth,
            Auth::Anthropic("smoll-key".to_string())
        );
    }

    /// Edge-case table: every non-summary phase keeps its existing body shape.
    /// None of them asks for reasoning effort, neither nested nor at the top
    /// level, and each keeps the pre-existing request keys.
    #[test]
    fn when_preparing_given_each_non_summary_phase_then_omits_reasoning_effort() {
        for phase in [
            Phase::Plan,
            Phase::Spec,
            Phase::Tickets,
            Phase::Triage,
            Phase::Code,
            Phase::Review,
        ] {
            let prepared = provider().prepare(&[Message::user("hi")], phase);
            let body = &prepared.outgoing.body;

            assert!(
                body.get("output_config").is_none(),
                "phase {phase:?} must not carry output_config"
            );
            assert!(
                body.get("effort").is_none(),
                "phase {phase:?} must not carry a top-level effort"
            );
            assert!(
                !prepared.outgoing.body.to_string().contains("effort"),
                "phase {phase:?} must not mention effort anywhere"
            );
            assert_eq!(
                body_keys(body),
                vec!["max_tokens", "messages", "model", "tools"],
                "phase {phase:?}"
            );
            assert_eq!(body["model"], json!("smoll-model"), "phase {phase:?}");
            assert_eq!(body["max_tokens"], json!(4096), "phase {phase:?}");
        }
    }

    /// Empty and populated boundaries: an absent transcript and a transcript
    /// with turns both ask for the same non-null high effort string.
    #[test]
    fn when_preparing_given_empty_and_populated_summary_transcripts_then_requests_high_effort() {
        let populated = [
            Message::system("You summarize the change."),
            Message::user("summarize the diff"),
        ];

        for messages in [&[][..], &populated[..]] {
            let prepared = provider().prepare(messages, Phase::Summary);
            let body = &prepared.outgoing.body;

            assert_high_effort(body);
            assert_eq!(body["output_config"]["effort"].as_str(), Some("high"));
            assert!(!body["output_config"]["effort"].is_null());
            assert_integer_budget(body, 16384);
        }
    }

    /// Minimum valid boundary: one user turn is a valid summary transcript and
    /// still asks for the exact nested high effort.
    #[test]
    fn when_preparing_given_one_user_summary_message_then_requests_high_effort() {
        let prepared = provider().prepare(
            &[Message::user("summarize the last change")],
            Phase::Summary,
        );
        let body = &prepared.outgoing.body;

        assert_eq!(body["output_config"], json!({"effort": "high"}));
        assert_high_effort(body);
        assert!(body.get("system").is_none());
        assert_eq!(
            body["messages"],
            json!([{
                "role": "user",
                "content": [{"type": "text", "text": "summarize the last change"}],
            }])
        );
        assert_integer_budget(body, 16384);
    }

    /// Upper boundary: a longer transcript and a later summary turn -- the
    /// correction turn of the same pass -- ask for the same fixed high effort,
    /// whatever the transcript carries and whatever the provider settings are.
    #[test]
    fn when_preparing_given_long_and_repeated_summary_transcripts_then_requests_fixed_high_effort()
    {
        let long = vec![
            Message::system("You summarize the change."),
            Message::user("inspect the repository ".repeat(128)),
            assistant(
                Some("checking the diff"),
                Some(vec![call(
                    "toolu_1",
                    "execute_script",
                    r#"{"code":"return tools.diff();"}"#,
                )]),
            ),
            Message::tool("toolu_1", "a very long diff"),
            Message::user("write the summary"),
        ];
        let correction = vec![
            Message::user("write the summary"),
            assistant(Some("feat: add the exporter"), None),
            Message::user("that header is not canonical; write it again"),
        ];

        let provider = provider();
        let minimal = provider.prepare(&[Message::user("hi")], Phase::Summary);
        let longer = provider.prepare(&long, Phase::Summary);
        let later_turn = provider.prepare(&correction, Phase::Summary);
        let repeated = provider.prepare(&long, Phase::Summary);

        for prepared in [&minimal, &longer, &later_turn, &repeated] {
            assert_high_effort(&prepared.outgoing.body);
        }
        assert_eq!(
            later_turn.outgoing.body["output_config"]["effort"],
            longer.outgoing.body["output_config"]["effort"]
        );

        // A differently configured endpoint asks for the same fixed effort:
        // no existing provider setting reaches this value.
        let other = AnthropicProvider::new(
            SmollSettings::new("http://other.test/v1/messages", "other-model", "other-key")
                .expect("a non-blank key configures the provider"),
        );
        let elsewhere = other.prepare(&long, Phase::Summary);
        assert_high_effort(&elsewhere.outgoing.body);
        assert_eq!(elsewhere.outgoing.body["model"], json!("other-model"));
        assert_eq!(elsewhere.outgoing.url, "http://other.test/v1/messages");
    }

    /// Malformed payload boundary: the generated effort is a string inside a
    /// one-member object, so a scalar, array, null or wider object can never
    /// satisfy the contract.
    #[test]
    fn when_preparing_given_summary_effort_contract_then_uses_object_with_string_value() {
        let prepared = provider().prepare(&[Message::user("summarize")], Phase::Summary);
        let body = &prepared.outgoing.body;

        let config = body.get("output_config").expect("output_config is present");
        assert!(config.is_object(), "output_config is a JSON object");
        assert!(!config.is_string());
        assert!(!config.is_number());
        assert!(!config.is_array());
        assert!(!config.is_boolean());
        assert!(!config.is_null());

        let members = config.as_object().expect("output_config is an object");
        assert_eq!(members.len(), 1);
        let effort = members.get("effort").expect("output_config names effort");
        assert!(effort.is_string(), "effort is a JSON string");
        assert!(!effort.is_number());
        assert!(!effort.is_array());
        assert!(!effort.is_object());
        assert_eq!(effort, &json!("high"));
        assert_eq!(effort.as_str(), Some("high"));

        assert!(body.get("effort").is_none());
        assert_integer_budget(body, 16384);
    }

    /// Core entry-point behavior: an ordinary Summary request declares the
    /// fixed integer 16384-token output budget and still advertises the
    /// Summary phase's native read-only tool.
    #[test]
    fn when_preparing_given_a_summary_request_then_uses_integer_16384_tokens() {
        let messages = [
            Message::system("You summarize the change."),
            Message::user("summarize the diff"),
        ];
        let prepared = provider().prepare(&messages, Phase::Summary);
        let body = &prepared.outgoing.body;

        assert_integer_budget(body, 16384);
        assert_high_effort(body);

        let tools = body["tools"].as_array().expect("tools is an array");
        let definitions = Tool::definitions(Phase::Summary);
        assert_eq!(tools.len(), definitions.len());
        assert_eq!(tools.len(), 1);
        for (tool, definition) in tools.iter().zip(definitions) {
            assert_eq!(tool["name"], json!(definition.function.name));
            assert_eq!(
                tool["input_schema"],
                serde_json::to_value(&definition.function.parameters).unwrap()
            );
        }
        assert_eq!(prepared.outgoing.url, "http://smoll.test/v1/messages");
        assert_eq!(
            prepared.outgoing.auth,
            Auth::Anthropic("smoll-key".to_string())
        );
    }

    /// Core entry-point behavior: the tool-free Summary request -- the one the
    /// loop's compaction handover builds -- declares the same fixed integer
    /// 16384-token budget, keeps the high effort and advertises no tool.
    #[test]
    fn when_preparing_given_a_tool_free_summary_request_then_uses_integer_16384_tokens_and_no_tools()
     {
        let provider = provider();
        let messages = [
            Message::system("You summarize the change."),
            Message::user("summarize the diff"),
        ];

        let populated = provider.prepare_tool_free(&messages, Phase::Summary);
        let body = &populated.outgoing.body;
        assert_integer_budget(body, 16384);
        assert_high_effort(body);
        assert_eq!(body["tools"], json!([]));
        assert_eq!(
            body_keys(body),
            vec![
                "max_tokens",
                "messages",
                "model",
                "output_config",
                "system",
                "tools"
            ]
        );
        assert_eq!(body["model"], json!("smoll-model"));
        assert_eq!(populated.outgoing.url, "http://smoll.test/v1/messages");

        // The absent transcript keeps the same native shape.
        let empty = provider.prepare_tool_free(&[], Phase::Summary);
        assert_integer_budget(&empty.outgoing.body, 16384);
        assert_high_effort(&empty.outgoing.body);
        assert_eq!(empty.outgoing.body["tools"], json!([]));
        assert_eq!(
            empty.outgoing.auth,
            Auth::Anthropic("smoll-key".to_string())
        );
    }

    /// Core entry-point behavior: every non-Summary phase keeps the retained
    /// integer 4096-token budget in ordinary and tool-free preparation, and
    /// neither mode adds a reasoning-effort request.
    #[test]
    fn when_preparing_given_each_non_summary_phase_then_uses_integer_4096_tokens_in_both_request_modes()
     {
        let provider = provider();
        for phase in non_summary_phases() {
            for prepared in [
                provider.prepare(&[Message::user("hi")], phase),
                provider.prepare_tool_free(&[Message::user("hi")], phase),
            ] {
                let body = &prepared.outgoing.body;
                assert_integer_budget(body, 4096);
                assert_eq!(body["model"], json!("smoll-model"), "phase {phase:?}");
                assert!(
                    body.get("output_config").is_none(),
                    "phase {phase:?} must not carry output_config"
                );
                assert!(
                    body.get("effort").is_none(),
                    "phase {phase:?} must not carry a top-level effort"
                );
                assert!(
                    !body.to_string().contains("effort"),
                    "phase {phase:?} must not mention effort anywhere"
                );
                assert_eq!(prepared.outgoing.url, "http://smoll.test/v1/messages");
                assert_eq!(
                    prepared.outgoing.auth,
                    Auth::Anthropic("smoll-key".to_string()),
                    "phase {phase:?}"
                );
            }
        }
    }

    /// Upper boundary: the full non-Summary phase table keeps 4096 in both
    /// request modes, while ordinary preparation keeps the phase's native
    /// tools and tool-free preparation advertises none.
    #[test]
    fn when_preparing_given_each_non_summary_phase_in_both_request_modes_then_keeps_4096_tokens() {
        let provider = provider();
        for phase in non_summary_phases() {
            let ordinary = provider.prepare(&[], phase);
            assert_integer_budget(&ordinary.outgoing.body, 4096);
            assert_eq!(
                ordinary.outgoing.body["tools"].as_array().map(Vec::len),
                Some(Tool::definitions(phase).len()),
                "phase {phase:?} keeps its native tools"
            );
            assert_eq!(
                body_keys(&ordinary.outgoing.body),
                vec!["max_tokens", "messages", "model", "tools"],
                "phase {phase:?}"
            );

            let tool_free = provider.prepare_tool_free(&[], phase);
            assert_integer_budget(&tool_free.outgoing.body, 4096);
            assert_eq!(
                tool_free.outgoing.body["tools"],
                json!([]),
                "phase {phase:?}"
            );
            assert_eq!(
                body_keys(&tool_free.outgoing.body),
                vec!["max_tokens", "messages", "model", "tools"],
                "phase {phase:?}"
            );
        }
    }

    /// Contract and invariant: Summary and non-Summary bodies keep their
    /// native shape -- exact integer budget types and values, Summary-only
    /// high effort, native tool representations, unchanged body keys, model,
    /// endpoint and authentication -- and the only budget member either
    /// category declares is its max_tokens value.
    #[test]
    fn when_preparing_given_summary_and_non_summary_requests_then_preserves_native_body_contracts()
    {
        let provider = provider();
        let messages = [
            Message::system("You summarize the change."),
            Message::user("summarize the diff"),
        ];

        let summary = provider.prepare(&messages, Phase::Summary);
        let body = &summary.outgoing.body;
        assert_integer_budget(body, 16384);
        assert_high_effort(body);
        assert_eq!(body["model"], json!("smoll-model"));
        assert_eq!(body["system"], json!("You summarize the change."));
        assert_eq!(
            body["messages"],
            json!([{
                "role": "user",
                "content": [{"type": "text", "text": "summarize the diff"}],
            }])
        );
        assert_eq!(
            body_keys(body),
            vec![
                "max_tokens",
                "messages",
                "model",
                "output_config",
                "system",
                "tools"
            ]
        );
        let tools = body["tools"].as_array().expect("tools is an array");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], json!("execute_script"));
        assert_eq!(tools[0].as_object().unwrap().len(), 3);
        assert!(tools[0].get("function").is_none());
        assert!(tools[0].get("parameters").is_none());
        assert!(tools[0].get("type").is_none());
        assert_eq!(summary.outgoing.url, "http://smoll.test/v1/messages");
        assert_eq!(
            summary.outgoing.auth,
            Auth::Anthropic("smoll-key".to_string())
        );

        let tool_free = provider.prepare_tool_free(&messages, Phase::Summary);
        let body = &tool_free.outgoing.body;
        assert_integer_budget(body, 16384);
        assert_high_effort(body);
        assert_eq!(body["tools"], json!([]));
        assert_eq!(
            body_keys(body),
            vec![
                "max_tokens",
                "messages",
                "model",
                "output_config",
                "system",
                "tools"
            ]
        );
        assert_eq!(tool_free.outgoing.url, "http://smoll.test/v1/messages");
        assert_eq!(
            tool_free.outgoing.auth,
            Auth::Anthropic("smoll-key".to_string())
        );

        let code = provider.prepare(&messages, Phase::Code);
        let body = &code.outgoing.body;
        assert_integer_budget(body, 4096);
        assert_eq!(
            body_keys(body),
            vec!["max_tokens", "messages", "model", "system", "tools"]
        );
        for key in [
            "choices",
            "effort",
            "input",
            "max_output_tokens",
            "output_config",
            "reasoning",
            "reasoning_effort",
            "store",
            "temperature",
        ] {
            assert!(body.get(key).is_none(), "unexpected body key {key}");
        }
        assert_eq!(code.outgoing.url, "http://smoll.test/v1/messages");
        assert_eq!(code.outgoing.auth, Auth::Anthropic("smoll-key".to_string()));

        // No request body gains any budget-related member besides the updated
        // max_tokens value.
        for prepared in [&summary, &tool_free, &code] {
            for key in prepared.outgoing.body.as_object().unwrap().keys() {
                assert!(
                    key == "max_tokens" || !key.contains("token"),
                    "unexpected budget-related member {key}"
                );
            }
        }
    }

    /// Malformed payload boundary: both phase categories emit the budget as a
    /// bare JSON integer literal -- never a quoted string, a float, null, an
    /// array or an object -- in ordinary and tool-free preparation alike.
    #[test]
    fn when_preparing_given_either_phase_category_then_emits_a_json_integer_budget() {
        let provider = provider();
        let cases = [
            (Phase::Summary, 16384u64),
            (Phase::Plan, 4096),
            (Phase::Spec, 4096),
            (Phase::Tickets, 4096),
            (Phase::Triage, 4096),
            (Phase::Code, 4096),
            (Phase::Review, 4096),
        ];

        for (phase, expected) in cases {
            for prepared in [
                provider.prepare(&[Message::user("hi")], phase),
                provider.prepare_tool_free(&[Message::user("hi")], phase),
            ] {
                let budget = prepared
                    .outgoing
                    .body
                    .get("max_tokens")
                    .expect("the body declares max_tokens");
                assert_eq!(budget, &json!(expected), "phase {phase:?}");
                assert!(budget.is_number(), "phase {phase:?}");
                assert!(!budget.is_f64(), "phase {phase:?} budget is an integer");
                assert_eq!(budget.as_u64(), Some(expected), "phase {phase:?}");
                assert_eq!(
                    budget.to_string(),
                    expected.to_string(),
                    "phase {phase:?} serializes a bare JSON integer"
                );
                assert!(budget.as_str().is_none(), "phase {phase:?}");
                assert!(budget.as_bool().is_none(), "phase {phase:?}");
                assert!(budget.as_array().is_none(), "phase {phase:?}");
                assert!(budget.as_object().is_none(), "phase {phase:?}");
            }
        }
    }

    /// Edge-case table: text-only, tool-only, mixed and unknown blocks each
    /// decode into the canonical assistant message, preserving usable order.
    #[test]
    fn when_decoding_given_text_tool_and_mixed_replies_then_preserves_usable_blocks_in_order() {
        let text = decoded(
            &provider(),
            raw(
                200,
                r#"{"content":[{"type":"text","text":"Hello, "},{"type":"thinking","thinking":"hmm"},{"type":"text","text":"world"}]}"#,
            ),
        );
        assert_eq!(text.role, Role::Assistant);
        assert_eq!(text.content.as_deref(), Some("Hello, world"));
        assert!(text.tool_calls.is_none());

        let tool = decoded(
            &provider(),
            raw(
                200,
                r#"{"content":[{"type":"tool_use","id":"toolu_1","name":"bash","input":{"command":"ls"}},{"type":"redacted_thinking","data":"x"},{"type":"tool_use","id":"toolu_2","name":"execute_script","input":{"code":"return 1;"}}]}"#,
            ),
        );
        assert!(tool.content.is_none());
        let calls = tool.tool_calls.expect("the tool calls survive");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id, "toolu_1");
        assert_eq!(calls[0].kind, "function");
        assert_eq!(calls[0].function.name, "bash");
        assert_eq!(calls[0].function.arguments, r#"{"command":"ls"}"#);
        assert_eq!(calls[1].id, "toolu_2");
        assert_eq!(calls[1].function.name, "execute_script");
        assert_eq!(calls[1].function.arguments, r#"{"code":"return 1;"}"#);

        let mixed = decoded(
            &provider(),
            raw(
                200,
                r#"{"content":[{"type":"text","text":"checking"},{"type":"tool_use","id":"toolu_3","name":"bash","input":{"command":"pwd"}},{"type":"text","text":" done"},{"type":"server_tool_use","id":"srv"}]}"#,
            ),
        );
        assert_eq!(mixed.role, Role::Assistant);
        assert_eq!(mixed.content.as_deref(), Some("checking done"));
        let calls = mixed.tool_calls.expect("the mixed call survives");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "toolu_3");
    }

    /// Contract and invariant: system text leaves the messages array,
    /// transcript order survives, tool ids and results match, adjacent
    /// same-role turns coalesce, and the context measurement covers exactly
    /// the projected context.
    #[test]
    fn when_projecting_given_system_and_tool_transcript_messages_then_preserves_order_and_coalesces_roles()
     {
        let messages = vec![
            Message::system("system A"),
            Message::system("system B"),
            Message::user("go"),
            assistant(
                Some("checking"),
                Some(vec![
                    call("toolu_1", "bash", r#"{"command":"ls"}"#),
                    call("toolu_2", "execute_script", r#"{"code":"return 1;"}"#),
                ]),
            ),
            Message::tool("toolu_1", "a.txt"),
            Message::tool("toolu_2", "1"),
            Message::user("and again"),
            assistant(
                None,
                Some(vec![call("toolu_3", "bash", r#"{"command":"pwd"}"#)]),
            ),
        ];
        let prepared = provider().prepare(&messages, Phase::Code);
        let body = &prepared.outgoing.body;

        assert_eq!(body["system"], json!("system A\n\nsystem B"));
        let native = native_messages(&prepared);
        assert!(!native.iter().any(|message| message["role"] == "system"));

        assert_eq!(
            native,
            &vec![
                json!({
                    "role": "user",
                    "content": [{"type": "text", "text": "go"}],
                }),
                json!({
                    "role": "assistant",
                    "content": [
                        {"type": "text", "text": "checking"},
                        {"type": "tool_use", "id": "toolu_1", "name": "bash", "input": {"command": "ls"}},
                        {"type": "tool_use", "id": "toolu_2", "name": "execute_script", "input": {"code": "return 1;"}},
                    ],
                }),
                json!({
                    "role": "user",
                    "content": [
                        {"type": "tool_result", "tool_use_id": "toolu_1", "content": "a.txt"},
                        {"type": "tool_result", "tool_use_id": "toolu_2", "content": "1"},
                        {"type": "text", "text": "and again"},
                    ],
                }),
                json!({
                    "role": "assistant",
                    "content": [
                        {"type": "tool_use", "id": "toolu_3", "name": "bash", "input": {"command": "pwd"}},
                    ],
                }),
            ]
        );

        for pair in native.windows(2) {
            assert_ne!(
                pair[0]["role"], pair[1]["role"],
                "adjacent native messages must not repeat a role"
            );
        }

        let context = json!({
            "system": body["system"],
            "messages": body["messages"],
        });
        assert_eq!(
            prepared.context_bytes,
            serde_json::to_string(&context).unwrap().len()
        );
    }

    /// Regression: nested and raw-body API errors preserve the status and
    /// message, and the shared retry policy keeps only transient statuses.
    #[test]
    fn when_decoding_given_anthropic_api_errors_then_preserves_status_message_and_retry_policy() {
        let nested = r#"{"type":"error","error":{"type":"invalid_request_error","message":"max_tokens too high"}}"#;
        match decode_error(&provider(), raw(400, nested)) {
            CallError::Api { status, message } => {
                assert_eq!(status, 400);
                assert_eq!(message, "max_tokens too high");
                assert!(
                    should_retry(
                        &CallError::Api {
                            status,
                            message: message.clone()
                        },
                        1
                    )
                    .is_none()
                );
            }
            other => panic!("expected an API error, got {}", other),
        }

        let embedded = r#"{"type":"error","error":{"message":"overloaded"},"content":[]}"#;
        match decode_error(&provider(), raw(200, embedded)) {
            CallError::Api { status, message } => {
                assert_eq!(status, 200);
                assert_eq!(message, "overloaded");
            }
            other => panic!("expected an API error, got {}", other),
        }

        let raw_body = r#"{"detail":"bad request"}"#;
        match decode_error(&provider(), raw(400, raw_body)) {
            CallError::Api { status, message } => {
                assert_eq!(status, 400);
                assert_eq!(message, raw_body);
            }
            other => panic!("expected an API error, got {}", other),
        }

        for status in [429, 500, 502, 503, 504] {
            let error = decode_error(
                &provider(),
                raw(status, r#"{"error":{"message":"try later"}}"#),
            );
            assert_eq!(should_retry(&error, 1), Some(Duration::from_secs(1)));
            assert_eq!(should_retry(&error, 2), Some(Duration::from_secs(2)));
            assert_eq!(should_retry(&error, 3), None);
        }

        let malformed = decode_error(&provider(), raw(200, r#"{"content":[]}"#));
        assert!(matches!(malformed, CallError::Malformed(_)));
        assert!(should_retry(&malformed, 1).is_none());

        let transport = decode_error(&provider(), raw(200, "not json"));
        assert!(matches!(transport, CallError::Transport(_)));
        assert_eq!(should_retry(&transport, 1), Some(Duration::from_secs(1)));
    }

    /// Empty/zero boundary: absent, empty and unusable content is malformed,
    /// never a panic.
    #[test]
    fn when_decoding_given_empty_or_missing_content_then_returns_malformed_without_panicking() {
        for body in [
            r#"{}"#,
            r#"{"content":[]}"#,
            r#"{"content":null}"#,
            r#"{"content":[{"type":"thinking","thinking":"quiet"}]}"#,
            r#"{"content":[{"type":"text","text":""}]}"#,
        ] {
            match decode_error(&provider(), raw(200, body)) {
                CallError::Malformed(_) => {}
                other => panic!("body {body}: expected a malformed error, got {}", other),
            }
        }
    }

    /// Minimum valid boundary: a one-message, configured-model request is a
    /// well-formed native Messages body carrying native tools.
    #[test]
    fn when_preparing_given_minimal_transcript_then_builds_a_valid_native_request() {
        let prepared = provider().prepare(&[Message::user("hi")], Phase::Code);
        let body = &prepared.outgoing.body;

        assert_eq!(body["model"], json!("smoll-model"));
        assert_eq!(body["max_tokens"], json!(4096));
        assert_eq!(
            body["messages"],
            json!([{"role": "user", "content": [{"type": "text", "text": "hi"}]}])
        );
        assert!(body.get("system").is_none());

        let tools = body["tools"].as_array().expect("tools is an array");
        let definitions = Tool::definitions(Phase::Code);
        assert_eq!(tools.len(), definitions.len());
        for (tool, definition) in tools.iter().zip(definitions) {
            assert_eq!(tool["name"], json!(definition.function.name));
            assert_eq!(tool["description"], json!(definition.function.description));
            assert_eq!(
                tool["input_schema"],
                serde_json::to_value(&definition.function.parameters).unwrap()
            );
            assert_eq!(tool.as_object().unwrap().len(), 3);
        }

        assert_eq!(
            body_keys(body),
            vec!["max_tokens", "messages", "model", "tools"]
        );
        assert_eq!(prepared.outgoing.url, "http://smoll.test/v1/messages");
        assert_eq!(
            prepared.outgoing.auth,
            Auth::Anthropic("smoll-key".to_string())
        );
    }

    /// Empty/zero/nil boundary: an absent transcript does not omit or alter
    /// the fixed Summary budget, in either request mode.
    #[test]
    fn when_preparing_given_an_empty_summary_transcript_then_uses_integer_16384_tokens() {
        let provider = provider();
        let ordinary = provider.prepare(&[], Phase::Summary);
        assert_integer_budget(&ordinary.outgoing.body, 16384);
        assert_high_effort(&ordinary.outgoing.body);
        assert_eq!(ordinary.outgoing.body["messages"], json!([]));

        let tool_free = provider.prepare_tool_free(&[], Phase::Summary);
        assert_integer_budget(&tool_free.outgoing.body, 16384);
        assert_high_effort(&tool_free.outgoing.body);
        assert_eq!(tool_free.outgoing.body["tools"], json!([]));
    }

    /// Minimum valid boundary: one user message is a valid Summary transcript
    /// and keeps the exact integer budget in both request modes.
    #[test]
    fn when_preparing_given_one_user_summary_message_then_uses_integer_16384_tokens() {
        let provider = provider();
        let messages = [Message::user("summarize the last change")];

        let ordinary = provider.prepare(&messages, Phase::Summary);
        assert_integer_budget(&ordinary.outgoing.body, 16384);
        assert_high_effort(&ordinary.outgoing.body);
        assert_eq!(
            ordinary.outgoing.body["tools"].as_array().map(Vec::len),
            Some(1)
        );

        let tool_free = provider.prepare_tool_free(&messages, Phase::Summary);
        assert_integer_budget(&tool_free.outgoing.body, 16384);
        assert_high_effort(&tool_free.outgoing.body);
        assert_eq!(tool_free.outgoing.body["tools"], json!([]));
    }

    /// Upper boundary: a long populated transcript and the later correction
    /// turn of the same pass keep the same fixed integer budget, whatever the
    /// transcript carries and whatever the provider settings are.
    #[test]
    fn when_preparing_given_long_and_correction_summary_transcripts_then_uses_integer_16384_tokens()
    {
        let provider = provider();
        let long = long_summary_transcript();
        let correction = correction_summary_transcript();

        let mut budgets = Vec::new();
        for messages in [&long, &correction] {
            for prepared in [
                provider.prepare(messages, Phase::Summary),
                provider.prepare_tool_free(messages, Phase::Summary),
            ] {
                assert_integer_budget(&prepared.outgoing.body, 16384);
                assert_high_effort(&prepared.outgoing.body);
                budgets.push(prepared.outgoing.body["max_tokens"].clone());
            }
        }
        assert!(
            budgets.windows(2).all(|pair| pair[0] == pair[1]),
            "the fixed Summary budget must not vary with the transcript: {budgets:?}"
        );

        // A differently configured endpoint keeps the same fixed budget: no
        // existing provider setting reaches this value.
        let other = AnthropicProvider::new(
            SmollSettings::new("http://other.test/v1/messages", "other-model", "other-key")
                .expect("a non-blank key configures the provider"),
        );
        let elsewhere = other.prepare(&long, Phase::Summary);
        assert_integer_budget(&elsewhere.outgoing.body, 16384);
        assert_eq!(elsewhere.outgoing.body["model"], json!("other-model"));
        assert_eq!(elsewhere.outgoing.url, "http://other.test/v1/messages");
    }

    /// Edge-case table: the fixed Summary budget survives the empty
    /// transcript, the one-message minimum, a populated/long transcript and a
    /// later correction turn, in ordinary and tool-free preparation alike.
    #[test]
    fn when_preparing_given_empty_single_long_and_correction_summary_transcripts_then_keeps_16384_tokens()
     {
        let provider = provider();
        let cases: [(&str, Vec<Message>); 4] = [
            ("empty", Vec::new()),
            ("single", vec![Message::user("summarize the last change")]),
            ("long", long_summary_transcript()),
            ("correction", correction_summary_transcript()),
        ];

        for (label, messages) in cases {
            let ordinary = provider.prepare(&messages, Phase::Summary);
            assert_integer_budget(&ordinary.outgoing.body, 16384);
            assert_high_effort(&ordinary.outgoing.body);

            let tool_free = provider.prepare_tool_free(&messages, Phase::Summary);
            assert_integer_budget(&tool_free.outgoing.body, 16384);
            assert_high_effort(&tool_free.outgoing.body);
            assert_eq!(
                tool_free.outgoing.body["tools"],
                json!([]),
                "{label}: tool-free preparation advertises no tool"
            );
        }
    }

    /// Malformed payload boundary: invalid JSON, non-object JSON and malformed
    /// block fields all come back as a decode error without a panic.
    #[test]
    fn when_decoding_given_invalid_json_non_object_or_malformed_blocks_then_returns_a_decode_error()
    {
        match decode_error(&provider(), raw(200, "not json")) {
            CallError::Transport(message) => assert!(
                message.starts_with("invalid JSON response (HTTP 200): "),
                "unexpected message: {message}"
            ),
            other => panic!("expected a transport error, got {}", other),
        }

        for status in [200, 500] {
            for body in ["[]", "\"hello\"", "42", "null", "true"] {
                match decode_error(&provider(), raw(status, body)) {
                    CallError::Transport(message) => assert!(
                        message.starts_with(&format!("invalid JSON response (HTTP {status}): ")),
                        "status {status}, body {body}: unexpected message: {message}"
                    ),
                    other => panic!(
                        "status {status}, body {body}: expected a transport error, got {}",
                        other
                    ),
                }
            }
        }

        for body in [
            r#"{"content":"hello"}"#,
            r#"{"content":[{"type":"tool_use"}]}"#,
            r#"{"content":[{"type":"tool_use","id":7,"name":null,"input":[]}]}"#,
            r#"{"content":[1,"x",null]}"#,
        ] {
            let result = provider().decode(raw(200, body));
            assert!(result.is_err(), "body {body}: expected a decode error");
        }
    }

    /// Forbidden boundary: 401 and 403 map the nested message and status and
    /// are never retried.
    #[test]
    fn when_decoding_given_unauthorized_or_forbidden_anthropic_errors_then_preserves_api_status() {
        for (status, message) in [(401, "invalid x-api-key"), (403, "forbidden")] {
            let body = json!({
                "type": "error",
                "error": {"type": "authentication_error", "message": message},
            })
            .to_string();

            match decode_error(&provider(), raw(status, &body)) {
                CallError::Api {
                    status: actual_status,
                    message: actual_message,
                } => {
                    assert_eq!(actual_status, status);
                    assert_eq!(actual_message, message);
                    assert!(
                        should_retry(
                            &CallError::Api {
                                status: actual_status,
                                message: actual_message,
                            },
                            1
                        )
                        .is_none()
                    );
                }
                other => panic!("status {status}: expected an API error, got {}", other),
            }
        }
    }

    /// Contract: every request the provider prepares selects Anthropic's native
    /// authentication scheme with the configured key, so this endpoint can never
    /// send an unauthenticated request in any phase.
    #[test]
    fn when_preparing_given_each_phase_then_selects_anthropic_authentication() {
        for phase in [
            Phase::Plan,
            Phase::Spec,
            Phase::Tickets,
            Phase::Triage,
            Phase::Code,
            Phase::Review,
            Phase::Summary,
        ] {
            let prepared = provider().prepare(&[Message::user("hi")], phase);

            assert_eq!(
                prepared.outgoing.auth,
                Auth::Anthropic("smoll-key".to_string()),
                "phase {phase:?}"
            );
        }
    }

    /// The provider name is stable for diagnostics and the factory.
    #[test]
    fn when_reporting_the_name_given_a_native_messages_provider_then_names_the_anthropic_messages_provider()
     {
        assert_eq!(provider().name(), "anthropic-messages");
    }

    /// The exact malformed detail a provider reports for one 200 body.
    fn decoded_error_detail(body: &str) -> String {
        match provider().decode(raw(200, body)) {
            Err(CallError::Malformed(detail)) => detail,
            Err(other) => panic!("body {body}: expected a malformed error, got {}", other),
            Ok(_) => panic!("body {body}: expected a malformed error, got a decoded message"),
        }
    }

    /// Core entry-point behavior: an unusable response reports the safe shape
    /// a caller needs to distinguish it -- stop reason and content-block types
    /// -- in a fixed order, and keeps the exact base message when neither is
    /// available.
    #[test]
    fn when_decoding_given_unusable_anthropic_content_then_reports_safe_metadata() {
        let both = decoded_error_detail(
            r#"{"stop_reason":"end_turn","content":[{"type":"thinking","thinking":"private reasoning"},{"type":"redacted_thinking","data":"private"}]}"#,
        );
        assert_eq!(
            both,
            r#"the response carried no usable content (stop_reason="end_turn"; content_types=["thinking","redacted_thinking"])"#
        );

        let stop_only = decoded_error_detail(r#"{"stop_reason":"max_tokens"}"#);
        assert_eq!(
            stop_only,
            r#"the response carried no usable content (stop_reason="max_tokens")"#
        );

        let content_only = decoded_error_detail(r#"{"content":[]}"#);
        assert_eq!(
            content_only,
            r#"the response carried no usable content (content_types=[])"#
        );

        let thinking_only =
            decoded_error_detail(r#"{"content":[{"type":"thinking","thinking":"quiet"}]}"#);
        assert_eq!(
            thinking_only,
            r#"the response carried no usable content (content_types=["thinking"])"#
        );

        let empty_array = decoded_error_detail(r#"{"stop_reason":"max_tokens","content":[]}"#);
        assert_eq!(
            empty_array,
            r#"the response carried no usable content (stop_reason="max_tokens"; content_types=[])"#
        );

        for body in [r#"{}"#, r#"{"stop_reason":null}"#] {
            assert_eq!(
                decoded_error_detail(body),
                "the response carried no usable content"
            );
        }
    }

    /// Edge-case table: absent, null, non-string, empty and whitespace-only
    /// metadata is omitted, while unknown non-empty strings, duplicates and
    /// original block order survive verbatim; missing, empty-array and
    /// non-array content stay distinguishable.
    #[test]
    fn when_decoding_given_metadata_boundary_values_then_includes_only_usable_values() {
        for stop_reason in [
            json!(null),
            json!(7),
            json!(true),
            json!({}),
            json!([]),
            json!(""),
            json!("   "),
            json!("\t\n"),
        ] {
            let label = stop_reason.to_string();
            let body = json!({"stop_reason": stop_reason, "content": []}).to_string();
            assert_eq!(
                decoded_error_detail(&body),
                "the response carried no usable content (content_types=[])",
                "stop_reason {label}"
            );
        }

        let blocks = json!([
            {},
            {"type": null},
            {"type": 7},
            {"type": true},
            {"type": []},
            {"type": {}},
            {"type": ""},
            {"type": "   "},
            {"type": "\t\n"},
        ]);
        let body = json!({"content": blocks}).to_string();
        assert_eq!(
            decoded_error_detail(&body),
            "the response carried no usable content (content_types=[])"
        );

        let body = json!({
            "stop_reason": "end_turn",
            "content": [
                {"type": "thinking"},
                null,
                "not an object",
                {"type": "unknown_type"},
                {"type": "thinking"},
                {"type": ""},
                {"type": "server_tool_use"},
                {"no_type": true},
            ],
        })
        .to_string();
        assert_eq!(
            decoded_error_detail(&body),
            r#"the response carried no usable content (stop_reason="end_turn"; content_types=["thinking","unknown_type","thinking","server_tool_use"])"#
        );

        assert_eq!(
            decoded_error_detail(r#"{"stop_reason":"end_turn"}"#),
            r#"the response carried no usable content (stop_reason="end_turn")"#
        );
        assert_eq!(
            decoded_error_detail(r#"{"content":[]}"#),
            "the response carried no usable content (content_types=[])"
        );
        assert_eq!(
            decoded_error_detail(r#"{"stop_reason":"end_turn","content":null}"#),
            "the response content was not an array"
        );
        assert_eq!(
            decoded_error_detail(r#"{"content":"hello"}"#),
            "the response content was not an array"
        );
    }

    /// Contract and invariant: metadata is compact JSON and never leaks
    /// response text, reasoning, tool inputs, identifiers or the raw body; the
    /// resulting malformed error stays non-retryable.
    #[test]
    fn when_decoding_given_sensitive_unusable_content_then_excludes_private_payload() {
        let body = json!({
            "stop_reason": "quote\" slash\\ tab\t line\n",
            "content": [
                {"type": "thinking", "thinking": "PRIVATE_REASONING"},
                {"type": "weird\"type\\with\ttabs", "text": "PRIVATE_RESPONSE_TEXT"},
                {"type": "tool_use", "id": "toolu_PRIVATE_ID", "name": "", "input": {"secret": "PRIVATE_INPUT"}},
                {"type": "text", "text": ""},
            ],
            "private_field": "PRIVATE_BODY",
        })
        .to_string();

        let error = decode_error(&provider(), raw(200, &body));
        let detail = match &error {
            CallError::Malformed(detail) => detail.clone(),
            other => panic!("expected a malformed error, got {}", other),
        };

        assert!(
            detail.contains(r#"stop_reason="quote\" slash\\ tab\t line\n""#),
            "detail must JSON-escape the stop reason: {detail}"
        );
        assert!(
            detail.contains(r#""weird\"type\\with\ttabs""#),
            "detail must JSON-escape a block type: {detail}"
        );
        assert!(
            !detail.contains('\n') && !detail.contains('\t') && !detail.contains('\r'),
            "detail must not carry raw control characters: {detail}"
        );
        for secret in [
            "PRIVATE_REASONING",
            "PRIVATE_RESPONSE_TEXT",
            "PRIVATE_INPUT",
            "toolu_PRIVATE_ID",
            "PRIVATE_BODY",
        ] {
            assert!(
                !detail.contains(secret),
                "detail must not expose {secret}: {detail}"
            );
        }
        assert!(
            !detail.contains(&body),
            "detail must not echo the raw response body"
        );
        assert!(matches!(error, CallError::Malformed(_)));
        assert!(should_retry(&error, 1).is_none());
    }

    /// Regression: usable text and valid tool calls still decode to the same
    /// canonical message, while invalid JSON, non-array content, API errors and
    /// unauthorized/forbidden responses keep their existing classifications.
    #[test]
    fn when_decoding_given_usable_text_or_tool_calls_then_preserves_message() {
        let text = decoded(
            &provider(),
            raw(
                200,
                r#"{"stop_reason":"end_turn","content":[{"type":"text","text":"hello"}]}"#,
            ),
        );
        assert_eq!(text.role, Role::Assistant);
        assert_eq!(text.content.as_deref(), Some("hello"));
        assert!(text.tool_calls.is_none());
        assert!(text.tool_call_id.is_none());
        assert!(text.reasoning_content.is_none());
        assert!(text.extra.is_empty());

        let tool = decoded(
            &provider(),
            raw(
                200,
                r#"{"stop_reason":"tool_use","content":[{"type":"tool_use","id":"toolu_1","name":"bash","input":{"command":"ls"}}]}"#,
            ),
        );
        assert_eq!(tool.role, Role::Assistant);
        assert!(tool.content.is_none());
        let calls = tool.tool_calls.expect("the tool call survives");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "toolu_1");
        assert_eq!(calls[0].kind, "function");
        assert_eq!(calls[0].function.name, "bash");
        assert_eq!(calls[0].function.arguments, r#"{"command":"ls"}"#);
        assert!(tool.extra.is_empty());

        match decode_error(&provider(), raw(200, "not json")) {
            CallError::Transport(message) => assert!(
                message.starts_with("invalid JSON response (HTTP 200): "),
                "unexpected message: {message}"
            ),
            other => panic!("expected a transport error, got {}", other),
        }

        assert_eq!(
            decoded_error_detail(r#"{"stop_reason":"end_turn","content":{}}"#),
            "the response content was not an array"
        );
        assert_eq!(
            decoded_error_detail(r#"{"stop_reason":"end_turn","content":42}"#),
            "the response content was not an array"
        );

        match decode_error(
            &provider(),
            raw(400, r#"{"error":{"message":"max_tokens too high"}}"#),
        ) {
            CallError::Api { status, message } => {
                assert_eq!(status, 400);
                assert_eq!(message, "max_tokens too high");
                assert!(!message.contains("stop_reason"));
                assert!(!message.contains("content_types"));
            }
            other => panic!("expected an API error, got {}", other),
        }

        for (status, message) in [(401, "invalid x-api-key"), (403, "forbidden")] {
            let body = json!({
                "type": "error",
                "error": {"type": "authentication_error", "message": message},
            })
            .to_string();
            match decode_error(&provider(), raw(status, &body)) {
                CallError::Api {
                    status: actual_status,
                    message: actual_message,
                } => {
                    assert_eq!(actual_status, status);
                    assert_eq!(actual_message, message);
                    assert!(!actual_message.contains("stop_reason"));
                    assert!(!actual_message.contains("content_types"));
                }
                other => panic!("status {status}: expected an API error, got {}", other),
            }
        }
    }

    /// Empty/zero/nil table: missing content with no usable stop reason keeps
    /// the exact base detail, an empty array announces an empty content-types
    /// list, and null content keeps the existing non-array-content error.
    #[test]
    fn when_decoding_given_missing_empty_or_null_content_then_preserves_diagnostic_contract() {
        for body in [
            r#"{}"#,
            r#"{"stop_reason":null}"#,
            r#"{"stop_reason":""}"#,
            r#"{"stop_reason":" \t "}"#,
        ] {
            assert_eq!(
                decoded_error_detail(body),
                "the response carried no usable content",
                "body {body}"
            );
        }

        assert_eq!(
            decoded_error_detail(r#"{"content":[]}"#),
            "the response carried no usable content (content_types=[])"
        );

        for body in [
            r#"{"content":null}"#,
            r#"{"stop_reason":"end_turn","content":null}"#,
            r#"{"content":"text"}"#,
        ] {
            assert_eq!(
                decoded_error_detail(body),
                "the response content was not an array",
                "body {body}"
            );
        }
    }

    /// Minimum valid boundary: one usable stop reason followed by one usable
    /// block type is reported in the fixed stop_reason then content_types order.
    #[test]
    fn when_decoding_given_one_valid_value_for_each_metadata_field_then_reports_both_in_order() {
        assert_eq!(
            decoded_error_detail(r#"{"stop_reason":"end_turn","content":[{"type":"thinking"}]}"#),
            r#"the response carried no usable content (stop_reason="end_turn"; content_types=["thinking"])"#
        );
    }

    /// Upper boundary: every one of many typed blocks is collected in original
    /// order, duplicates included, with no truncation.
    #[test]
    fn when_decoding_given_many_typed_blocks_then_preserves_every_type_in_order() {
        let types: Vec<String> = (0..32)
            .map(|index| format!("block_type_{}", index % 7))
            .collect();
        let blocks: Vec<Value> = types
            .iter()
            .map(|block_type| json!({ "type": block_type }))
            .collect();
        let body = json!({"stop_reason": "max_tokens", "content": blocks}).to_string();
        let expected_types = serde_json::to_string(&types).expect("the types encode as JSON");

        let detail = decoded_error_detail(&body);
        assert_eq!(
            detail,
            format!(
                r#"the response carried no usable content (stop_reason="max_tokens"; content_types={expected_types})"#
            )
        );
        assert_eq!(detail.matches("block_type_").count(), 32);
        assert_eq!(expected_types.matches("block_type_").count(), 32);
    }

    /// Malformed/invalid payload table: malformed or blank metadata is omitted
    /// without panicking, and invalid JSON and non-array content keep their
    /// existing error classifications.
    #[test]
    fn when_decoding_given_malformed_metadata_and_payloads_then_preserves_error_classification() {
        for body in [
            r#"{"stop_reason":{"nested":true},"content":[{"type":["bad"]},{"type":{"bad":true}},{"type":null},{"type":""}]}"#,
            r#"{"stop_reason":["end_turn"],"content":[{"type":" "}]}"#,
            r#"{"stop_reason":42,"content":[{}]}"#,
        ] {
            assert_eq!(
                decoded_error_detail(body),
                "the response carried no usable content (content_types=[])",
                "body {body}"
            );
        }

        match decode_error(&provider(), raw(200, "{")) {
            CallError::Transport(message) => assert!(
                message.starts_with("invalid JSON response (HTTP 200): "),
                "unexpected message: {message}"
            ),
            other => panic!("expected a transport error, got {}", other),
        }

        for body in [r#"{"content":1}"#, r#"{"content":false}"#] {
            assert_eq!(
                decoded_error_detail(body),
                "the response content was not an array",
                "body {body}"
            );
        }
    }
}
