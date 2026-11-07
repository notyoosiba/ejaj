# Proposal 5: Forward Triage `notes` and `AgentBrief` to the Code Agent

## Problem

During `Pipeline::triage_pass` (`src/pipeline.rs:381-459`), the Triage agent inspects the repository read-only and produces a validated `TriageTicket` (`src/triage.rs:156-177`) for every `ready-for-agent` ticket containing:
- `notes`: concrete audit findings naming the exact files, functions, existing behavior, and remaining gaps in the repository.
- `agent_brief`: a focused `AgentBrief` (`target_behavior`, `acceptance_criteria`, `out_of_scope`, `blocked_by`).

However, `Pipeline::triage_pass` only writes `ticket-triage.md` to disk and checks `authorizes_queue(&report)`. When `orchestrator` runs the ticket queue (`Production::work` in `src/orchestrator.rs:296-309`), it passes only `&ticket.to_markdown()` to `code::Session::run_ticket_attempt_in` and **discards the `TriageReport`**. As a result, the Code agent spends its first 10-20 steps re-discovering the exact files, seams, and partial implementations that Triage just audited.

---

## Proposed Design

### 1. Render One Ticket's Triage Audit Block (`src/triage.rs`)

Add a helper on `TriageReport` that renders the audit `notes` and `AgentBrief` for a given `ticket_id` as a markdown section suitable for appending to the ticket kickoff prompt:

```rust
impl TriageReport {
    /// Render the validated triage notes and Agent Brief for `ticket_id`, when
    /// present and `ready-for-agent`.
    pub fn agent_context_for(&self, ticket_id: &str) -> Option<String> {
        let id = ticket_id.trim();
        let classification = self.tickets.iter().find(|t| t.ticket_id.trim() == id)?;
        let brief = classification.agent_brief.as_ref()?;
        let mut out = String::from("\n# Triage Audit\n\n## Audit Notes\n\n");
        for note in &classification.notes {
            out.push_str(&format!("- {}\n", note.trim()));
        }
        out.push_str(&format!(
            "\n{BRIEF_HEADING}\n\n**Target behavior:** {}\n\n**Acceptance criteria:**\n\n",
            brief.target_behavior.trim()
        ));
        for criterion in &brief.acceptance_criteria {
            out.push_str(&format!("- {}\n", criterion.trim()));
        }
        out.push_str("\n**Out of scope:**\n\n");
        for item in &brief.out_of_scope {
            out.push_str(&format!("- {}\n", item.trim()));
        }
        Some(out)
    }
}
```

### 2. Thread Optional Triage Context to `code::Session` (`src/pipeline.rs`, `src/orchestrator.rs`)

- Store `triage: Option<TriageReport>` on `ReviewProgress` in `src/pipeline.rs` (populated from `triage_pass` for the initial draft queue round; review-generated fix tickets have no triage entry and receive `None` from `agent_context_for`).
- Pass `triage: Option<&'a TriageReport>` into `Production<'a>` in `src/orchestrator.rs:267-279`.
- In `Production::work` (`src/orchestrator.rs:296-309`), build `let mut prompt = ticket.to_markdown();`, append `triage_context` when `self.triage.and_then(|t| t.agent_context_for(&ticket.id))` returns `Some(ctx)`, and pass `&prompt` to `session.run_ticket_attempt_in`.
- Keep the on-disk `tickets/<id>.md` files unchanged as the canonical rendering of `Ticket`.

---

## Affected Files

- `src/triage.rs`
- `src/orchestrator.rs`
- `src/pipeline.rs`
- `tests/pipeline.rs`

## Verification

```bash
cargo test --lib triage && cargo test --lib orchestrator && cargo test --test pipeline && cargo clippy --all-targets -- -D warnings
```
