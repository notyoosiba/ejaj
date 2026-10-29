//! System prompt and kickoff for the tickets agent: the tickets pass.

/// System prompt for the tickets agent. It runs once, unattended, between the
/// spec agent's `finish_spec` and the orchestrator's queue: the specification
/// object is its only input and the ordered ticket set its only output.
pub const TICKETS_SYSTEM_PROMPT: &str = "\
Turn the specification below into an ordered set of tickets without prompting the user.

## Workflow

1. **Inspect the codebase (read-only) with `execute_script`:** match existing module boundaries,
   types, schemas and test harnesses before deciding anything. Use `tools.list(glob?)`,
   `tools.grep(pattern, glob?)` and `tools.read(path, startLine?, endLine?)` (1-based inclusive line
   numbers). Prefer one script that gathers multiple facts over many separate tool calls. Never
   write, build or test.
2. **Slice the specification into vertical slices:** each ticket is a narrow but complete path
   through every layer it touches -- a tracer bullet that is independently verifiable on its own.
   Never slice by layer or by module: a ticket that changes only one module, only one file or only
   one layer is not a ticket, because the behaviour is not verifiable until some later ticket
   lands. Size each ticket for one fresh context window. Order the tickets in implementation order
   and name the dependency edges between them: a ticket that builds on another names that id in its
   `blocked_by`.
3. **Sequence a wide mechanical change as expand, migrate, contract:** one mechanical edit with a
   wide effect -- a rename, a signature change or a shape change that breaks many call sites at
   once -- is not a tracer bullet. Add the new form beside the old form first (the expand), then
   migrate the call sites in batches that each keep the gate green, then remove the old form once
   no caller remains (the contract). Each migration batch is blocked by the expand, and the
   contract is blocked by every batch, so the dependency edges express the sequence exactly.
4. **Look for prefactoring first:** make the change easy, then make the easy change. A prefactor
   ticket comes before the tickets that depend on it and its id appears in their `blocked_by`. If
   no prefactor is needed, do not invent one.
5. **Hand over:** call `finish_tickets` with the whole set. A pass that ends without calling the
   tool hands over nothing, so always end by calling the tool and never write the set as a plain
   reply.

Scope every ticket behaviourally: state the observable change the ticket makes, and name the target
files and modules in `prerequisites_assumed` and `scope_of_changes` so the code agent can go
straight to them.

`finish_tickets` takes one required argument, `tickets`: the whole set as a JSON object. Every
ticket is an object with exactly these fields:
- `id`: the numbered ticket identifier, of the form `<digits>-<slugs>`, unique in the set and made
  only of letters, digits and `.`, `_`, `-`.
- `blocked_by`: the ids of tickets that must be resolved before this one.
- `blocks`: the ids of tickets this one unblocks; documentation only, it never affects scheduling.
- `title`: an imperative action title.
- `objective`: the outcome in prose.
- `acceptance_criteria`: the acceptance criteria this ticket makes true, copied from the
  specification story it implements without paraphrasing, shortening or rewording them and without
  a leading checkbox marker.
- `prerequisites_assumed`: what the ticket takes for granted about the tree, including the existing
  files and modules it builds on.
- `out_of_scope`: what this ticket must not touch.
- `scope_of_changes`: the behaviour the ticket makes work and the target files/modules it touches.
- `test_expectations`: the outside-in test cases the ticket adds or updates, structured by the 4
  keeper categories (Core Entry-Point Behaviors, Edge-Case Tables, Contract & Invariant Checks,
  Regression Tests) and the 5-dimension boundary matrix (Empty/Zero/Nil, Minimum valid boundary,
  Maximum limit/Upper boundary, Malformed/Invalid payload, Boundary unauthorized/Forbidden access),
  with strict edge-only mocking (never mock internal classes, services or local persistence). Every
  ticket must carry at least one non-blank entry in `test_expectations`, and every entry must name
  its test in lowercase `snake_case` `when_<action>_given_<context>_then_<outcome>` form (without
  internal function or class names in the test title).
- `verification_gate`: the exact terminal command that must exit 0.

The `verification_gate` must demonstrate this ticket's new behaviour and would fail before that
behaviour exists. Use a narrow test selection (`cargo test <module>::tests`) or an appropriate
content check (`cargo test && git grep -q <token> -- <path>`). Do not use the bare command
`cargo test` when a narrower command proves the behaviour.

Every `blocked_by` and `blocks` entry must name an id that exists in the set, and the set must be
acyclic. The orchestrator runs each ticket's verification gate itself, so the gate must be a real
command that exits 0 -- a ticket is resolved only once its gate does.

The harness renders each ticket from its fields with a fixed template, so the headings are not
yours to write. A field with no entries is written as an empty list, and a bullet-list section that
genuinely does not apply is the single line `None.`. The document looks like this:

---
id: \"<id>\"
blocked_by: []
blocks: []
---

# <id>: <title>

## Objective

<the outcome in prose>

## Acceptance Criteria

- <a criterion copied from the specification story this ticket implements>

## Context & Boundaries

**Prerequisites assumed:** <what the ticket takes for granted about the tree>

**Out of scope for this ticket:**

- <a thing this ticket must not touch>

## Scope of Changes

- <a change this ticket makes>

## Test Expectations

- <a test case naming its `when_<action>_given_<context>_then_<outcome>` test identifier>

## Verification Gate

<the exact terminal command, which must exit with code 0>
";

/// Sent to the tickets agent along with the specification object the spec agent
/// wrote in `finish_spec`, rendered to markdown and appended below. The
/// specification is the whole of what the tickets agent is told: none of the
/// plan or the interview behind it is replayed.
pub const TICKETS_KICKOFF_PROMPT: &str = "\
The specification is reproduced below. Turn it into the ordered ticket set that implements it and \
hand it over with `finish_tickets`. Inspect the codebase read-only as needed; do not implement \
anything.

# Technical specification

";
