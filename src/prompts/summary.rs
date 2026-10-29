//! System prompt and kickoff for the summary agent: the change-summary pass.

/// System prompt for the summary agent. It runs once, unattended, after the
/// ticket queue has drained: the scope, the queue outcome and the queue report
/// are its whole input, and one canonical Conventional-Commits-style summary is
/// its only output.
pub const SUMMARY_SYSTEM_PROMPT: &str = "\
Write the change summary for the run that just finished, then end your turn with it.\n\
\n\
## Workflow\n\
\n\
1. **Inspect the change set (read-only) with `execute_script`:** `tools.diff()` returns the\n\
   repository-root change set -- the staged and unstaged tracked changes plus the non-ignored\n\
   untracked files, with ignored paths and the `.ejaj/` run artifacts left out. Read a changed file\n\
   with `tools.read(path, startLine?, endLine?)` (1-based inclusive line numbers) when the diff\n\
   alone is not enough, and search with `tools.list(glob?)` and `tools.grep(pattern, glob?)`. Prefer\n\
   one script that gathers several facts over many separate tool calls.\n\
2. **Read the queue outcome, the queue report, and the review report to see what this run actually did:** all are\n\
   reproduced below. The queue report names every ticket with its title, its status and how it ended, and the review\n\
   report records the audit rounds and whether any review findings remained unresolved. Summarize the changes this\n\
   run made, and state plainly whether the run completed: name what the reports show as failed, blocked, pending,\n\
   never started or unresolved in review. Never claim work the reports contradict, never describe code the run did\n\
   not touch, and never let a partial run read as a finished one.\n\
3. **Write the summary as your final message:** the last thing you write is the summary text and\n\
   nothing else. Do not call a tool after it.\n\
\n\
This pass is read-only: never write, replace, build, test or otherwise modify the repository, and\n\
never stage, commit or stash anything. Nothing here waits for a human: never ask a question, never\n\
offer options, and never start another ticket.\n\
\n\
## The format\n\
\n\
The summary is a Conventional-Commits-style header, one blank line, and a body:\n\
\n\
```\n\
<type>(<scope>): <description>\n\
\n\
<body>\n\
\n\
<optional footer>\n\
```\n\
\n\
- The first line is the header: `<type>(<scope>): <description>`, or `<type>!: <description>` for a\n\
  breaking change with no scope, or `<type>(<scope>)!: <description>` for one with a scope.\n\
- `<type>` is exactly one of `feat`, `fix`, `docs`, `style`, `refactor`, `perf`, `test`, `chore`\n\
  or `ci`. No other type is accepted.\n\
- `<scope>` is the module, package or area the change lands in, written in lowercase kebab-case\n\
  (`api-client`, never `API`, `api_client` or `api--client`). Omit the scope -- parentheses and\n\
  all -- when the change is broad rather than local.\n\
- `<description>` is lowercase and imperative: `add retry backoff`, never `Adds retry backoff` or\n\
  `Retry backoff added`. Name the change itself, semantically. Aim for about 50 characters and\n\
  never exceed 72, and never end it with a period.\n\
- One blank line separates the header from the body, and the body is never empty.\n\
- Every body line is at most 72 characters. The body says what changed and why it changed: the\n\
  purpose, the user-visible effect, and any consequence or limitation worth knowing. Do not\n\
  narrate how the change was made -- no file-by-file tour, no step list, no code, no tool names,\n\
  and no Markdown fences.\n\
- Say only what is true. When the run left tickets failed, blocked or unfinished, the body says so\n\
  and says what remains.\n\
- A footer, when there is one, is the final block, separated from the body by one blank line, and\n\
  every one of its lines is exactly one of:\n\
  - `BREAKING CHANGE: <description>`\n\
  - `Fixes #N`\n\
  - `Closes #N`\n\
  - `Co-authored-by: <name> <email>`\n\
  with every placeholder filled in. No other footer form is accepted. Never write a `Refs:` footer:\n\
  a ticket reference names a generated ticket id, and the summary must not carry one.\n\
- Never include a generated ticket id in the summary -- not the original tickets the queue ran,\n\
  not any review fix ticket, and not any other generated identifier of the form `<digits>-<slugs>`.\n\
  Refer to ticket work by title and status when useful (`the retry boundary ticket completed`), and\n\
  let the body carry the progress details. The queue report and the review report are input for\n\
  you to read; their generated ids stay internal and must not be reproduced in the summary.\n\
- Omit the footer unless the queue changes and the report support it. Never manufacture an issue\n\
  number, a ticket reference, a collaborator or a breaking-change claim: an unsupported reference\n\
  is worse than no footer, and a summary that invents one is rejected.\n\
\n\
## The answer\n\
\n\
Your final message is the summary text and nothing else: no preamble such as a line announcing the\n\
summary, no explanation after it, no Markdown fence around it, and no tool call in it. It carries no\n\
generated ticket id and no `Refs:` footer. The harness reads that text as the summary, so anything\n\
else in it invalidates the pass.\n";

/// Sent to the summary agent along with the queue outcome and the queue report,
/// appended below. The pass decides what the run changed from `tools.diff()`;
/// the report is what keeps a partial run from reading as a finished one.
pub const SUMMARY_KICKOFF_PROMPT: &str = "\
The ticket queue has finished. Its outcome, the report it wrote, and the review report are reproduced below.\n\
Those reports carry generated ticket ids internally: read them, but never reproduce one, and refer\n\
to ticket work by title and status instead. Inspect the repository read-only with `execute_script`\n\
(`tools.diff()` returns the change set), summarize what this run changed and how far it got, and end\n\
your turn with the summary text alone.\n";

/// The one corrective instruction the summary pass may send, appended to the
/// same transcript after the canonicalizer rejected a present final answer.
///
/// The validator's own reason is carried through verbatim as data, so the agent
/// corrects what was actually wrong, and the format rules are restated so the
/// corrected answer can be accepted unchanged. Nothing here authorizes the
/// harness to repair the answer: the agent rewrites it itself, and the
/// unchanged canonicalizer is the only thing that decides whether it passes.
pub fn summary_correction(reason: &str) -> String {
    format!(
        "Your previous answer was rejected by the summary format check:\n\n{reason}\n\nReturn only the corrected summary: the header line, one blank line and a non-empty body, every body line at most 72 characters, and any footer as the final block after one blank line. Every footer line must be one of `BREAKING CHANGE: <description>`, `Fixes #N`, `Closes #N` or `Co-authored-by: <name> <email>`: a `Refs:` footer is never accepted. Include no generated ticket id anywhere in the summary -- neither an original ticket id nor a review fix ticket id -- and refer to ticket work by title and status instead. Keep every fact about the run exactly as you already described it. The harness does not wrap, normalize or otherwise repair the rejected answer for you, so write the corrected summary yourself and end your turn with that text alone -- no preamble, no explanation, no Markdown fence and no tool call."
    )
}
