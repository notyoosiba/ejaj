//! Hardcoded model-facing prompt texts: one prompt file per agent.
//!
//! Declared here so `main.rs` only needs `mod prompts;`. This module owns the
//! five prompt files sitting alongside it in this directory: `plan` for the
//! plan agent, `spec` for the spec agent, `tickets` for the tickets agent,
//! `code` for the code agent, and `summary` for the change-summary pass.

mod code;
mod plan;
mod review;
mod spec;
mod summary;
mod tickets;
mod triage;

pub use code::{
    CODE_SYSTEM_PROMPT, CODE_TDD_DISCIPLINE, CODE_TICKET_KICKOFF_PROMPT, ticket_kickoff,
};
pub use plan::PLAN_SYSTEM_PROMPT;
pub use review::{REVIEW_KICKOFF_PROMPT, REVIEW_SYSTEM_PROMPT};
pub use spec::{SPEC_KICKOFF_PROMPT, SPEC_SYSTEM_PROMPT};
pub use summary::{SUMMARY_KICKOFF_PROMPT, SUMMARY_SYSTEM_PROMPT, summary_correction};
pub use tickets::{TICKETS_KICKOFF_PROMPT, TICKETS_SYSTEM_PROMPT};
pub use triage::{TRIAGE_KICKOFF_PROMPT, TRIAGE_SYSTEM_PROMPT};
