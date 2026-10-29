//! The summary agent: the change-summary pass.
//!
//! One fresh, unattended pass after the ticket queue has drained. It is handed
//! the scope, the queue outcome and the queue report, inspects the repository
//! read-only through `tools.diff()`, and ends by writing the summary itself: the
//! phase offers no control tool, so the pass stops when the model stops calling
//! tools and its final text is the answer. That text is normalized and validated
//! here into the canonical summary the caller stores -- a model answer that does
//! not satisfy the format is a generation failure, never a successful result. A
//! present answer the format refuses buys the next candidate in the same
//! transcript, up to four candidates total, and every corrected answer is held
//! to the unchanged canonicalizer before it can become the result.

use crate::Config;
use crate::api::Message;
use crate::prompts;
use crate::tools::{Phase, Source};

use super::FailureKind;
use super::InvocationContext;
use super::agent::{self, Stop};

/// The Conventional-Commits types a header may carry. Nothing else is a valid
/// type: a summary is a commit-style headline, not free prose.
pub const TYPES: [&str; 9] = [
    "feat", "fix", "docs", "style", "refactor", "perf", "test", "chore", "ci",
];

/// The longest header description accepted, in Unicode characters.
pub const DESCRIPTION_LIMIT: usize = 72;
/// The description length the prompt aims at, in Unicode characters.
pub const DESCRIPTION_TARGET: usize = 50;
/// The longest body line accepted, in Unicode characters.
pub const BODY_LINE_LIMIT: usize = 72;

/// The reason a summary is refused for reproducing a generated ticket id. It
/// names the violation and never the id itself, so neither the console
/// diagnostic nor the correction instruction echoes an id back.
const GENERATED_ID_REASON: &str = "the summary reproduces a generated ticket id; a summary refers to ticket work by title and status and never carries a generated original-ticket or review-fix-ticket id";

/// The reason a `Refs:` footer is refused. It names the form and never the
/// reference the line carried, so a `Refs:` attempt that named a ticket id
/// cannot echo that id back either.
const REFS_FOOTER_REASON: &str = "a `Refs:` footer is not accepted: a footer line is one of `BREAKING CHANGE: <description>`, `Fixes #N`, `Closes #N` or `Co-authored-by: <name> <email>`, and ticket work is described by title and status in the body instead";

/// Why one [`summarize`] returned.
#[derive(Debug)]
pub enum Outcome {
    /// The canonical summary text: one header line, one blank line, a non-empty
    /// body and an optional footer, ending with exactly one LF.
    Summary(String),
    /// The pass produced no usable summary. The reason names what was wrong --
    /// empty text, a header or a footer the format does not allow, an overlong
    /// line, a fence or a preamble -- so the caller can report it as the
    /// generation failure it is.
    Invalid(String),
    /// The loop broke off on an error or the step limit; the detail is already
    /// on screen.
    Failed(FailureKind),
}

/// Write one change summary for the run the queue just finished.
///
/// One fresh transcript per call: the scope, the queue outcome and the queue
/// report are the whole of what the pass is told, and nothing from any other
/// pass is replayed. The pass is unattended and ends on its own final turn, so
/// it neither asks the user anything nor starts another ticket. `outcome` is the
/// queue outcome in the caller's own wording and `report` is the report the
/// queue wrote; both are reproduced to the agent so it can tell what this run
/// changed and how far it got.
///
/// Each present final answer the canonicalizer rejects buys the next candidate
/// in the same transcript, up to four candidates total, with that candidate's
/// own validator reason handed over as data. A candidate is accepted only when
/// the same unchanged canonicalizer -- format and generated-ticket-id gate
/// together -- accepts it; an absent answer, a generation or correction call
/// that fails, and a fourth rejected candidate all keep their fatal outcome
/// variants, and nothing is ever repaired on the agent's behalf.
///
/// `ticket_ids` is the complete set of generated ticket ids this run created:
/// every original typed ticket and every accepted review report's fix tickets.
/// A canonical answer that reproduces one of them is refused, while the reports
/// themselves -- and the ids they carry -- are reproduced to the agent
/// unchanged.
pub fn summarize(
    config: &Config,
    scope: &str,
    outcome: &str,
    report: &str,
    review: &str,
    ticket_ids: &[String],
) -> Outcome {
    summarize_in(
        config,
        scope,
        outcome,
        report,
        review,
        ticket_ids,
        InvocationContext::default(),
    )
}

/// The same summary pass with an explicit invocation context, forwarded to
/// every tool dispatch the pass makes and to every correction it sends.
pub(crate) fn summarize_in(
    config: &Config,
    scope: &str,
    outcome: &str,
    report: &str,
    review: &str,
    ticket_ids: &[String],
    invocation: InvocationContext<'_>,
) -> Outcome {
    let mut messages = vec![
        Message::system(format!("{}{}", prompts::SUMMARY_SYSTEM_PROMPT, scope)),
        Message::user(format!(
            "{}{}",
            prompts::SUMMARY_KICKOFF_PROMPT,
            kickoff(outcome, report, review)
        )),
    ];
    summary_pass(&mut messages, config, ticket_ids, invocation)
}

/// The most summary answers one pass may generate: the initial candidate plus
/// up to three corrections, all in the same transcript. A present answer the
/// validator rejects at this limit fails the pass with that candidate's own
/// reason instead of buying a fifth request.
const MAX_CANDIDATES: usize = 4;

/// Run the bounded candidate-and-correction pass over one summary transcript.
///
/// The caller owns the transcript's opening system and kickoff turns. This owns
/// the candidate loop: at most MAX_CANDIDATES total answers are generated in the
/// same transcript. A present answer the unchanged canonicalizer rejects below
/// the limit clears reasoning from the messages already present and appends
/// summary_correction with that candidate's exact reason; the first accepted
/// answer returns immediately, and the fourth rejection is the pass's failure.
/// An absent answer or a generation failure stays fatal and never buys a
/// correction.
fn summary_pass(
    messages: &mut Vec<Message>,
    config: &Config,
    ticket_ids: &[String],
    invocation: InvocationContext<'_>,
) -> Outcome {
    let mut candidate = 0;
    loop {
        candidate += 1;
        let answer = match generate(messages, config, invocation) {
            Ok(answer) => answer,
            Err(outcome) => return outcome,
        };
        // An absent answer is not a rejected one: there is no text to correct,
        // so the pass keeps its existing fatal behavior.
        let Some(answer) = answer else {
            return Outcome::Invalid(absent_answer_reason());
        };

        match canonicalize_with_ticket_ids(&answer, ticket_ids) {
            Ok(summary) => return Outcome::Summary(summary),
            Err(reason) => {
                if candidate == MAX_CANDIDATES {
                    return Outcome::Invalid(reason);
                }
                // A present answer the format refuses below the limit buys the
                // next candidate in the same transcript: the validator's own
                // reason goes back to the agent as data, and the next answer is
                // held to the unchanged rules. Nothing here repairs the answer
                // on the agent's behalf. Reasoning belongs to the turns that
                // produced it, not to the correction boundary, so every earlier
                // message loses only that.
                clear_reasoning(messages);
                messages.push(Message::user(prompts::summary_correction(&reason)));
            }
        }
    }
}

/// Clear every earlier message's reasoning at the correction boundary, leaving
/// every other transcript field untouched.
fn clear_reasoning(messages: &mut [Message]) {
    for message in messages {
        message.reasoning_content = None;
    }
}

/// One summary generation turn: the tool loop's own stop mapped onto the pass's
/// outcomes and run over the caller's transcript, so each correction is appended
/// to the same messages the rejected answer was written into.
///
/// `Ok(Some(text))` is the turn's final text, which the caller canonicalizes;
/// `Ok(None)` is a turn that carried no final content at all, which is the
/// difference between an absent answer and an empty one.
fn generate(
    messages: &mut Vec<Message>,
    config: &Config,
    invocation: InvocationContext<'_>,
) -> Result<Option<String>, Outcome> {
    match agent::run_agent_loop_with_context(
        messages,
        config,
        Phase::Summary,
        Source::None,
        invocation,
    ) {
        Stop::Summary(answer) => Ok(answer),
        // The summary phase offers no control tool, so the loop can only end on
        // the model's own final turn. Any other stop means the pass was cut
        // short before a summary existed.
        Stop::TurnEnd => Err(Outcome::Invalid(
            "the pass ended without any final text to summarise the run".to_string(),
        )),
        Stop::Failed(kind) => Err(Outcome::Failed(kind)),
        Stop::Handoff(_)
        | Stop::Spec(_)
        | Stop::Tickets(_)
        | Stop::Review(_)
        | Stop::Triage(_)
        | Stop::Complete => {
            unreachable!("the summary phase offers no control tool")
        }
    }
}

/// Why an absent final answer fails the pass, in the same terms
/// [`canonicalize`] uses for an empty one: there is nothing to correct, so no
/// correction request is sent.
fn absent_answer_reason() -> String {
    "the answer is empty: a summary is a header line, one blank line and a non-empty body"
        .to_string()
}

/// The payload the pass is handed after the kickoff: the queue's own outcome,
/// then the report it wrote. Each half falls back to a named placeholder, so an
/// empty half is visible to the agent instead of silently vanishing.
fn kickoff(outcome: &str, report: &str, review: &str) -> String {
    let outcome = non_empty_or(outcome, "(the queue reported no outcome)");
    let report = non_empty_or(report, "(the queue wrote no report)");
    let review = non_empty_or(review, "(no review report was written)");
    format!("## Outcome\n\n{outcome}\n\n## Report\n\n{report}\n\n## Review\n\n{review}\n")
}

/// `text` when it carries something, `fallback` when it is only whitespace.
fn non_empty_or<'a>(text: &'a str, fallback: &'a str) -> &'a str {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        fallback
    } else {
        trimmed
    }
}

/// Normalize, validate and canonicalize one model answer into the summary text.
///
/// The normalization is the whole of what is repaired: CRLF (and a lone CR)
/// becomes LF, trailing whitespace is dropped from every line, blank lines
/// around the body are trimmed and the result ends in exactly one LF. Everything
/// the format fixes is enforced instead of repaired -- the type, the scope, the
/// lowercase description, the line widths, the blank line between the header and
/// the body, the non-empty body and the four accepted footer forms -- so a
/// malformed answer is reported as the generation failure it is and never
/// silently rewritten into a summary the model did not write.
pub fn canonicalize(answer: &str) -> Result<String, String> {
    let normalized = answer.replace("\r\n", "\n").replace('\r', "\n");
    let mut lines: Vec<&str> = normalized.split('\n').map(str::trim_end).collect();
    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }

    if lines.is_empty() {
        return Err(
            "the answer is empty: a summary is a header line, one blank line and a non-empty body"
                .to_string(),
        );
    }
    if let Some(fence) = lines
        .iter()
        .find(|line| line.trim_start().starts_with("```"))
    {
        return Err(format!(
            "the answer carries a Markdown fence (`{}`); a summary is plain text with no fence",
            fence.trim()
        ));
    }

    let header = lines[0];
    if let Err(reason) = validate_header(header) {
        // A header that parses on a later line means the answer opened with
        // prose -- an announcement, a fence or a duplicate header. Naming that
        // is more useful than reporting the first line as an invalid header.
        if lines
            .iter()
            .skip(1)
            .any(|line| validate_header(line).is_ok())
        {
            return Err(format!(
                "the answer starts with `{}`, which is not the header; a summary begins with exactly one header line and carries no preamble, no fence and no duplicate header",
                header.trim()
            ));
        }
        return Err(reason);
    }

    if lines.len() < 2 {
        return Err(
            "the summary has no body: the header must be followed by a blank line and then a \
             non-empty body that says what changed and why"
                .to_string(),
        );
    }
    if !lines[1].is_empty() {
        return Err(
            "the header must be followed by exactly one blank line and then the body".to_string(),
        );
    }

    // Everything after that separator: the body, plus the footer when the last
    // block is one. A footer is recognized by its own keywords, so a block of
    // ordinary prose is always body.
    let rest: Vec<&str> = lines[2..].to_vec();
    let mut body = rest.clone();
    let mut footer: Vec<String> = Vec::new();
    if let Some(blank) = rest.iter().rposition(|line| line.is_empty()) {
        let candidate = &rest[blank + 1..];
        if candidate.first().is_some_and(|line| is_footer_like(line)) {
            footer = candidate
                .iter()
                .map(|line| line.trim().to_string())
                .collect();
            body = rest[..blank].to_vec();
        }
    }

    // The format has exactly one place for a footer. A footer line anywhere else
    // -- most often a missing blank line -- is a malformed structure, not body
    // prose.
    if let Some(stray) = body.iter().find(|line| is_footer_like(line)) {
        if is_refs_footer(stray) {
            return Err(REFS_FOOTER_REASON.to_string());
        }
        return Err(format!(
            "the footer line `{}` is not in a final block separated from the body by one blank line",
            stray.trim()
        ));
    }
    for line in &footer {
        validate_footer(line)?;
    }

    // Blank lines around the body are canonicalized away, so the body is exactly
    // what sits between the header's blank line and the footer's blank line.
    while body.first().is_some_and(|line| line.is_empty()) {
        body.remove(0);
    }
    while body.last().is_some_and(|line| line.is_empty()) {
        body.pop();
    }
    if body.iter().all(|line| line.is_empty()) {
        return Err(
            "the summary has no body: the header must be followed by a blank line and a non-empty body that says what changed and why"
                .to_string(),
        );
    }
    for line in &body {
        let width = line.chars().count();
        if width > BODY_LINE_LIMIT {
            return Err(format!(
                "a body line is {width} characters; every body line must be at most {BODY_LINE_LIMIT} characters"
            ));
        }
    }

    let mut canonical = String::new();
    canonical.push_str(header);
    canonical.push_str("\n\n");
    canonical.push_str(&body.join("\n"));
    if !footer.is_empty() {
        canonical.push_str("\n\n");
        canonical.push_str(&footer.join("\n"));
    }
    canonical.push('\n');
    Ok(canonical)
}

/// Canonicalize one answer and refuse any answer that reproduces a generated
/// ticket id.
///
/// The format is validated first, so a malformed answer keeps its own reason;
/// then the canonical text -- header, body and footer together -- is scanned,
/// case-sensitively, for each non-empty trimmed id. A match anywhere is a
/// generation failure: the answer is rejected, never rewritten to strip the id
/// out. `ticket_ids` is the run's complete set of generated original-ticket and
/// review-fix-ticket ids; blank entries are ignored.
pub fn canonicalize_with_ticket_ids(answer: &str, ticket_ids: &[String]) -> Result<String, String> {
    let canonical = canonicalize(answer)?;
    for id in ticket_ids {
        let id = id.trim();
        if id.is_empty() {
            continue;
        }
        if canonical.contains(id) {
            return Err(GENERATED_ID_REASON.to_string());
        }
    }
    Ok(canonical)
}

/// Validate one header line: `<type>(<scope>): <description>`,
/// `<type>!: <description>`, or `<type>(<scope>)!: <description>`.
fn validate_header(header: &str) -> Result<(), String> {
    let type_end = header
        .find(|c: char| !c.is_ascii_lowercase())
        .unwrap_or(header.len());
    let (type_name, rest) = header.split_at(type_end);

    if !TYPES.contains(&type_name) {
        return Err(format!(
            "the header must start with one of the types {}; found `{header}`",
            TYPES.join(", ")
        ));
    }

    let mut rest = rest;
    if let Some(after_open) = rest.strip_prefix('(') {
        let Some(close) = after_open.find(')') else {
            return Err(format!(
                "the header `{header}` opens a scope with `(` but never closes it"
            ));
        };
        let scope = &after_open[..close];
        if let Err(reason) = validate_scope(scope) {
            return Err(format!("the header `{header}`: {reason}"));
        }
        rest = &after_open[close + 1..];
    }
    if let Some(after_mark) = rest.strip_prefix('!') {
        rest = after_mark;
    }

    let Some(description) = rest.strip_prefix(": ") else {
        return Err(format!(
            "the header `{header}` must continue with `: <description>` after the type, its optional scope and its optional `!`"
        ));
    };

    validate_description(description).map_err(|reason| format!("the header `{header}`: {reason}"))
}

/// Validate a scope: lowercase kebab-case. `api-client` is a scope; `API`,
/// `api_client`, `api--client`, `-api` and `api-` are not, and an empty scope is
/// simply omitted -- parentheses and all. This is where the "a scope is omitted
/// for broad changes" rule becomes checkable.
fn validate_scope(scope: &str) -> Result<(), String> {
    if scope.is_empty() {
        return Err(
            "the scope must not be empty; omit the parentheses entirely for a broad change"
                .to_string(),
        );
    }
    if !scope
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(format!(
            "the scope `{scope}` is not lowercase kebab-case: use lowercase letters, digits and single hyphens"
        ));
    }
    if scope.starts_with('-') || scope.ends_with('-') || scope.contains("--") {
        return Err(format!(
            "the scope `{scope}` is not lowercase kebab-case: hyphens separate non-empty parts"
        ));
    }
    Ok(())
}

/// Validate the description: lowercase, imperative in tone, about
/// [`DESCRIPTION_TARGET`] characters, never longer than [`DESCRIPTION_LIMIT`],
/// and without a final period. Length, case and the absent period are enforced
/// here; imperative meaning and the target length are the prompt's job.
fn validate_description(description: &str) -> Result<(), String> {
    if description.is_empty() || description.starts_with(' ') {
        return Err(
            "the description must be non-empty and must start immediately after `: `".to_string(),
        );
    }
    if description.chars().any(char::is_uppercase) {
        return Err(format!("the description `{description}` must be lowercase"));
    }
    let width = description.chars().count();
    if width > DESCRIPTION_LIMIT {
        return Err(format!(
            "the description is {width} characters; it must never exceed {DESCRIPTION_LIMIT} characters (aim for about {DESCRIPTION_TARGET})"
        ));
    }
    if description.ends_with('.') {
        return Err(format!(
            "the description `{description}` must not end with a period"
        ));
    }
    Ok(())
}

/// Whether a line is meant as a footer. The test is deliberately loose -- it
/// catches a wrong attempt, which [`validate_footer`] then rejects with the list
/// of accepted forms -- and deliberately tight about prose, so an ordinary body
/// sentence that merely starts with `Closes` is still body text.
fn is_footer_like(line: &str) -> bool {
    let lowered = line.trim().to_ascii_lowercase();
    if lowered.starts_with("breaking change") || lowered.starts_with("breaking-change") {
        return true;
    }
    for keyword in ["fixes", "closes"] {
        if let Some(rest) = lowered.strip_prefix(keyword) {
            let rest = rest.trim_start();
            // `#12` and `: #12` are footer attempts, and so is a bare number
            // (`Fixes 12`): none of them is body prose, so the strict check gets
            // to reject them. A word after the keyword (`Closes the loop`) is.
            if rest.starts_with('#')
                || rest.starts_with(':')
                || rest.starts_with(|c: char| c.is_ascii_digit())
            {
                return true;
            }
        }
    }
    lowered.starts_with("refs:") || lowered.starts_with("co-authored-by")
}

/// Whether `line` is a `Refs:` footer attempt, which the format refuses outright:
/// a ticket reference names a generated ticket id, and the summary must not
/// carry one.
fn is_refs_footer(line: &str) -> bool {
    line.trim().to_ascii_lowercase().starts_with("refs:")
}

/// Validate one footer line: exactly the four forms the format allows, each with
/// a non-empty value. A `Refs:` footer is refused outright -- a ticket reference
/// names a generated ticket id -- and everything else -- a `Signed-off-by`
/// trailer, a made-up reference, an issue number that is not one -- is rejected,
/// so no unsupported footer can reach the artifact.
fn validate_footer(line: &str) -> Result<(), String> {
    if line == "BREAKING CHANGE:" || line == "BREAKING-CHANGE:" {
        return Err(format!(
            "`{line}` is not an accepted footer line: `BREAKING CHANGE: <description>` needs a non-empty description"
        ));
    }
    if let Some(description) = line.strip_prefix("BREAKING CHANGE: ")
        && !description.trim().is_empty()
    {
        return Ok(());
    }
    for (keyword, form) in [("Fixes #", "Fixes #N"), ("Closes #", "Closes #N")] {
        if let Some(number) = line.strip_prefix(keyword) {
            if is_issue_number(number) {
                return Ok(());
            }
            return Err(format!(
                "`{line}` is not an accepted footer line: `{form}` needs a positive issue number"
            ));
        }
    }
    if is_refs_footer(line) {
        return Err(REFS_FOOTER_REASON.to_string());
    }
    if let Some(authors) = line.strip_prefix("Co-authored-by: ") {
        if is_coauthor(authors) {
            return Ok(());
        }
        return Err(format!(
            "`{line}` is not an accepted footer line: `Co-authored-by: <name> <email>` needs both a name and an email"
        ));
    }
    Err(format!(
        "`{line}` is not an accepted footer line: every footer line must be one of `BREAKING CHANGE: <description>`, `Fixes #N`, `Closes #N` or `Co-authored-by: <name> <email>`"
    ))
}

/// Whether `number` is a positive issue number: digits only, and not zero.
fn is_issue_number(number: &str) -> bool {
    !number.is_empty()
        && number.chars().all(|c| c.is_ascii_digit())
        && number
            .parse::<u64>()
            .map(|value| value > 0)
            .unwrap_or(false)
}

/// Whether `value` carries both halves of `Co-authored-by: <name> <email>`: a
/// name and a plausible address, neither of them empty.
fn is_coauthor(value: &str) -> bool {
    let Some((name, address)) = value.rsplit_once(|c: char| c.is_whitespace()) else {
        return false;
    };
    if name.trim().is_empty() {
        return false;
    }
    let address = address.trim();
    let address = address
        .strip_prefix('<')
        .and_then(|rest| rest.strip_suffix('>'))
        .unwrap_or(address);
    match address.split_once('@') {
        Some((local, domain)) => !local.is_empty() && !domain.is_empty() && !domain.contains('@'),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal valid summary: one header line, one blank line, one body line.
    const VALID: &str = "feat(api-client): add retry backoff\n\nRetry the call twice so a flaky endpoint stops failing the run.\n";

    /// The canonical form of `answer`, or a loud failure naming the rejection.
    fn canonical(answer: &str) -> String {
        canonicalize(answer).unwrap_or_else(|reason| panic!("`{answer:?}` must be valid: {reason}"))
    }

    /// The reason `answer` was rejected, or a loud failure saying it was not.
    fn rejection(answer: &str) -> String {
        match canonicalize(answer) {
            Err(reason) => reason,
            Ok(text) => panic!("`{answer:?}` must be rejected; it canonicalized to `{text:?}`"),
        }
    }

    /// The three numbers the format fixes: the target description length, the
    /// hard description limit, and the body line limit.
    #[test]
    fn the_format_constants_are_the_documented_ones() {
        assert_eq!(DESCRIPTION_TARGET, 50);
        assert_eq!(DESCRIPTION_LIMIT, 72);
        assert_eq!(BODY_LINE_LIMIT, 72);
        assert_eq!(
            TYPES,
            [
                "feat", "fix", "docs", "style", "refactor", "perf", "test", "chore", "ci"
            ]
        );
    }

    /// The nine allowed types are accepted and nothing else is: the type list is
    /// closed.
    #[test]
    fn every_allowed_type_is_accepted_and_nothing_else_is() {
        for kind in TYPES {
            let answer = format!("{kind}: add the thing\n\nA body line.\n");
            assert_eq!(canonical(&answer), answer, "type {kind}");
        }

        for other in ["feature", "bugfix", "featx", "build", "revert", "Feat", ""] {
            let answer = format!("{other}: add the thing\n\nA body line.\n");
            let reason = rejection(&answer);
            assert!(reason.contains("type"), "{other:?}: {reason}");
        }
    }

    /// A scope is lowercase kebab-case, and a broad change omits it entirely
    /// rather than writing empty parentheses.
    #[test]
    fn scopes_are_lowercase_kebab_case_and_omitted_for_broad_changes() {
        for header in [
            "refactor(api-client): split the transport",
            "fix(a2): split the transport",
            "chore(deps-dev): split the transport",
            "refactor: split the transport",
        ] {
            let answer = format!("{header}\n\nA body line.\n");
            assert_eq!(canonical(&answer), answer, "header {header}");
        }

        for scope in [
            "API",
            "Api",
            "api_client",
            "api--client",
            "api-client-",
            "-api-client",
            "api client",
            "api.client",
            "",
        ] {
            let answer = format!("refactor({scope}): split the transport\n\nA body line.\n");
            let reason = rejection(&answer);
            assert!(reason.contains("scope"), "{scope:?}: {reason}");
        }
    }

    /// Both breaking-marker positions are accepted, and the marker alone is not
    /// a summary.
    #[test]
    fn both_breaking_marker_positions_are_accepted() {
        for header in [
            "feat!: drop the legacy flag",
            "feat(api)!: drop the legacy flag",
        ] {
            let answer = format!("{header}\n\nA body line.\n");
            assert_eq!(canonical(&answer), answer, "header {header}");
        }

        for header in ["feat!:", "feat(api)!:", "feat!", "feat(api)!", "feat!!: x"] {
            let answer = format!("{header}\n\nA body line.\n");
            assert!(canonicalize(&answer).is_err(), "{header}");
        }
    }

    /// The description is lowercase, has no final period, and is measured in
    /// Unicode characters against the 72-character limit -- 50 is the target the
    /// prompt asks for, not a second limit.
    #[test]
    fn the_description_is_lowercase_unpunctuated_and_bounded() {
        let body = "\n\nA body line.\n";

        let at_limit = "a".repeat(DESCRIPTION_LIMIT);
        assert_eq!(
            canonical(&format!("feat: {at_limit}{body}")),
            format!("feat: {at_limit}{body}")
        );

        let unicode_at_limit = "é".repeat(DESCRIPTION_LIMIT);
        assert_eq!(
            canonical(&format!("feat: {unicode_at_limit}{body}")),
            format!("feat: {unicode_at_limit}{body}"),
            "the limit counts characters, not bytes"
        );

        let over_limit = "a".repeat(DESCRIPTION_LIMIT + 1);
        assert!(rejection(&format!("feat: {over_limit}{body}")).contains("73 characters"));
        assert!(
            rejection(&format!(
                "feat: {}{body}",
                "é".repeat(DESCRIPTION_LIMIT + 1)
            ))
            .contains("73 characters")
        );

        // The target is a target: a longer description inside the limit passes.
        let over_target = "a".repeat(DESCRIPTION_TARGET + 1);
        assert_eq!(
            canonical(&format!("feat: {over_target}{body}")),
            format!("feat: {over_target}{body}")
        );

        assert!(rejection(&format!("feat: add the thing.{body}")).contains("period"));
        assert!(rejection(&format!("feat: Add the thing{body}")).contains("lowercase"));
        assert!(rejection(&format!("feat: {body}")).contains("description"));
    }

    /// The header, one blank line and a non-empty body: a missing separator, a
    /// missing body and a whitespace-only body are all generation failures.
    #[test]
    fn the_header_is_separated_from_a_non_empty_body_by_one_blank_line() {
        assert!(
            rejection("feat: add the thing\nThe body follows immediately.\n")
                .contains("blank line")
        );
        assert!(rejection("feat: add the thing\n").contains("no body"));
        assert!(rejection("feat: add the thing\n\n").contains("no body"));
        assert!(rejection("feat: add the thing\n\n   \n\t\n").contains("no body"));
        assert!(rejection("feat: add the thing\n\n\n\nFixes #42\n").contains("no body"));
    }

    /// No final text, whitespace-only text, a fence and a preamble are all
    /// generation failures, never an empty summary that looks successful.
    #[test]
    fn empty_fenced_and_prefaced_answers_are_generation_failures() {
        for answer in ["", "   ", "\n", "   \n\t\n\n"] {
            assert!(rejection(answer).contains("empty"), "{answer:?}");
        }

        let fenced = format!("```\n{VALID}```\n");
        assert!(
            rejection(&fenced).contains("fence"),
            "a fence is not a summary"
        );

        let fenced_body = "feat: add the thing\n\nBody line.\n\n```\ncode\n```\n";
        assert!(rejection(fenced_body).contains("fence"));

        let prefaced = format!("Here is the summary:\n\n{VALID}");
        let reason = rejection(&prefaced);
        assert!(reason.contains("preamble"), "{reason}");
    }

    /// CRLF is normalized to LF, trailing whitespace and surrounding blank lines
    /// are canonicalized away, and the result ends with exactly one LF.
    #[test]
    fn crlf_is_canonicalized_and_the_output_ends_with_exactly_one_lf() {
        let crlf = VALID.replace('\n', "\r\n");
        assert_eq!(canonical(&crlf), VALID);
        assert!(!canonical(&crlf).contains('\r'));

        let messy = "feat(api): add retry backoff\r\rBody line one.   \nBody line two.\n\n\n";
        assert_eq!(
            canonical(messy),
            "feat(api): add retry backoff\n\nBody line one.\nBody line two.\n"
        );

        let spaced =
            "feat(api): add retry backoff\n\n\n\nFirst paragraph.\n\nSecond paragraph.\n\n\n";
        assert_eq!(
            canonical(spaced),
            "feat(api): add retry backoff\n\nFirst paragraph.\n\nSecond paragraph.\n",
            "blank lines inside the body survive; the ones around it do not"
        );

        let once = canonical(VALID);
        assert!(once.ends_with('\n') && !once.ends_with("\n\n"));
        assert_eq!(
            canonical(&once),
            once,
            "canonicalization must be idempotent"
        );
    }

    /// Every body line is measured in Unicode characters against 72.
    #[test]
    fn every_body_line_is_at_most_seventy_two_characters() {
        let at_limit = "b".repeat(BODY_LINE_LIMIT);
        let answer = format!("feat: add the thing\n\n{at_limit}\n");
        assert_eq!(canonical(&answer), answer);

        let unicode_at_limit = "é".repeat(BODY_LINE_LIMIT);
        assert!(canonicalize(&format!("feat: add the thing\n\n{unicode_at_limit}\n")).is_ok());

        for over in [
            "b".repeat(BODY_LINE_LIMIT + 1),
            "é".repeat(BODY_LINE_LIMIT + 1),
        ] {
            let answer = format!("feat: add the thing\n\nA short line.\n{over}\n");
            let reason = rejection(&answer);
            assert!(reason.contains("73 characters"), "{reason}");
        }
    }

    /// Each of the four remaining footer forms is accepted, alone and in one
    /// block, with every placeholder filled in. A `Refs:` footer is not one of
    /// them any more.
    #[test]
    fn every_allowed_footer_form_is_accepted() {
        for footer in [
            "BREAKING CHANGE: the `--legacy` flag is gone",
            "Fixes #42",
            "Closes #7",
            "Co-authored-by: Ada Lovelace <ada@example.com>",
        ] {
            let answer = format!("feat(api)!: drop the legacy flag\n\nA body line.\n\n{footer}\n");
            assert_eq!(canonical(&answer), answer, "footer {footer:?}");
        }

        let block = "fix: close the retry gap\n\nA body line.\n\nFixes #42\nCloses #7\n";
        assert_eq!(canonical(block), block);
    }

    /// No other footer form is accepted: a wrong number, a wrong keyword, an
    /// empty value, an invented trailer in a footer block.
    #[test]
    fn invalid_footer_forms_are_rejected() {
        for footer in [
            "Fixes 42",
            "Fixes #",
            "Fixes #0",
            "Fixes #42a",
            "Closes: #7",
            "Refs:",
            "Refs: 1-verify-command-output",
            "Co-authored-by: Ada Lovelace",
            "Co-authored-by: <ada@example.com>",
            "BREAKING CHANGE:",
            "BREAKING-CHANGE: the flag is gone",
            "fixes #42",
        ] {
            let answer = format!("feat: add the thing\n\nA body line.\n\n{footer}\n");
            let reason = rejection(&answer);
            assert!(reason.contains("footer"), "{footer:?}: {reason}");
        }

        let mixed = "feat: add the thing\n\nA body line.\n\nFixes #42\nSigned-off-by: Ada <ada@example.com>\n";
        let reason = rejection(mixed);
        assert!(reason.contains("Signed-off-by"), "{reason}");
    }

    /// The footer lives in one place: a final block separated from the body by a
    /// blank line.
    #[test]
    fn a_footer_is_a_final_block_separated_from_the_body_by_one_blank_line() {
        let reason = rejection("feat: add the thing\n\nA body line.\nFixes #42\n");
        assert!(
            reason.contains("Fixes #42") && reason.contains("blank line"),
            "{reason}"
        );

        let reason = rejection("feat: add the thing\n\nFixes #42\n\nA later paragraph.\n");
        assert!(reason.contains("Fixes #42"), "{reason}");

        let reason = rejection("feat: add the thing\n\nA body line.\n\nFixes #42\nnot a footer\n");
        assert!(reason.contains("not a footer"), "{reason}");
    }

    /// The prompt states the rules the validator enforces and the ones only a
    /// human-readable instruction can state: the exact format, when a footer is
    /// allowed, the read-only pass and the unattended one.
    #[test]
    fn the_system_prompt_states_the_format_and_the_read_only_unattended_rules() {
        let prompt = prompts::SUMMARY_SYSTEM_PROMPT;
        for needle in [
            "tools.diff()",
            "BREAKING CHANGE: <description>",
            "Fixes #N",
            "Closes #N",
            "Co-authored-by: <name> <email>",
            "generated ticket id",
            "`Refs:` footer",
            "by title and status",
            "72",
            "50",
            "read-only",
            "never ask a question",
            "never start another ticket",
            "what changed and why",
            "no preamble",
            "Omit the footer",
        ] {
            assert!(
                prompt.contains(needle),
                "the prompt must mention `{needle}`"
            );
        }

        // The queue report and the queue outcome are the pass's own input.
        assert!(
            prompts::SUMMARY_KICKOFF_PROMPT.contains("queue"),
            "{}",
            prompts::SUMMARY_KICKOFF_PROMPT
        );
        assert!(prompts::SUMMARY_KICKOFF_PROMPT.contains("outcome"));
        assert!(prompts::SUMMARY_KICKOFF_PROMPT.contains("report"));
    }

    /// The kickoff payload names both halves the pass is handed, so an empty
    /// outcome or report is visible to the agent instead of vanishing.
    #[test]
    fn the_kickoff_payload_carries_the_outcome_and_the_report() {
        assert_eq!(
            kickoff("resolved", "# Report\n", "# Review report\n"),
            "## Outcome\n\nresolved\n\n## Report\n\n# Report\n\n## Review\n\n# Review report\n"
        );
        let placeholders = kickoff("  ", "   ", " ");
        assert!(placeholders.contains("no outcome"), "{placeholders}");
        assert!(placeholders.contains("no report"), "{placeholders}");
        assert!(placeholders.contains("no review report"), "{placeholders}");
    }

    // -----------------------------------------------------------------------
    // The pass itself: a scripted transport, no network
    // -----------------------------------------------------------------------

    use crate::api::{CallError, FunctionCall, Outgoing, RawResponse, Role, ToolCall, Transport};
    use std::sync::{Arc, Mutex};

    const MID_URL: &str = "http://mid.test/v1/chat/completions";
    const GIGA_URL: &str = "https://giga.test/v1/responses";
    const SMOLL_URL: &str = "https://smoll.test/v1/messages";
    const SMOLL_MODEL: &str = "smoll-model";
    const SMOLL_KEY: &str = "smoll-key";

    /// A transport that serves canned native Anthropic Messages bodies in order
    /// and records every request it saw. Running out of bodies fails the test:
    /// it means the pass sent a request the script did not expect.
    struct Script {
        bodies: Mutex<Vec<String>>,
        requests: Mutex<Vec<(String, serde_json::Value)>>,
    }

    impl Script {
        fn new(bodies: Vec<String>) -> Arc<Script> {
            Arc::new(Script {
                bodies: Mutex::new(bodies),
                requests: Mutex::new(Vec::new()),
            })
        }

        /// Every request the pass sent, in order: its URL and its body.
        fn requests(&self) -> Vec<(String, serde_json::Value)> {
            self.requests.lock().unwrap().clone()
        }
    }

    impl Transport for Script {
        fn send(&self, outgoing: &Outgoing) -> Result<RawResponse, CallError> {
            self.requests
                .lock()
                .unwrap()
                .push((outgoing.url.clone(), outgoing.body.clone()));
            let mut bodies = self.bodies.lock().unwrap();
            assert!(
                !bodies.is_empty(),
                "the pass made an unexpected extra request"
            );
            Ok(RawResponse {
                status: 200,
                body: bodies.remove(0),
            })
        }
    }

    /// One native Anthropic Messages reply: optional text and the given
    /// `(name, arguments)` tool_use blocks. Summary always runs on the Smoll
    /// provider, so the scripted external boundary answers in that wire format.
    fn reply(content: Option<&str>, calls: &[(&str, String)]) -> String {
        let mut blocks = Vec::new();
        if let Some(text) = content {
            blocks.push(serde_json::json!({ "type": "text", "text": text }));
        }
        for (index, (name, arguments)) in calls.iter().enumerate() {
            let input = serde_json::from_str(arguments).unwrap_or_else(|_| serde_json::json!({}));
            blocks.push(serde_json::json!({
                "type": "tool_use",
                "id": format!("toolu_{index}"),
                "name": name,
                "input": input,
            }));
        }
        serde_json::json!({ "content": blocks }).to_string()
    }

    /// One plain answer: the shape a summary pass ends on.
    fn answer(text: &str) -> String {
        reply(Some(text), &[])
    }

    fn mid_config(script: Arc<Script>) -> Config {
        Config::new(MID_URL, "mid-model", Some("mid-key".to_string()))
            .with_smoll(SMOLL_URL, SMOLL_MODEL, SMOLL_KEY)
            .with_transport(script)
    }

    /// A tool call and then a plain answer: the pass runs the tool, asks again,
    /// and the final turn's own text is the answer.
    #[test]
    fn a_tool_call_before_the_final_turn_ends_the_pass_with_that_text() {
        let script = Script::new(vec![
            reply(
                Some("Inspecting the change set."),
                &[(
                    "execute_script",
                    serde_json::json!({ "code": "return tools.diff().length > 0;" }).to_string(),
                )],
            ),
            answer(VALID),
        ]);
        let config = mid_config(script.clone());

        match summarize(
            &config,
            "# scope",
            "resolved: every ticket completed",
            "# Report\n",
            "# Review report\n",
            &[],
        ) {
            Outcome::Summary(text) => assert_eq!(text, VALID),
            other => panic!("expected the canonical summary, got {other:?}"),
        }
        assert_eq!(
            script.requests().len(),
            2,
            "the tool call must be answered before the pass can end"
        );
    }

    /// The request is the summary pass's own: the summary prompt, the scope, the
    /// queue outcome and the queue report, and exactly one tool -- the read-only
    /// inspection tool, with no `bash` and no control tool.
    #[test]
    fn the_request_carries_the_prompt_the_queue_result_and_one_read_only_tool() {
        let script = Script::new(vec![answer(VALID)]);
        let config = mid_config(script.clone());

        let _ = summarize(
            &config,
            "# scope notice",
            "incomplete: 1 of 2 tickets completed",
            "# Ticket queue report\n\n## 2-second\n\n- Status: blocked\n",
            "",
            &[],
        );

        let requests = script.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].0, SMOLL_URL,
            "summary always uses the Smoll Anthropic Messages provider"
        );
        let body = serde_json::to_string(&requests[0].1).unwrap();

        assert!(
            body.contains("Conventional-Commits-style header"),
            "the summary prompt is missing"
        );
        assert!(
            body.contains("tools.diff()"),
            "the prompt must point at the change set"
        );
        assert!(
            body.contains("# scope notice"),
            "the scope notice must reach the agent"
        );
        assert!(
            body.contains("incomplete: 1 of 2 tickets completed"),
            "the outcome must reach the agent"
        );
        assert!(
            body.contains("Status: blocked"),
            "the report must reach the agent"
        );
        assert!(
            body.contains("execute_script"),
            "the pass needs its inspection tool"
        );
        assert!(
            !body.contains("task_complete"),
            "the summary phase offers no completion tool"
        );
        assert!(
            !body.contains("finish_plan"),
            "the summary phase offers no handoff tool"
        );
        assert!(
            !body.contains("\"bash\""),
            "the summary phase offers no bash"
        );
    }

    /// Mid, giga and hybrid mode each route the summary pass to the dedicated
    /// Smoll provider the phase resolves to -- never to the mode's own provider.
    #[test]
    fn when_summarising_given_each_mode_then_uses_the_provider_the_summary_phase_resolves_to() {
        for mode in [
            crate::llm::Mode::Mid,
            crate::llm::Mode::Giga,
            crate::llm::Mode::Hybrid,
        ] {
            let script = Script::new(vec![answer(VALID)]);
            let config = mid_config(script.clone()).with_mode(mode);
            let config = if mode == crate::llm::Mode::Mid {
                config
            } else {
                config.with_giga(GIGA_URL, "giga-model", Some("giga-key".to_string()))
            };

            match summarize(&config, "", "resolved", "report", "review", &[]) {
                Outcome::Summary(text) => assert_eq!(text, VALID, "{mode:?}"),
                other => panic!("expected the canonical summary in {mode:?}, got {other:?}"),
            }
            assert_eq!(
                script.requests()[0].0,
                SMOLL_URL,
                "{mode:?} summary must use the Smoll provider"
            );
            assert_eq!(
                config.provider_role(Phase::Summary),
                "smoll",
                "{mode:?} summary role"
            );
        }
    }

    /// Four present candidates the validator rejects leave the pass a
    /// generation failure with the fourth candidate's reason, never a summary to
    /// store; an empty native text block is still an immediate provider failure.
    #[test]
    fn when_summarising_given_rejected_final_text_and_rejected_correction_then_fails() {
        for (answer_text, expected) in [
            ("feat: add the thing\n", "no body"),
            ("", "empty"),
            ("   \n\t\n", "empty"),
            ("feat: Add the thing\n\nBody line.\n", "lowercase"),
            ("feat: add the thing\n\nBody line.\n\nFixes #\n", "footer"),
        ] {
            // Every candidate is answered with the same malformed text, so the
            // pass exhausts its four-candidate allowance with that same reason.
            let script = Script::new(vec![answer(answer_text); MAX_CANDIDATES]);
            let config = mid_config(script);
            match summarize(&config, "", "resolved", "report", "review", &[]) {
                Outcome::Invalid(reason) => {
                    assert!(reason.contains(expected), "{answer_text:?}: {reason}")
                }
                // An empty native text block is a response with no usable
                // content: the provider reports it before canonicalization can.
                Outcome::Failed(kind) if answer_text.is_empty() => {
                    assert_eq!(kind.label(), "malformed response");
                }
                other => panic!("`{answer_text:?}` must fail, got {other:?}"),
            }
        }
    }

    /// One earlier summary transcript carrying a complete assistant tool-call
    /// group, a plain assistant turn and a later user turn, every message with
    /// reasoning and provider fields a correction must preserve.
    fn reasoned_summary_transcript() -> Vec<Message> {
        fn evidence(
            field: &str,
            value: serde_json::Value,
        ) -> serde_json::Map<String, serde_json::Value> {
            let mut extra = serde_json::Map::new();
            extra.insert(field.to_string(), value);
            extra
        }

        let mut messages = vec![
            Message::system("summary system prompt"),
            Message::user("summary kickoff"),
        ];
        messages[0].reasoning_content = Some("system reasoning".to_string());
        messages[0].extra = evidence("provider_field", serde_json::json!("system"));
        messages[1].reasoning_content = Some("kickoff reasoning".to_string());
        messages[1].extra = evidence("provider_field", serde_json::json!("kickoff"));
        messages.push(Message {
            role: Role::Assistant,
            content: Some("inspecting the change set".to_string()),
            tool_calls: Some(vec![
                ToolCall {
                    id: "call-1".to_string(),
                    kind: "function".to_string(),
                    function: FunctionCall {
                        name: "execute_script".to_string(),
                        arguments: serde_json::json!({ "code": "return 1;" }).to_string(),
                    },
                },
                ToolCall {
                    id: "call-2".to_string(),
                    kind: "function".to_string(),
                    function: FunctionCall {
                        name: "execute_script".to_string(),
                        arguments: serde_json::json!({ "code": "return 2;" }).to_string(),
                    },
                },
            ]),
            tool_call_id: None,
            reasoning_content: Some("group reasoning".to_string()),
            extra: evidence("provider_field", serde_json::json!("group")),
        });
        messages.push(Message {
            role: Role::Tool,
            content: Some("first result".to_string()),
            tool_calls: None,
            tool_call_id: Some("call-1".to_string()),
            reasoning_content: Some("tool reasoning one".to_string()),
            extra: evidence("trace", serde_json::json!(1)),
        });
        messages.push(Message {
            role: Role::Tool,
            content: Some("second result".to_string()),
            tool_calls: None,
            tool_call_id: Some("call-2".to_string()),
            reasoning_content: Some("tool reasoning two".to_string()),
            extra: evidence("trace", serde_json::json!(2)),
        });
        messages.push(Message {
            role: Role::Assistant,
            content: Some("earlier narration".to_string()),
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: Some("narration reasoning".to_string()),
            extra: evidence("provider_field", serde_json::json!("narration")),
        });
        messages.push(Message {
            role: Role::User,
            content: Some("earlier user follow-up".to_string()),
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: Some("user reasoning".to_string()),
            extra: evidence("provider_field", serde_json::json!("user")),
        });
        messages
    }

    /// The call ids and the result ids one transcript carries, in order.
    fn transcript_pairing_ids(messages: &[Message]) -> (Vec<String>, Vec<String>) {
        let mut calls = Vec::new();
        let mut results = Vec::new();
        for message in messages {
            if let Some(tool_calls) = &message.tool_calls {
                calls.extend(tool_calls.iter().map(|call| call.id.clone()));
            }
            if let Some(id) = &message.tool_call_id {
                results.push(id.clone());
            }
        }
        (calls, results)
    }

    /// The tool-call ids and tool-result ids one native Anthropic request
    /// carries, in order.
    fn native_pairing_ids(body: &serde_json::Value) -> (Vec<String>, Vec<String>) {
        let mut calls = Vec::new();
        let mut results = Vec::new();
        for message in body["messages"].as_array().expect("native messages") {
            for block in message["content"]
                .as_array()
                .expect("native content blocks")
            {
                match block["type"].as_str() {
                    Some("tool_use") => calls.push(block["id"].as_str().unwrap().to_string()),
                    Some("tool_result") => {
                        results.push(block["tool_use_id"].as_str().unwrap().to_string())
                    }
                    _ => {}
                }
            }
        }
        (calls, results)
    }

    /// The native messages one Messages request carries, in wire order.
    fn native_messages(body: &serde_json::Value) -> &[serde_json::Value] {
        body["messages"]
            .as_array()
            .expect("native messages")
            .as_slice()
    }

    /// The text blocks one native message carries, in order.
    fn native_texts(message: &serde_json::Value) -> Vec<&str> {
        message["content"]
            .as_array()
            .expect("native content blocks")
            .iter()
            .filter(|block| block["type"].as_str() == Some("text"))
            .map(|block| block["text"].as_str().expect("text block"))
            .collect()
    }

    /// The `id` (for `tool_use`) or `tool_use_id` (for `tool_result`) of every
    /// block of `kind` one native message carries, in order.
    fn native_block_ids<'a>(
        message: &'a serde_json::Value,
        kind: &str,
        id_field: &str,
    ) -> Vec<&'a str> {
        message["content"]
            .as_array()
            .expect("native content blocks")
            .iter()
            .filter(|block| block["type"].as_str() == Some(kind))
            .map(|block| block[id_field].as_str().expect("block id"))
            .collect()
    }

    /// The paired id and content of every `tool_result` block one native
    /// request carries, in wire order.
    fn native_tool_results(body: &serde_json::Value) -> Vec<(&str, &str)> {
        body["messages"]
            .as_array()
            .expect("native messages")
            .iter()
            .flat_map(|message| {
                message["content"]
                    .as_array()
                    .expect("native content blocks")
                    .iter()
                    .filter(|block| block["type"].as_str() == Some("tool_result"))
                    .map(|block| {
                        (
                            block["tool_use_id"].as_str().expect("tool_use_id"),
                            block["content"].as_str().expect("tool_result content"),
                        )
                    })
            })
            .collect()
    }

    /// Core entry-point behavior: a rejected initial answer clears only the
    /// earlier transcript reasoning, and the captured second request proves the
    /// correction carries those earlier turns in their original order, the
    /// rejected answer, and one separate correction user turn.
    #[test]
    fn when_summarizing_given_rejected_initial_answer_then_asserts_preserved_transcript_in_correction_request()
     {
        let original = reasoned_summary_transcript();
        let mut messages = original.clone();
        let rejected = "feat: add the thing\n";
        let reason = canonicalize(rejected).unwrap_err();
        let correction = prompts::summary_correction(&reason);
        let script = Script::new(vec![answer(rejected), answer(VALID)]);
        let config = mid_config(script.clone());

        match summary_pass(&mut messages, &config, &[], InvocationContext::default()) {
            Outcome::Summary(text) => assert_eq!(text, VALID),
            other => panic!("expected the corrected summary, got {other:?}"),
        }

        let requests = script.requests();
        assert_eq!(requests.len(), 2, "exactly one correction is requested");

        // The local transcript keeps every earlier field but reasoning.
        for (index, (before, after)) in original.iter().zip(messages.iter()).enumerate() {
            assert_eq!(before.role, after.role, "role changes at {index}");
            assert_eq!(before.content, after.content, "content changes at {index}");
            assert_eq!(
                serde_json::to_value(&before.tool_calls).unwrap(),
                serde_json::to_value(&after.tool_calls).unwrap(),
                "tool calls change at {index}"
            );
            assert_eq!(
                before.tool_call_id, after.tool_call_id,
                "tool id changes at {index}"
            );
            assert_eq!(before.extra, after.extra, "extra changes at {index}");
            assert_eq!(
                after.reasoning_content, None,
                "earlier reasoning survives at {index}"
            );
        }
        let corrections: Vec<usize> = messages
            .iter()
            .enumerate()
            .filter(|(_, message)| {
                message.role == Role::User
                    && message.content.as_deref() == Some(correction.as_str())
            })
            .map(|(index, _)| index)
            .collect();
        assert_eq!(
            corrections,
            vec![original.len() + 1],
            "the correction is one separate user message after the rejected answer"
        );
        assert_eq!(messages[original.len()].content.as_deref(), Some(rejected));

        // The captured second request is the genuine correction request: the
        // system turn, every earlier turn in order, the rejected answer and
        // then the correction as one separate user turn.
        let body = &requests[1].1;
        assert_eq!(
            body["system"].as_str(),
            Some("summary system prompt"),
            "the earlier system turn is carried unchanged"
        );
        let native = native_messages(body);
        let roles: Vec<&str> = native
            .iter()
            .map(|message| message["role"].as_str().expect("native role"))
            .collect();
        assert_eq!(
            roles,
            vec![
                "user",
                "assistant",
                "user",
                "assistant",
                "user",
                "assistant",
                "user"
            ],
            "the earlier transcript keeps its order before the correction"
        );
        assert_eq!(native_texts(&native[0]), vec!["summary kickoff"]);
        assert_eq!(
            native_texts(&native[1]),
            vec!["inspecting the change set"],
            "the assistant turn keeps its original content"
        );
        assert_eq!(
            native_block_ids(&native[1], "tool_use", "id"),
            vec!["call-1", "call-2"],
            "the tool calls keep their original ids and order"
        );
        assert_eq!(
            native_block_ids(&native[2], "tool_result", "tool_use_id"),
            vec!["call-1", "call-2"],
            "every tool call is answered in its original order"
        );
        assert_eq!(native_texts(&native[3]), vec!["earlier narration"]);
        assert_eq!(native_texts(&native[4]), vec!["earlier user follow-up"]);
        assert_eq!(native_texts(&native[5]), vec![rejected]);
        assert_eq!(
            native_texts(&native[6]),
            vec![correction.as_str()],
            "the correction is one separate user turn after the rejected answer"
        );
        let encoded = serde_json::to_string(body).unwrap();
        for reasoning in [
            "system reasoning",
            "kickoff reasoning",
            "group reasoning",
            "tool reasoning one",
            "tool reasoning two",
            "narration reasoning",
            "user reasoning",
        ] {
            assert!(
                !encoded.contains(reasoning),
                "cleared reasoning reached the correction request: {reasoning}"
            );
        }
    }

    /// Empty/zero boundary: an absent initial answer keeps the existing fatal
    /// outcome and never buys a correction.
    #[test]
    fn when_summarizing_given_absent_initial_answer_then_preserves_existing_failure_without_correction()
     {
        let mut messages = reasoned_summary_transcript();
        let script = Script::new(vec![reply(None, &[])]);
        let config = mid_config(script.clone());

        match summary_pass(&mut messages, &config, &[], InvocationContext::default()) {
            Outcome::Failed(kind) => assert_eq!(kind.label(), "malformed response"),
            other => panic!("an absent answer must fail as before, got {other:?}"),
        }
        assert_eq!(
            script.requests().len(),
            1,
            "an absent answer must not trigger a correction request"
        );
    }

    /// Minimum valid boundary: a complete tool-call/result group keeps its ids,
    /// result contents and ordering through the correction boundary, the
    /// outgoing correction request still pairs every call with its result, and
    /// the correction follows the rejected answer as one separate user turn.
    #[test]
    fn when_summarizing_given_complete_tool_call_group_then_asserts_result_content_and_pairing_in_correction_request()
     {
        let original = reasoned_summary_transcript();
        let mut messages = original.clone();
        let rejected = "feat: add the thing\n";
        let reason = canonicalize(rejected).unwrap_err();
        let correction = prompts::summary_correction(&reason);
        let script = Script::new(vec![answer(rejected), answer(VALID)]);
        let config = mid_config(script.clone());

        let _ = summary_pass(&mut messages, &config, &[], InvocationContext::default());

        let requests = script.requests();
        assert_eq!(requests.len(), 2, "exactly one correction is requested");

        // The local transcript keeps the group's call/result ids in order and
        // still passes the pairing validator the provider relies on.
        let expected = transcript_pairing_ids(&original);
        assert_eq!(transcript_pairing_ids(&messages), expected);
        agent::validate_tool_pairing(&messages)
            .expect("the correction transcript keeps valid tool pairing");

        // Clearing reasoning leaves every other earlier field untouched: role,
        // content, tool calls, tool ids and the `extra` provider fields.
        for (index, (before, after)) in original.iter().zip(messages.iter()).enumerate() {
            assert_eq!(before.role, after.role, "role changes at {index}");
            assert_eq!(before.content, after.content, "content changes at {index}");
            assert_eq!(
                serde_json::to_value(&before.tool_calls).unwrap(),
                serde_json::to_value(&after.tool_calls).unwrap(),
                "tool calls change at {index}"
            );
            assert_eq!(
                before.tool_call_id, after.tool_call_id,
                "tool id changes at {index}"
            );
            assert_eq!(before.extra, after.extra, "extra changes at {index}");
            assert_eq!(
                after.reasoning_content, None,
                "earlier reasoning survives at {index}"
            );
        }

        // The captured second request is the correction request: the complete
        // group travels with the same ids and result contents in the same
        // order, and the correction follows the rejected answer as the only
        // correction user turn.
        let body = &requests[1].1;
        let (calls, results) = native_pairing_ids(body);
        assert_eq!(calls, expected.0, "tool-call ids and order are preserved");
        assert_eq!(
            results, expected.1,
            "every tool call is answered in its original order"
        );
        assert_eq!(
            native_tool_results(body),
            vec![("call-1", "first result"), ("call-2", "second result")],
            "tool-result contents travel with their paired ids in the original order"
        );

        let native = native_messages(body);
        let correction_turns: Vec<usize> = native
            .iter()
            .enumerate()
            .filter(|(_, message)| {
                message["role"].as_str() == Some("user")
                    && native_texts(message) == vec![correction.as_str()]
            })
            .map(|(index, _)| index)
            .collect();
        assert_eq!(
            correction_turns,
            vec![native.len() - 1],
            "the correction is the last and only correction user turn"
        );
        assert_eq!(
            native_texts(&native[native.len() - 2]),
            vec![rejected],
            "the rejected answer immediately precedes the correction"
        );
    }

    /// Malformed/invalid boundary: four rejected candidates keep the existing
    /// invalid outcome and stop at the four-candidate limit.
    #[test]
    fn when_summarizing_given_rejected_final_correction_then_preserves_existing_invalid_outcome() {
        let original = reasoned_summary_transcript();
        let mut messages = original.clone();
        let rejected = "feat: Add the thing\n\nBody line.\n";
        let reason = canonicalize(rejected).unwrap_err();
        let script = Script::new(vec![answer(rejected); MAX_CANDIDATES]);
        let config = mid_config(script.clone());

        match summary_pass(&mut messages, &config, &[], InvocationContext::default()) {
            Outcome::Invalid(actual) => assert_eq!(actual, reason),
            other => panic!("a rejected candidate must stay invalid, got {other:?}"),
        }
        assert_eq!(
            script.requests().len(),
            MAX_CANDIDATES,
            "the fourth rejected candidate ends the pass without a fifth request"
        );
        assert!(
            messages[..original.len()]
                .iter()
                .all(|message| message.reasoning_content.is_none())
        );
    }

    /// Contract and invariant: at the correction boundary only reasoning
    /// changes; role, content, tool calls, tool ids and provider fields survive.
    #[test]
    fn when_summarizing_given_correction_boundary_then_preserves_all_non_reasoning_message_fields()
    {
        let original = reasoned_summary_transcript();
        let mut messages = original.clone();
        let script = Script::new(vec![answer("feat: add the thing\n"), answer(VALID)]);
        let config = mid_config(script.clone());

        let _ = summary_pass(&mut messages, &config, &[], InvocationContext::default());
        for (index, (before, after)) in original.iter().zip(messages.iter()).enumerate() {
            assert_eq!(before.role, after.role, "role changes at {index}");
            assert_eq!(before.content, after.content, "content changes at {index}");
            assert_eq!(
                serde_json::to_value(&before.tool_calls).unwrap(),
                serde_json::to_value(&after.tool_calls).unwrap(),
                "tool calls change at {index}"
            );
            assert_eq!(
                before.tool_call_id, after.tool_call_id,
                "tool id changes at {index}"
            );
            assert_eq!(before.extra, after.extra, "provider fields at {index}");
            assert_eq!(
                after.reasoning_content, None,
                "only reasoning changes at {index}"
            );
        }
        assert!(
            original.iter().any(|message| !message.extra.is_empty()),
            "the fixture must carry provider fields to preserve"
        );
    }

    // -----------------------------------------------------------------------
    // The generated-ticket-id gate and the `Refs:` prohibition
    // -----------------------------------------------------------------------

    /// The canonical form of `answer` under the generated-ticket-id gate, or a
    /// loud failure naming the rejection.
    fn canonical_with_ids(answer: &str, ticket_ids: &[&str]) -> String {
        let ids: Vec<String> = ticket_ids.iter().map(|id| id.to_string()).collect();
        canonicalize_with_ticket_ids(answer, &ids)
            .unwrap_or_else(|reason| panic!("`{answer:?}` must be valid: {reason}"))
    }

    /// The reason the generated-ticket-id gate rejected `answer`, or a loud
    /// failure saying it did not.
    fn id_rejection(answer: &str, ticket_ids: &[&str]) -> String {
        let ids: Vec<String> = ticket_ids.iter().map(|id| id.to_string()).collect();
        match canonicalize_with_ticket_ids(answer, &ids) {
            Err(reason) => reason,
            Ok(text) => panic!("`{answer:?}` must be rejected; it canonicalized to `{text:?}`"),
        }
    }

    /// With nothing to check, the format is the only gate: a valid summary is
    /// accepted unchanged, blank id entries are ignored, and the existing
    /// empty-answer failures are untouched.
    #[test]
    fn when_validating_summary_given_no_generated_ticket_ids_then_accepts_valid_text() {
        assert_eq!(canonical_with_ids(VALID, &[]), VALID);
        assert_eq!(canonical_with_ids(VALID, &["", "   "]), VALID);

        for answer in ["", "   \n\t\n"] {
            let reason = id_rejection(answer, &["1-verify-command-output"]);
            assert!(reason.contains("empty"), "{answer:?}: {reason}");
        }
    }

    /// One id, padded with whitespace, is trimmed before the check and matched
    /// exactly: the canonical text containing it is refused, and the reason
    /// never reproduces the id.
    #[test]
    fn when_validating_summary_given_one_trimmed_ticket_id_then_rejects_exact_case_sensitive_match()
    {
        const ID: &str = "1-verify-command-output";
        let answer = format!("feat: add the thing\n\nThis run resolved {ID} in full.\n");
        let padded = format!("  {ID}  ");
        let reason = id_rejection(&answer, &[padded.as_str()]);
        assert!(reason.contains("generated ticket id"), "{reason}");
        assert!(
            !reason.contains(ID),
            "the reason must not echo the id: {reason}"
        );
    }

    /// The id may sit anywhere in the canonical text -- the header's scope, a
    /// body line, a footer value -- and each region is refused the same way.
    #[test]
    fn when_validating_summary_given_generated_id_in_each_region_then_rejects_exact_match() {
        const ID: &str = "1-verify-command-output";
        for answer in [
            format!("feat({ID}): add the thing\n\nA body line.\n"),
            format!("feat: add the thing\n\nSee {ID} for the context of this change.\n"),
            format!("feat: add the thing\n\nA body line.\n\nCo-authored-by: {ID} <a@b.example>\n"),
        ] {
            let reason = id_rejection(&answer, &[ID]);
            assert!(
                reason.contains("generated ticket id"),
                "{answer:?}: {reason}"
            );
            assert!(
                !reason.contains(ID),
                "the reason must not echo the id: {reason}"
            );
        }
    }

    /// The scan is case-sensitive and blank entries are ignored: a
    /// case-changed mention is body prose, not the generated id.
    #[test]
    fn when_validating_summary_given_case_changed_or_empty_id_then_does_not_reject() {
        let case_changed = "feat: add the thing\n\nMention 1-VERIFY-COMMAND-OUTPUT here.\n";
        assert_eq!(
            canonical_with_ids(case_changed, &["1-verify-command-output"]),
            case_changed,
            "the match is case-sensitive"
        );
        assert_eq!(canonical_with_ids(VALID, &[""]), VALID);
    }

    /// Every `Refs:` attempt -- empty or filled in -- is refused, while the four
    /// remaining footer forms keep their existing validation.
    #[test]
    fn when_validating_summary_given_refs_or_supported_footer_then_applies_footer_contract() {
        for footer in [
            "Refs:",
            "Refs: 1-verify-command-output",
            "Refs: some other reference",
        ] {
            let answer = format!("feat: add the thing\n\nA body line.\n\n{footer}\n");
            let reason = rejection(&answer);
            assert!(reason.contains("footer"), "{footer:?}: {reason}");
        }

        for footer in [
            "BREAKING CHANGE: the `--legacy` flag is gone",
            "Fixes #42",
            "Closes #7",
            "Co-authored-by: Ada Lovelace <ada@example.com>",
        ] {
            let answer = format!("feat: add the thing\n\nA body line.\n\n{footer}\n");
            assert_eq!(canonical(&answer), answer, "footer {footer:?}");
        }
    }

    /// A `Refs:` footer is refused with a reason that names the form and never
    /// echoes the reference it carried, in the footer block and outside it.
    #[test]
    fn when_validating_summary_given_refs_footer_then_rejects_without_echoing_reference() {
        const ID: &str = "1-verify-command-output";
        for answer in [
            format!("feat: add the thing\n\nA body line.\n\nRefs: {ID}\n"),
            format!("feat: add the thing\n\nA body line.\nRefs: {ID}\n"),
        ] {
            let reason = rejection(&answer);
            assert!(reason.contains("footer"), "{answer:?}: {reason}");
            assert!(
                !reason.contains(ID),
                "the reason must not echo the reference: {reason}"
            );
        }
    }

    /// A `Refs:` footer and a validly structured id match are both generation
    /// failures: the format rejects them, it never strips the offending text
    /// out and hands back a summary the model did not write.
    #[test]
    fn when_validating_summary_given_refs_or_malformed_structure_then_rejects_without_rewriting() {
        const ID: &str = "1-verify-command-output";

        let structured = format!("feat: add the thing\n\nA body line.\n\nRefs: {ID}\n");
        assert!(
            canonicalize(&structured).is_err(),
            "a `Refs:` footer is never rewritten into an accepted summary"
        );

        let malformed = format!("feat: add the thing\n\nA body line.\nRefs: {ID}\n");
        assert!(
            canonicalize(&malformed).is_err(),
            "a stray `Refs:` line is never rewritten into an accepted summary"
        );

        let body_match = format!("feat: add the thing\n\nThis run resolved {ID} in full.\n");
        assert!(
            canonicalize_with_ticket_ids(&body_match, &[ID.to_string()]).is_err(),
            "a validly structured id match is rejected, not stripped"
        );
    }

    /// The summary phase keeps its read-only boundary: the request offers the
    /// inspection tool and no control tool, no handoff and no `bash`.
    #[test]
    fn when_building_summary_request_given_read_only_phase_then_offers_no_broader_tool_access() {
        let script = Script::new(vec![answer(VALID)]);
        let config = mid_config(script.clone());

        let _ = summarize(&config, "# scope", "resolved", "report", "review", &[]);

        let requests = script.requests();
        assert_eq!(requests.len(), 1);
        let body = serde_json::to_string(&requests[0].1).unwrap();
        assert!(
            body.contains("execute_script"),
            "the inspection tool must be offered: {body}"
        );
        assert!(
            !body.contains("task_complete"),
            "no completion tool: {body}"
        );
        assert!(!body.contains("finish_plan"), "no handoff tool: {body}");
        assert!(!body.contains("\"bash\""), "no bash: {body}");
    }
}
