//! The responses provider: OpenAI's `/v1/responses` wire format.
//!
//! The loop's transcript stays chat-shaped; [`ResponsesProvider::prepare`]
//! projects it into the `input` item array the responses API expects (one item
//! per source element) and sends flat tool definitions, a `reasoning` object
//! and `store: false`. [`ResponsesProvider::decode`] turns the wire's `output`
//! items -- `function_call` and `message` -- back into the same [`Message`] the
//! chat provider produces, so a phase can be served by the giga provider without
//! a second transcript type.
//!
//! Both operations are pure: nothing here touches the network, the environment
//! or a clock, so the whole provider is asserted against literal JSON with no
//! fixture, no key and no network.

use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::api::{self, ApiError, CallError, Message, Outgoing, RawResponse, Role, ToolCall};
use crate::llm::{LlmProvider, Prepared, factory};
use crate::tools::{Phase, Tool};

/// The decoded shape of a responses body: the `output` item array and the
/// optional `error` object. `output` items are left as raw JSON because the
/// wire mixes known and unknown item types, and a degenerate item must never
/// panic the decoder.
#[derive(Deserialize)]
struct ResponsesResponse {
    #[serde(default)]
    output: Option<Vec<Value>>,
    #[serde(default)]
    error: Option<ApiError>,
}

/// A provider that speaks OpenAI's `/v1/responses` wire format.
pub struct ResponsesProvider {
    settings: factory::ProviderSettings,
}

impl ResponsesProvider {
    pub fn new(settings: factory::ProviderSettings) -> ResponsesProvider {
        ResponsesProvider { settings }
    }

    /// Build one request with exactly `tools` advertised. The ordinary
    /// `prepare` passes the phase's flat tool list; the compaction-only
    /// `prepare_tool_free` passes an empty list, which is this wire's own
    /// "advertise no tools" form.
    fn prepare_with_tools(&self, messages: &[Message], tools: Vec<Value>) -> Prepared {
        let input = project(messages);
        // The context is the transcript's own encoding, through the same
        // `serde_json` the transport uses: it cannot fail, so the fallback is
        // `0` and there is no error path.
        let context_bytes = serde_json::to_string(&input)
            .map(|json| json.len())
            .unwrap_or(0);

        let mut body = Map::new();
        body.insert(
            "model".to_string(),
            Value::String(self.settings.model.clone()),
        );
        body.insert("input".to_string(), Value::Array(input));
        body.insert("tools".to_string(), Value::Array(tools));
        body.insert("store".to_string(), Value::Bool(false));

        // `temperature`, `top_p` and `thinking` are never sent: this wire
        // format controls depth through `reasoning` alone.
        if let Some(effort) = self.settings.reasoning_effort.as_deref() {
            let effort = effort.trim();
            if !effort.eq_ignore_ascii_case("none") {
                body.insert("reasoning".to_string(), json!({ "effort": effort }));
            }
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

impl LlmProvider for ResponsesProvider {
    fn name(&self) -> &'static str {
        "responses"
    }

    fn prepare(&self, messages: &[Message], phase: Phase) -> Prepared {
        self.prepare_with_tools(messages, flatten_tools(phase))
    }

    fn prepare_tool_free(&self, messages: &[Message], _phase: Phase) -> Prepared {
        self.prepare_with_tools(messages, Vec::new())
    }

    fn decode(&self, raw: RawResponse) -> Result<Message, CallError> {
        let status = raw.status;
        // Parse to a `Value` first so a body that is valid JSON but not an
        // object (serde's derived struct deserialization accepts a sequence) is
        // still rejected as a transport error.
        let value: Value =
            serde_json::from_str(&raw.body).map_err(|e| transport_error(status, &e))?;
        if !value.is_object() {
            let e: serde_json::Error =
                serde::de::Error::custom("invalid type: expected a JSON object");
            return Err(transport_error(status, &e));
        }
        let response: ResponsesResponse =
            serde_json::from_value(value).map_err(|e| transport_error(status, &e))?;

        if !(200..300).contains(&status) {
            let message = response
                .error
                .as_ref()
                .map(ApiError::message)
                .unwrap_or_else(|| raw.body.clone());
            return Err(CallError::Api { status, message });
        }

        // A 2xx response carrying an `error` object is still an API error.
        if let Some(error) = &response.error {
            return Err(CallError::Api {
                status,
                message: error.message(),
            });
        }

        let output = response.output.ok_or_else(no_output_items)?;

        let mut text = String::new();
        let mut calls = Vec::new();
        for item in &output {
            match item.get("type").and_then(Value::as_str) {
                Some("function_call") => calls.push(ToolCall {
                    id: string_field(item, "call_id"),
                    kind: "function".to_string(),
                    function: api::FunctionCall {
                        name: string_field(item, "name"),
                        arguments: string_field(item, "arguments"),
                    },
                }),
                Some("message") => text.push_str(&output_text(item)),
                // Reasoning items and item types we do not model carry nothing
                // the loop replays, so they are ignored.
                _ => {}
            }
        }

        let content = (!text.is_empty()).then_some(text);
        let tool_calls = (!calls.is_empty()).then_some(calls);
        if content.is_none() && tool_calls.is_none() {
            return Err(no_output_items());
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

/// The one malformed-response message, shared by a missing `output` and an
/// array that yields neither text nor calls.
fn no_output_items() -> CallError {
    CallError::Malformed("the response carried no output items".to_string())
}

/// The one transport-error shape, matching the chat provider's wording.
fn transport_error(status: i32, error: &serde_json::Error) -> CallError {
    CallError::Transport(format!(
        "invalid JSON response (HTTP {}): {}",
        status, error
    ))
}

/// Project the chat-shaped transcript into responses `input` items, in order,
/// one item per source element. `reasoning_content`, `extra`, `tool_calls` and
/// `tool_call_id` are never sent as such: the narration becomes an assistant
/// content item, calls become `function_call` items and tool results become
/// `function_call_output` items.
fn project(messages: &[Message]) -> Vec<Value> {
    let mut input = Vec::new();

    for message in messages {
        match message.role {
            Role::System | Role::User => {
                let role = if message.role == Role::System {
                    "system"
                } else {
                    "user"
                };
                input.push(json!({
                    "role": role,
                    "content": message.content.as_deref().unwrap_or(""),
                }));
            }
            Role::Assistant => {
                // Only non-blank narration is a content item; a tool-only turn
                // contributes just its calls.
                if let Some(text) = message.content.as_deref()
                    && !text.trim().is_empty()
                {
                    input.push(json!({ "role": "assistant", "content": text }));
                }
                if let Some(calls) = &message.tool_calls {
                    for call in calls {
                        input.push(json!({
                            "type": "function_call",
                            "call_id": call.id,
                            "name": call.function.name,
                            "arguments": call.function.arguments,
                        }));
                    }
                }
            }
            Role::Tool => {
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": message.tool_call_id.as_deref().unwrap_or(""),
                    "output": message.content.as_deref().unwrap_or(""),
                }));
            }
        }
    }

    input
}

/// Flatten the phase's tool definitions into the responses wire shape:
/// `{"type","name","description","parameters"}`, with no nested `function`
/// object.
fn flatten_tools(phase: Phase) -> Vec<Value> {
    Tool::definitions(phase)
        .into_iter()
        .map(|definition| {
            json!({
                "type": definition.kind,
                "name": definition.function.name,
                "description": definition.function.description,
                "parameters": serde_json::to_value(&definition.function.parameters)
                    .unwrap_or(Value::Null),
            })
        })
        .collect()
}

/// Read a string field, defaulting to the empty string when it is missing or
/// is not a string: a degenerate item can never panic.
fn string_field(item: &Value, key: &str) -> String {
    item.get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// Concatenate a `message` item's `output_text` parts, ignoring every other
/// part type.
fn output_text(item: &Value) -> String {
    let mut text = String::new();
    if let Some(parts) = item.get("content").and_then(Value::as_array) {
        for part in parts {
            if part.get("type").and_then(Value::as_str) == Some("output_text")
                && let Some(chunk) = part.get("text").and_then(Value::as_str)
            {
                text.push_str(chunk);
            }
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Auth, FunctionCall, ToolCall};
    use crate::llm::factory::ProviderSettings;
    use serde_json::Value;

    /// A provider whose transport coordinates do not matter to `prepare`.
    fn provider(reasoning_effort: Option<&str>) -> ResponsesProvider {
        ResponsesProvider::new(ProviderSettings {
            url: "http://example.test/v1/responses".to_string(),
            model: "test-model".to_string(),
            api_key: Some("secret".to_string()),
            auth_disabled: false,
            reasoning_effort: reasoning_effort.map(str::to_string),
            input_price_per_million_usd: None,
        })
    }

    fn raw(status: i32, body: &str) -> RawResponse {
        RawResponse {
            status,
            body: body.to_string(),
        }
    }

    fn decoded(provider: &ResponsesProvider, raw: RawResponse) -> Message {
        match provider.decode(raw) {
            Ok(message) => message,
            Err(error) => panic!("expected a decoded message, got {}", error),
        }
    }

    fn decode_error(provider: &ResponsesProvider, raw: RawResponse) -> CallError {
        match provider.decode(raw) {
            Ok(_) => panic!("expected a decode error"),
            Err(error) => error,
        }
    }

    /// The request body's keys, sorted, for the "no other key" assertion.
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

    /// System and user messages project to `{"role","content"}` verbatim.
    #[test]
    fn projection_carries_system_and_user_text_verbatim() {
        let messages = vec![Message::system("sys"), Message::user("hi")];
        let prepared = provider(None).prepare(&messages, Phase::Code);

        assert_eq!(
            prepared.outgoing.body["input"],
            json!([
                {"role": "system", "content": "sys"},
                {"role": "user", "content": "hi"},
            ])
        );
    }

    /// Absent text is the empty string, never `null`.
    #[test]
    fn projection_uses_the_empty_string_for_absent_text() {
        let messages = vec![Message {
            role: Role::System,
            content: None,
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            extra: Map::new(),
        }];
        let prepared = provider(None).prepare(&messages, Phase::Code);

        assert_eq!(prepared.outgoing.body["input"][0]["content"], json!(""));
        assert!(!prepared.outgoing.body["input"][0]["content"].is_null());
    }

    /// Narration comes first, then one `function_call` per entry, then the
    /// tool result -- all in transcript order, with both call ids taken from
    /// their source fields.
    #[test]
    fn projection_keeps_order_and_takes_call_ids_from_their_sources() {
        let messages = vec![
            Message::user("go"),
            assistant(
                Some("let me check"),
                Some(vec![call("call-1", "bash", r#"{"command":"ls"}"#)]),
            ),
            Message::tool("call-1", "file.txt"),
        ];
        let prepared = provider(None).prepare(&messages, Phase::Code);

        assert_eq!(
            prepared.outgoing.body["input"],
            json!([
                {"role": "user", "content": "go"},
                {"role": "assistant", "content": "let me check"},
                {
                    "type": "function_call",
                    "call_id": "call-1",
                    "name": "bash",
                    "arguments": "{\"command\":\"ls\"}",
                },
                {"type": "function_call_output", "call_id": "call-1", "output": "file.txt"},
            ])
        );
    }

    /// A tool-only assistant turn carries no content item.
    #[test]
    fn projection_skips_blank_assistant_narration() {
        let messages = vec![assistant(
            Some("   "),
            Some(vec![call("call-1", "bash", "{}")]),
        )];
        let prepared = provider(None).prepare(&messages, Phase::Code);

        assert_eq!(
            prepared.outgoing.body["input"],
            json!([
                {"type": "function_call", "call_id": "call-1", "name": "bash", "arguments": "{}"},
            ])
        );
    }

    /// The narration is sent even when the turn also calls tools: the blank
    /// check only drops whitespace, never a real message.
    #[test]
    fn projection_sends_narration_alongside_calls() {
        let messages = vec![assistant(
            Some("working"),
            Some(vec![call("call-2", "bash", "{}")]),
        )];
        let prepared = provider(None).prepare(&messages, Phase::Code);

        assert_eq!(
            prepared.outgoing.body["input"],
            json!([
                {"role": "assistant", "content": "working"},
                {"type": "function_call", "call_id": "call-2", "name": "bash", "arguments": "{}"},
            ])
        );
    }

    /// Fields the responses wire does not model never leak into `input`.
    #[test]
    fn projection_never_sends_reasoning_content_or_extra() {
        let assistant = Message {
            role: Role::Assistant,
            content: Some("answer".to_string()),
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: Some("hidden chain of thought".to_string()),
            extra: serde_json::from_value(json!({"foo": "bar", "tool_call_id": "nope"})).unwrap(),
        };
        let prepared = provider(None).prepare(&[assistant], Phase::Code);
        let serialized = serde_json::to_string(&prepared.outgoing.body["input"]).unwrap();

        assert!(
            !serialized.contains("hidden chain of thought"),
            "reasoning leaked: {serialized}"
        );
        assert!(!serialized.contains("foo"), "extra leaked: {serialized}");
        assert!(
            !serialized.contains("nope"),
            "stray id leaked: {serialized}"
        );
        assert_eq!(
            prepared.outgoing.body["input"],
            json!([{"role": "assistant", "content": "answer"}])
        );
    }

    /// Without a configured effort the body carries exactly `model`, `input`,
    /// `tools` and `store`, and no other key.
    #[test]
    fn prepare_emits_exactly_model_input_tools_and_store_without_effort() {
        let prepared = provider(None).prepare(&[Message::user("hi")], Phase::Code);
        let body = &prepared.outgoing.body;

        assert_eq!(body["model"], json!("test-model"));
        assert_eq!(body["store"], json!(false));
        assert!(body.get("reasoning").is_none());
        assert!(body.get("temperature").is_none());
        assert!(body.get("top_p").is_none());
        assert!(body.get("thinking").is_none());
        assert_eq!(body_keys(body), vec!["input", "model", "store", "tools"]);
    }

    /// A configured, non-`none` effort becomes a `reasoning` object -- and
    /// still adds no `temperature`, `top_p` or `thinking`.
    #[test]
    fn prepare_sends_reasoning_for_a_non_none_effort() {
        let prepared = provider(Some("high")).prepare(&[Message::user("hi")], Phase::Plan);
        let body = &prepared.outgoing.body;

        assert_eq!(body["reasoning"], json!({"effort": "high"}));
        assert_eq!(
            body_keys(body),
            vec!["input", "model", "reasoning", "store", "tools"]
        );
        assert!(body.get("temperature").is_none());
        assert!(body.get("top_p").is_none());
        assert!(body.get("thinking").is_none());
    }

    /// `none`, in any case and around whitespace, means no `reasoning` key.
    #[test]
    fn prepare_omits_reasoning_for_the_none_effort_case_insensitively() {
        for effort in ["none", "NONE", "NoNe", " none ", "\tnone\n"] {
            let prepared = provider(Some(effort)).prepare(&[Message::user("hi")], Phase::Code);
            let body = &prepared.outgoing.body;

            assert!(body.get("reasoning").is_none(), "effort {effort:?}");
            assert_eq!(body_keys(body), vec!["input", "model", "store", "tools"]);
        }
    }

    /// Every tool is flat: four keys, no nested `function` object, and the
    /// parameters are the generated schema serialised straight through.
    #[test]
    fn prepare_emits_flat_tool_entries() {
        let prepared = provider(None).prepare(&[Message::user("hi")], Phase::Code);
        let tools = prepared.outgoing.body["tools"]
            .as_array()
            .expect("the tools are an array");

        let bash = &tools[0];
        assert_eq!(bash["type"], json!("function"));
        assert_eq!(bash["name"], json!("bash"));
        assert_eq!(bash["description"], json!("Runs a bash command."));
        assert!(bash.get("function").is_none());
        assert_eq!(bash.as_object().unwrap().len(), 4);
        assert_eq!(
            bash["parameters"],
            serde_json::to_value(&Tool::definitions(Phase::Code)[0].function.parameters).unwrap()
        );
    }

    /// The advertised names follow the phase, exactly as `Tool::definitions`
    /// builds them.
    #[test]
    fn prepare_advertises_the_phases_tools() {
        let prepared = provider(None).prepare(&[Message::user("hi")], Phase::Spec);
        let names: Vec<&str> = prepared.outgoing.body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();

        assert_eq!(names, ["execute_script", "finish_spec"]);
    }

    /// The outgoing request carries the settings' URL, key and auth choice.
    #[test]
    fn prepare_carries_the_settings_transport() {
        let prepared = provider(None).prepare(&[Message::user("hi")], Phase::Code);

        assert_eq!(prepared.outgoing.url, "http://example.test/v1/responses");
        assert_eq!(prepared.outgoing.auth, Auth::Bearer("secret".to_string()));
    }

    /// `context_bytes` is exactly the encoded length of the projected input.
    #[test]
    fn context_bytes_matches_the_encoded_input() {
        let prepared =
            provider(None).prepare(&[Message::system("sys"), Message::user("hi")], Phase::Code);

        assert_eq!(
            prepared.context_bytes,
            serde_json::to_string(&prepared.outgoing.body["input"])
                .unwrap()
                .len()
        );
    }

    /// A `function_call` item becomes a tool call with `kind: "function"`.
    #[test]
    fn decode_maps_function_call_items_to_tool_calls() {
        let body = r#"{"output":[{"type":"function_call","call_id":"call-1","name":"bash","arguments":"{\"command\":\"ls\"}"}]}"#;
        let message = decoded(&provider(None), raw(200, body));

        assert_eq!(message.role, Role::Assistant);
        assert!(message.content.is_none());
        assert!(message.tool_call_id.is_none());
        assert!(message.reasoning_content.is_none());
        assert!(message.extra.is_empty());

        let calls = message.tool_calls.expect("the tool calls survive");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call-1");
        assert_eq!(calls[0].kind, "function");
        assert_eq!(calls[0].function.name, "bash");
        assert_eq!(calls[0].function.arguments, r#"{"command":"ls"}"#);
    }

    /// A `message` item concatenates its `output_text` parts and ignores the
    /// other part types.
    #[test]
    fn decode_concatenates_output_text_parts_and_ignores_other_parts() {
        let body = r#"{"output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Hello, "},{"type":"refusal","refusal":"nope"},{"type":"output_text","text":"world"}]}]}"#;
        let message = decoded(&provider(None), raw(200, body));

        assert_eq!(message.content.as_deref(), Some("Hello, world"));
        assert!(message.tool_calls.is_none());
    }

    /// Text from several `message` items is concatenated too.
    #[test]
    fn decode_concatenates_text_across_message_items() {
        let body = r#"{"output":[{"type":"message","content":[{"type":"output_text","text":"one"}]},{"type":"message","content":[{"type":"output_text","text":"two"}]}]}"#;
        let message = decoded(&provider(None), raw(200, body));

        assert_eq!(message.content.as_deref(), Some("onetwo"));
    }

    /// `reasoning` items and unknown item types carry nothing to replay.
    #[test]
    fn decode_ignores_reasoning_and_unknown_items() {
        let body = r#"{"output":[{"type":"reasoning","summary":[]},{"type":"web_search_call","id":"ws-1"},{"type":"message","content":[{"type":"output_text","text":"done"}]}]}"#;
        let message = decoded(&provider(None), raw(200, body));

        assert_eq!(message.content.as_deref(), Some("done"));
        assert!(message.tool_calls.is_none());
    }

    /// A text-only reply has content and no tool calls.
    #[test]
    fn decode_text_only_reply_has_content_and_no_tool_calls() {
        let body =
            r#"{"output":[{"type":"message","content":[{"type":"output_text","text":"hello"}]}]}"#;
        let message = decoded(&provider(None), raw(200, body));

        assert_eq!(message.content.as_deref(), Some("hello"));
        assert!(message.tool_calls.is_none());
    }

    /// A call-only reply has tool calls and no content.
    #[test]
    fn decode_call_only_reply_has_tool_calls_and_no_content() {
        let body = r#"{"output":[{"type":"function_call","call_id":"call-1","name":"bash","arguments":"{}"}]}"#;
        let message = decoded(&provider(None), raw(200, body));

        assert!(message.content.is_none());
        assert_eq!(message.tool_calls.expect("the tool calls survive").len(), 1);
    }

    /// Empty text is `content: None`, even when a call is present.
    #[test]
    fn decode_empty_text_gives_no_content() {
        let body = r#"{"output":[{"type":"message","content":[{"type":"output_text","text":""}]},{"type":"function_call","call_id":"call-1","name":"bash","arguments":"{}"}]}"#;
        let message = decoded(&provider(None), raw(200, body));

        assert!(message.content.is_none());
        assert_eq!(message.tool_calls.expect("the tool calls survive").len(), 1);
    }

    /// An `output` array with nothing to replay is malformed.
    #[test]
    fn decode_rejects_an_empty_output_array_as_malformed() {
        match decode_error(&provider(None), raw(200, r#"{"output":[]}"#)) {
            CallError::Malformed(message) => {
                assert_eq!(message, "the response carried no output items");
            }
            other => panic!("expected a malformed error, got {}", other),
        }
    }

    /// An array that yields neither text nor calls is malformed too.
    #[test]
    fn decode_rejects_output_with_neither_text_nor_calls_as_malformed() {
        let body = r#"{"output":[{"type":"reasoning","summary":[]}]}"#;
        match decode_error(&provider(None), raw(200, body)) {
            CallError::Malformed(message) => {
                assert_eq!(message, "the response carried no output items");
            }
            other => panic!("expected a malformed error, got {}", other),
        }
    }

    /// A missing `output` key is malformed.
    #[test]
    fn decode_rejects_a_missing_output_key_as_malformed() {
        match decode_error(&provider(None), raw(200, r#"{"id":"resp_1"}"#)) {
            CallError::Malformed(message) => {
                assert_eq!(message, "the response carried no output items");
            }
            other => panic!("expected a malformed error, got {}", other),
        }
    }

    /// A non-2xx body with an `error` object reports that object's message.
    #[test]
    fn decode_maps_a_non_2xx_error_object_to_an_api_error() {
        let body = r#"{"error":{"message":"no such model"}}"#;
        match decode_error(&provider(None), raw(404, body)) {
            CallError::Api { status, message } => {
                assert_eq!(status, 404);
                assert_eq!(message, "no such model");
            }
            other => panic!("expected an API error, got {}", other),
        }
    }

    /// A non-2xx body with no `error` object falls back to the raw body.
    #[test]
    fn decode_falls_back_to_the_raw_body_without_an_error_object() {
        let body = r#"{"detail":"bad request"}"#;
        match decode_error(&provider(None), raw(400, body)) {
            CallError::Api { status, message } => {
                assert_eq!(status, 400);
                assert_eq!(message, body);
            }
            other => panic!("expected an API error, got {}", other),
        }
    }

    /// A 2xx body carrying an `error` object is still an API error.
    #[test]
    fn decode_treats_a_2xx_body_carrying_error_as_an_api_error() {
        let body = r#"{"error":{"message":"overloaded"},"output":[]}"#;
        match decode_error(&provider(None), raw(200, body)) {
            CallError::Api { status, message } => {
                assert_eq!(status, 200);
                assert_eq!(message, "overloaded");
            }
            other => panic!("expected an API error, got {}", other),
        }
    }

    /// A body that is not JSON is a transport error naming the status.
    #[test]
    fn decode_rejects_a_non_json_body_as_a_transport_error() {
        match decode_error(&provider(None), raw(200, "not json")) {
            CallError::Transport(message) => assert!(
                message.starts_with("invalid JSON response (HTTP 200): "),
                "unexpected message: {message}"
            ),
            other => panic!("expected a transport error, got {}", other),
        }
    }

    /// JSON that is not an object is a transport error too.
    #[test]
    fn decode_rejects_json_that_is_not_an_object_as_a_transport_error() {
        for body in ["[]", "\"hello\"", "42", "null", "true"] {
            match decode_error(&provider(None), raw(200, body)) {
                CallError::Transport(message) => assert!(
                    message.starts_with("invalid JSON response (HTTP 200): "),
                    "body {body}: unexpected message: {message}"
                ),
                other => panic!("body {body}: expected a transport error, got {}", other),
            }
        }
    }

    /// A `function_call` with absent fields defaults each to the empty string
    /// rather than panicking.
    #[test]
    fn decode_defaults_degenerate_function_call_fields_to_the_empty_string() {
        let body = r#"{"output":[{"type":"function_call"}]}"#;
        let message = decoded(&provider(None), raw(200, body));

        let calls = message.tool_calls.expect("the degenerate call survives");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "");
        assert_eq!(calls[0].kind, "function");
        assert_eq!(calls[0].function.name, "");
        assert_eq!(calls[0].function.arguments, "");
    }

    /// Non-string `function_call` fields are ignored the same way.
    #[test]
    fn decode_defaults_non_string_function_call_fields_to_the_empty_string() {
        let body =
            r#"{"output":[{"type":"function_call","call_id":7,"name":null,"arguments":[]}]}"#;
        let message = decoded(&provider(None), raw(200, body));

        let calls = message.tool_calls.expect("the degenerate call survives");
        assert_eq!(calls[0].id, "");
        assert_eq!(calls[0].function.name, "");
        assert_eq!(calls[0].function.arguments, "");
    }

    /// The provider's name is stable and used by the factory.
    #[test]
    fn the_provider_names_the_responses_provider() {
        assert_eq!(provider(Some("high")).name(), "responses");
    }
}
