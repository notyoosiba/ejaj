# Proposal 7: Include Failing Verification Gate Excerpts in `queue-report.md` and Retry Handovers

## Problem

When a ticket fails in `work_with_retry` / `retry_once` (`src/orchestrator.rs:507-554, 605-625, 731-744`), `Ending::GateFailed` and `Ending::RetryExhausted` discard the gate output:

```rust
enum Ending {
    Declared,
    GateAccepted,
    RetryExhausted,
    GateFailed,
    AgentFailure(FailureKind),
    Blocked(String),
    NeverStarted,
}
```

Because the failing gate's stdout/stderr is only printed to stderr and omitted from `Ending` and `queue-report.md`:
1. The `summary` agent cannot tell which check or test failed when summarizing an incomplete run.
2. `queue_retry_detail` (`src/pipeline.rs:890-951`) hands the `plan` agent only `"<id>: gate failed"` without any diagnostic context.

---

## Proposed Design

### 1. Carry a Bounded Gate Failure Excerpt in `Ending` (`src/orchestrator.rs`)

Add a helper `gate_failure_excerpt(output: &str) -> String` in `src/orchestrator.rs` (retaining up to the last 1,200 characters of trimmed output, matching `transition_failure_excerpt` in `src/pipeline.rs:1101`) and store it on `Ending::GateFailed(String)` and `Ending::RetryExhausted(String)`:

```rust
enum Ending {
    /// The agent declared it, and the gate passed.
    Declared,
    /// The agent never declared it, but the gate passed.
    GateAccepted,
    /// The agent never declared it, and the gate failed too; carries a bounded
    /// excerpt of the final gate output.
    RetryExhausted(String),
    /// The agent declared it, and the gate failed; carries a bounded excerpt of
    /// the final gate output.
    GateFailed(String),
    /// The loop broke off; the label names why.
    AgentFailure(FailureKind),
    /// The queue never reached it: the named failure blocks it.
    Blocked(String),
    /// The queue stopped before it was reached.
    NeverStarted,
}
```

### 2. Render the Excerpt in `Report::markdown` (`src/orchestrator.rs:614-623`)

When `ending` is `Ending::GateFailed(excerpt)` or `Ending::RetryExhausted(excerpt)` with a non-empty `excerpt`, append `- Gate output: ` followed by a single-line or fenced excerpt in `Report::markdown` so both `Phase::Summary` and `queue_failures` in `src/pipeline.rs:914` can read why the gate failed.

---

## Affected Files

- `src/orchestrator.rs`
- `src/pipeline.rs`
- `tests/pipeline.rs`

## Verification

```bash
cargo test --lib orchestrator && cargo test --test pipeline && cargo clippy --all-targets -- -D warnings
```
