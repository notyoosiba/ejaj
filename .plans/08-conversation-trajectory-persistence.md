# 08: Conversation Trajectory Persistence (`src/loops/agent.rs`)

This document specifies how to persist full conversation trajectories (`*.traj.json`) inside the active session directory `.ejaj/<timestamp>/trajectories/`.

## Problem Statement

Currently, `ejaj` writes phase handoff artifacts (`plan.md`, `spec.md`, `tickets/`, `ticket-triage.md`, `queue-status.json`, `queue-report.md`, `review.md`, `change-summary.md`) and oversized `bash` spill logs (`tools-output/step-N-call-M.log`) into `.ejaj/<timestamp>/`. However, the conversation transcript (`Vec<Message>`) for each agent loop is dropped from memory as soon as `run_agent_loop_capturing` returns.

When a ticket attempt fails, hits `FailureKind::StepLimit` or `FailureKind::ContextGuard`, or triggers Tier 1/Tier 2 compaction, there is no saved record of the assistant's reasoning, the `execute_script` calls and outputs, or the compacted history.

Persisting `messages` and loop exit metadata to `.ejaj/<timestamp>/trajectories/<phase>.traj.json` (with numeric collision suffixes `<phase>-1.traj.json`, `<phase>-2.traj.json`, etc. across ticket attempts and review rounds) provides a complete post-mortem audit trail of every agent invocation.

---

## Guardrails for Existing `ejaj` Tests

- **Confine writes strictly to an explicitly supplied `session_dir`:** Just like `save_spill` in `src/tools.rs:1052`, trajectory persistence must only run when `invocation.session_dir` is `Some(session_dir)`. Context-free unit test calls (`InvocationContext::default()`) must never create directories or write files to disk.
- **Keep `.ejaj/.gitignore` intact:** Call `crate::tools::ignore_run_artifacts(&directory)` (or ensure `ignore_run_artifacts` is `pub(crate)` in `src/tools.rs`) when creating `<session-dir>/trajectories/` so saved trajectories never appear in `git status` or `tools.diff()`.
- **Best-effort persistence:** A failure to write a diagnostic trajectory file must log a `[WARN]` on `stderr` and never abort or alter the `Stop` result of a phase or ticket.

---

## Improvements Checklist

1. **Define a Structured Trajectory Document (`TrajectorySnapshot`):**
   - Serialize a JSON object with:
     - `phase`: canonical phase label (`"plan"`, `"spec"`, `"tickets"`, `"triage"`, `"code"`, `"review"`, `"summary"`)
     - `role`: provider role (`"mid"`, `"giga"`, `"smoll"` from `config.provider_role(phase)`)
     - `model`: provider model identifier (`config.provider_model(phase)`)
     - `steps_taken`: number of model turns executed in the loop
     - `exit_status`: string label describing how the loop exited (`"handoff"`, `"spec"`, `"tickets"`, `"triage"`, `"review"`, `"complete"`, `"turn_end"`, `"summary"`, or `FailureKind::label()`)
     - `messages`: the final `&[Message]` transcript at loop exit
2. **Persist Trajectories Under `<session-dir>/trajectories/`:**
   - Resolve `<session-dir>/trajectories` (anchored to `invocation.root` when `session_dir` is relative, matching `spill_directory` in `src/tools.rs:1028`).
   - Use `OpenOptions::new().write(true).create_new(true)` with collision suffixes `0..10_000`:
     - Suffix `0`: `<phase>.traj.json` (e.g., `spec.traj.json`, `code.traj.json`)
     - Suffix `1..`: `<phase>-<suffix>.traj.json` (e.g., `code-1.traj.json`, `code-2.traj.json`, `review-1.traj.json`)
   - Ensure `run_agent_loop_capturing` saves the trajectory before returning `Stop` on every exit path.

---

## Implementation Outline for `src/loops/agent.rs`

```rust
const TRAJECTORY_DIRECTORY: &str = "trajectories";

#[derive(serde::Serialize)]
struct TrajectorySnapshot<'a> {
    phase: &'static str,
    role: &'static str,
    model: &'a str,
    steps_taken: usize,
    exit_status: &'static str,
    messages: &'a [Message],
}

impl Stop {
    fn exit_status_label(&self) -> &'static str {
        match self {
            Stop::TurnEnd => "turn_end",
            Stop::Summary(_) => "summary",
            Stop::Failed(kind) => kind.label(),
            Stop::Handoff(_) => "handoff",
            Stop::Spec(_) => "spec",
            Stop::Tickets(_) => "tickets",
            Stop::Review(_) => "review",
            Stop::Triage(_) => "triage",
            Stop::Complete => "complete",
        }
    }
}

fn save_trajectory(
    invocation: InvocationContext<'_>,
    config: &Config,
    phase: Phase,
    steps_taken: usize,
    stop: &Stop,
    messages: &[Message],
) -> Result<(), String> {
    let Some(session_dir) = invocation.session_dir else {
        return Ok(());
    };
    let base = match invocation.root {
        Some(root) if !session_dir.is_absolute() => root.join(session_dir),
        _ => session_dir.to_path_buf(),
    };
    let directory = base.join(TRAJECTORY_DIRECTORY);
    std::fs::create_dir_all(&directory)
        .map_err(|e| format!("could not create the trajectories directory: {e}"))?;
    crate::tools::ignore_run_artifacts(&directory)
        .map_err(|e| format!("could not prepare the self-ignoring .ejaj directory: {e}"))?;

    let snapshot = TrajectorySnapshot {
        phase: phase.label(),
        role: config.provider_role(phase),
        model: config.provider_model(phase),
        steps_taken,
        exit_status: stop.exit_status_label(),
        messages,
    };
    let encoded = serde_json::to_vec_pretty(&snapshot)
        .map_err(|e| format!("could not serialize trajectory JSON: {e}"))?;

    let phase_label = phase.label();
    for suffix in 0..10_000usize {
        let file_name = if suffix == 0 {
            format!("{phase_label}.traj.json")
        } else {
            format!("{phase_label}-{suffix}.traj.json")
        };
        let path = directory.join(&file_name);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                if let Err(e) = std::io::Write::write_all(&mut file, &encoded) {
                    let _ = std::fs::remove_file(&path);
                    return Err(format!("could not write trajectory file: {e}"));
                }
                return Ok(());
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("could not create trajectory file: {e}")),
        }
    }
    Err("could not create a unique trajectory file after 10000 attempts".to_string())
}
```

---

## Verification

```bash
cargo test --lib loops::agent && cargo test --test pipeline && cargo clippy --all-targets -- -D warnings
```

Add unit tests in `src/loops/agent.rs`:
- `when_loop_finishes_given_session_dir_then_writes_numbered_traj_json_under_trajectories`
- `when_loop_finishes_given_no_session_dir_then_writes_no_trajectory_file`
