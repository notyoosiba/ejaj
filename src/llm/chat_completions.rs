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

        response
            .choices
            .into_iter()
            .next()
            .map(|choice| choice.message)
            .ok_or_else(|| {
                CallError::Malformed("the response carried no choices[0].message".to_string())
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Auth, Role};
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
}
