# ejaj

`ejaj` is an interactive agent workflow. It takes a plan through specification
and ticket creation, triage, code work, review, and a change summary. You work
with it in a terminal.

## What you need

- An existing Rust and Cargo toolchain. Use the one you have, or install one.
- A Git repository for the project you want to change.
- Model endpoints and provider keys that work.

The project does not require a particular Rust version. It has no installation
script.

## Basic setup

1. Open a terminal in the project root.
2. Copy the example environment file:

   ```sh
   cp .env.example .env
   ```

3. Edit `.env`. Put in working endpoints, models, and keys for your providers.
4. Run the application:

   ```sh
   cargo run
   ```

`.env.example` contains placeholders. `.env` is ignored by Git. Do not put a
real secret in `.env.example`, because Git tracks that file.

## Configuration

`ejaj` reads `.env`. Values in the process environment take precedence over
values in `.env`. `.env` is optional only when you supply the required
settings in the process environment.

If `.env` contains a key more than once, `ejaj` prints a warning. The first
value in the file wins.

Use `.env.example` as the worked example of the application configuration. Do
not store real keys in it.

### Modes and routing

`AGENT_MODE` selects the mode. An unset or blank `AGENT_MODE` means `mid`.
The supported modes are `mid`, `giga`, and `hybrid`.

| Phase | `mid` | `giga` | `hybrid` |
| --- | --- | --- | --- |
| plan | mid | giga | giga |
| spec | mid | giga | giga |
| tickets | mid | giga | giga |
| triage | mid | giga | giga |
| code | mid | giga | mid |
| review | mid | giga | giga |
| summary | smoll | smoll | smoll |

- In `mid` mode, plan, spec, tickets, triage, code, and review use the mid
  provider.
- In `giga` mode, those phases use the giga provider.
- In `hybrid` mode, plan, spec, tickets, triage, and review use the giga
  provider. Code uses the mid provider.
- The summary always uses Smoll in every mode. The mid or giga provider cannot
  replace Smoll for the summary.

### Wire formats

A role selects the wire format:

- The mid provider uses the chat completions wire format.
- The giga provider uses the responses API wire format.
- The Smoll provider uses the Anthropic Messages wire format.

The application selects a provider by wire format and role settings, not by a
vendor name. Any endpoint that speaks the required format can fill a role.

### Mid provider settings

- `LLM_MID_URL`
- `LLM_MID_MODEL`
- `LLM_MID_API_KEY`
- `LLM_MID_AUTH`
- `LLM_MID_REASONING_EFFORT`
- `LLM_MID_INPUT_PRICE_PER_MILLION_USD` (optional)

The mid defaults are unchanged. When `LLM_MID_URL` and `LLM_MID_MODEL` are
unset, they use `http://192.168.56.1:1234/v1/chat/completions` and
`local-model`.

Set `LLM_MID_AUTH=none` for a keyless mid endpoint. `LLM_GIGA_AUTH=none` is
the matching giga switch.

### Giga provider settings

- `LLM_GIGA_URL`
- `LLM_GIGA_MODEL`
- `LLM_GIGA_API_KEY`
- `LLM_GIGA_AUTH`
- `LLM_GIGA_REASONING_EFFORT`
- `LLM_GIGA_INPUT_PRICE_PER_MILLION_USD` (optional)

A giga configuration needs both `LLM_GIGA_URL` and `LLM_GIGA_MODEL`. The
`giga` and `hybrid` modes need that configuration.

### Smoll provider settings

- `LLM_SMOLL_URL` (optional)
- `LLM_SMOLL_MODEL`
- `LLM_SMOLL_API_KEY`
- `LLM_SMOLL_INPUT_PRICE_PER_MILLION_USD` (optional)

The Smoll model and key are required in every mode. The URL is optional. A blank
or unset `LLM_SMOLL_URL` uses `https://api.anthropic.com/v1/messages`. Smoll
has no keyless option.

### Reasoning effort and prices

`LLM_MID_REASONING_EFFORT` and `LLM_GIGA_REASONING_EFFORT` are provider
request settings. `.env.example` shows these values: `none`, `low`,
`medium`, `high`, and `max`. The application does not validate them against
a fixed range.

The optional input price variables are local metadata. They are in USD (United
States dollars) per million input tokens. A price must be finite and
non-negative.

### Authentication and startup errors

For mid and giga, the application rejects a remote `https://` endpoint without
a key. The endpoint is accepted only when you set that role's auth switch to
`none`: `LLM_MID_AUTH=none` for mid or `LLM_GIGA_AUTH=none` for giga.

Startup is fatal for these conditions:

- an unknown mode;
- a half-written giga block;
- a missing required giga block;
- a missing Smoll model or key;
- a required remote key failure.

The application stops startup with an error and exit status 1.

## Run the application

Run `cargo run`. The program opens the `plan>>` prompt. Enter a request and
answer the plan interview. Ctrl-C abandons the current input line. Ctrl-D exits.

A run needs a Git repository with a clean working tree before the agents start.
Ignored files, such as files under `.ejaj/`, do not stop the run.

### Handoff artifacts

Each handoff writes its artifacts under `.ejaj/<timestamp>/`. The pipeline
writes an artifact only when its phase produces it:

- `plan.md` after a plan handoff;
- `spec.md` after the specification pass;
- `ticket-triage.md` after the triage pass;
- one file per ticket under `tickets/` after the tickets pass;
- `queue-report.md` when the ticket queue runs;
- `review.md` when the review pass runs;
- `change-summary.md` when the summary pass runs.

### Git result

A completed run records its changes in Git.

- On the default branch with a valid `origin/HEAD`, the application updates the
  default branch by fast-forward, creates a summary branch, commits, and pushes
  the summary branch.
- On a non-default branch, or when `origin/HEAD` is confirmed absent, the
  application commits locally on the current branch.

The application does not force a push. It does not roll back automatically.

## Test

Run `cargo test`. This is the normal test command. The normal test suite uses
offline fixtures or a deterministic in-memory model transport. It does not need
a live model.

The ignored live tests use the network. `AGENTS.md` gives the live commands,
recording rules, and fixture guidance.
