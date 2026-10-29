//! The HTTP boundary: the production transport's real request bytes.
//!
//! Each test binds a one-connection listener on loopback, hands its URL to the
//! production transport, and reads the exact request the peer received, so what
//! is asserted is the wire -- headers and body -- rather than a double's idea of
//! it. The only stand-in is the network peer, the non-deterministic boundary
//! itself; no internal type is mocked and no private state is inspected.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::thread::JoinHandle;
use std::time::Duration;

use ejaj::api::{Auth, HttpTransport, Message, Outgoing, RawResponse, Transport};
use ejaj::llm::LlmProvider;
use ejaj::llm::chat_completions::ChatProvider;
use ejaj::llm::factory::ProviderSettings;
use ejaj::tools::Phase;
use serde_json::{Value, json};

/// The one reply the peer sends: a chat-completions body, so the regression test
/// can decode the answer through the provider that asked for it. A raw
/// transport test only cares that a real response came back.
const REPLY: &str = r#"{"choices":[{"message":{"role":"assistant","content":"captured"}}]}"#;

/// The key the Anthropic cases authenticate with. It is distinctive so a leak
/// into another header is unmistakable.
const ANTHROPIC_KEY: &str = "sk-ant-api03-ticket-secret";

/// The Anthropic message body: the fields the ticket's provider sends, none of
/// which the transport is allowed to reshape.
fn anthropic_body() -> Value {
    json!({
        "model": "claude-3-5-sonnet-20241022",
        "max_tokens": 64,
        "messages": [
            {"role": "user", "content": "Reply with exactly one word: pong"},
        ],
    })
}

/// Send `outgoing` through the production transport, failing with the
/// transport's own message when no exchange happened -- the error type has no
/// Debug, and its Display is the useful report anyway.
fn send(outgoing: &Outgoing) -> RawResponse {
    match HttpTransport.send(outgoing) {
        Ok(response) => response,
        Err(error) => panic!("the local peer did not answer: {error}"),
    }
}

/// Decode `raw` through `provider`, failing with the decoder's own message when
/// the peer's reply is rejected.
fn decode(provider: &ChatProvider, raw: RawResponse) -> Message {
    match provider.decode(raw) {
        Ok(message) => message,
        Err(error) => panic!("the peer's reply was rejected: {error}"),
    }
}

/// The request a `LocalPeer` actually received, split at the head/body boundary
/// so a test can read either half independently.
struct CapturedRequest {
    head: String,
    body: String,
}

impl CapturedRequest {
    /// The request line, `METHOD TARGET VERSION`.
    fn request_line(&self) -> &str {
        self.head
            .lines()
            .next()
            .expect("the request carries a request line")
    }

    /// The request method.
    fn method(&self) -> &str {
        self.request_line()
            .split_whitespace()
            .next()
            .expect("the request line carries a method")
    }

    /// The request target: the path the configured URL named.
    fn target(&self) -> &str {
        self.request_line()
            .split_whitespace()
            .nth(1)
            .expect("the request line carries a target")
    }

    /// Every header the transport wrote, lowercased by name, in wire order.
    fn headers(&self) -> Vec<(String, String)> {
        self.head
            .lines()
            .skip(1)
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
            .collect()
    }

    /// The value of `name`, matched case-insensitively because HTTP header names
    /// are case-insensitive.
    fn header(&self, name: &str) -> Option<String> {
        let name = name.to_ascii_lowercase();
        self.headers()
            .into_iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value)
    }

    /// The request body, decoded: the transport is the one that encoded it.
    fn json_body(&self) -> Value {
        serde_json::from_str(&self.body).expect("the request body is JSON")
    }
}

/// A one-connection HTTP peer on loopback: it accepts the transport's request,
/// captures it, answers it, and hands the captured bytes back.
struct LocalPeer {
    address: SocketAddr,
    capture: Option<JoinHandle<CapturedRequest>>,
}

impl LocalPeer {
    /// Bind a free loopback port and start waiting for one request.
    fn start() -> LocalPeer {
        let listener =
            TcpListener::bind("127.0.0.1:0").expect("the local peer binds a loopback port");
        let address = listener
            .local_addr()
            .expect("the bound loopback address is readable");
        let capture = std::thread::spawn(move || serve_one_request(listener));
        LocalPeer {
            address,
            capture: Some(capture),
        }
    }

    /// The configured URL for `path` on this peer: what the outgoing request
    /// carries and what the peer observes.
    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.address, path)
    }

    /// Wait for the peer to capture and answer the one request, then return it.
    fn received(mut self) -> CapturedRequest {
        self.capture
            .take()
            .expect("the peer was started")
            .join()
            .expect("the peer captures one request and answers it")
    }
}

/// Accept one connection, read the whole request, answer it with the fixed
/// reply, and return the bytes that arrived.
fn serve_one_request(listener: TcpListener) -> CapturedRequest {
    let (mut stream, _) = listener
        .accept()
        .expect("the peer accepts the transport's connection");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("the peer can bound its read");

    let mut received = Vec::new();
    let mut chunk = [0u8; 1024];
    let head_end = loop {
        if let Some(end) = find(&received, b"\r\n\r\n") {
            break end;
        }
        let read = stream
            .read(&mut chunk)
            .expect("the peer reads the request head");
        assert!(
            read > 0,
            "the transport closed the connection before completing the request head"
        );
        received.extend_from_slice(&chunk[..read]);
    };

    let head = String::from_utf8(received[..head_end].to_vec())
        .expect("the request head is UTF-8")
        .replace("\r\n", "\n");
    let body_start = head_end + 4;
    let body_len = content_length(&head);
    while received.len() < body_start + body_len {
        let read = stream
            .read(&mut chunk)
            .expect("the peer reads the request body");
        assert!(
            read > 0,
            "the transport closed the connection before completing the request body"
        );
        received.extend_from_slice(&chunk[..read]);
    }
    let body = String::from_utf8(received[body_start..body_start + body_len].to_vec())
        .expect("the request body is UTF-8");

    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        REPLY.len(),
        REPLY
    );
    stream
        .write_all(response.as_bytes())
        .expect("the peer answers the request");
    stream.flush().expect("the peer flushes its answer");

    CapturedRequest { head, body }
}

/// The first index at which `needle` appears in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// The body length the head declares, `0` when it declares none -- which is the
/// right reading for a body-less request.
fn content_length(head: &str) -> usize {
    head.lines()
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse().ok())
        .unwrap_or(0)
}

/// Core entry-point behavior: the Anthropic scheme writes the configured key and
/// the pinned version header, at the peer, alongside the configured URL and the
/// unchanged body.
#[test]
fn when_sending_given_anthropic_authentication_then_uses_exact_key_and_version_headers() {
    let peer = LocalPeer::start();
    let body = anthropic_body();
    let outgoing = Outgoing {
        url: peer.url("/v1/messages"),
        auth: Auth::Anthropic(ANTHROPIC_KEY.to_string()),
        body: body.clone(),
    };

    let response = send(&outgoing);
    let request = peer.received();

    assert_eq!(response.status, 200, "the peer's status reaches the caller");
    assert_eq!(request.method(), "POST");
    assert_eq!(request.target(), "/v1/messages");
    assert_eq!(
        request.header("x-api-key").as_deref(),
        Some(ANTHROPIC_KEY),
        "the configured key is the x-api-key header"
    );
    assert_eq!(
        request.header("anthropic-version").as_deref(),
        Some("2023-06-01"),
        "Anthropic requests declare the pinned wire version"
    );
    assert_eq!(
        request.header("content-type").as_deref(),
        Some("application/json"),
        "the Anthropic body stays JSON"
    );
    assert_eq!(
        request.json_body(),
        body,
        "the transport must not reshape an Anthropic body"
    );
}

/// Edge-case table: the existing Bearer scheme keeps its single Authorization
/// header and acquires no Anthropic headers.
#[test]
fn when_sending_given_bearer_authentication_then_preserves_existing_authorization_header() {
    let peer = LocalPeer::start();
    let outgoing = Outgoing {
        url: peer.url("/v1/chat/completions"),
        auth: Auth::Bearer("mid-key".to_string()),
        body: json!({"model": "mid-model", "messages": []}),
    };

    send(&outgoing);
    let request = peer.received();

    assert_eq!(
        request.header("authorization").as_deref(),
        Some("Bearer mid-key"),
        "Bearer authentication is unchanged"
    );
    assert_eq!(
        request.header("x-api-key"),
        None,
        "a Bearer request carries no Anthropic key header"
    );
    assert_eq!(
        request.header("anthropic-version"),
        None,
        "a Bearer request carries no Anthropic version header"
    );
}

/// Contract check: the Anthropic scheme never accompanies its key with a Bearer
/// Authorization header.
#[test]
fn when_sending_given_anthropic_authentication_then_omits_bearer_authorization() {
    let peer = LocalPeer::start();
    let outgoing = Outgoing {
        url: peer.url("/v1/messages"),
        auth: Auth::Anthropic(ANTHROPIC_KEY.to_string()),
        body: anthropic_body(),
    };

    send(&outgoing);
    let request = peer.received();

    assert_eq!(
        request.header("authorization"),
        None,
        "no Authorization header may accompany an Anthropic key: {:?}",
        request.headers()
    );
    assert_eq!(
        request.header("x-api-key").as_deref(),
        Some(ANTHROPIC_KEY),
        "the key is sent through its own header"
    );
}

/// Regression: a provider-built request keeps today's Bearer header, JSON
/// content type and wire body at the peer, and its answer still decodes.
#[test]
fn when_sending_given_existing_provider_authentication_then_preserves_json_content_type_and_request_body()
 {
    let peer = LocalPeer::start();
    let provider = ChatProvider::new(ProviderSettings {
        url: peer.url("/v1/chat/completions"),
        model: "mid-model".to_string(),
        api_key: Some("mid-key".to_string()),
        auth_disabled: false,
        reasoning_effort: None,
        input_price_per_million_usd: None,
    });
    let prepared = provider.prepare(&[Message::system("sys"), Message::user("hi")], Phase::Code);
    let expected_body = prepared.outgoing.body.clone();

    let response = send(&prepared.outgoing);
    let request = peer.received();

    assert_eq!(request.method(), "POST");
    assert_eq!(request.target(), "/v1/chat/completions");
    assert_eq!(
        request.header("content-type").as_deref(),
        Some("application/json"),
        "the chat-completions body is still JSON"
    );
    assert_eq!(
        request.header("authorization").as_deref(),
        Some("Bearer mid-key"),
        "the existing provider still authenticates with Bearer"
    );
    assert_eq!(request.header("x-api-key"), None);
    assert_eq!(request.header("anthropic-version"), None);

    let sent = request.json_body();
    assert_eq!(
        sent, expected_body,
        "the transport sends the body the provider prepared, unreshaped"
    );
    assert_eq!(sent["model"], json!("mid-model"));
    assert_eq!(
        sent["messages"],
        json!([
            {"role": "system", "content": "sys"},
            {"role": "user", "content": "hi"},
        ])
    );
    assert_eq!(sent["temperature"], json!(0.0));

    let message = decode(&provider, response);
    assert_eq!(message.content.as_deref(), Some("captured"));
}

/// Forbidden-access boundary: the configured key appears in the Anthropic header
/// and nowhere else -- in particular never inside a Bearer Authorization header.
#[test]
fn when_sending_given_anthropic_authentication_then_exposes_the_configured_key_only_in_the_anthropic_header()
 {
    let peer = LocalPeer::start();
    let outgoing = Outgoing {
        url: peer.url("/v1/messages"),
        auth: Auth::Anthropic(ANTHROPIC_KEY.to_string()),
        body: anthropic_body(),
    };

    send(&outgoing);
    let request = peer.received();

    let leaked: Vec<(String, String)> = request
        .headers()
        .into_iter()
        .filter(|(name, value)| name != "x-api-key" && value.contains(ANTHROPIC_KEY))
        .collect();
    assert!(
        leaked.is_empty(),
        "the configured key must appear only in the Anthropic header, but leaked into {leaked:?}"
    );
    assert_eq!(
        request.header("x-api-key").as_deref(),
        Some(ANTHROPIC_KEY),
        "the one place the key is exposed is its own header"
    );
}

/// Nil case: a request with no authentication sends no authentication header at
/// all, and still sends its JSON body.
#[test]
fn when_sending_given_no_authentication_then_sends_no_authentication_headers() {
    let peer = LocalPeer::start();
    let body = json!({"model": "local-model", "messages": []});
    let outgoing = Outgoing {
        url: peer.url("/v1/chat/completions"),
        auth: Auth::None,
        body: body.clone(),
    };

    let response = send(&outgoing);
    let request = peer.received();

    assert_eq!(response.status, 200);
    assert_eq!(request.header("authorization"), None);
    assert_eq!(request.header("x-api-key"), None);
    assert_eq!(request.header("anthropic-version"), None);
    assert_eq!(
        request.header("content-type").as_deref(),
        Some("application/json")
    );
    assert_eq!(request.json_body(), body);
}
