# Proposal 30: Deterministic Sibling-Dependency Validation & Transitive Triage Producer Audits (`src/tickets.rs`, `src/prompts/tickets.rs`, `src/prompts/triage.rs`, `tests/prompt_contract.rs`)

## Problem

In multi-ticket vertical slices (observed in `.ejaj/20261114-140556`), a clean specification can stall at `triage` and drop back to `plan>>` due to a direct-vs-transitive `blocked_by` mismatch between the Tickets pass, the Triage pass, and the runtime queue scheduler:

1. **Runtime scheduling is already transitive (`src/orchestrator.rs:30-38`):**
   - `Scheduler` executes a ticket only after every entry in its `blocked_by` list completes. When `03` is `blocked_by: ["01"]` and `04` is `blocked_by: ["03"]`, `01` is guaranteed to complete before `04` is ever scheduled; if `01` fails, neither `03` nor `04` runs.
2. **`TicketSet::validate` misses undeclared sibling references (`src/tickets.rs:185-243`):**
   - When the Tickets agent writes `blocked_by: ["03-..."]` on ticket `04` while explicitly stating in `prerequisites_assumed` that `04` consumes `RunMetrics` fields from `01-...`, `finish_tickets` accepts the payload without returning a self-correcting tool error.
3. **`TRIAGE_SYSTEM_PROMPT` audits only direct `blocked_by` entries (`src/prompts/triage.rs:23, 57`):**
   - Step 2 instructs Triage to check `prerequisites_assumed` against `scope_of_changes` of *"every ticket in its `blocked_by` list"*, while `src/triage.rs:532-575` forbids Triage from adding missing IDs to `AgentBrief.blocked_by`. Triage therefore classifies an otherwise valid, transitively ordered ticket as `ready-for-human`, blocking the entire batch.

---

## Guardrails for Existing `ejaj` Tests & Clippy Policy

- **Preserve all pinned `TRIAGE_SYSTEM_PROMPT` substrings (`src/prompts/triage.rs:85-111, 222-269`, `tests/prompt_contract.rs:706-873`):**
  - Keep `"each consumer ticket"`, `"prerequisites_assumed"`, `"scope_of_changes"`, `"every ticket in its"`, `"blocked_by"`, and `"files, types, and function signatures"` intact.
  - Keep every string in `TRIAGE_LEGACY_STEP_TWO_NEEDLES` and every shared-story narrowing assertion in `tests/prompt_contract.rs:759-872` unchanged (in particular, `"narrow that ticket's `acceptance_criteria`"` must continue to appear **exactly once** in Step 2).
- **Preserve `AgentBrief.blocked_by` immutability (`src/triage.rs:532-575`):**
  - Do not allow `AgentBrief.blocked_by` to diverge from the draft ticket's YAML `blocked_by` list; `check_dependencies` stays unchanged.
- **Preserve all pinned `TICKETS_SYSTEM_PROMPT` substrings and 11-field schema block (`src/prompts/tickets.rs:227-295`, `tests/prompt_contract.rs`):**
  - Keep `"When a ticket consumes a contract from a ticket named in its `blocked_by`"`, `"When a ticket produces a contract for a ticket named in its `blocks`"`, `"Do not invent an interface when the ticket has no applicable `blocked_by` dependency or no downstream `blocks` consumer"`, and the exact 11 `- `<field>`: ` lines between `"with exactly these fields:"` and `"The `verification_gate` must demonstrate"`.
- **Token-boundary matching in `TicketSet::validate` (`src/tickets.rs`):**
  - Match sibling ticket IDs only on `!is_id_char` boundaries so `01-foo` never false-matches `01-foo-bar` or a file path segment like `src/01-foo.rs` when `/` vs `is_id_char` applies (note: `.`, `_`, `-` are already `is_id_char`). Ignore self-references (`sibling_id == id`).
- **Enforce Clippy complexity (`<= 15`) and caller-intent test titles (`tests/guidance.rs`, `tests/test_titles.rs`):**
  - Extract small helpers in `src/tickets.rs` (`contains_ticket_id_token`, `validate_referenced_prerequisites`) and name all new tests `when_<action>_given_<context>_then_<outcome>` with no banned `INTERNAL_IDENTIFIERS`.

---

## Proposed Design

### 1. Deterministic Sibling-ID Prerequisite Validation in `TicketSet::validate` (`src/tickets.rs`)

After cycle detection (`src/tickets.rs:236-242`) and before or alongside per-ticket field checks:

1. Add `fn contains_ticket_id_token(text: &str, candidate_id: &str) -> bool`:
   - Scans `text` for occurrences of `candidate_id` where the preceding char (if any) satisfies `!is_id_char(prev)` and the following char (if any) satisfies `!is_id_char(next)`.
2. Add `fn validate_prerequisite_references(tickets: &[Ticket]) -> Result<(), String>`:
   - Collect each ticket's trimmed `id` in emitted order.
   - For each `ticket`, build its trimmed direct `blocked_by` set.
   - Check every other `sibling_id != id` in emitted order: if `contains_ticket_id_token(&ticket.prerequisites_assumed, sibling_id)` and `!direct_blocked_by.contains(sibling_id)`, return:
     ```text
     [ERROR] ticket `{id}` names sibling ticket `{sibling_id}` in `prerequisites_assumed`, which is not in its `blocked_by` list; add `{sibling_id}` to `blocked_by` so every consumed producer ticket is declared directly
     ```
   - Because `finish_tickets` returns validation errors to the Tickets agent as recoverable tool errors inside the same loop, the Tickets agent adds `sibling_id` to `blocked_by` immediately before Triage ever starts.

### 2. Direct-Producer Rule in `TICKETS_SYSTEM_PROMPT` (`src/prompts/tickets.rs`)

In Step 2 and the contract paragraph around lines 27 and 46 of `src/prompts/tickets.rs`:
- Instruct the Tickets agent that whenever `prerequisites_assumed` consumes types, fields, functions, or behavior introduced by an earlier ticket in the set, that producer ticket's `id` must appear **directly** in `blocked_by`—never rely solely on transitive `blocked_by` chains through intermediate tickets.

### 3. Transitive Ancestor Producer Satisfaction in `TRIAGE_SYSTEM_PROMPT` (`src/prompts/triage.rs`)

In Workflow Step 2 (`src/prompts/triage.rs:23`), immediately after the existing direct `blocked_by` sentence:
- Clarify that when a consumer ticket's `prerequisites_assumed` relies on types, fields, or signatures declared in the `scope_of_changes` of an earlier draft ticket reachable **transitively** through its `blocked_by` chain, that transitive producer's `scope_of_changes` satisfies the dependency contract check because the queue scheduler enforces transitive `blocked_by` completion before running the consumer: record the audited transitive producer chain in `notes`, keep `AgentBrief.blocked_by` equal to the draft ticket's own YAML `blocked_by` list unchanged, and keep the classification `ready-for-agent` rather than marking it `ready-for-human` or `needs-info` solely for omitting a redundant direct edge.
- A prerequisite contract that neither the live repository nor any direct or transitive `blocked_by` ancestor's `scope_of_changes` supplies remains a finding.

### 4. Test Coverage

- `src/tickets.rs`:
  - `when_validating_tickets_given_prerequisite_naming_unlisted_sibling_id_then_rejects_missing_blocker`: rejects `04` naming `01-metrics-domain` in `prerequisites_assumed` when `blocked_by` only has `["03-post-handoff"]`.
  - `when_validating_tickets_given_prefix_overlapping_ids_or_declared_blocker_then_accepts_set`: table-drives declared `blocked_by`, self-ID mention, and prefix IDs (`01-core` vs `01-core-ext`) without false rejection.
- `src/prompts/tickets.rs`, `src/prompts/triage.rs`, and `tests/prompt_contract.rs`:
  - Assert both the direct `blocked_by` producer rule in `TICKETS_SYSTEM_PROMPT` and the transitive `blocked_by` ancestor rule in `TRIAGE_SYSTEM_PROMPT` while verifying all legacy Step 2 and shared-story narrowing assertions continue to pass.

---

## Affected Files

- `src/tickets.rs`
- `src/prompts/tickets.rs`
- `src/prompts/triage.rs`
- `tests/prompt_contract.rs`

## Verification

```bash
cargo test --lib tickets && cargo test --lib prompts::tickets && cargo test --lib prompts::triage && cargo test --test prompt_contract && cargo test --test test_titles && cargo clippy --all-targets -- -D warnings
```
