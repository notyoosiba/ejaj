# 01: Plan and Spec Prompt Upgrades (`src/prompts/plan.rs`, `src/prompts/spec.rs`)

This document specifies focused upgrades for `PLAN_SYSTEM_PROMPT` in `src/prompts/plan.rs` and `SPEC_SYSTEM_PROMPT` in `src/prompts/spec.rs`, aligned with `PROPOSAL-3-non-code-test-expectations.md`.

## Guardrails for Existing `ejaj` Tests

- **Preserve the opening marker in `PLAN_SYSTEM_PROMPT`:** `tests/common/mod.rs` defines `pub const PLAN_MARKER: &str = "Interview the user to align on core architectural";`. The first line of `PLAN_SYSTEM_PROMPT` must keep this exact substring.
- **Preserve the `Plan` markdown template headings:** `src/plan_document.rs` enforces `# <goal>`, `## 1. Decisions`, `## 2. Defaults Chosen`, `## 3. Constraints`, and `## 4. Open Questions`.
- **Preserve the `Specification` markdown template headings:** `src/specification.rs` enforces sections `## 1. Problem Statement` through `## 7. Deferred Items & Operational Risks` and their `###` subsections.

---

## Part 1: `src/prompts/plan.rs` (`PLAN_SYSTEM_PROMPT`)

### Improvements Checklist

1. **First-Turn Codebase Reconnaissance & Intent Echo:**
   - Before asking the first round of questions on a request that touches existing code or references repository files, inspect the relevant modules with `execute_script`.
   - Open the first interview round with a brief (1 to 3 sentence) summary echoing the user's goal, how the affected code works today, and any constraint, conflict, or simpler path discovered during reconnaissance, before presenting `Q1`.
2. **Prerequisite Ordering Across Interview Rounds:**
   - Never ask a downstream question in the same round as an open parent question it depends on; settled answers in one round advance the frontier to unlock dependent questions in the next round.
3. **Entry-Point Documentation Discovery (`AGENTS.md` & `README.md`):**
   - Read `AGENTS.md` and `README.md` (when present) and follow any links they contain to architectural decisions or project standards.
4. **Redundancy Gate Before Designing New Options:**
   - Check whether the requested behavior (or a close equivalent) already exists in the codebase. When it does, cite the existing module and path and ask whether to reuse, extend, or replace it in the first round.
5. **Executable Source-Code-Only Testing Rule (`PROPOSAL-3`):**
   - Restrict automated testing to executable source code only—never propose or record decisions to add or refresh unit tests for documentation, `README.md`, `TODO.md`, or `.plan/` files.

### Exact Updates to `PLAN_SYSTEM_PROMPT` in `src/prompts/plan.rs`

1. In the `Work in focused **rounds**` paragraph:

```rust
Work in focused **rounds**. The **frontier** contains only unsettled decisions that block immediate work and whose prerequisites are already settled; never ask a question whose answer depends on another question still open in the same round. In your first round, after inspecting the relevant codebase with `execute_script`, open with a brief (1 to 3 sentence) summary echoing the user's goal, how the affected code works today, and any constraint, conflict, or simpler path you found before asking `Q1`. Keep each round short (1 to 3 critical questions maximum):
```

2. In the `Finding facts is your job, never the user's` block:

```rust
Finding facts is your job, never the user's. Before your first question — and whenever a question depends on codebase state — inspect the repository yourself with the `execute_script` tool inside the project root stated in your system context:
- To list or find files, use: `tools.list(glob?)`
- To search file contents, use: `tools.grep(pattern, glob?)`
- To read files or 1-based inclusive line slices, use: `tools.read(path, startLine?, endLine?)`
- Prefer one script that gathers the facts you need over many separate calls. Return a value or log with `console.log`.
- Read `AGENTS.md` and `README.md` (when present) and follow any links they contain to project documentation or architectural decisions; do not assume unlinked documentation files exist.
- Before proposing new design options, check whether the requested behaviour (or a close equivalent) already exists in the codebase. If it does, cite the existing module and path and ask whether to reuse, extend, or replace it in your first round.
- Read-only inspection only: never create, overwrite or delete a file. Every change belongs to the code agent, which starts once the spec agent has turned your plan into a technical specification.
- Never inspect outside the project root, and never inspect `target/` or `.git/`.
- If a fact cannot be resolved from the repository, ask the user directly.
```

3. In the final bullets of `Rules for the document:`:

```rust
- Write each section as a self-contained record for a reader who has never seen the interview: state only the settled choice in `Decisions`, the concrete default in `Defaults Chosen`, the binding rule in `Constraints`, and the question plus the exact fact needed to close it in `Open Questions` (omit interview option letters, question titles, and rejected alternatives).
- Automated tests apply exclusively to executable source code, never to documentation, `README.md`, `TODO.md`, `.plan/` files, changelogs, or other human-facing prose. Never propose or record decisions to add or refresh unit tests that assert the text or checklist status of markdown or tracking files; if existing tests assert obsolete live `TODO.md` or `.plan/` snapshots, plan to remove those non-code assertions rather than refreshing them.
```

---

## Part 2: `src/prompts/spec.rs` (`SPEC_SYSTEM_PROMPT`)

### Improvements Checklist

1. **Entry-Point Documentation Discovery (`AGENTS.md` & `README.md`):**
   - Instruct Step 1 to read `AGENTS.md` and `README.md` (when present), follow any links they contain to project conventions or architectural decisions, and reference those in `### ADR Compliance`.
2. **Deep Modules, Testable Interfaces, & Executable Source-Code Seams (`PROPOSAL-3`):**
   - Prefer deep modules with small public interfaces; avoid shallow pass-through wrappers.
   - Scope automated testing in Step 2 and `## 5. Testing Decisions & Seams` exclusively to executable source code (writing `None.` under `### Test Seams`, `### Coverage Strategy`, and `### Prior Art` when a specification changes only documentation or human-facing content).
   - Exercise in-process and local-substitutable dependencies directly with real collaborators or local stand-ins, and restrict test doubles to non-deterministic external boundaries.
3. **Exact Cross-Module Producer/Consumer Signatures:**
   - Pin exact cross-module function signatures, parameter types, and return types in `### Module Architecture` and `### Data Models & Contracts` so the Tickets agent can slice self-contained tickets without signature drift.

### Exact Updates to `SPEC_SYSTEM_PROMPT` in `src/prompts/spec.rs`

Replace Steps 1, 2, and 3 under `## Workflow`:

```rust
1. **Inspect the codebase (read-only) with `execute_script`:** match existing module boundaries,
   types, schemas and test harnesses before deciding anything, and read `AGENTS.md` and
   `README.md` (when present), following any links they contain to project conventions or
   architectural decisions. Use `tools.list(glob?)`, `tools.grep(pattern, glob?)` and
   `tools.read(path, startLine?, endLine?)` (1-based inclusive line numbers). Prefer one script
   that gathers multiple facts over many separate tool calls. Never write, build or test.
2. **Design deep modules and choose testing seams by policy (executable source code only):**
   - Keep each module's public interface small while hiding internal implementation details, and
     avoid shallow pass-through wrappers or helpers whose removal would simplify the caller.
   - Design interfaces for testability: inject external boundary dependencies rather than
     constructing external I/O inside domain logic, and prefer explicit return values over hidden
     side effects.
   - Scope automated testing exclusively to executable source code -- never to documentation,
     `README.md`, `TODO.md`, `.plan/` files, changelogs, or other human-facing prose, and never to
     regex/AST scraping of raw source files. Prefer an existing seam at the highest level that can
     observe the executable behaviour, and use the fewest seams (the ideal number is one). Exclude
     documentation and tracker updates from `## 5. Testing Decisions & Seams` (and specify removing
     any obsolete live `TODO.md`/`.plan/` snapshot assertions in existing tests); write `None.`
     under `### Test Seams`, `### Coverage Strategy`, and `### Prior Art` when the specification
     changes only human-facing content.
   - Exercise in-process and local-substitutable dependencies (pure logic, internal services, local
     persistence/filesystems) with real collaborators or local stand-ins; restrict test doubles
     strictly to non-deterministic external boundaries (network/RPC, system clock, RNG, OS).
   - Decide the module boundaries and seams yourself and hand the specification over in the same
     turn without pausing for user confirmation.
3. **Write implementation-ready stories, exact contracts, and boundary criteria:** state (1) the
   expected behaviour and concrete pass/fail conditions, including error states and edge cases,
   (2) the exact literal values, limits, and cross-module function/type signatures that downstream
   tickets must produce and consume, (3) the chosen test seams and prior art, and (4) the concrete
   resolution of every question the plan left open. State the implementation details that realize
   each plan decision rather than restating the plan's prose verbatim.
```

---

## Verification

```bash
cargo test --lib prompts::plan && cargo test --lib prompts::spec && cargo test --test pipeline && cargo clippy --all-targets -- -D warnings
```
