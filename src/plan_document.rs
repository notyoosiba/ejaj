//! The plan as a typed document.
//!
//! Markdown is only a rendering of it, produced by [`Plan::to_markdown`]: the type is the
//! contract the plan agent fills in through `finish_plan` and the spec agent consumes. The
//! document's headings are added here, never authored by the model.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
/// The prefix of the document's single title line: the goal, named by the model.
const TITLE_PREFIX: &str = "# ";
/// The heading of the decisions section.
pub const DECISIONS_HEADING: &str = "## 1. Decisions";
/// The heading of the defaults section.
pub const DEFAULTS_HEADING: &str = "## 2. Defaults Chosen";
/// The heading of the constraints section.
pub const CONSTRAINTS_HEADING: &str = "## 3. Constraints";
/// The heading of the open questions section.
pub const OPEN_QUESTIONS_HEADING: &str = "## 4. Open Questions";
/// Every section heading, in the order the document must use them.
pub const SECTION_HEADINGS: [&str; 4] = [
    DECISIONS_HEADING,
    DEFAULTS_HEADING,
    CONSTRAINTS_HEADING,
    OPEN_QUESTIONS_HEADING,
];
/// What a section with no entries renders as, and what the parser reads back as "empty".
pub const NONE: &str = "None.";

/// The plan the plan agent hands to the spec agent.
///
/// Every field below is required, and the markdown document the spec agent receives is derived
/// from this object by [`Plan::to_markdown`] -- the agent never authors the document's headings
/// itself.
#[derive(Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Plan {
    /// The change in one line; rendered as the document title.
    pub goal: String,
    /// One entry per agreed decision, in the order they must be implemented.
    pub decisions: Vec<PlanDecision>,
    /// The defaults chosen for anything not worth asking about, spelled out concretely.
    pub defaults: Vec<String>,
    /// Every constraint or behaviour the implementer must honour.
    pub constraints: Vec<String>,
    /// The questions that are still genuinely unanswered, each with the fact needed to settle it.
    pub open_questions: Vec<String>,
}

/// One agreed decision: a short label and the decision itself.
#[derive(Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PlanDecision {
    /// A short label for the decision.
    pub title: String,
    /// The decision itself: names, paths, signatures and values.
    pub detail: String,
}

impl Plan {
    /// Render this plan as the markdown document the spec agent receives.
    ///
    /// The template is fixed: the title plus the four numbered sections. Field values are
    /// inserted after trimming, and the returned string ends with exactly one `\n`.
    pub fn to_markdown(&self) -> String {
        let title = format!("{TITLE_PREFIX}{}", self.goal.trim());
        let decisions = format!("{DECISIONS_HEADING}\n\n{}", self.decisions_markdown());
        let defaults = format!(
            "{DEFAULTS_HEADING}\n\n{}",
            self.bullets_markdown(&self.defaults)
        );
        let constraints = format!(
            "{CONSTRAINTS_HEADING}\n\n{}",
            self.bullets_markdown(&self.constraints)
        );
        let open_questions = format!(
            "{OPEN_QUESTIONS_HEADING}\n\n{}",
            self.bullets_markdown(&self.open_questions)
        );

        let mut document = join_blocks([
            title.as_str(),
            decisions.as_str(),
            defaults.as_str(),
            constraints.as_str(),
            open_questions.as_str(),
        ]);
        document.push('\n');
        document
    }

    /// The body of section 1: numbered decisions, each with its title and detail.
    fn decisions_markdown(&self) -> String {
        if self.decisions.is_empty() {
            return NONE.to_string();
        }

        let blocks: Vec<String> = self
            .decisions
            .iter()
            .enumerate()
            .map(|(index, decision)| {
                join_non_empty([
                    format!("{}. **{}**", index + 1, decision.title.trim()),
                    decision.detail.trim().to_string(),
                ])
            })
            .collect();

        blocks.join("\n\n")
    }

    /// The body of a list section (defaults, constraints and open questions): one bullet per
    /// non-blank entry.
    fn bullets_markdown(&self, entries: &[String]) -> String {
        let bullets: Vec<String> = entries
            .iter()
            .map(|entry| entry.trim())
            .filter(|entry| !entry.is_empty())
            .map(|entry| format!("- {}", entry))
            .collect();

        if bullets.is_empty() {
            NONE.to_string()
        } else {
            bullets.join("\n")
        }
    }
}

impl Plan {
    /// Parse the markdown document back into a plan.
    ///
    /// This is the inverse of [`Plan::to_markdown`]: the same constants define the headings both
    /// sides use, so the template cannot drift. The document is the model's own text, so the
    /// scanner is deliberately small -- one line pass, no parser library and no regex. Anything
    /// that does not match the template is an `Err` naming the missing or wrong part, which the
    /// model reads as a tool result and corrects on its next step.
    pub fn from_markdown(text: &str) -> Result<Plan, String> {
        let document = scan(text)?;
        let decisions = parse_decisions(body(&document, DECISIONS_HEADING));
        let defaults = parse_bullets(body(&document, DEFAULTS_HEADING));
        let constraints = parse_bullets(body(&document, CONSTRAINTS_HEADING));
        let open_questions = parse_bullets(body(&document, OPEN_QUESTIONS_HEADING));

        Ok(Plan {
            goal: document.title,
            decisions,
            defaults,
            constraints,
            open_questions,
        })
    }
}

/// The document split into its title and its `##` sections, with code fences respected.
struct Document {
    title: String,
    sections: Vec<Section>,
}

/// One `##` section: its heading line and the raw lines that follow it.
struct Section {
    heading: String,
    body: Vec<String>,
}

/// Split `text` into the title and the `##` sections, rejecting any deviation from the template.
fn scan(text: &str) -> Result<Document, String> {
    let lines = unfence(text);
    let title_index = lines
        .iter()
        .position(|line| line.starts_with(TITLE_PREFIX))
        .ok_or_else(|| {
            format!("missing title: the document must start with one `{TITLE_PREFIX}` line")
        })?;
    let title = lines[title_index][TITLE_PREFIX.len()..].trim().to_string();

    let mut sections: Vec<Section> = Vec::new();
    let mut fenced = false;
    for line in &lines[title_index + 1..] {
        if is_fence(line) {
            fenced = !fenced;
            push_body(&mut sections, line);
            continue;
        }

        if !fenced && line.starts_with("## ") {
            sections.push(Section {
                heading: line.trim().to_string(),
                body: Vec::new(),
            });
            continue;
        }

        push_body(&mut sections, line);
    }

    check_headings(&sections)?;
    Ok(Document { title, sections })
}

/// The document without the single triple-backtick block that sometimes wraps the whole reply.
fn unfence(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = text.trim().lines().map(str::to_string).collect();
    let wrapped = lines.first().is_some_and(|line| is_fence(line));
    let last = lines.iter().rposition(|line| !line.trim().is_empty());
    if wrapped
        && let Some(last) = last
        && last > 0
        && lines[last].trim() == "```"
    {
        lines = lines[1..last].to_vec();
    }
    lines
}

/// A line whose trimmed start is a triple backtick opens or closes a fenced code block.
fn is_fence(line: &str) -> bool {
    line.trim_start().starts_with("```")
}

/// Append a raw line to the section currently being built, dropping it before the first heading.
fn push_body(sections: &mut [Section], line: &str) {
    if let Some(section) = sections.last_mut() {
        section.body.push(line.to_string());
    }
}

/// Reject any heading that is missing, extra, repeated, or out of order.
fn check_headings(sections: &[Section]) -> Result<(), String> {
    let headings: Vec<&str> = sections
        .iter()
        .map(|section| section.heading.as_str())
        .collect();

    for heading in &headings {
        if !SECTION_HEADINGS.contains(heading) {
            return Err(format!("unexpected section \"{heading}\""));
        }
        if headings.iter().filter(|other| *other == heading).count() > 1 {
            return Err(format!("duplicate section \"{heading}\""));
        }
    }

    for expected in SECTION_HEADINGS {
        if !headings.contains(&expected) {
            return Err(format!("missing section \"{expected}\""));
        }
    }

    for (index, expected) in SECTION_HEADINGS.iter().enumerate() {
        if headings[index] != *expected {
            return Err(format!(
                "section \"{}\" is out of order; expected \"{expected}\"",
                headings[index]
            ));
        }
    }

    Ok(())
}

/// The body lines of the section with this heading: the template's order guarantees it exists.
fn body<'a>(document: &'a Document, heading: &str) -> &'a [String] {
    document
        .sections
        .iter()
        .find(|section| section.heading == heading)
        .map(|section| section.body.as_slice())
        .unwrap_or(&[])
}

/// Section 1: numbered `N. **title**` blocks, each with the decision itself after a blank line.
fn parse_decisions(lines: &[String]) -> Vec<PlanDecision> {
    if is_empty_section(lines) {
        return Vec::new();
    }

    let mut decisions = Vec::new();
    let mut current: Option<PlanDecision> = None;
    let mut fenced = false;

    for line in lines {
        if is_fence(line) {
            fenced = !fenced;
        }

        if !fenced && let Some((title, trailing)) = numbered_title(line) {
            if let Some(decision) = current.take() {
                decisions.push(trim_detail(decision));
            }
            current = Some(PlanDecision {
                title,
                detail: trailing,
            });
            continue;
        }

        if let Some(decision) = &mut current {
            decision.detail.push('\n');
            decision.detail.push_str(line);
        }
    }

    if let Some(decision) = current.take() {
        decisions.push(trim_detail(decision));
    }

    decisions
}

/// The title of an `N. **title**` line plus any text that follows it, or `None` for other lines.
fn numbered_title(line: &str) -> Option<(String, String)> {
    let trimmed = line.trim();
    let (number, rest) = trimmed.split_once(". ")?;
    if number.is_empty() || !number.chars().all(|digit| digit.is_ascii_digit()) {
        return None;
    }

    let (title, trailing) = rest.strip_prefix("**")?.split_once("**")?;
    Some((title.trim().to_string(), trailing.trim().to_string()))
}

/// A decision's detail is the block text after its title line, trimmed.
fn trim_detail(mut decision: PlanDecision) -> PlanDecision {
    decision.detail = decision.detail.trim().to_string();
    decision
}

/// Whether a section body is the single `None.` line (blank lines aside).
fn is_empty_section(lines: &[String]) -> bool {
    let mut meaningful = lines
        .iter()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty());

    meaningful.next() == Some(NONE) && meaningful.next().is_none()
}

/// A list section (defaults, constraints and open questions): each non-blank line is one `- `
/// bullet.
fn parse_bullets(lines: &[String]) -> Vec<String> {
    if is_empty_section(lines) {
        return Vec::new();
    }

    lines
        .iter()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .map(|line| line.strip_prefix("- ").unwrap_or(line).trim())
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect()
}

/// Join the given parts with a blank line between them, dropping the parts that are empty.
fn join_non_empty<const N: usize>(parts: [String; N]) -> String {
    parts
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Join the given parts with a blank line between them.
fn join_blocks<const N: usize>(parts: [&str; N]) -> String {
    parts.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fixture the whole-document assertion is built around.
    fn sample() -> Plan {
        Plan {
            goal: "Typed plan handoff".to_string(),
            decisions: vec![
                PlanDecision {
                    title: "Add the document type".to_string(),
                    detail: "A new `plan_document` module owns the contract.".to_string(),
                },
                PlanDecision {
                    title: "Render at the boundary".to_string(),
                    detail: "`main` renders the plan into `plan.md`.".to_string(),
                },
            ],
            defaults: vec![
                "Keep `write_handoff`'s `text: &str` signature.".to_string(),
                "Copy the join helpers rather than widen `specification`.".to_string(),
            ],
            constraints: vec![
                "No new dependencies.".to_string(),
                "`cargo check` and `cargo test` stay warning-free.".to_string(),
            ],
            open_questions: vec![
                "Does the plan need a byte-stable rendering? The fact needed to settle it: whether \
                 any caller hashes the document."
                    .to_string(),
                "Should open questions gate the handoff? The fact needed to settle it: whether the \
                 spec agent can work with an unanswered question."
                    .to_string(),
            ],
        }
    }

    /// The whole document the fixture renders to.
    fn expected() -> &'static str {
        r#"# Typed plan handoff

## 1. Decisions

1. **Add the document type**

A new `plan_document` module owns the contract.

2. **Render at the boundary**

`main` renders the plan into `plan.md`.

## 2. Defaults Chosen

- Keep `write_handoff`'s `text: &str` signature.
- Copy the join helpers rather than widen `specification`.

## 3. Constraints

- No new dependencies.
- `cargo check` and `cargo test` stay warning-free.

## 4. Open Questions

- Does the plan need a byte-stable rendering? The fact needed to settle it: whether any caller hashes the document.
- Should open questions gate the handoff? The fact needed to settle it: whether the spec agent can work with an unanswered question.
"#
    }

    #[test]
    fn renders_the_fixed_template() {
        assert_eq!(sample().to_markdown(), expected());
    }

    #[test]
    fn the_document_starts_with_the_goal() {
        let doc = sample().to_markdown();
        assert!(doc.starts_with("# Typed plan handoff\n\n## 1. Decisions\n"));
    }

    #[test]
    fn every_section_appears_once_in_order() {
        let doc = sample().to_markdown();
        let headings = [
            "## 1. Decisions",
            "## 2. Defaults Chosen",
            "## 3. Constraints",
            "## 4. Open Questions",
        ];

        let mut previous = 0;
        for heading in headings {
            assert_eq!(doc.matches(heading).count(), 1, "`{}` once", heading);
            let position = doc.find(heading).expect("heading is present");
            assert!(position >= previous, "`{}` in order", heading);
            previous = position;
        }
    }

    #[test]
    fn decisions_render_numbered_with_their_details() {
        let doc = sample().to_markdown();
        assert!(doc.contains(
            "1. **Add the document type**\n\nA new `plan_document` module owns the contract."
        ));
        assert!(
            doc.contains(
                "2. **Render at the boundary**\n\n`main` renders the plan into `plan.md`."
            )
        );
    }

    /// A blank detail drops the block entirely, so the following heading sits exactly one blank
    /// line later -- no stray blank lines.
    #[test]
    fn a_decision_without_a_detail_renders_its_title_only() {
        let mut plan = sample();
        plan.decisions[1].detail = "   ".to_string();
        let doc = plan.to_markdown();
        assert!(doc.contains("2. **Render at the boundary**\n\n## 2. Defaults Chosen"));
        assert!(!doc.contains("3. **"));
    }

    #[test]
    fn empty_decisions_render_none() {
        let mut plan = sample();
        plan.decisions.clear();
        let doc = plan.to_markdown();
        assert!(doc.contains("## 1. Decisions\n\nNone.\n\n## 2. Defaults Chosen"));
    }

    #[test]
    fn empty_defaults_render_none() {
        let mut plan = sample();
        plan.defaults.clear();
        let doc = plan.to_markdown();
        assert!(doc.contains("## 2. Defaults Chosen\n\nNone.\n\n## 3. Constraints"));
    }

    #[test]
    fn empty_constraints_render_none() {
        let mut plan = sample();
        plan.constraints.clear();
        let doc = plan.to_markdown();
        assert!(doc.contains("## 3. Constraints\n\nNone.\n\n## 4. Open Questions"));
    }

    #[test]
    fn empty_open_questions_render_none() {
        let mut plan = sample();
        plan.open_questions.clear();
        let doc = plan.to_markdown();
        assert!(doc.ends_with("## 4. Open Questions\n\nNone.\n"));
    }

    #[test]
    fn open_questions_render_as_ordered_bullets() {
        let doc = sample().to_markdown();
        let first = doc
            .find("- Does the plan need a byte-stable rendering?")
            .expect("the first open question is a bullet");
        let second = doc
            .find("- Should open questions gate the handoff?")
            .expect("the second open question is a bullet");
        assert!(first < second, "open questions keep their order");
    }

    #[test]
    fn blank_list_entries_are_skipped() {
        let mut plan = sample();
        plan.defaults = vec!["  ".to_string(), "  Do the simple thing.  ".to_string()];
        plan.open_questions = vec!["  ".to_string(), "  What remains open?  ".to_string()];
        let doc = plan.to_markdown();
        assert!(
            doc.contains("## 2. Defaults Chosen\n\n- Do the simple thing.\n\n## 3. Constraints")
        );
        assert!(doc.contains("## 4. Open Questions\n\n- What remains open?\n"));
    }

    #[test]
    fn field_values_are_trimmed() {
        let mut plan = sample();
        plan.goal = "  Typed plan handoff  ".to_string();
        plan.decisions[0].title = "  Add the document type  ".to_string();
        plan.decisions[0].detail =
            "\n  A new `plan_document` module owns the contract.  \n".to_string();
        plan.constraints = vec!["  No new dependencies.  ".to_string()];
        plan.open_questions = vec!["  What is still open?  ".to_string()];
        let doc = plan.to_markdown();
        assert!(doc.starts_with("# Typed plan handoff\n\n"));
        assert!(doc.contains(
            "1. **Add the document type**\n\nA new `plan_document` module owns the contract."
        ));
        assert!(doc.contains("## 3. Constraints\n\n- No new dependencies.\n"));
        assert!(doc.contains("## 4. Open Questions\n\n- What is still open?\n"));
    }

    #[test]
    fn the_document_ends_with_exactly_one_newline() {
        let doc = sample().to_markdown();
        assert!(doc.ends_with('\n'));
        assert!(!doc.ends_with("\n\n"));
    }

    /// The renderer and the parser are inverses: parse the rendered fixture and get it back.
    #[test]
    fn the_document_round_trips() {
        assert_eq!(Plan::from_markdown(&sample().to_markdown()), Ok(sample()));
    }

    /// A populated open-question list round-trips in order.
    #[test]
    fn populated_open_questions_round_trip() {
        let plan = Plan {
            goal: "Open items".to_string(),
            decisions: Vec::new(),
            defaults: Vec::new(),
            constraints: Vec::new(),
            open_questions: vec![
                "Which flag wins? The fact needed to settle it: the existing CLI parser."
                    .to_string(),
                "Is the change reversible? The fact needed to settle it: whether the schema is \
                 persisted."
                    .to_string(),
            ],
        };

        let parsed = Plan::from_markdown(&plan.to_markdown()).expect("the document parses");
        assert_eq!(parsed.open_questions, plan.open_questions);
        assert_eq!(parsed, plan);
    }

    /// Every `None.` section parses back to the empty vectors it rendered from.
    #[test]
    fn empty_sections_round_trip() {
        let plan = Plan {
            goal: "Nothing to decide".to_string(),
            decisions: Vec::new(),
            defaults: Vec::new(),
            constraints: Vec::new(),
            open_questions: Vec::new(),
        };

        assert_eq!(Plan::from_markdown(&plan.to_markdown()), Ok(plan));
    }

    /// An empty open-question list is the `None.` line and comes back empty.
    #[test]
    fn empty_open_questions_round_trip() {
        let mut plan = sample();
        plan.open_questions.clear();

        assert_eq!(Plan::from_markdown(&plan.to_markdown()), Ok(plan));
    }

    /// A blank detail is dropped when rendering; the parsed detail is the empty string.
    #[test]
    fn a_decision_without_a_detail_round_trips() {
        let plan = Plan {
            goal: "Title only".to_string(),
            decisions: vec![PlanDecision {
                title: "Decide nothing".to_string(),
                detail: String::new(),
            }],
            defaults: Vec::new(),
            constraints: Vec::new(),
            open_questions: Vec::new(),
        };

        assert_eq!(Plan::from_markdown(&plan.to_markdown()), Ok(plan));
    }

    /// A decision body holding a fenced block with `#` and `##` lines keeps them as content.
    #[test]
    fn fenced_headings_inside_a_decision_round_trip() {
        let plan = Plan {
            goal: "Fences".to_string(),
            decisions: vec![PlanDecision {
                title: "Show the template".to_string(),
                detail: "Example:\n\n```markdown\n# Title\n## Section\n```".to_string(),
            }],
            defaults: Vec::new(),
            constraints: Vec::new(),
            open_questions: Vec::new(),
        };

        assert_eq!(Plan::from_markdown(&plan.to_markdown()), Ok(plan));
    }

    /// A reply wrapped in one triple-backtick block is unwrapped before parsing.
    #[test]
    fn a_fenced_reply_round_trips() {
        let text = format!("```markdown\n{}\n```\n", sample().to_markdown().trim_end());
        assert_eq!(Plan::from_markdown(&text), Ok(sample()));
    }

    /// A document with no title names the missing title.
    #[test]
    fn a_document_without_a_title_is_rejected() {
        for text in ["", "   ", "just some prose"] {
            let error = Plan::from_markdown(text).expect_err("must not parse");
            assert!(
                error.contains("missing title"),
                "`{text}` names the problem: {error}"
            );
        }
    }

    /// A document missing a section names the heading it needed.
    #[test]
    fn a_document_missing_a_section_is_rejected() {
        let text = document(
            "Goal",
            &[(DEFAULTS_HEADING, "None."), (CONSTRAINTS_HEADING, "None.")],
        );

        let error = Plan::from_markdown(&text).expect_err("must not parse");
        assert!(
            error.contains("missing section \"## 1. Decisions\""),
            "{error}"
        );
    }

    /// A document missing the open questions section names the heading it needed.
    #[test]
    fn a_document_missing_open_questions_is_rejected() {
        let text = document(
            "Goal",
            &[
                (DECISIONS_HEADING, "None."),
                (DEFAULTS_HEADING, "None."),
                (CONSTRAINTS_HEADING, "None."),
            ],
        );

        let error = Plan::from_markdown(&text).expect_err("must not parse");
        assert!(
            error.contains(&format!("missing section \"{OPEN_QUESTIONS_HEADING}\"")),
            "{error}"
        );
    }

    /// Headings out of order are rejected and the expected heading is named.
    #[test]
    fn sections_out_of_order_are_rejected() {
        let text = document(
            "Goal",
            &[
                (DEFAULTS_HEADING, "None."),
                (DECISIONS_HEADING, "None."),
                (CONSTRAINTS_HEADING, "None."),
                (OPEN_QUESTIONS_HEADING, "None."),
            ],
        );

        let error = Plan::from_markdown(&text).expect_err("must not parse");
        assert!(error.contains("out of order"), "{error}");
        assert!(error.contains(DECISIONS_HEADING), "{error}");
    }

    /// The right headings in the wrong order are rejected even when all four are present.
    #[test]
    fn misordered_open_questions_are_rejected() {
        let text = document(
            "Goal",
            &[
                (DECISIONS_HEADING, "None."),
                (DEFAULTS_HEADING, "None."),
                (OPEN_QUESTIONS_HEADING, "None."),
                (CONSTRAINTS_HEADING, "None."),
            ],
        );

        let error = Plan::from_markdown(&text).expect_err("must not parse");
        assert!(error.contains("out of order"), "{error}");
        assert!(error.contains(CONSTRAINTS_HEADING), "{error}");
    }

    /// An unknown heading is rejected.
    #[test]
    fn an_extra_section_is_rejected() {
        let text = document(
            "Goal",
            &[
                (DECISIONS_HEADING, "None."),
                (DEFAULTS_HEADING, "None."),
                (CONSTRAINTS_HEADING, "None."),
                (OPEN_QUESTIONS_HEADING, "None."),
                ("## 5. Extras", "None."),
            ],
        );

        let error = Plan::from_markdown(&text).expect_err("must not parse");
        assert!(error.contains("unexpected section"), "{error}");
        assert!(error.contains("## 5. Extras"), "{error}");
    }

    /// A heading used twice is rejected.
    #[test]
    fn a_duplicate_section_is_rejected() {
        let text = document(
            "Goal",
            &[
                (DECISIONS_HEADING, "None."),
                (DEFAULTS_HEADING, "None."),
                (CONSTRAINTS_HEADING, "None."),
                (OPEN_QUESTIONS_HEADING, "None."),
                (OPEN_QUESTIONS_HEADING, "None."),
            ],
        );

        let error = Plan::from_markdown(&text).expect_err("must not parse");
        assert!(error.contains("duplicate section"), "{error}");
        assert!(error.contains(OPEN_QUESTIONS_HEADING), "{error}");
    }

    /// Build a document from a title and `(heading, body)` pairs, for the rejection tests.
    fn document(title: &str, sections: &[(&str, &str)]) -> String {
        let mut text = format!("{TITLE_PREFIX}{title}");
        for (heading, body) in sections {
            text.push_str(&format!("\n\n{heading}\n\n{body}"));
        }
        text.push('\n');
        text
    }
}
