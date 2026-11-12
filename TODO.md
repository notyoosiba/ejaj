# TODO

## Task DAG

```text
[T29] ──▶ [T30] ──▶ [T31] ──▶ [T32] ──▶ [T33]
```

## Status

| ID | Status | Depends On | Task |
| --- | --- | --- | --- |
| `T29` | `[x]` | — | [Proposal 25: Provider token usage decoding and pricing telemetry](./.plans/PROPOSAL-25-provider-token-usage-and-pricing-telemetry.md) |
| `T30` | `[ ]` | `T29` | [Proposal 26: Agent loop diagnostics, tool/sandbox counters, and trajectory enrichment](./.plans/PROPOSAL-26-agent-loop-and-tool-telemetry-enrichment.md) |
| `T31` | `[ ]` | `T30` | [Proposal 27: Queue timing breakdown, transition-gate timing, and ticket attempt metrics](./.plans/PROPOSAL-27-queue-contributions-and-attempt-metrics.md) |
| `T32` | `[ ]` | `T31` | [Proposal 28: Pipeline convergence metrics and `.ejaj/<timestamp>/metrics.json` artifact](./.plans/PROPOSAL-28-pipeline-convergence-and-run-metrics-artifact.md) |
| `T33` | `[ ]` | `T32` | [Proposal 29: Multi-run corpus aggregation and offline `ejaj --metrics` scorecard](./.plans/PROPOSAL-29-corpus-metrics-aggregation-and-cli-report.md) |
