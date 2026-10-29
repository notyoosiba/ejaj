//! The agents and the shared tool loop behind them.
//!
//! `plan` owns the plan agent (the design interview), `spec` owns the spec
//! agent (one fresh specification pass per handoff), `tickets` owns the tickets
//! agent (one fresh ticket set per specification), `triage` owns the triage
//! agent (one fresh read-only audit of the draft ticket set, after the ticket
//! writes and before the queue), `code` owns the code agent (one fresh
//! transcript per ticket), `review` owns the review pass (one fresh audit per
//! resolved queue round), `summary` owns the change-summary pass (one fresh
//! read-only transcript per queue run), and `agent` holds the one tool loop
//! every one of them runs on. `agent` is never an agent name.

mod agent;
pub mod code;
pub mod plan;
pub mod review;
pub mod spec;
pub mod summary;
pub mod tickets;
pub mod triage;

pub use agent::FailureKind;
pub(crate) use agent::InvocationContext;
