# AGENTS.md

## Scope

This file tells contributors how to configure, test, and record the `ejaj`
crate. Read it before you change a prompt, a tool, or an agent loop.
`tests/fixtures/README.md` gives more detail about the fixtures.

README.md and AGENTS.md must always be written in ASD-STE100 Simplified Technical English.

## Rules

- Read this guide before you change a prompt, a tool, or an agent loop.
- Run `cargo test` after each change.
- Do not edit a fixture by hand. Record it again.
- Keep the crate warning-free.

## Configuration

`AGENT_MODE` selects how the providers work together. An unset or blank
`AGENT_MODE` means `mid`. The only supported modes are `mid`, `giga`, and
`hybrid`.

| Phase | `mid` | `giga` | `hybrid` |
| --- | --- | --- | --- |
| plan | mid | giga | giga |
| spec | mid | giga | giga |
| tickets | mid | giga | giga |
| triage | mid | giga | giga |
| code | mid | giga | mid |
| review | mid | giga | giga |
| summary | smoll | smoll | smoll |

Summary always uses Smoll in every mode. The mid and giga providers cannot
replace Smoll for the summary.

The `mid` mode sends plan, spec, tickets, triage, code, and review to the mid
provider. The `giga` mode sends those phases to the giga provider. The
`hybrid` mode sends plan, spec, tickets, triage, and review to the giga
provider, and code to the mid provider.

A giga configuration needs both `LLM_GIGA_URL` and `LLM_GIGA_MODEL`. The
`giga` and `hybrid` modes need that configuration.

## Wire formats

A role selects the wire format:

- The mid provider uses the chat completions wire format.
- The giga provider uses the responses API wire format.
- The Smoll provider uses the Anthropic Messages wire format.

The application selects a provider by wire format and role settings, not by a
vendor name. Any endpoint that speaks the required format can fill a role.

## Provider settings

### Mid provider

The mid block has these settings:

- `LLM_MID_URL`
- `LLM_MID_MODEL`
- `LLM_MID_API_KEY`
- `LLM_MID_AUTH`
- `LLM_MID_REASONING_EFFORT`
- `LLM_MID_INPUT_PRICE_PER_MILLION_USD` (optional)

The mid defaults are unchanged. When `LLM_MID_URL` and `LLM_MID_MODEL` are
unset, they use `http://192.168.56.1:1234/v1/chat/completions` and
`local-model`.

`LLM_MID_AUTH=none` permits keyless use. LM Studio ignores the placeholder
key.

### Giga provider

The giga block has these settings:

- `LLM_GIGA_URL`
- `LLM_GIGA_MODEL`
- `LLM_GIGA_API_KEY`
- `LLM_GIGA_AUTH`
- `LLM_GIGA_REASONING_EFFORT`
- `LLM_GIGA_INPUT_PRICE_PER_MILLION_USD` (optional)

`LLM_GIGA_AUTH=none` is the giga switch for a keyless endpoint.

### Smoll provider

The Smoll block has these settings:

- `LLM_SMOLL_URL` (optional)
- `LLM_SMOLL_MODEL`
- `LLM_SMOLL_API_KEY`
- `LLM_SMOLL_INPUT_PRICE_PER_MILLION_USD` (optional)

The Smoll model and key are required in every mode. The URL is optional. A blank
or unset `LLM_SMOLL_URL` uses `https://api.anthropic.com/v1/messages`. Smoll
has no keyless option.

## Reasoning effort and prices

`LLM_MID_REASONING_EFFORT` and `LLM_GIGA_REASONING_EFFORT` are provider
request settings. `.env.example` shows these values: `none`, `low`,
`medium`, `high`, and `max`. The application does not validate them against
a fixed range.

The optional input price variables are local metadata. They are in USD (United
States dollars) per million input tokens. A price must be finite and
non-negative.

## Authentication and startup errors

For mid and giga, the application rejects a remote `https://` endpoint without
a key. The endpoint is accepted only when you set that role's auth switch to
`none`: `LLM_MID_AUTH=none` for mid or `LLM_GIGA_AUTH=none` for giga. The
mid error names `LLM_MID_API_KEY`, and the giga error names
`LLM_GIGA_API_KEY`.

Startup is fatal for these conditions:

- an unknown `AGENT_MODE`;
- a half-written giga block, where only one of `LLM_GIGA_URL` and
  `LLM_GIGA_MODEL` is set;
- a missing required giga block;
- a missing Smoll model or key;
- a required remote key failure.

The application prints one error line and stops startup with exit status 1.

## .env and the process environment

Process environment variables take precedence over `.env` values.

If `.env` contains duplicate keys, the application prints a warning. The first
file value wins.

`.env.example` is the worked application configuration. Do not store real keys
in it.

## Tests

`cargo test` is the offline default. It uses committed fixtures or a
deterministic in-memory model transport. It does not need a live model.

The ignored live tests use the network. They read these settings:

- `EJAJ_TEST_LLM_URL`
- `EJAJ_TEST_LLM_MODEL`
- `EJAJ_TEST_LLM_API_KEY` (optional)
- `EJAJ_TEST_ARCHITECT_URL` for two-provider tests
- `EJAJ_TEST_ARCHITECT_MODEL` for two-provider tests

The `llm_wire` test binary is the wire boundary check. Run it against a live
endpoint with the values from your `.env`:

```sh
set -a && . ./.env && set +a
EJAJ_TEST_LLM_URL="$LLM_MID_URL" \
EJAJ_TEST_LLM_MODEL="$LLM_MID_MODEL" \
EJAJ_TEST_LLM_API_KEY="$LLM_MID_API_KEY" \
  cargo test --test llm_wire -- --ignored --test-threads=1
```

The pipeline tests (`pipeline` and `hybrid_pipeline`) use a deterministic
scripted transport. They check artifact persistence, validation, and provider
routing without network access.

## Replay and fixtures

Replay does not use the network. It checks the request data against committed
fixtures.

A fixture path is `tests/fixtures/<test binary>/<test name>.json`.
`tests/llm.env` holds the local recording configuration.

Record the wire fixtures with:

```sh
EJAJ_LLM_RECORD=1 cargo test --test llm_wire -- --ignored --test-threads=1
```

The committed fixtures must name the local endpoints in `tests/llm.env`. The
recorder refuses a different hosted endpoint override while
`EJAJ_LLM_RECORD=1` is set.

Do not edit a fixture by hand. Recording stages each exchange to
`<name>.json.tmp` as it arrives. The test moves that file onto the fixture only
after the run reaches its terminal step.

## Replay failures

A replay failure shows a contract change. Read the message.

- `missing fixture ...`: The test has no fixture. Run the command in the
  message.
- `request drift at exchange N: ...`: The code builds a different request. For
  example, a prompt, a tool schema, or the transcript changed. If the change is
  correct, record the fixture again.
- `the run made N requests, the fixture records M`: The loop makes a different
  number of requests.
- `fixture ... names the model ... instead of the local ...`: A hosted
  recording reached the tree. Delete it and record it again against the local
  endpoint.

`tests/fixtures/README.md` gives more detail.
