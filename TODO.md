# TODO

## Task DAG

```text
[T34] ──▶ [T35] ──▶ [T36] ──▶ [T37] ──▶ [T38]
```

## Status

| ID | Status | Depends On | Task |
| --- | --- | --- | --- |
| `T34` | `[ ]` | — | [Proposal 30: Deterministic sibling-dependency validation and transitive triage producer audits](./.plans/PROPOSAL-30-ticket-dependency-validation-and-transitive-triage.md) |
| `T35` | `[ ]` | `T34` | [Proposal 31: Unchanged-plan specification reuse on clean-tree downstream retries](./.plans/PROPOSAL-31-unchanged-plan-spec-reuse.md) |
| `T36` | `[ ]` | `T35` | [Proposal 32: Bounded `tickets ↔ triage` self-repair pass](./.plans/PROPOSAL-32-bounded-tickets-triage-repair-loop.md) |
| `T37` | `[ ]` | `T36` | [Proposal 23: In-memory `[DOWNSTREAM RETRY]` scope preservation and live-diff recovery guidance](./.plans/PROPOSAL-23-downstream-retry-and-dirty-tree-resume.md) |
| `T38` | `[ ]` | `T37` | [Proposal 20: Interactive read-only `ejaj --diagnose [path]` CLI mode](./.plans/PROPOSAL-20-self-diagnostic-for-self-improvement.md) |
