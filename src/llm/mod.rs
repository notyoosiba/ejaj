//! The synchronous provider layer.
//!
//! One seam sits between the agent loop and the wire. [`LlmProvider`] turns a
//! transcript plus a [`Phase`] into an [`Outgoing`] request, and turns a
//! [`RawResponse`] back into the [`Message`] the loop replays. Both operations
//! are pure: nothing here touches the network, the environment or a clock, so
//! the whole layer is unit-tested against literal JSON with no fixture and no
//! key.
//!
//! The types the loop already speaks are the seam's currency -- [`Outgoing`],
//! [`RawResponse`], [`Message`] and [`CallError`] all live in [`crate::api`] --
//! so the provider layer adds no second transcript type set. The
//! chat-completions provider in [`chat_completions`] reproduces today's request
//! and response handling byte for byte; the native Anthropic Messages provider
//! in [`anthropic`] serves the summary pass in every mode; and the factory in
//! [`factory`] resolves one provider's settings.

pub mod anthropic;
pub mod chat_completions;
pub mod factory;
pub mod responses_api;

use crate::api::{CallError, Message, Outgoing, RawResponse};
use crate::tools::Phase;

/// How the available providers are combined.
///
/// `Summary` is not a mode-routed phase: it always runs on the separately
/// configured Smoll provider (the Anthropic Messages wire format) in every
/// mode. The mode routes every other phase. `Mid` runs them all through the
/// mid provider (the chat-completions wire format), `Giga` runs them all
/// through the giga provider (the responses wire format), and `Hybrid` splits
/// them by phase: plan, spec, tickets, triage and review run on the giga
/// provider, while code runs on the mid provider. The mode is named from the
/// environment and never inferred, and only the canonical lowercase `mid`,
/// `giga` and `hybrid` names are recognised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Mid,
    Giga,
    Hybrid,
}

impl Mode {
    /// Parse a canonical mode name. The comparison is trimmed and
    /// case-insensitive, and anything unrecognised -- an empty string and the
    /// old `worker` and `architect` role spellings included -- is `None`.
    pub fn from_value(raw: &str) -> Option<Mode> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "mid" => Some(Mode::Mid),
            "giga" => Some(Mode::Giga),
            "hybrid" => Some(Mode::Hybrid),
            _ => None,
        }
    }

    /// The canonical lowercase name of the mode, for messages and diagnostics.
    pub fn label(self) -> &'static str {
        match self {
            Mode::Mid => "mid",
            Mode::Giga => "giga",
            Mode::Hybrid => "hybrid",
        }
    }

    /// Whether this mode needs the giga provider's coordinates to start.
    pub fn needs_giga(self) -> bool {
        matches!(self, Mode::Giga | Mode::Hybrid)
    }
}

/// One prepared request and the size of the transcript it carries.
///
/// `context_bytes` is the encoded transcript as the provider prepares it -- the
/// encoded `messages` for chat, the encoded `input` for responses; it is what
/// the status line reports while the request is in flight.
pub struct Prepared {
    pub outgoing: Outgoing,
    pub context_bytes: usize,
}

/// The synchronous provider seam.
///
/// There is deliberately no `async`, no `async_trait` and no `Send + Sync`
/// bound: the program is synchronous, and every provider is a value the loop
/// builds once and calls directly. `prepare` and `decode` are pure, which is
/// what keeps a provider testable without a network, a fixture or a key.
pub trait LlmProvider {
    /// The provider's stable name, for diagnostics.
    fn name(&self) -> &'static str;

    /// Build the request for `messages` in `phase`.
    fn prepare(&self, messages: &[Message], phase: Phase) -> Prepared;

    /// Build the same request for `messages` in `phase` without advertising
    /// any tool, for the loop's one compaction-only handover-summary request.
    ///
    /// The method is synchronous like every other operation on this seam. The
    /// default keeps a provider that does not override it usable: it strips the
    /// body's `tools` member, which is the one field all three wire formats
    /// share. The shipped providers override it with their own wire-valid empty
    /// tool list, so a normal phase's tools and routing are never touched.
    fn prepare_tool_free(&self, messages: &[Message], phase: Phase) -> Prepared {
        let mut prepared = self.prepare(messages, phase);
        match prepared.outgoing.body.as_object_mut() {
            Some(object) if object.contains_key("tools") => {
                object.insert("tools".to_string(), serde_json::Value::Array(Vec::new()));
            }
            _ => {}
        }
        prepared
    }

    /// Turn one raw wire response into the assistant message it carries.
    fn decode(&self, raw: RawResponse) -> Result<Message, CallError>;
}
