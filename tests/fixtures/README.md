# Recorded model exchanges

These fixtures are real HTTP exchanges captured between the test suite and an
OpenAI-compatible provider. Following outside-in testing principles, integration
tests against live models are strictly scoped to the wire protocol boundary
(`tests/llm_wire.rs`). They verify wire formats, JSON schema compatibility, and
tool call round trips in seconds without generating thousands of tokens or running
creative multi-agent design passes.

## How it works

The library sends every model request through `api::Transport` (`src/api.rs`)
as an `api::Outgoing` — a URL plus the already-encoded body. `Config` carries
the transport, so tests inject a different one without changing production call sites:

- **Replay** (`common::replay`) serves the recorded responses in order and
  asserts that the request the code built matches the wire contract. It never
  touches the network.
- **Live** (`common::live`) uses real HTTP. With `EJAJ_LLM_RECORD=1` it writes
  the exchanges back to the fixture.

The wire boundary tests in `tests/llm_wire.rs` exist in both forms: the default
`#[test]` replays the fixture offline, and the `live_` twin is `#[ignore]`d and
talks to the live model.

The end-to-end pipeline tests (`tests/pipeline.rs` and `tests/hybrid_pipeline.rs`)
run as outside-in behavior tests using a deterministic scripted transport,
verifying complete artifact persistence, validation, and routing in milliseconds
without model dependencies.

## Running

```sh
# Offline, deterministic, fast (this is the everyday command, finishes in <0.2s).
cargo test

# Live check of the wire boundary against the model in tests/llm.env.
cargo test --test llm_wire -- --ignored --test-threads=1

# Live check AND refresh the wire fixtures (finishes in seconds, <200 tokens).
EJAJ_LLM_RECORD=1 cargo test --test llm_wire -- --ignored --test-threads=1
```

## Layout

```
tests/fixtures/llm_wire/<test name>.json
```

Each file carries the endpoint, the model, a `recorded_at` timestamp, and the
ordered exchanges (request, status, response body).
