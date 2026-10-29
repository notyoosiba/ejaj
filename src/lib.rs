#![cfg_attr(test, allow(clippy::cognitive_complexity, clippy::manual_let_else))]

//! The ejaj library: the four agents and the typed boundaries they exchange.
//!
//! The binary in `src/main.rs` is a thin REPL around this crate; declaring the
//! modules here is what makes them reachable from `tests/`.

// Private: the Git preflight is the pipeline's own integration detail, not a
// boundary a caller may reach for.
mod git;

pub mod api;
pub mod config;
pub mod llm;
pub mod loops;
pub mod orchestrator;
pub mod pipeline;
pub mod plan_document;
pub mod prompts;
pub mod retry;
pub mod review;
pub mod script;
pub mod specification;
pub mod status;
pub mod tickets;
pub mod timing;
pub mod tools;
pub mod triage;

pub use config::Config;
