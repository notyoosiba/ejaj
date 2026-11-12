# TODO — `.plans/` Implementation Status & Task DAG

Tracks the implementation status of active retry and diagnostic proposals (`PROPOSAL-22` and `PROPOSAL-21`) in [`.plans/`](./.plans/) as an ordered Directed Acyclic Graph (DAG) of implementation tasks. All prior specifications and proposals (`01`–`12`, `PROPOSAL-1`–`PROPOSAL-19`) have been completed and verified in `git` history (`T01`–`T24`).

---

## 1. Active Proposals Status Matrix

| Proposal | Status | Mapped DAG Task | Target Modules |
| --- | --- | --- | --- |
| [`PROPOSAL-22-transition-gate-diagnostic-excerpt.md`](./.plans/PROPOSAL-22-transition-gate-diagnostic-excerpt.md) | `[ ]` Pending | `T25` | `src/tools.rs`, `src/pipeline.rs` |
| [`PROPOSAL-21-hybrid-code-retry-giga-escalation.md`](./.plans/PROPOSAL-21-hybrid-code-retry-giga-escalation.md) | `[ ]` Pending | `T26` | `src/config.rs`, `src/loops/agent.rs`, `src/loops/code.rs`, `src/orchestrator.rs`, `tests/common/mod.rs`, `tests/hybrid_pipeline.rs` |

---

## 2. Task Dependency Graph (DAG)

The pending tasks form a strictly ordered, serialized pipeline designed to stay well within the 100 tool-call ceiling per ticket:

1. **`T25` (Transition Diagnostics First) — `[ ]` Pending:** Exposes `tools::diagnostic_text` to `crate::pipeline` and upgrades `transition_failure_excerpt` so any full-suite transition-gate failure surfaces Cargo/compiler primary failure blocks at the front of synthetic repair tickets instead of losing them behind noisy passing-test `stderr` logs.
2. **`T26` (Hybrid Retry Escalation & Tool Trail Second) — `[ ]` Pending:** Escalates `Phase::Code` second-attempt ticket retries (`handover.is_some()`) from `mid` to `giga` in `Mode::Hybrid` via `Config::for_code_retry`, and captures Attempt 1's last 3 delivered tool calls, error lines, and spill paths in `[RETRY HANDOVER]`.

```text
[T25] [ ] Pending PROPOSAL-22: High-signal failure extraction in transition_failure_excerpt (src/{tools,pipeline}.rs)
  │
  ▼
[T26] [ ] Pending PROPOSAL-21: Escalate hybrid ticket retries to giga & enrich [RETRY HANDOVER] (src/{config,orchestrator}.rs, src/loops/{agent,code}.rs)
```

---

## 3. Ordered Task Backlog

- [ ] **`T25` — High-Signal Failure Extraction in `transition_failure_excerpt`**
  - `depends_on`: `[]`
  - `unblocks`: `["T26"]`
  - `plans`: [`.plans/PROPOSAL-22-transition-gate-diagnostic-excerpt.md`](./.plans/PROPOSAL-22-transition-gate-diagnostic-excerpt.md)
  - `files`: `src/tools.rs`, `src/pipeline.rs`, `TODO.md`
  - `tasks`:
    - In `src/tools.rs`, widen `fn diagnostic_text(rendered: &str) -> String` to `pub(crate) fn diagnostic_text(rendered: &str) -> String` without changing any diagnostic classification, block-continuation, per-line cap, or deduplication behavior.
    - In `src/pipeline.rs` (`transition_failure_excerpt`), preserve `"command exited non-zero with no output"` on empty/whitespace input and verbatim pass-through on `<= 1,200` Unicode scalar values; on oversized inputs (`> 1,200` scalar values), split off any leading `[ERROR: ...]` wrapper line before calling `crate::tools::diagnostic_text(body)`, re-attach that header when `!diagnostics.is_empty()` and cap at `1,200` retained scalar values with an additive `"..."` suffix when shortened, or fall back to `...{tail}` of the final `1,200` scalar values when `diagnostics.is_empty()`.
    - Add consolidated caller-intent (`when_<action>_given_<context>_then_<outcome>`) unit tests in `src/pipeline.rs` covering empty/short/UTF-8 boundaries, oversized `cargo test` failures buried ahead of noisy `stderr` logs, and oversized plain failures without diagnostic keywords.
    - In `TODO.md` only (do not edit `.plans/PROPOSAL-22-transition-gate-diagnostic-excerpt.md`), mark `T25` complete (`[x]`) in the DAG and ordered backlog and update the `PROPOSAL-22-transition-gate-diagnostic-excerpt.md` row in the `TODO.md` status matrix to `[x] Complete (T25 done)` right before running `verification_gate`.
  - `verification_gate`: `cargo test --lib tools && cargo test --lib pipeline && cargo test --test pipeline && cargo test --test test_titles && cargo clippy --all-targets -- -D warnings`

- [ ] **`T26` — Escalate Ticket Retries to `giga` in `hybrid` Mode & Enrich `[RETRY HANDOVER]`**
  - `depends_on`: `["T25"]`
  - `unblocks`: `[]`
  - `plans`: [`.plans/PROPOSAL-21-hybrid-code-retry-giga-escalation.md`](./.plans/PROPOSAL-21-hybrid-code-retry-giga-escalation.md)
  - `files`: `src/config.rs`, `src/loops/agent.rs`, `src/loops/code.rs`, `src/orchestrator.rs`, `tests/common/mod.rs`, `tests/hybrid_pipeline.rs`, `TODO.md`
  - `tasks`:
    - In `src/config.rs` and `src/orchestrator.rs` (`Production::work`), add `pub(crate) fn for_code_retry(&self) -> Config` escalating `Mode::Hybrid` (when `giga` is present) to `Mode::Giga`, and use `self.config.for_code_retry()` only when `handover.is_some()` so Attempt 1 and `Config::provider(Phase::Code)` stay routed to `mid`.
    - In `src/loops/agent.rs` and `src/loops/code.rs`, add `pub tool_trail: Option<String>` to `AgentAttempt` and `code::Attempt` (defaulting to `None` in `Attempt::new`), and compute `summarize_recent_tool_activity(messages)` (last 3 delivered tool calls in chronological order, skipping unanswered control handoffs, bounding raw JSON argument parsing at `16 KiB` and the trail at `1,500` UTF-8 bytes, and preserving error detail lines plus `[full output saved to ...]` spill footers).
    - In `src/orchestrator.rs` (`work_with_retry`, `retry_once`, `render_retry_handover`), thread `first.tool_trail.as_deref()` into `render_retry_handover`, omitting `Recent tool activity:` when `None`/blank and rendering `Recent tool activity:\n{trail}\n\n` immediately before `Repository working-tree diff (staged, unstaged, and non-ignored untracked changes; .ejaj/ excluded):` when present.
    - In `tests/common/mod.rs` (`code_reply_response`), emit an empty assistant message item when `is_responses` is `true` and `output.is_empty()` (`CodeReply::TurnEnd(None)`), and add caller-intent unit and integration tests across `src/config.rs`, `src/loops/agent.rs`, `src/orchestrator.rs`, and `tests/hybrid_pipeline.rs`.
    - In `TODO.md` only (do not edit `.plans/PROPOSAL-21-hybrid-code-retry-giga-escalation.md`), mark `T26` complete (`[x]`) in the DAG and ordered backlog and update the `PROPOSAL-21-hybrid-code-retry-giga-escalation.md` row in the `TODO.md` status matrix to `[x] Complete (T26 done)` right before running `verification_gate`.
  - `verification_gate`: `cargo test --lib config && cargo test --lib loops::agent && cargo test --lib orchestrator && cargo test --test hybrid_pipeline && cargo test --test pipeline && cargo test --test test_titles && cargo clippy --all-targets -- -D warnings`
