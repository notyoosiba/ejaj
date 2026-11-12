# TODO

## Task DAG

```text
[T28] ──▶ [T25]
      └──▶ [T26] ──▶ [T27]
```

## Status

| ID | Status | Depends On | Task |
| --- | --- | --- | --- |
| `T28` | `[x]` | — | [Proposal 24: Phase handoff isolation and slice-scoped gates](./.plans/PROPOSAL-24-phase-handoff-isolation-and-slice-gates.md) |
| `T25` | `[x]` | `T28` | [Transition-gate diagnostic excerpt](./.plans/T25-transition-gate-diagnostic-excerpt.md) |
| `T26` | `[x]` | `T28` | [Escalate hybrid code retry to giga](./.plans/T26-hybrid-code-retry-giga-escalation.md) |
| `T27` | `[ ]` | `T26` | [Recent tool-activity trail in retry handover](./.plans/T27-retry-handover-tool-trail.md) |
