
 ────────────────────────────────────────────────────────────────────────────────

 ### 1. Timeline of What the Agent Pipeline Did

 1. Run 1 (plan → spec → tickets → triage):
     - plan interviewed the user, produced a complete 4-decision Plan, and called finish_plan.
     - spec produced a complete technical specification covering all 4 parts (finish_spec).
     - tickets split the spec into 4 tickets (1, 2, 3, 4), but introduced a slicing bug on Ticket 4:
         - It set blocked_by: [] and restricted scope_of_changes / verification_gate to a single UI module and test file,
         - while also copying a cross-cutting acceptance criterion from a shared spec story and adding cross-module assertions into Ticket 4's scope_of_changes and test_expectations (surfaces owned by Tickets 2 and 3).
     - triage marked Tickets 1–3 ready (using its shared-story narrowing rule on Ticket 3, where only acceptance_criteria overlapped), but marked Ticket 4 needs-info because the cross-module leak also appeared in scope_of_changes and test_expectations
       without blocked_by edges.
     - Because not every ticket was ready-for-agent, the harness aborted execution and instructed the user to resolve the blocker and call finish_plan again.

 2. Run 2 ([DOWNSTREAM RETRY] into plan → Silent Scope Collapse):
     - On retry, the harness initialized plan with a synthetic 3-message transcript:
         - messages[0] (system): Standard plan interview prompt.
         - messages[1] (assistant): <full 4-decision Plan from Run 1>\n\n[DOWNSTREAM RETRY]\nphase: triage\noutcome: <ticket-4> (needs-info): <blocker_explanation>
         - messages[2] (user): The user's targeted reply fixing the Ticket 4 blocker ("Restrict ticket 4 strictly to <file>... since tickets 2 and 3 already own those surfaces. Call finish_plan now.").
     - plan inspected only <file> and called finish_plan with a brand-new 1-decision Plan covering ONLY Ticket 4's slice—silently dropping Decisions 1, 2, and 3 (~80% of the feature).
     - Because spec and tickets strictly follow "the confirmed Plan is the only source of task scope; anything not in the Plan stays out", downstream phases faithfully implemented only that 1 slice, passed review, committed, and reported 100%
       completed, 0 failed.

 ────────────────────────────────────────────────────────────────────────────────

 ### 2. Root Causes in the Agent Repo

 1. Missing [DOWNSTREAM RETRY] Contract in plan + Dropped Initial User Message (Scope Collapse)
     - In Run 2's plan.traj.json, the original user request from Run 1 was dropped—the transcript goes straight from system → assistant (old plan + [DOWNSTREAM RETRY]) → user (the narrow Ticket 4 fix).
     - Meanwhile, plan's system prompt contains no instructions explaining [DOWNSTREAM RETRY]. It only tells the model to define the minimal design for the user's request and pass "the complete agreed plan" to finish_plan.
     - Seeing only a single user message saying "Restrict ticket 4 strictly to <file>...", the plan model treated that delta patch as the entire task scope, overwriting the 4-part plan with a 1-part plan.

 2. Abstraction-Layer Mismatch (triage → plan Instead of triage → tickets)
     - The triage failure was not an unanswered product question—plan.md and spec.md were already complete and consistent. It was purely a ticket-slicing / blocked_by mistake made by tickets.
     - Routing that failure back to plan puts it in the wrong layer:
         1. plan cannot read spec.md or tickets/*.md (so it has no idea what Tickets 1–4 actually say beyond the 2-line [DOWNSTREAM RETRY] snippet).
         2. plan's own system prompt explicitly forbids putting ticket boundaries, file-level scopes, acceptance criteria, or test seams into Decisions.
         3. Re-entering at plan discards the valid spec.md and forces both spec and tickets to regenerate from scratch.

 3. Missing needs-reslice State in triage
     - triage only has ready-for-agent, ready-for-human, needs-info (human question), done, and wontfix.
     - When tickets violates its own slicing rules (e.g., leaking sibling-ticket checks into scope_of_changes / test_expectations or forgetting a blocked_by edge) on something spec.md already specifies, triage is forced to emit needs-info, escalating
       an internal LLM slicing error to the human user.

 ────────────────────────────────────────────────────────────────────────────────

 ### 3. Recommended Fixes for the Agent Repo

 1. Add an unattended tickets ↔ triage repair loop for slicing defects:
     - Distinguish spec/product ambiguity (genuinely missing human info → bounce to plan) from ticket-slicing inconsistency (scope / gate / blocked_by / shared-criterion mismatch already settled by spec.md → bounce back to tickets with [TRIAGE RETRY]
       + the draft ticket set + triage findings, capped at 1–2 retries).
     - In this run, a single triage → tickets retry would have fixed Ticket 4 automatically without involving the user or re-running plan/spec.

 2. Preserve the initial user message and document [DOWNSTREAM RETRY] in plan's prompt:
     - Keep Run 1's initial user prompt in the retry transcript (system → user (original goal) → assistant (prior plan + [DOWNSTREAM RETRY]) → user (blocker answer)).
     - Add an explicit rule to plan's system prompt for [DOWNSTREAM RETRY]:
       │ "When the prior assistant turn ends with [DOWNSTREAM RETRY], the markdown above it is the active agreed Plan, and the user's reply resolves only the reported blocker. Preserve every unaffected Decision, Default, and Constraint from that Plan
       │ unless the user explicitly asks to remove or narrow the overall goal."

 3. Add a harness guardrail on finish_plan during [DOWNSTREAM RETRY]:
     - If finish_plan is called on a [DOWNSTREAM RETRY] turn and drops existing top-level numbered Decisions (e.g., going from 4 Decisions down to 1), reject the tool call with a validation error telling plan to retain the full plan while
       incorporating the blocker fix (unless an explicit drop_prior_decisions: true flag or user cancellation is present).
