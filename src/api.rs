//! Typed contract for the OpenAI-compatible chat-completions API.
//!
//! Requests and responses are concrete structs; nothing in the message loop
//! pokes at `serde_json::Value`. The one escape hatch is [`Message::extra`],
//! which keeps fields a provider sends that we do not model, so an assistant
//! message can be replayed byte-for-byte.

use schemars::Schema;
use serde::{Deserialize, Serialize};
use serde_json::Map;

/// Who authored a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// Provider-reported token counts for one completed model response, or an
/// accumulated sum across multiple turns. Every counter is independent: a
/// saturated counter never wraps or borrows from another.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Total input/prompt tokens processed for the request, cached tokens
    /// included.
    pub input_tokens: u64,
    /// The subset of `input_tokens` served from the provider's prompt cache.
    pub cached_input_tokens: u64,
    /// Total output/completion tokens generated, including reasoning tokens
    /// where the provider counts them as output.
    pub output_tokens: u64,
    /// Reasoning/thinking tokens the provider reports separately. Informational:
    /// they remain part of `output_tokens` and are never billed twice.
    pub reasoning_tokens: u64,
}

impl TokenUsage {
    /// Accumulate `other` into `self`, saturating each counter independently at
    /// `u64::MAX` so a long-running conversation can never wrap a count.
    pub fn saturating_add(self, other: TokenUsage) -> TokenUsage {
        TokenUsage {
            input_tokens: self.input_tokens.saturating_add(other.input_tokens),
            cached_input_tokens: self
                .cached_input_tokens
                .saturating_add(other.cached_input_tokens),
            output_tokens: self.output_tokens.saturating_add(other.output_tokens),
            reasoning_tokens: self.reasoning_tokens.saturating_add(other.reasoning_tokens),
        }
    }

    /// Input tokens that were not served from the prompt cache. A cached count
    /// at or above the total input yields zero rather than underflowing.
    pub fn uncached_input_tokens(&self) -> u64 {
        self.input_tokens.saturating_sub(self.cached_input_tokens)
    }

    /// Estimate the USD cost of this usage at per-million-token rates.
    ///
    /// Uncached input is charged at the input rate, cached input at the
    /// cached-input rate when configured (falling back to the input rate
    /// otherwise), and output at the output rate. Reasoning tokens are
    /// informational and remain part of output, so they are never charged a
    /// second time.
    ///
    /// Returns `None` only when all three rates are absent: an unpriced
    /// provider stays unpriced rather than reporting a misleading zero.
    /// Otherwise a missing applicable rate contributes zero, and configured
    /// rates with zero usage return `Some(0.0)`.
    pub fn estimate_cost_usd(
        &self,
        input_price_per_million_usd: Option<f64>,
        cached_input_price_per_million_usd: Option<f64>,
        output_price_per_million_usd: Option<f64>,
    ) -> Option<f64> {
        if input_price_per_million_usd.is_none()
            && cached_input_price_per_million_usd.is_none()
            && output_price_per_million_usd.is_none()
        {
            return None;
        }

        let input_rate = input_price_per_million_usd.unwrap_or(0.0);
        let cached_rate = cached_input_price_per_million_usd
            .or(input_price_per_million_usd)
            .unwrap_or(0.0);
        let output_rate = output_price_per_million_usd.unwrap_or(0.0);

        let priced_tokens = self.uncached_input_tokens() as f64 * input_rate
            + self.cached_input_tokens as f64 * cached_rate
            + self.output_tokens as f64 * output_rate;

        Some(priced_tokens / 1_000_000.0)
    }
}

/// A single message in the conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    /// Assistant messages may carry a null `content` when they only call tools.
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Thinking-model output (DeepSeek). Echoed back on replay where present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    /// Any other field the provider sent, preserved verbatim.
    #[serde(flatten, default)]
    pub extra: Map<String, serde_json::Value>,
    /// Internal state: provider-reported token usage for this message, when
    /// the provider decoder recognized at least one counter. Never part of the
    /// wire and never counted by context sizing.
    #[serde(skip)]
    pub usage: Option<TokenUsage>,
    /// Internal state: whether a provider stopped this message at an output
    /// limit. Never part of the wire and never counted by context sizing.
    #[serde(skip)]
    pub truncated: bool,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Message {
        Message::text(Role::System, content)
    }

    pub fn user(content: impl Into<String>) -> Message {
        Message::text(Role::User, content)
    }

    pub fn assistant(content: impl Into<String>) -> Message {
        Message::text(Role::Assistant, content)
    }

    fn text(role: Role, content: impl Into<String>) -> Message {
        Message {
            role,
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            extra: Map::new(),
            usage: None,
            truncated: false,
        }
    }

    /// The result of a tool call, addressed back to the call it answers.
    pub fn tool(tool_call_id: &str, content: impl Into<String>) -> Message {
        Message {
            role: Role::Tool,
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: Some(tool_call_id.to_string()),
            reasoning_content: None,
            extra: Map::new(),
            usage: None,
            truncated: false,
        }
    }
}

/// A tool call emitted by the assistant.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionCall,
}

/// The function name and raw JSON arguments of a tool call. `arguments` is kept
/// as a string (what the spec mandates) and parsed per-tool on dispatch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

/// A tool definition sent to the API. `parameters` is a schema generated from
/// the tool's argument struct, never a hand-written object.
#[derive(Serialize)]
pub struct ToolDefinition {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub function: FunctionDefinition,
}

#[derive(Serialize)]
pub struct FunctionDefinition {
    pub name: &'static str,
    pub description: &'static str,
    pub parameters: Schema,
}

/// Disables thinking on providers that support it.
#[derive(Debug, Serialize)]
pub struct Thinking {
    #[serde(rename = "type")]
    pub kind: &'static str,
}

impl Thinking {
    pub fn disabled() -> Thinking {
        Thinking { kind: "disabled" }
    }
}

/// A chat-completions request.
#[derive(Serialize)]
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    /// Sampling temperature. Always sent, and always `0`: the agent is a
    /// deterministic workflow, so the wire pins the most deterministic setting
    /// the format offers.
    pub temperature: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<Thinking>,
}

impl ChatCompletionRequest {
    /// The bytes the transcript occupies once encoded: the context the request
    /// carries, excluding the model name and the tool schemas.
    ///
    /// This is derived from `self.messages` alone, through the same
    /// `serde_json` encoding the transport uses for the wire, so it is
    /// independent of [`model`](Self::model), [`tools`](Self::tools),
    /// `temperature`, `reasoning_effort` and `thinking`. The value is
    /// recomputed on each call
    /// rather than cached: encoding these types cannot fail, so the fallback is
    /// `0` and there is no `Result` and no error path.
    pub fn context_len(&self) -> usize {
        serde_json::to_string(&self.messages)
            .map(|json| json.len())
            .unwrap_or(0)
    }
}

/// A chat-completions response. A body that carries `error` -- with or without
/// a 2xx status -- is treated as a failure by the provider's decoder.
#[derive(Deserialize)]
pub struct ChatCompletionResponse {
    #[serde(default)]
    pub choices: Vec<Choice>,
    #[serde(default)]
    pub error: Option<ApiError>,
    /// The provider's raw token-usage object, decoded tolerantly by the chat
    /// provider so a malformed usage payload never discards a response.
    #[serde(default)]
    pub usage: Option<serde_json::Value>,
}

#[derive(Deserialize)]
pub struct Choice {
    pub message: Message,
    /// The provider's stop reason, retained as optional metadata. A wire that
    /// omits the field, sends `null`, or sends a value that is not a string
    /// must still decode, so anything but a JSON string becomes `None`.
    #[serde(default, deserialize_with = "finish_reason")]
    pub finish_reason: Option<String>,
}

/// Decode the optional `finish_reason` metadata tolerantly: an absent, `null`,
/// or wrongly typed value is `None` rather than a decoding failure, so a
/// provider's stray metadata never discards an otherwise usable response.
fn finish_reason<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    match Option::<serde_json::Value>::deserialize(deserializer)? {
        Some(serde_json::Value::String(reason)) => Ok(Some(reason)),
        _ => Ok(None),
    }
}

/// An API error body: `{"error": {...}}` or the rarer `{"error": "..."}`.
#[derive(Deserialize)]
#[serde(untagged)]
pub enum ApiError {
    Structured {
        #[serde(default)]
        message: Option<String>,
    },
    Text(String),
}

impl ApiError {
    pub fn message(&self) -> String {
        match self {
            ApiError::Structured { message: Some(m) } => m.clone(),
            ApiError::Structured { message: None } => "unknown API error".to_string(),
            ApiError::Text(text) => text.clone(),
        }
    }
}

/// Why a chat-completions call failed: the transport, an HTTP/API error
/// carrying the provider's status code and message, or a response that could
/// not be understood.
pub enum CallError {
    Transport(String),
    Api { status: i32, message: String },
    Malformed(String),
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallError::Transport(msg) => write!(f, "transport error: {}", msg),
            CallError::Api { status, message } => write!(f, "API error {}: {}", status, message),
            CallError::Malformed(msg) => write!(f, "malformed response: {}", msg),
        }
    }
}

/// A response as it came off the wire, before decoding: the HTTP status and the
/// raw body. Recording a transport exchange records exactly this.
#[derive(Debug, Clone)]
pub struct RawResponse {
    pub status: i32,
    pub body: String,
}

/// The Anthropic wire version every Anthropic request declares. The header value
/// is fixed by the wire, not configured, so it can only be right.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// How an [`Outgoing`] request authenticates. The scheme is typed, so a provider
/// selects exactly one and [`HttpTransport`] emits exactly that scheme's headers.
/// There is deliberately no way to ask for an arbitrary caller-supplied header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Auth {
    /// No authentication header at all: a keyless endpoint, a provider with
    /// auth disabled, or one configured without a key.
    None,
    /// `Authorization: Bearer <key>`, what the chat-completions and responses
    /// wires expect.
    Bearer(String),
    /// Anthropic's `x-api-key` plus the fixed [`ANTHROPIC_VERSION`] header.
    /// The key is carried there and nowhere else: no `Authorization` header
    /// accompanies it.
    Anthropic(String),
}

/// A request on its way to a provider: where it goes, how it authenticates, and
/// the body.
///
/// The body is *already encoded* as a `serde_json::Value`, which is what makes
/// the transport the same for every provider: [`HttpTransport`] only serialises
/// it, posts it, and adds the headers its [`Auth`] scheme mandates -- it never
/// knows which wire format the body belongs to.
pub struct Outgoing {
    pub url: String,
    pub auth: Auth,
    pub body: serde_json::Value,
}

/// Where an [`Outgoing`] request is sent. The one production implementation is
/// [`HttpTransport`]; tests inject a recording or replaying transport through
/// [`crate::Config`], which is what lets the agent loops run without a network.
pub trait Transport: Send + Sync {
    fn send(&self, outgoing: &Outgoing) -> Result<RawResponse, CallError>;
}

/// How long a production [`HttpTransport`] request may wait, in seconds.
///
/// A stalled provider connection becomes a [`CallError::Transport`] in bounded
/// time, so the retry policy can act on it instead of blocking forever.
pub const HTTP_TIMEOUT_SECS: u64 = 300;

/// The real transport: `minreq` over the outgoing request's URL.
pub struct HttpTransport;

impl HttpTransport {
    /// Send `outgoing`, giving the whole exchange at most `timeout_secs`
    /// seconds.
    ///
    /// The supplied value is forwarded unchanged to `minreq`, which is what
    /// lets a focused caller or a test use a short bound. Production callers go
    /// through [`Transport::send`], which supplies [`HTTP_TIMEOUT_SECS`].
    pub fn send_with_timeout(
        &self,
        outgoing: &Outgoing,
        timeout_secs: u64,
    ) -> Result<RawResponse, CallError> {
        let body = serde_json::to_string(&outgoing.body)
            .map_err(|e| CallError::Transport(format!("could not encode request: {}", e)))?;

        let mut http = minreq::post(&outgoing.url)
            .with_timeout(timeout_secs)
            .with_header("Content-Type", "application/json");
        http = match &outgoing.auth {
            Auth::None => http,
            Auth::Bearer(key) => http.with_header("Authorization", format!("Bearer {}", key)),
            Auth::Anthropic(key) => http
                .with_header("x-api-key", key.as_str())
                .with_header("anthropic-version", ANTHROPIC_VERSION),
        };

        let res = http
            .with_body(body)
            .send()
            .map_err(|e| CallError::Transport(e.to_string()))?;

        Ok(RawResponse {
            status: res.status_code,
            body: res.as_str().unwrap_or("").to_string(),
        })
    }
}

impl Transport for HttpTransport {
    fn send(&self, outgoing: &Outgoing) -> Result<RawResponse, CallError> {
        self.send_with_timeout(outgoing, HTTP_TIMEOUT_SECS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{Phase, Tool};
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener};
    use std::sync::mpsc;
    use std::thread::JoinHandle;
    use std::time::Duration;

    /// A request built by hand: the fields the measurement must ignore are
    /// supplied explicitly so a test can vary them on purpose.
    fn request(messages: Vec<Message>, tools: Vec<ToolDefinition>) -> ChatCompletionRequest {
        ChatCompletionRequest {
            model: "test-model".to_string(),
            messages,
            tools,
            temperature: 0.0,
            reasoning_effort: None,
            thinking: None,
        }
    }

    /// The measure is exactly the length of the transcript's own encoding.
    #[test]
    fn context_len_matches_the_encoded_transcript() {
        let request = request(
            vec![
                Message::system("you are a helpful agent"),
                Message::user("run the tests"),
                Message::tool("call-1", "all green"),
            ],
            Vec::new(),
        );

        assert_eq!(
            request.context_len(),
            serde_json::to_string(&request.messages).unwrap().len()
        );
    }

    /// The tool schemas are not part of the transcript, so they do not count.
    #[test]
    fn context_len_ignores_the_tool_list() {
        let messages = vec![Message::user("hello")];

        let without_tools = request(messages.clone(), Vec::new());
        let with_tools = request(messages, Tool::definitions(Phase::Code));

        assert_eq!(without_tools.context_len(), with_tools.context_len());
    }

    /// A longer transcript is a larger context.
    #[test]
    fn context_len_grows_when_a_message_is_added() {
        let mut request = request(vec![Message::user("first")], Vec::new());
        let before = request.context_len();

        request.messages.push(Message::user("second"));
        let after = request.context_len();

        assert!(
            after > before,
            "adding a message must grow the context: {before} -> {after}"
        );
    }

    /// The number is derived on demand, never cached on the request.
    #[test]
    fn context_len_is_recomputed_per_call() {
        let mut request = request(vec![Message::user("first")], Vec::new());
        let first = request.context_len();

        request.messages.push(Message::user("second"));
        let second = request.context_len();

        assert_ne!(
            first, second,
            "a mutated request must report a new context length"
        );
    }

    /// Core entry-point behavior at the minimum valid boundary: every public
    /// constructor yields an untruncated message.
    #[test]
    fn when_messages_are_constructed_given_public_message_helpers_then_truncation_defaults_to_false()
     {
        assert!(
            !Message::system("you are a helpful agent").truncated,
            "a system message starts untruncated"
        );
        assert!(
            !Message::user("run the tests").truncated,
            "a user message starts untruncated"
        );
        assert!(
            !Message::assistant("all green").truncated,
            "an assistant message starts untruncated"
        );
        assert!(
            !Message::tool("call-1", "all green").truncated,
            "a tool message starts untruncated"
        );
    }

    /// Empty/nil boundary: legacy wire JSON carries no internal state field, and
    /// decoding it must still produce an untruncated message.
    #[test]
    fn when_legacy_message_json_is_deserialized_given_no_internal_state_field_then_message_is_untruncated()
     {
        let message: Message =
            serde_json::from_str(r#"{"role":"assistant","content":"legacy reply"}"#)
                .expect("the legacy wire shape decodes");

        assert_eq!(message.role, Role::Assistant);
        assert_eq!(message.content.as_deref(), Some("legacy reply"));
        assert!(
            !message.truncated,
            "a message without the internal field is untruncated"
        );
    }

    /// Malformed/invalid boundary: the internal flag is serde-skipped, so neither
    /// value changes the wire JSON, which stays exactly the legacy shape.
    #[test]
    fn when_truncation_changes_given_a_message_is_encoded_then_wire_json_is_unchanged() {
        let expected_wire = serde_json::json!({
            "role": "tool",
            "content": "all green",
            "tool_call_id": "call-1",
        });
        let mut message = Message::tool("call-1", "all green");

        assert!(!message.truncated);
        assert_eq!(
            serde_json::to_value(&message).unwrap(),
            expected_wire,
            "an untruncated message keeps the legacy wire shape"
        );

        message.truncated = true;

        assert_eq!(
            serde_json::to_value(&message).unwrap(),
            expected_wire,
            "a truncated message keeps the legacy wire shape"
        );
        assert!(
            serde_json::to_value(&message)
                .unwrap()
                .get("truncated")
                .is_none(),
            "the internal flag must never be emitted on the wire"
        );
    }

    /// Maximum/upper boundary: toggling the internal flag changes neither the
    /// encoded transcript nor the context size derived from it.
    #[test]
    fn when_truncation_changes_given_a_transcript_is_sized_then_context_bytes_are_unchanged() {
        const EXPECTED_TRANSCRIPT: &str = concat!(
            r#"[{"role":"system","content":"you are a helpful agent"},"#,
            r#"{"role":"user","content":"run the tests"},"#,
            r#"{"role":"tool","content":"all green","tool_call_id":"call-1"}]"#,
        );
        let messages = vec![
            Message::system("you are a helpful agent"),
            Message::user("run the tests"),
            Message::tool("call-1", "all green"),
        ];
        let untruncated = request(messages.clone(), Vec::new());
        let untruncated_context = untruncated.context_len();

        let mut truncated_messages = messages;
        for message in &mut truncated_messages {
            message.truncated = true;
        }
        let truncated = request(truncated_messages, Vec::new());
        let truncated_context = truncated.context_len();

        assert_eq!(
            serde_json::to_string(&untruncated.messages).unwrap(),
            EXPECTED_TRANSCRIPT,
            "the untruncated transcript encodes to the known wire shape"
        );
        assert_eq!(
            serde_json::to_string(&truncated.messages).unwrap(),
            EXPECTED_TRANSCRIPT,
            "the truncated transcript encodes to the same known wire shape"
        );
        assert_eq!(
            untruncated_context,
            EXPECTED_TRANSCRIPT.len(),
            "the context size is the known transcript length"
        );
        assert_eq!(
            truncated_context, untruncated_context,
            "toggling the internal flag must not change the context size"
        );
    }

    /// Core entry-point behavior: every public ordinary message constructor
    /// starts with no provider usage attached.
    #[test]
    fn when_constructing_messages_given_non_provider_helpers_then_usage_is_unset() {
        assert_eq!(
            Message::system("you are a helpful agent").usage,
            None,
            "a system message carries no provider usage"
        );
        assert_eq!(
            Message::user("run the tests").usage,
            None,
            "a user message carries no provider usage"
        );
        assert_eq!(
            Message::assistant("all green").usage,
            None,
            "an assistant message carries no provider usage"
        );
        assert_eq!(
            Message::tool("call-1", "all green").usage,
            None,
            "a tool message carries no provider usage"
        );
    }

    /// Contract and invariant: internal usage is serde-skipped in both
    /// directions. Attaching it never changes the message wire shape, and a
    /// wire `usage` field never becomes the internal value.
    #[test]
    fn when_message_usage_is_encoded_given_internal_usage_then_wire_shape_is_unchanged() {
        const EXPECTED_WIRE: &str =
            r#"{"role":"tool","content":"all green","tool_call_id":"call-1"}"#;
        let mut message = Message::tool("call-1", "all green");
        assert_eq!(message.usage, None);
        assert_eq!(
            serde_json::to_string(&message).unwrap(),
            EXPECTED_WIRE,
            "an ordinary message keeps the legacy wire shape"
        );

        message.usage = Some(TokenUsage {
            input_tokens: 1_200,
            cached_input_tokens: 200,
            output_tokens: 340,
            reasoning_tokens: 40,
        });

        assert_eq!(
            serde_json::to_string(&message).unwrap(),
            EXPECTED_WIRE,
            "internal usage must not appear on the wire"
        );
        assert!(
            serde_json::to_value(&message)
                .unwrap()
                .get("usage")
                .is_none(),
            "the internal usage field must never be emitted"
        );

        let decoded: Message = serde_json::from_str(
            r#"{"role":"assistant","content":"legacy reply","usage":{"input_tokens":1200,"cached_input_tokens":200,"output_tokens":340,"reasoning_tokens":40}}"#,
        )
        .expect("a message carrying a usage field still decodes");
        assert_eq!(decoded.role, Role::Assistant);
        assert_eq!(decoded.content.as_deref(), Some("legacy reply"));
        assert_eq!(
            decoded.usage, None,
            "a wire usage field must not populate the internal field"
        );
    }

    /// Contract and invariant: only the internal usage changes, so neither the
    /// request's context length nor the provider-prepared context bytes move.
    #[test]
    fn when_sizing_a_request_given_only_internal_usage_changes_then_context_length_and_prepared_bytes_are_unchanged()
     {
        const EXPECTED_TRANSCRIPT: &str = concat!(
            r#"[{"role":"system","content":"you are a helpful agent"},"#,
            r#"{"role":"user","content":"run the tests"},"#,
            r#"{"role":"tool","content":"all green","tool_call_id":"call-1"}]"#,
        );
        let config = crate::Config::new(
            "http://example.test/v1/chat/completions",
            "test-model",
            None,
        );
        let provider = config.provider(Phase::Code);
        let mut messages = vec![
            Message::system("you are a helpful agent"),
            Message::user("run the tests"),
            Message::tool("call-1", "all green"),
        ];

        let without_usage = request(messages.clone(), Vec::new());
        let prepared_without_usage = provider.prepare(&messages, Phase::Code);
        assert_eq!(
            without_usage.context_len(),
            EXPECTED_TRANSCRIPT.len(),
            "the context length is the known transcript length"
        );
        assert_eq!(
            prepared_without_usage.context_bytes,
            EXPECTED_TRANSCRIPT.len(),
            "the provider-prepared bytes are the known transcript length"
        );

        for message in &mut messages {
            message.usage = Some(TokenUsage {
                input_tokens: 1_200,
                cached_input_tokens: 200,
                output_tokens: 340,
                reasoning_tokens: 40,
            });
        }
        let with_usage = request(messages.clone(), Vec::new());
        let prepared_with_usage = provider.prepare(&messages, Phase::Code);

        assert_eq!(
            with_usage.context_len(),
            EXPECTED_TRANSCRIPT.len(),
            "internal usage must not change the context length"
        );
        assert_eq!(
            prepared_with_usage.context_bytes,
            EXPECTED_TRANSCRIPT.len(),
            "internal usage must not change the provider-prepared bytes"
        );
        assert_eq!(
            prepared_with_usage.outgoing.body["messages"],
            serde_json::json!([
                {"role": "system", "content": "you are a helpful agent"},
                {"role": "user", "content": "run the tests"},
                {"role": "tool", "content": "all green", "tool_call_id": "call-1"},
            ]),
            "the prepared transcript keeps the known wire shape"
        );
        assert!(
            prepared_with_usage.outgoing.body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .all(|message| message.get("usage").is_none()),
            "no prepared message may carry the internal usage field"
        );
    }

    /// The `Outgoing` the loopback tests send: a minimal encoded body to a
    /// peer at `address`.
    fn outgoing_to(address: SocketAddr) -> Outgoing {
        Outgoing {
            url: format!("http://{}/v1/chat/completions", address),
            auth: Auth::None,
            body: serde_json::json!({"model": "loopback-probe", "messages": []}),
        }
    }

    /// Bind a free loopback port, then close it so connecting to its address is
    /// refused: an invalid destination with no peer behind it.
    fn closed_loopback_address() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").expect("the test binds a loopback port");
        let address = listener
            .local_addr()
            .expect("the bound loopback address is readable");
        drop(listener);
        address
    }

    /// A peer that accepts one connection, sends no HTTP response, and holds the
    /// connection open until the caller drops the returned sender. The thread
    /// only returns after that, so a peer-side close cannot be what ends the
    /// waiting request.
    fn stalling_peer() -> (SocketAddr, mpsc::Sender<()>, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("the peer binds a loopback port");
        let address = listener
            .local_addr()
            .expect("the bound loopback address is readable");
        let (release, released) = mpsc::channel::<()>();
        let peer = std::thread::spawn(move || {
            let (_stream, _) = listener
                .accept()
                .expect("the peer accepts the transport's connection");
            let _ = released.recv();
        });
        (address, release, peer)
    }

    /// A one-connection peer that reads the request and answers it with
    /// `status_line` and `body`, so the response path is exercised for real.
    fn answering_peer(
        status_line: &'static str,
        body: &'static str,
    ) -> (SocketAddr, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("the peer binds a loopback port");
        let address = listener
            .local_addr()
            .expect("the bound loopback address is readable");
        let peer = std::thread::spawn(move || {
            let (mut stream, _) = listener
                .accept()
                .expect("the peer accepts the transport's connection");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("the peer can bound its read");
            let mut received = Vec::new();
            let mut chunk = [0u8; 1024];
            let head_end = loop {
                if let Some(end) = first_index(&received, b"\r\n\r\n") {
                    break end;
                }
                let read = stream.read(&mut chunk).expect("the peer reads the request");
                assert!(
                    read > 0,
                    "the transport closed before the request head completed"
                );
                received.extend_from_slice(&chunk[..read]);
            };
            let head = String::from_utf8(received[..head_end].to_vec())
                .expect("the request head is UTF-8");
            let body_start = head_end + 4;
            let body_len = declared_content_length(&head);
            while received.len() < body_start + body_len {
                let read = stream
                    .read(&mut chunk)
                    .expect("the peer reads the request body");
                assert!(
                    read > 0,
                    "the transport closed before the request body completed"
                );
                received.extend_from_slice(&chunk[..read]);
            }
            let response = format!(
                "{}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                status_line,
                body.len(),
                body
            );
            stream
                .write_all(response.as_bytes())
                .expect("the peer answers the request");
            stream.flush().expect("the peer flushes its answer");
        });
        (address, peer)
    }

    /// The first index at which `needle` appears in `haystack`.
    fn first_index(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

    /// The body length the request head declares, `0` when it declares none.
    fn declared_content_length(head: &str) -> usize {
        head.lines()
            .skip(1)
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.trim().parse().ok())
            .unwrap_or(0)
    }

    /// Upper boundary: the production bound is exactly five minutes.
    #[test]
    fn when_production_timeout_is_defined_given_default_contract_then_equals_three_hundred_seconds()
    {
        assert_eq!(
            HTTP_TIMEOUT_SECS, 300,
            "production requests are bounded to five minutes"
        );
    }

    /// Nil case: zero seconds with an invalid destination still surfaces through
    /// the existing transport-error contract.
    #[test]
    fn when_invalid_destination_is_used_given_zero_timeout_then_returns_transport_error() {
        let outgoing = outgoing_to(closed_loopback_address());

        match HttpTransport.send_with_timeout(&outgoing, 0) {
            Err(CallError::Transport(_)) => {}
            Err(other) => panic!("an unreachable destination must fail at the transport: {other}"),
            Ok(response) => panic!(
                "an unreachable destination must not produce a response: status {}",
                response.status
            ),
        }
    }

    /// Core entry-point behavior at the minimum valid boundary: a peer that
    /// stays open and silent makes the one-second request fail as a transport
    /// error, not as a response and not because the peer closed.
    #[test]
    fn when_provider_stalls_given_one_second_timeout_then_returns_transport_error() {
        let (address, release, peer) = stalling_peer();
        let outgoing = outgoing_to(address);

        let result = HttpTransport.send_with_timeout(&outgoing, 1);

        drop(release);
        peer.join()
            .expect("the peer thread stops after the request times out");

        match result {
            Err(CallError::Transport(_)) => {}
            Err(other) => panic!("a stalled provider must surface as a transport error: {other}"),
            Ok(response) => panic!(
                "a stalled provider must not produce a response: status {}",
                response.status
            ),
        }
    }

    /// Forbidden-access boundary: an HTTP error status is still a raw response,
    /// with its status and body preserved, not a timeout failure.
    #[test]
    fn when_provider_denies_access_given_http_error_response_then_preserves_raw_status_and_body() {
        const DENIED_BODY: &str = r#"{"error":{"message":"forbidden"}}"#;
        let (address, peer) = answering_peer("HTTP/1.1 403 Forbidden", DENIED_BODY);
        let outgoing = outgoing_to(address);

        let result = HttpTransport.send_with_timeout(&outgoing, 5);
        peer.join().expect("the peer answers and stops");

        match result {
            Ok(raw) => {
                assert_eq!(raw.status, 403, "the provider's status reaches the caller");
                assert_eq!(
                    raw.body, DENIED_BODY,
                    "the provider's body reaches the caller"
                );
            }
            Err(error) => panic!("an HTTP error status must stay a response: {error}"),
        }
    }

    /// Core entry-point behavior at the upper boundary: each of the four
    /// counters saturates independently at `u64::MAX` and never wraps.
    #[test]
    fn when_accumulating_usage_given_counters_near_the_upper_limit_then_each_counter_saturates_independently()
     {
        let near_limit = TokenUsage {
            input_tokens: u64::MAX - 1,
            cached_input_tokens: u64::MAX,
            output_tokens: 7,
            reasoning_tokens: u64::MAX / 2,
        };
        let additional = TokenUsage {
            input_tokens: 10,
            cached_input_tokens: 1,
            output_tokens: u64::MAX,
            reasoning_tokens: u64::MAX,
        };

        let sum = near_limit.saturating_add(additional);

        assert_eq!(sum.input_tokens, u64::MAX, "input saturates at the limit");
        assert_eq!(
            sum.cached_input_tokens,
            u64::MAX,
            "cached input saturates at the limit"
        );
        assert_eq!(sum.output_tokens, u64::MAX, "output saturates at the limit");
        assert_eq!(
            sum.reasoning_tokens,
            u64::MAX,
            "reasoning saturates at the limit"
        );

        let ordinary = TokenUsage {
            input_tokens: 1,
            cached_input_tokens: 2,
            output_tokens: 3,
            reasoning_tokens: 4,
        }
        .saturating_add(TokenUsage {
            input_tokens: 10,
            cached_input_tokens: 20,
            output_tokens: 30,
            reasoning_tokens: 40,
        });
        assert_eq!(ordinary.input_tokens, 11);
        assert_eq!(ordinary.cached_input_tokens, 22);
        assert_eq!(ordinary.output_tokens, 33);
        assert_eq!(ordinary.reasoning_tokens, 44);
    }

    /// Edge-case table: cached input below the total leaves the remainder
    /// uncached, while equal or greater cached input yields zero, never a
    /// wrapped count.
    #[test]
    fn when_calculating_uncached_input_given_cached_input_is_at_or_above_total_then_returns_zero() {
        let cases = [
            (10, 3, 7),
            (10, 10, 0),
            (10, 15, 0),
            (0, 0, 0),
            (0, 5, 0),
            (u64::MAX, u64::MAX - 1, 1),
            (u64::MAX, u64::MAX, 0),
        ];

        for (input_tokens, cached_input_tokens, expected) in cases {
            let usage = TokenUsage {
                input_tokens,
                cached_input_tokens,
                output_tokens: 0,
                reasoning_tokens: 0,
            };
            assert_eq!(
                usage.uncached_input_tokens(),
                expected,
                "input {input_tokens} with {cached_input_tokens} cached"
            );
        }
    }

    /// Compare an estimated USD amount against a known literal, tolerating only
    /// the last-bit rounding of `f64` division.
    fn assert_usd(actual: Option<f64>, expected: f64) {
        let actual = actual.expect("configured rates must yield an estimate");
        assert!(
            (actual - expected).abs() < 1e-15,
            "expected {expected} USD, got {actual} USD"
        );
    }

    /// Contract: uncached input is charged at the input rate, cached input at
    /// its own rate when configured, and output once. Reasoning stays part of
    /// output and is never charged again.
    #[test]
    fn when_estimating_cost_given_configured_rates_then_prices_uncached_cached_and_output_tokens_once()
     {
        // 7 uncached input * 2 + 3 cached * 0.5 + 4 output * 8 = 47.5 -> $0.0000475.
        let usage = TokenUsage {
            input_tokens: 10,
            cached_input_tokens: 3,
            output_tokens: 4,
            reasoning_tokens: 2,
        };
        assert_usd(
            usage.estimate_cost_usd(Some(2.0), Some(0.5), Some(8.0)),
            0.0000475,
        );

        // The ticket's stated literal: 8 uncached input * 2 + 3 cached * 0.5
        // + 4 output * 8 = 49.5 -> $0.0000495.
        let eight_uncached = TokenUsage {
            input_tokens: 11,
            ..usage
        };
        assert_usd(
            eight_uncached.estimate_cost_usd(Some(2.0), Some(0.5), Some(8.0)),
            0.0000495,
        );

        // Reasoning is informational: changing it does not change the cost.
        let no_reasoning = TokenUsage {
            reasoning_tokens: 0,
            ..usage
        };
        let lots_of_reasoning = TokenUsage {
            reasoning_tokens: 1_000,
            ..usage
        };
        assert_usd(
            no_reasoning.estimate_cost_usd(Some(2.0), Some(0.5), Some(8.0)),
            0.0000475,
        );
        assert_usd(
            lots_of_reasoning.estimate_cost_usd(Some(2.0), Some(0.5), Some(8.0)),
            0.0000475,
        );

        // No cached-input rate configured: cached tokens fall back to the
        // input rate. 7 * 2 + 3 * 2 + 4 * 8 = 52 -> $0.000052.
        assert_usd(
            usage.estimate_cost_usd(Some(2.0), None, Some(8.0)),
            0.000052,
        );
    }

    /// Edge-case table: absent rates make their components contribute zero, an
    /// entirely unpriced provider stays `None`, and zero usage or zero rates
    /// with a configured price is still a known `Some(0.0)`.
    #[test]
    fn when_estimating_cost_given_missing_rates_or_zero_usage_then_returns_the_optional_zero_estimate()
     {
        let usage = TokenUsage {
            input_tokens: 10,
            cached_input_tokens: 3,
            output_tokens: 4,
            reasoning_tokens: 2,
        };

        assert_eq!(
            usage.estimate_cost_usd(None, None, None),
            None,
            "an entirely unpriced provider must stay unpriced"
        );

        // Only the cached-input rate is configured: 3 * 0.5 = 1.5 -> $0.0000015.
        assert_usd(usage.estimate_cost_usd(None, Some(0.5), None), 0.0000015);

        // Only the input rate is configured: cached tokens fall back to it,
        // output contributes zero. (7 + 3) * 2 = 20 -> $0.00002.
        assert_usd(usage.estimate_cost_usd(Some(2.0), None, None), 0.00002);

        // Only the output rate is configured: input contributes zero.
        // 4 * 8 = 32 -> $0.000032.
        assert_usd(usage.estimate_cost_usd(None, None, Some(8.0)), 0.000032);

        let no_usage = TokenUsage::default();
        assert_eq!(
            no_usage.estimate_cost_usd(Some(2.0), Some(0.5), Some(8.0)),
            Some(0.0),
            "configured rates with zero usage cost zero"
        );
        assert_eq!(
            no_usage.estimate_cost_usd(None, None, None),
            None,
            "absent rates stay absent even without usage"
        );
        assert_eq!(
            usage.estimate_cost_usd(Some(0.0), Some(0.0), Some(0.0)),
            Some(0.0),
            "configured zero rates cost zero"
        );
    }
}
