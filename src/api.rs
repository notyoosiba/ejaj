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
}

#[derive(Deserialize)]
pub struct Choice {
    pub message: Message,
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
}
