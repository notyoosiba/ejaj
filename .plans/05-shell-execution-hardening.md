# 05: Shell Execution Hardening (`src/tools.rs`)

This document specifies three self-contained upgrades to `shell_output_with_timeout`, `run_command_in`, and `bash_in` in `src/tools.rs`:
1. **Process-group isolation (`process_group(0)`) and whole-group termination on timeout:** prevent orphaned descendant processes from surviving a command timeout, holding Cargo build locks or open ports, or blocking `collect_stream` pipe readers past `SHELL_TIMEOUT`.
2. **Partial output preservation and spill-to-disk on command timeout:** capture and surface buffered `stdout` and `stderr` bytes produced prior to a timeout instead of discarding them.
3. **Non-interactive environment variable defaults:** disable interactive pagers and progress bars in every spawned `bash` subshell.

## Guardrails for Existing `ejaj` Tests

- **Preserve the timeout error header:** `shell_timeout` in `src/tools.rs:887` formats:
  ```text
  [ERROR: exit code -1 for `<command>`]
  command timed out after <N>s
  ```
  Existing unit tests in `src/tools.rs` check this exact prefix and message line; any partial output captured before the timeout must follow on a new line after `command timed out after <N>s` (and be omitted when the captured output is empty).
- **Preserve the 512-byte command truncation in `bash_in`:** `bash_failure_prefix` (`src/tools.rs:988`) truncates quoted commands longer than `BASH_FAILURE_COMMAND_BYTES` (512 bytes) with `...`. Apply the same `BASH_FAILURE_COMMAND_BYTES` truncation to timeout prefixes in `bash_in` so a long timed-out command cannot push the delivered error past `TOOL_OUTPUT_LIMIT_BYTES` (16 KiB).
- **Preserve the 16 KiB delivery cap and spill contract:** when a timed-out command produces more than `TOOL_OUTPUT_LIMIT_BYTES` (16 KiB) of output before timing out, `bash_in` must spill the full raw bytes to `<session-dir>/tools-output/` and return the bounded digest after the timeout header.

---

## Improvements Checklist

1. **Process-Group Isolation & Whole-Group Termination (`shell_output_with_timeout`):**
   - On Unix (`#[cfg(unix)]`), configure `Command::new("bash")` with `std::os::unix::process::CommandExt::process_group(0)` so the `bash` child becomes the leader of a new process group (`pgid == child.id()`).
   - When `started.elapsed() >= timeout` (or when `collect_stream` times out because a background descendant kept an output pipe open after `bash` exited), kill the entire process group (`kill -s KILL -- -<pgid>`) before killing and reaping `child` and draining the stream reader threads.
2. **Preserve Partial `stdout` and `stderr` on Timeout (`shell_output_with_timeout`, `run_command_in`, & `bash_in`):**
   - Once `kill_process_tree` kills the process group, every descendant holding the write end of `stdout` or `stderr` terminates and the pipes close, allowing `stdout_reader` and `stderr_reader` to finish and send the partial bytes captured prior to the timeout.
   - Return a structured `ShellOutcome` (containing `status: Option<std::process::ExitStatus>`, `timeout: Duration`, `stdout: Vec<u8>`, and `stderr: Vec<u8>`) from `shell_output_with_timeout`:
     - In `run_command_in` (`src/tools.rs:760`), if the command timed out, format `shell_timeout(command, timeout)` and append `\n{text}` when `text` is non-empty.
     - In `bash_in` (`src/tools.rs:942`), if the command timed out, build the header `[ERROR: exit code -1 for `<shown>`]\ncommand timed out after <N>s` and append the captured output (or `spill_and_digest` within `TOOL_OUTPUT_LIMIT_BYTES.saturating_sub(prefix.len())` when `raw.len() > TOOL_OUTPUT_LIMIT_BYTES`).
3. **Non-Interactive Environment Defaults (`shell_output_with_timeout`):**
   - Inject non-interactive environment defaults on the `Command::new("bash")` builder:
     - `GIT_TERMINAL_PROMPT=0`
     - `PAGER=cat`
     - `GIT_PAGER=cat`
     - `MANPAGER=cat`
     - `LESS=-R`
     - `CARGO_TERM_PROGRESS_WHEN=never`
     - `PIP_PROGRESS_BAR=off`
     - `TQDM_DISABLE=1`

---

## Implementation Outline for `src/tools.rs`

```rust
#[cfg(unix)]
use std::os::unix::process::CommandExt;

/// Grace period to drain the pipe reader threads after the process group has
/// been killed on timeout.
const STREAM_DRAIN_GRACE: Duration = Duration::from_millis(200);

/// The outcome of a spawned shell command once it either exits or hits its
/// wall-clock deadline.
struct ShellOutcome {
    /// `Some(status)` when the command exited within the deadline; `None` when
    /// it timed out after `timeout`.
    status: Option<std::process::ExitStatus>,
    timeout: Duration,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Kill the process group led by `child` (on Unix) as well as `child` itself,
/// then reap `child`.
fn kill_process_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        if let Ok(pgid) = i32::try_from(child.id()) {
            let _ = Command::new("kill")
                .args(["-s", "KILL", "--", &format!("-{pgid}")])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Drain a stream reader after the process group has been killed on timeout,
/// returning whatever bytes were captured before the pipe closed.
fn drain_stream_on_timeout(reader: Receiver<std::io::Result<Vec<u8>>>) -> Vec<u8> {
    match reader.recv_timeout(STREAM_DRAIN_GRACE) {
        Ok(Ok(bytes)) => bytes,
        _ => Vec::new(),
    }
}
```

---

## Verification

```bash
cargo test --lib tools && cargo clippy --all-targets -- -D warnings
```

Add unit tests in `src/tools.rs`:
- `when_running_bash_given_background_descendant_and_timeout_then_kills_process_group_and_returns_partial_output`
- `when_running_bash_given_oversized_partial_output_before_timeout_then_spills_and_returns_bounded_digest`
- `when_running_bash_given_default_environment_then_sets_non_interactive_pager_and_progress_vars`
