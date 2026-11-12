//! The chat-completions provider: LM Studio, DeepSeek and anything else that
//! speaks `POST /v1/chat/completions`.
//!
//! [`ChatProvider::prepare`] builds the request and wraps it in an
//! [`Outgoing`]; [`ChatProvider::decode`] turns one raw wire response into the
//! assistant [`Message`] the loop replays. Both are pure, so the provider is
//! exercised against literal JSON.

use crate::api::{
    self, CallError, ChatCompletionRequest, Message, Outgoing, RawResponse, Thinking,
};
use crate::llm::{LlmProvider, Prepared, factory};
use crate::tools::{Phase, Tool};

/// The sampling temperature every chat-completions request pins. The agent is a
/// deterministic workflow, so the wire asks for the most deterministic setting
/// the format offers.
const TEMPERATURE: f64 = 0.0;

/// A provider that speaks the OpenAI-compatible chat-completions wire format.
pub struct ChatProvider {
    settings: factory::ProviderSettings,
}

impl ChatProvider {
    pub fn new(settings: factory::ProviderSettings) -> ChatProvider {
        ChatProvider { settings }
    }

    /// Build one request with exactly `tools` advertised. The ordinary
    /// `prepare` passes the phase's tool list; the compaction-only
    /// `prepare_tool_free` passes an empty list, which is the wire's own
    /// "advertise no tools" form.
    fn prepare_with_tools(
        &self,
        messages: &[Message],
        tools: Vec<crate::api::ToolDefinition>,
    ) -> Prepared {
        // Thinking is only sent when explicitly requested; otherwise the
        // provider's own default applies. `none` (case-insensitively) disables
        // it.
        let thinking = self
            .settings
            .reasoning_effort
            .as_deref()
            .filter(|effort| effort.eq_ignore_ascii_case("none"))
            .map(|_| Thinking::disabled());

        let request = ChatCompletionRequest {
            model: self.settings.model.clone(),
            messages: messages.to_vec(),
            tools,
            temperature: TEMPERATURE,
            reasoning_effort: self.settings.reasoning_effort.clone(),
            thinking,
        };

        // `ChatCompletionRequest` is always serialisable, so this cannot fail;
        // if it somehow did, the request degrades to an empty body with no
        // recorded context rather than panicking.
        let (body, context_bytes) = match serde_json::to_value(&request) {
            Ok(body) => (body, request.context_len()),
            Err(_) => (serde_json::Value::Null, 0),
        };

        Prepared {
            outgoing: Outgoing {
                url: self.settings.url.clone(),
                auth: self.settings.auth(),
                body,
            },
            context_bytes,
        }
    }
}

impl LlmProvider for ChatProvider {
    fn name(&self) -> &'static str {
        "chat-completions"
    }

    fn prepare(&self, messages: &[Message], phase: Phase) -> Prepared {
        self.prepare_with_tools(messages, Tool::definitions(phase))
    }

    fn prepare_tool_free(&self, messages: &[Message], _phase: Phase) -> Prepared {
        self.prepare_with_tools(messages, Vec::new())
    }

    fn decode(&self, raw: RawResponse) -> Result<Message, CallError> {
        let status = raw.status;
        let response: api::ChatCompletionResponse =
            serde_json::from_str(&raw.body).map_err(|e| {
                CallError::Transport(format!("invalid JSON response (HTTP {}): {}", status, e))
            })?;

        if !(200..300).contains(&status) {
            let message = response
                .error
                .as_ref()
                .map(api::ApiError::message)
                .unwrap_or(raw.body);
            return Err(CallError::Api { status, message });
        }

        // A 2xx response carrying an `error` object is still an API error.
        if let Some(error) = &response.error {
            return Err(CallError::Api {
                status,
                message: error.message(),
            });
        }

        let usage = decode_chat_usage(response.usage.as_ref());
        response
            .choices
            .into_iter()
            .next()
            .map(|choice| {
                let mut message = choice.message;
                // Only the provider's exact `length` stop reason flags the
                // message as truncated; every other metadata shape, absent or
                // present, leaves it false.
                message.truncated = choice.finish_reason.as_deref() == Some("length");
                message.usage = usage;
                message
            })
            .ok_or_else(|| {
                CallError::Malformed("the response carried no choices[0].message".to_string())
            })
    }
}

/// Decode the chat-completions `usage` object tolerantly. Absent, `null`,
/// non-object, or wholly non-numeric usage yields no usage at all; each counter
/// that does not have a valid non-negative integer value simply contributes
/// zero. OpenAI's nested detail objects and DeepSeek's flat cache counter are
/// both recognized.
fn decode_chat_usage(raw_usage: Option<&serde_json::Value>) -> Option<api::TokenUsage> {
    let object = raw_usage?.as_object()?;
    let input_tokens = object
        .get("prompt_tokens")
        .and_then(serde_json::Value::as_u64);
    let cached_input_tokens = object
        .get("prompt_tokens_details")
        .and_then(serde_json::Value::as_object)
        .and_then(|details| details.get("cached_tokens"))
        .and_then(serde_json::Value::as_u64)
        .or_else(|| {
            object
                .get("prompt_cache_hit_tokens")
                .and_then(serde_json::Value::as_u64)
        });
    let output_tokens = object
        .get("completion_tokens")
        .and_then(serde_json::Value::as_u64);
    let reasoning_tokens = object
        .get("completion_tokens_details")
        .and_then(serde_json::Value::as_object)
        .and_then(|details| details.get("reasoning_tokens"))
        .and_then(serde_json::Value::as_u64);

    if input_tokens.is_none()
        && cached_input_tokens.is_none()
        && output_tokens.is_none()
        && reasoning_tokens.is_none()
    {
        return None;
    }

    Some(api::TokenUsage {
        input_tokens: input_tokens.unwrap_or(0),
        cached_input_tokens: cached_input_tokens.unwrap_or(0),
        output_tokens: output_tokens.unwrap_or(0),
        reasoning_tokens: reasoning_tokens.unwrap_or(0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Auth, Role, TokenUsage};
    use crate::llm::factory::ProviderSettings;
    use serde_json::{Value, json};

    /// A provider whose transport coordinates do not matter to `prepare`.
    fn provider(reasoning_effort: Option<&str>) -> ChatProvider {
        ChatProvider::new(ProviderSettings {
            url: "http://example.test/v1/chat/completions".to_string(),
            model: "test-model".to_string(),
            api_key: Some("secret".to_string()),
            auth_disabled: false,
            reasoning_effort: reasoning_effort.map(str::to_string),
            input_price_per_million_usd: None,
            cached_input_price_per_million_usd: None,
            output_price_per_million_usd: None,
        })
    }

    /// A provider whose auth configuration is exactly what a test needs.
    fn provider_with_auth(api_key: Option<&str>, auth_disabled: bool) -> ChatProvider {
        ChatProvider::new(ProviderSettings {
            url: "http://example.test/v1/chat/completions".to_string(),
            model: "test-model".to_string(),
            api_key: api_key.map(str::to_string),
            auth_disabled,
            reasoning_effort: None,
            input_price_per_million_usd: None,
            cached_input_price_per_million_usd: None,
            output_price_per_million_usd: None,
        })
    }

    fn raw(status: i32, body: &str) -> RawResponse {
        RawResponse {
            status,
            body: body.to_string(),
        }
    }

    fn decoded(provider: &ChatProvider, raw: RawResponse) -> Message {
        match provider.decode(raw) {
            Ok(message) => message,
            Err(error) => panic!("expected a decoded message, got {}", error),
        }
    }

    fn decode_error(provider: &ChatProvider, raw: RawResponse) -> CallError {
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

    /// Without a configured effort the body carries the pinned temperature
    /// besides the base keys: `model`, `messages`, `tools` and `temperature`.
    /// No `reasoning_effort`, no `thinking` and nothing else.
    #[test]
    fn prepare_matches_todays_body_for_an_unset_effort() {
        let messages = vec![Message::system("sys"), Message::user("hi")];
        let prepared = provider(None).prepare(&messages, Phase::Code);
        let body = &prepared.outgoing.body;

        assert_eq!(body["model"], json!("test-model"));
        assert_eq!(body["messages"], serde_json::to_value(&messages).unwrap());
        assert_eq!(
            body["tools"],
            serde_json::to_value(Tool::definitions(Phase::Code)).unwrap()
        );
        assert_eq!(body["temperature"], json!(0.0));
        assert_eq!(
            body_keys(body),
            vec!["messages", "model", "temperature", "tools"]
        );

        assert_eq!(
            prepared.outgoing.url,
            "http://example.test/v1/chat/completions"
        );
        assert_eq!(prepared.outgoing.auth, Auth::Bearer("secret".to_string()));

        assert_eq!(
            prepared.context_bytes,
            serde_json::to_string(&body["messages"]).unwrap().len()
        );
    }

    /// Auth disabled selects no authentication at all, even with a key
    /// configured on the provider.
    #[test]
    fn when_preparing_given_auth_disabled_then_selects_no_authentication() {
        let prepared =
            provider_with_auth(Some("secret"), true).prepare(&[Message::user("hi")], Phase::Code);

        assert_eq!(prepared.outgoing.auth, Auth::None);
    }

    /// A provider configured without a key selects no authentication, exactly as
    /// it does today: no Authorization header is produced from an absent key.
    #[test]
    fn when_preparing_given_no_key_then_selects_no_authentication() {
        let prepared = provider_with_auth(None, false).prepare(&[Message::user("hi")], Phase::Code);

        assert_eq!(prepared.outgoing.auth, Auth::None);
    }

    /// A configured effort is sent as-is, and `thinking` stays absent.
    #[test]
    fn prepare_sends_a_configured_effort_without_thinking() {
        let prepared = provider(Some("high")).prepare(&[Message::user("hi")], Phase::Plan);
        let body = &prepared.outgoing.body;

        assert_eq!(body["reasoning_effort"], json!("high"));
        assert!(body.get("thinking").is_none());
        assert_eq!(
            body_keys(body),
            vec![
                "messages",
                "model",
                "reasoning_effort",
                "temperature",
                "tools"
            ]
        );
        assert_eq!(
            prepared.context_bytes,
            serde_json::to_string(&body["messages"]).unwrap().len()
        );
    }

    /// An effort of `none`, in any case, disables thinking explicitly.
    #[test]
    fn prepare_disables_thinking_for_the_none_effort_case_insensitively() {
        for effort in ["none", "NONE"] {
            let prepared = provider(Some(effort)).prepare(&[Message::user("hi")], Phase::Tickets);
            let body = &prepared.outgoing.body;

            assert_eq!(body["reasoning_effort"], json!(effort), "effort {effort}");
            assert_eq!(
                body["thinking"],
                json!({ "type": "disabled" }),
                "effort {effort}"
            );
            assert_eq!(
                body_keys(body),
                vec![
                    "messages",
                    "model",
                    "reasoning_effort",
                    "temperature",
                    "thinking",
                    "tools"
                ],
                "effort {effort}"
            );
        }
    }

    /// The tool list follows the phase, exactly as `Tool::definitions` builds
    /// it.
    #[test]
    fn prepare_advertises_the_phases_tools() {
        let prepared = provider(None).prepare(&[Message::user("hi")], Phase::Spec);
        assert_eq!(prepared.outgoing.body["temperature"], json!(0.0));
        assert_eq!(
            prepared.outgoing.body["tools"],
            serde_json::to_value(Tool::definitions(Phase::Spec)).unwrap()
        );
    }

    /// A plain reply decodes to its `choices[0].message`.
    #[test]
    fn decode_returns_the_first_choice_message() {
        let body = r#"{"choices":[{"message":{"role":"assistant","content":"hello"}}]}"#;
        let message = decoded(&provider(None), raw(200, body));

        assert_eq!(message.role, Role::Assistant);
        assert_eq!(message.content.as_deref(), Some("hello"));
        assert!(message.tool_calls.is_none());
    }

    /// A reply carrying tool calls keeps them.
    #[test]
    fn decode_keeps_tool_calls() {
        let body = r#"{"choices":[{"message":{"role":"assistant","content":null,"tool_calls":[{"id":"call-1","type":"function","function":{"name":"bash","arguments":"{\"command\":\"ls\"}"}}]}}]}"#;
        let message = decoded(&provider(None), raw(200, body));

        let calls = message.tool_calls.expect("the tool calls survive");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call-1");
        assert_eq!(calls[0].kind, "function");
        assert_eq!(calls[0].function.name, "bash");
        assert_eq!(calls[0].function.arguments, r#"{"command":"ls"}"#);
    }

    /// The exact `length` finish reason marks the selected first choice as
    /// truncated while everything the provider sent survives: text only, tool
    /// calls only, and both together.
    #[test]
    fn when_first_choice_reports_length_given_text_and_tool_outputs_then_message_is_marked_truncated()
     {
        let text_only = r#"{"choices":[{"finish_reason":"length","message":{"role":"assistant","content":"a partial answer"}}]}"#;
        let message = decoded(&provider(None), raw(200, text_only));

        assert!(
            message.truncated,
            "an exact length finish must mark the message truncated"
        );
        assert_eq!(message.role, Role::Assistant);
        assert_eq!(message.content.as_deref(), Some("a partial answer"));
        assert!(message.tool_calls.is_none());

        let tool_only = r#"{"choices":[{"finish_reason":"length","message":{"role":"assistant","content":null,"tool_calls":[{"id":"call-1","type":"function","function":{"name":"bash","arguments":"{\"command\":\"ls\"}"}}]}}]}"#;
        let message = decoded(&provider(None), raw(200, tool_only));

        assert!(
            message.truncated,
            "a tool-only length finish must mark the message truncated"
        );
        assert_eq!(message.role, Role::Assistant);
        assert_eq!(message.content, None);
        let calls = message.tool_calls.expect("the tool calls survive");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call-1");
        assert_eq!(calls[0].kind, "function");
        assert_eq!(calls[0].function.name, "bash");
        assert_eq!(calls[0].function.arguments, r#"{"command":"ls"}"#);

        let both = r#"{"choices":[{"finish_reason":"length","message":{"role":"assistant","content":"calling a tool","tool_calls":[{"id":"call-2","type":"function","function":{"name":"bash","arguments":"{\"command\":\"pwd\"}"}}]}}]}"#;
        let message = decoded(&provider(None), raw(200, both));

        assert!(
            message.truncated,
            "a text-and-tool length finish must mark the message truncated"
        );
        assert_eq!(message.content.as_deref(), Some("calling a tool"));
        let calls = message.tool_calls.expect("the tool calls survive");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call-2");
        assert_eq!(calls[0].function.name, "bash");
        assert_eq!(calls[0].function.arguments, r#"{"command":"pwd"}"#);
    }

    /// An absent or expressly null finish reason is ordinary metadata: the
    /// first-choice message decodes untouched and is never marked truncated.
    #[test]
    fn when_finish_reason_is_missing_or_null_given_a_first_choice_then_message_is_not_truncated() {
        let missing = r#"{"choices":[{"message":{"role":"assistant","content":"hello"}}]}"#;
        let null = r#"{"choices":[{"finish_reason":null,"message":{"role":"assistant","content":"hello"}}]}"#;

        for body in [missing, null] {
            let message = decoded(&provider(None), raw(200, body));

            assert!(!message.truncated, "unexpected truncation for {body}");
            assert_eq!(message.role, Role::Assistant, "body {body}");
            assert_eq!(message.content.as_deref(), Some("hello"), "body {body}");
            assert!(message.tool_calls.is_none(), "body {body}");
        }
    }

    /// A finish reason that is not the exact JSON string `length` -- wrongly
    /// typed, empty, differently cased, or another stop reason -- is ignored.
    #[test]
    fn when_finish_reason_is_wrong_shaped_or_nonmatching_given_a_first_choice_then_message_is_not_truncated()
     {
        for finish in [
            r#"7"#,
            r#"true"#,
            r#"{"reason":"length"}"#,
            r#"["length"]"#,
            r#""""#,
            r#""stop""#,
            r#""tool_calls""#,
            r#""LENGTH""#,
            r#""lengths""#,
            r#"" length""#,
        ] {
            let body = format!(
                r#"{{"choices":[{{"finish_reason":{finish},"message":{{"role":"assistant","content":"hello"}}}}]}}"#
            );
            let message = decoded(&provider(None), raw(200, &body));

            assert!(!message.truncated, "unexpected truncation for {finish}");
            assert_eq!(message.role, Role::Assistant, "finish {finish}");
            assert_eq!(message.content.as_deref(), Some("hello"), "finish {finish}");
        }
    }

    /// Only the selected first choice decides truncation: a later choice's
    /// length finish never leaks onto the returned message.
    #[test]
    fn when_a_later_choice_reports_length_given_a_complete_first_choice_then_the_first_choice_is_returned_untruncated()
     {
        let body = r#"{"choices":[{"finish_reason":"stop","message":{"role":"assistant","content":"first"}},{"finish_reason":"length","message":{"role":"assistant","content":"second"}}]}"#;
        let message = decoded(&provider(None), raw(200, body));

        assert!(!message.truncated);
        assert_eq!(message.content.as_deref(), Some("first"));
    }

    /// A rejected call keeps its API error classification and message even when
    /// the payload also carries length finish metadata.
    #[test]
    fn when_provider_rejects_given_unauthorized_or_forbidden_responses_with_truncation_metadata_then_api_errors_are_preserved()
     {
        for (status, expected_message) in [(401, "invalid api key"), (403, "forbidden")] {
            let body = format!(
                r#"{{"error":{{"message":"{expected_message}"}},"choices":[{{"finish_reason":"length","message":{{"role":"assistant","content":"partial"}}}}]}}"#
            );

            match decode_error(&provider(None), raw(status, &body)) {
                CallError::Api {
                    status: reported_status,
                    message: reported_message,
                } => {
                    assert_eq!(reported_status, status);
                    assert_eq!(reported_message, expected_message);
                }
                other => panic!("expected an API error for {status}, got {}", other),
            }
        }
    }

    /// Truncation metadata never rescues a response that was already an error:
    /// the no-choice, malformed-choice, invalid-JSON and 2xx error-body
    /// classifications survive exactly as before.
    #[test]
    fn when_decoding_given_error_or_choice_free_bodies_with_length_metadata_then_preserves_each_original_error()
     {
        match decode_error(
            &provider(None),
            raw(
                200,
                r#"{"error":{"message":"overloaded"},"choices":[{"finish_reason":"length","message":{"role":"assistant","content":"partial"}}]}"#,
            ),
        ) {
            CallError::Api { status, message } => {
                assert_eq!(status, 200);
                assert_eq!(message, "overloaded");
            }
            other => panic!("expected an API error, got {}", other),
        }

        match decode_error(&provider(None), raw(200, r#"{"choices":[]}"#)) {
            CallError::Malformed(message) => {
                assert_eq!(message, "the response carried no choices[0].message");
            }
            other => panic!("expected a malformed error, got {}", other),
        }

        match decode_error(
            &provider(None),
            raw(200, r#"{"choices":[{"finish_reason":"length"}]}"#),
        ) {
            CallError::Transport(message) => assert!(
                message.starts_with("invalid JSON response (HTTP 200): "),
                "unexpected message: {message}"
            ),
            other => panic!("expected a transport error, got {}", other),
        }

        match decode_error(&provider(None), raw(200, "not json")) {
            CallError::Transport(message) => assert!(
                message.starts_with("invalid JSON response (HTTP 200): "),
                "unexpected message: {message}"
            ),
            other => panic!("expected a transport error, got {}", other),
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
        let body = r#"{"error":{"message":"overloaded"},"choices":[]}"#;
        match decode_error(&provider(None), raw(200, body)) {
            CallError::Api { status, message } => {
                assert_eq!(status, 200);
                assert_eq!(message, "overloaded");
            }
            other => panic!("expected an API error, got {}", other),
        }
    }

    /// An empty `choices` array is malformed, with the loop's exact message.
    #[test]
    fn decode_rejects_an_empty_choices_array_as_malformed() {
        match decode_error(&provider(None), raw(200, r#"{"choices":[]}"#)) {
            CallError::Malformed(message) => {
                assert_eq!(message, "the response carried no choices[0].message");
            }
            other => panic!("expected a malformed error, got {}", other),
        }
    }

    /// Core entry-point behavior: every counter a chat-completions provider
    /// reports maps independently onto the shared usage type.
    #[test]
    fn when_decoding_given_chat_completion_usage_then_maps_all_reported_counters() {
        let body = r#"{"choices":[{"finish_reason":"stop","message":{"role":"assistant","content":"hello"}}],"usage":{"prompt_tokens":10,"completion_tokens":4,"prompt_tokens_details":{"cached_tokens":3},"completion_tokens_details":{"reasoning_tokens":2}}}"#;
        let message = decoded(&provider(None), raw(200, body));

        assert_eq!(
            message.usage,
            Some(TokenUsage {
                input_tokens: 10,
                cached_input_tokens: 3,
                output_tokens: 4,
                reasoning_tokens: 2,
            })
        );
        assert_eq!(message.role, Role::Assistant);
        assert_eq!(message.content.as_deref(), Some("hello"));
        assert!(!message.truncated);
    }

    /// Edge-case table: a usage payload that is absent, null, wrongly shaped,
    /// negative, fractional, or otherwise non-numeric never fails an otherwise
    /// valid response. Valid counters survive independently; a recognized zero
    /// counts as valid; no valid counter means no usage at all.
    #[test]
    fn when_decoding_given_missing_or_malformed_usage_counters_then_keeps_valid_response_and_uses_only_valid_counts()
     {
        let zero = TokenUsage {
            input_tokens: 0,
            cached_input_tokens: 0,
            output_tokens: 0,
            reasoning_tokens: 0,
        };
        let cases: [(&str, Option<&str>, Option<TokenUsage>); 16] = [
            ("absent usage", None, None),
            ("null usage", Some("null"), None),
            ("numeric usage", Some("7"), None),
            ("array usage", Some("[]"), None),
            ("string usage", Some(r#""10""#), None),
            ("empty usage object", Some("{}"), None),
            ("negative counter", Some(r#"{"prompt_tokens":-1}"#), None),
            (
                "fractional counter",
                Some(r#"{"prompt_tokens":10.5}"#),
                None,
            ),
            ("string counter", Some(r#"{"prompt_tokens":"10"}"#), None),
            ("null counter", Some(r#"{"prompt_tokens":null}"#), None),
            (
                "non-object details",
                Some(r#"{"prompt_tokens_details":5,"completion_tokens_details":[]}"#),
                None,
            ),
            (
                "all counters invalid",
                Some(
                    r#"{"prompt_tokens":-10,"completion_tokens":-4.5,"prompt_tokens_details":{"cached_tokens":"3"},"completion_tokens_details":{"reasoning_tokens":2.5}}"#,
                ),
                None,
            ),
            (
                "valid zero input",
                Some(r#"{"prompt_tokens":0}"#),
                Some(zero),
            ),
            (
                "valid zero nested counters",
                Some(
                    r#"{"prompt_tokens_details":{"cached_tokens":0},"completion_tokens_details":{"reasoning_tokens":0}}"#,
                ),
                Some(zero),
            ),
            (
                "one valid counter",
                Some(
                    r#"{"prompt_tokens":-1,"completion_tokens":4,"prompt_tokens_details":{"cached_tokens":null}}"#,
                ),
                Some(TokenUsage {
                    input_tokens: 0,
                    cached_input_tokens: 0,
                    output_tokens: 4,
                    reasoning_tokens: 0,
                }),
            ),
            (
                "valid and invalid counters together",
                Some(
                    r#"{"prompt_tokens":10,"completion_tokens":"4","prompt_tokens_details":{"cached_tokens":-3},"completion_tokens_details":{"reasoning_tokens":2.5}}"#,
                ),
                Some(TokenUsage {
                    input_tokens: 10,
                    cached_input_tokens: 0,
                    output_tokens: 0,
                    reasoning_tokens: 0,
                }),
            ),
        ];

        for (case, usage, expected) in cases {
            let body = match usage {
                Some(usage) => format!(
                    r#"{{"choices":[{{"message":{{"role":"assistant","content":"hello"}}}}],"usage":{usage}}}"#
                ),
                None => r#"{"choices":[{"message":{"role":"assistant","content":"hello"}}]}"#
                    .to_string(),
            };
            let message = decoded(&provider(None), raw(200, &body));

            assert_eq!(message.role, Role::Assistant, "case {case}");
            assert_eq!(message.content.as_deref(), Some("hello"), "case {case}");
            assert!(!message.truncated, "case {case}");
            assert_eq!(message.usage, expected, "case {case}");
        }
    }

    /// Contract: OpenAI's nested cached count is authoritative when valid,
    /// while DeepSeek's root cache-hit counter is the fallback only when the
    /// nested count is absent or invalid.
    #[test]
    fn when_decoding_given_nested_cache_count_and_root_fallback_then_prefers_nested_and_uses_valid_fallback()
     {
        let cases: [(&str, &str, u64); 10] = [
            (
                "valid nested count",
                r#"{"prompt_tokens":10,"prompt_tokens_details":{"cached_tokens":3},"prompt_cache_hit_tokens":7}"#,
                3,
            ),
            (
                "absent nested count",
                r#"{"prompt_tokens":10,"prompt_cache_hit_tokens":7}"#,
                7,
            ),
            (
                "null nested count",
                r#"{"prompt_tokens":10,"prompt_tokens_details":{"cached_tokens":null},"prompt_cache_hit_tokens":7}"#,
                7,
            ),
            (
                "negative nested count",
                r#"{"prompt_tokens":10,"prompt_tokens_details":{"cached_tokens":-1},"prompt_cache_hit_tokens":7}"#,
                7,
            ),
            (
                "fractional nested count",
                r#"{"prompt_tokens":10,"prompt_tokens_details":{"cached_tokens":2.5},"prompt_cache_hit_tokens":7}"#,
                7,
            ),
            (
                "string nested count",
                r#"{"prompt_tokens":10,"prompt_tokens_details":{"cached_tokens":"3"},"prompt_cache_hit_tokens":7}"#,
                7,
            ),
            (
                "object nested count",
                r#"{"prompt_tokens":10,"prompt_tokens_details":{"cached_tokens":{}},"prompt_cache_hit_tokens":7}"#,
                7,
            ),
            (
                "non-object nested details",
                r#"{"prompt_tokens":10,"prompt_tokens_details":5,"prompt_cache_hit_tokens":7}"#,
                7,
            ),
            (
                "valid zero nested count",
                r#"{"prompt_tokens":10,"prompt_tokens_details":{"cached_tokens":0},"prompt_cache_hit_tokens":7}"#,
                0,
            ),
            (
                "no valid cache counter",
                r#"{"prompt_tokens":10,"prompt_tokens_details":{"cached_tokens":-1},"prompt_cache_hit_tokens":"7"}"#,
                0,
            ),
        ];

        for (case, usage, expected_cached) in cases {
            let body = format!(
                r#"{{"choices":[{{"message":{{"role":"assistant","content":"hello"}}}}],"usage":{usage}}}"#
            );
            let message = decoded(&provider(None), raw(200, &body));

            assert_eq!(
                message.usage,
                Some(TokenUsage {
                    input_tokens: 10,
                    cached_input_tokens: expected_cached,
                    output_tokens: 0,
                    reasoning_tokens: 0,
                }),
                "case {case}"
            );
            assert_eq!(message.content.as_deref(), Some("hello"), "case {case}");
        }
    }

    /// Regression: an exact `length` stop with no replayable content is still
    /// accepted, and its recognized usage survives the truncation signal.
    #[test]
    fn when_decoding_given_length_stop_with_valid_usage_then_retains_usage_without_replayable_content()
     {
        let body = r#"{"choices":[{"finish_reason":"length","message":{"role":"assistant","content":null}}],"usage":{"prompt_tokens":10,"completion_tokens":4,"prompt_tokens_details":{"cached_tokens":3},"completion_tokens_details":{"reasoning_tokens":2}}}"#;
        let message = decoded(&provider(None), raw(200, body));

        assert!(message.truncated);
        assert_eq!(message.content, None);
        assert!(message.tool_calls.is_none());
        assert_eq!(
            message.usage,
            Some(TokenUsage {
                input_tokens: 10,
                cached_input_tokens: 3,
                output_tokens: 4,
                reasoning_tokens: 2,
            })
        );
    }

    /// Regression: a rejected status is still the existing API error even when
    /// the body carries valid usage and a decodable choice.
    #[test]
    fn when_receiving_given_non_success_status_with_valid_usage_then_returns_the_existing_api_error()
     {
        let with_error_object = r#"{"error":{"message":"overloaded"},"choices":[{"finish_reason":"length","message":{"role":"assistant","content":"partial"}}],"usage":{"prompt_tokens":10,"completion_tokens":4}}"#;
        match decode_error(&provider(None), raw(503, with_error_object)) {
            CallError::Api { status, message } => {
                assert_eq!(status, 503);
                assert_eq!(message, "overloaded");
            }
            other => panic!("expected an API error, got {}", other),
        }

        let without_error_object = r#"{"detail":"bad request","usage":{"prompt_tokens":10}}"#;
        match decode_error(&provider(None), raw(400, without_error_object)) {
            CallError::Api { status, message } => {
                assert_eq!(status, 400);
                assert_eq!(message, without_error_object);
            }
            other => panic!("expected an API error, got {}", other),
        }
    }
}
