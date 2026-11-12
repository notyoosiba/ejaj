# T26: Escalate Ticket Retries to `giga` in `hybrid` Mode (`src/config.rs`, `src/orchestrator.rs`, `tests/common/mod.rs`, `tests/hybrid_pipeline.rs`)

## Problem

When a ticket's first code-agent attempt fails in `hybrid` mode (`TurnEnd`, `FailureKind::StepLimit`, `FailureKind::ContextGuard`, or `task_complete` followed by a failed first verification gate), `work_with_retry` (`src/orchestrator.rs:734-795`) runs `retry_once` (`src/orchestrator.rs:803-835`) on the `mid` provider:

- In `Mode::Hybrid` (where `giga` is guaranteed present), `Config::provider(Phase::Code)` (`src/config.rs:195-215`) routes `Phase::Code` to `self.mid` (`chat-completions`).
- Because `Production::work` (`src/orchestrator.rs:302-318`) always builds `code::Session::new(self.config, self.scope)`, Attempt 2 (`handover.is_some()`) runs on the exact same `mid` model that just failed Attempt 1, instead of escalating the single rescue attempt to the configured `giga` provider (`responses` wire format, `giga` role/model/pricing, and `giga` trajectory metadata).

---

## Guardrails for Existing `ejaj` Tests

- **Preserve `Config::provider(Phase::Code)` and Attempt 1 `mid` routing in `hybrid` mode (`tests/hybrid_pipeline.rs`):**
  - `assert_hybrid_routing` (`tests/hybrid_pipeline.rs:63-87`), `when_running_given_recorded_hybrid_pipeline_then_preserves_giga_mid_and_smoll_routes` (`tests/hybrid_pipeline.rs:200-267`), `when_routing_given_each_hybrid_phase_then_uses_its_established_endpoint` (`tests/hybrid_pipeline.rs:272-295`), `when_building_given_explicit_giga_settings_then_uses_canonical_builder` (`tests/hybrid_pipeline.rs:301-342`), and `when_triaging_tickets_given_hybrid_mode_then_routes_the_audit_to_giga` (`tests/hybrid_pipeline.rs:409-460`) assert that `config.provider(Phase::Code)` is `"chat-completions"` (`mid`) and that first-attempt `Phase::Code` requests in `hybrid` mode go to `mid`.
  - Keep `Config::provider(Phase::Code)` and Attempt 1 (`handover.is_none()`) routed to `mid`; escalate to `giga` **only** when `handover.is_some()` (Attempt 2 / `retry_once`).
- **Preserve `Mode::Mid`, `Mode::Giga`, and `Phase::Summary` routing (`src/config.rs:195-300`):**
  - `Mode::Mid` retries stay on `Mode::Mid`; `Mode::Giga` retries stay on `Mode::Giga`; `Phase::Summary` continues to route exclusively to Smoll in every mode.
- **Preserve all existing `retry_tests.rs` and `src/orchestrator.rs` invariants (`tests/support/retry_tests.rs`, `src/orchestrator.rs`):**
  - Keep `retries_after_first_stop` (`src/orchestrator.rs:722-724`) limited to `FailureKind::StepLimit | FailureKind::ContextGuard` (plus `TurnEnd` and first-gate failure in `work_with_retry`), so fatal `Api`, `Transport`, and `Malformed` stops (`tests/support/retry_tests.rs:706, 810`) still skip retry and gate execution.
  - Preserve `CodeReply::Malformed` returning `{"output": []}` (`tests/common/mod.rs:1459-1469`) so malformed-response tests still decode as `Malformed`.
- **Preserve caller-intent test-title rules (`tests/test_titles.rs`):**
  - Every new test must follow `when_<action>_given_<context>_then_<outcome>` and avoid forbidden internal/builder identifiers (`new`, `with_giga`, `provider_role`, etc.).
- **Scope tracker updates strictly to `TODO.md`:**
  - Mark the `T26` row `[x]` in `TODO.md` right before running the verification gate; do not edit `.plans/T26-hybrid-code-retry-giga-escalation.md`.

---

## Proposed Design

### 1. Escalate `Phase::Code` Retry Attempts to `giga` in `hybrid` Mode (`src/config.rs`, `src/orchestrator.rs`)

1. Add `pub(crate) fn for_code_retry(&self) -> Config` on `Config` (`src/config.rs`):
   ```rust
   /// Configuration for a ticket's second (retry) code-agent attempt:
   /// in `Mode::Hybrid` (where `giga` is configured), escalate the retry to
   /// `Mode::Giga` so the `giga` provider rescues a ticket that `mid` did not
   /// finish. `Mode::Mid` and `Mode::Giga` keep their existing mode unchanged.
   pub(crate) fn for_code_retry(&self) -> Config {
       let mut retry_config = self.clone();
       if retry_config.mode == llm::Mode::Hybrid && retry_config.giga.is_some() {
           retry_config.mode = llm::Mode::Giga;
       }
       retry_config
   }
   ```
   Because `provider`, `provider_role`, `provider_model`, `input_price`, and trajectory persistence (`src/loops/agent.rs:1025-1032`) all resolve from `self.mode`, Attempt 2 in `hybrid` mode automatically dispatches over the `responses` wire format to the `giga` endpoint, reports `giga/<giga_model>` on the status line and in `trajectories/code-*.traj.json`, and prices input tokens using `giga`'s configured rate.
2. In `Production::work` (`src/orchestrator.rs:302-318`), select `self.config.for_code_retry()` when `handover.is_some()`:
   ```rust
   let selected_config = if handover.is_some() {
       self.config.for_code_retry()
   } else {
       self.config.clone()
   };
   let session = code::Session::new(&selected_config, self.scope);
   ```

### 2. Scripted Responses-API Harness Seam & Test Coverage (`tests/common/mod.rs`, `src/config.rs`, `tests/hybrid_pipeline.rs`)

1. **Responses-API empty `TurnEnd(None)` support in `ScriptedTransport` (`tests/common/mod.rs:1482-1505`):**
   - In `code_reply_response`, when `is_responses` is `true` and `output.is_empty()` (`CodeReply::TurnEnd(None)` routed to `giga`), push an empty assistant message item (`{"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": ""}]}`) instead of returning `{"output": []}` (which `responses_api` rejects as `Malformed`), so scripted `TurnEnd(None)` replies behave identically on both `mid` and `giga`.
2. **Test coverage:**
   - `src/config.rs`: caller-intent unit tests verifying `for_code_retry` escalates `Mode::Hybrid` (with `giga`) to `Mode::Giga` (`responses` provider, `"giga"` role, `giga` model, `giga` input price) while preserving `Mode::Mid`, `Mode::Giga`, and `Phase::Summary` Smoll routing.
   - `tests/hybrid_pipeline.rs`: caller-intent end-to-end hybrid retry integration test verifying Attempt 1 runs on `mid` (`common::test_url()`), fails after a tool call, and Attempt 2 escalates to `giga` (`common::test_giga_url()`, Responses API wire format) carrying `[RETRY HANDOVER]` before `Review` and `Summary` complete the run.

---

## Affected Files

- `src/config.rs`
- `src/orchestrator.rs`
- `tests/common/mod.rs`
- `tests/hybrid_pipeline.rs`
- `TODO.md`

## Verification

```bash
cargo test --lib config && cargo test --lib orchestrator && cargo test --test hybrid_pipeline && cargo test --test pipeline && cargo test --test test_titles && cargo clippy --all-targets -- -D warnings
```
