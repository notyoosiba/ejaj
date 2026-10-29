//! The technical specification as a typed document.
//!
//! Markdown is only a rendering of it: [`Specification::to_markdown`] produces it from the type and
//! [`Specification::from_markdown`] parses it back, so the type is the contract the spec agent and
//! the tickets agent share and the document's headings stay with the harness.
//!
//! The six core sections and the non-deferred `###` subsections are mandatory and ordered. The
//! deferred-risk section and its two subsections carry content that only sometimes applies, so
//! they are optional: an absent section or subsection parses as an empty string, and an empty
//! value renders neither its heading nor, when the whole section is empty, the section itself.
//! Everything that is supplied is still checked: unknown, duplicate and out-of-order headings are
//! rejected.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The prefix of the document's single title line: the feature name, named by the model.
const TITLE_PREFIX: &str = "# ";
/// The heading that opens the problem-statement section.
pub const PROBLEM_STATEMENT_HEADING: &str = "## 1. Problem Statement";
/// The heading that opens the solution-overview section.
pub const SOLUTION_OVERVIEW_HEADING: &str = "## 2. Solution Overview";
/// The heading that opens the user-stories section.
pub const USER_STORIES_HEADING: &str = "## 3. User Stories & Acceptance Criteria";
/// The heading that opens the implementation-decisions section.
pub const IMPLEMENTATION_DECISIONS_HEADING: &str = "## 4. Implementation Decisions";
/// The heading that opens the testing-decisions section.
pub const TESTING_DECISIONS_HEADING: &str = "## 5. Testing Decisions & Seams";
/// The heading that opens the out-of-scope section.
pub const OUT_OF_SCOPE_HEADING: &str = "## 6. Out of Scope";
/// The heading that opens the deferred-items section.
pub const DEFERRED_ITEMS_HEADING: &str = "## 7. Deferred Items & Operational Risks";
/// The six sections every document must carry, in the order the document must use them. The
/// deferred-items section that follows them is optional.
pub const REQUIRED_SECTION_HEADINGS: [&str; 6] = [
    PROBLEM_STATEMENT_HEADING,
    SOLUTION_OVERVIEW_HEADING,
    USER_STORIES_HEADING,
    IMPLEMENTATION_DECISIONS_HEADING,
    TESTING_DECISIONS_HEADING,
    OUT_OF_SCOPE_HEADING,
];
/// The one section at the end of the document that may be omitted: deferred risk content that
/// does not apply. When it is present it must still come last.
pub const OPTIONAL_SECTION_HEADINGS: [&str; 1] = [DEFERRED_ITEMS_HEADING];
/// Every section heading, the required ones first and the optional one last, in the order the
/// document must use them.
pub const SECTION_HEADINGS: [&str; 7] = [
    PROBLEM_STATEMENT_HEADING,
    SOLUTION_OVERVIEW_HEADING,
    USER_STORIES_HEADING,
    IMPLEMENTATION_DECISIONS_HEADING,
    TESTING_DECISIONS_HEADING,
    OUT_OF_SCOPE_HEADING,
    DEFERRED_ITEMS_HEADING,
];
/// The subheading that opens the module-architecture subsection of section 4.
pub const MODULE_ARCHITECTURE_HEADING: &str = "### Module Architecture";
/// The subheading that opens the data-models subsection of section 4.
pub const DATA_MODELS_HEADING: &str = "### Data Models & Contracts";
/// The subheading that opens the ADR-compliance subsection of section 4.
pub const ADR_COMPLIANCE_HEADING: &str = "### ADR Compliance";
/// The subheading that opens the test-seams subsection of section 5.
pub const TEST_SEAMS_HEADING: &str = "### Test Seams";
/// The subheading that opens the coverage-strategy subsection of section 5.
pub const COVERAGE_STRATEGY_HEADING: &str = "### Coverage Strategy";
/// The subheading that opens the prior-art subsection of section 5.
pub const PRIOR_ART_HEADING: &str = "### Prior Art";
/// The subheading that opens the operational-risks subsection of section 7.
pub const OPERATIONAL_RISKS_HEADING: &str = "### Operational Risks";
/// The subheading that opens the follow-up-work subsection of section 7.
pub const FOLLOW_UP_WORK_HEADING: &str = "### Follow-Up Work";
/// The `###` subsections of section 4, in the order the document must use them. All of them are
/// required.
pub const IMPLEMENTATION_SUBSECTIONS: [&str; 3] = [
    MODULE_ARCHITECTURE_HEADING,
    DATA_MODELS_HEADING,
    ADR_COMPLIANCE_HEADING,
];
/// The `###` subsections of section 5, in the order the document must use them. All of them are
/// required.
pub const TESTING_SUBSECTIONS: [&str; 3] = [
    TEST_SEAMS_HEADING,
    COVERAGE_STRATEGY_HEADING,
    PRIOR_ART_HEADING,
];
/// The `###` subsections of section 7, in the order the document must use them when they are
/// present. Each one is optional on its own.
pub const DEFERRED_SUBSECTIONS: [&str; 2] = [OPERATIONAL_RISKS_HEADING, FOLLOW_UP_WORK_HEADING];
/// Heading-shaped variants of a canonical `##` heading that a real model has
/// actually produced. The canonical headings stay the contract: a variant is
/// accepted on input and parsed as its canonical heading, so the rendered
/// document -- and the template -- still cannot drift. Each entry is
/// `(variant the model wrote, canonical heading)`.
const HEADING_ALIASES: [(&str, &str); 1] = [
    // The model drops the `& Seams` suffix when it fills section 5 from prose.
    ("## 5. Testing Decisions", TESTING_DECISIONS_HEADING),
];
/// What a section with no entries renders as, and what the parser reads back as "empty".
pub const NONE: &str = "None.";
/// The prefix of a `###` subsection heading.
const SUBSECTION_PREFIX: &str = "### ";
/// The technical specification the spec agent hands to the tickets agent.
///
/// Every field below is required, and the markdown document the tickets agent receives is derived
/// from this object by [`Specification::to_markdown`] -- the model writes the document and the
/// harness parses it back with [`Specification::from_markdown`], so the headings stay with the
/// harness. Only the deferred items may be empty, and an empty deferred value renders no heading.
#[derive(Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Specification {
    /// The change in one line; rendered as the document title.
    pub feature_name: String,
    /// The problem, the motivation, and why the current behaviour is insufficient.
    pub problem_statement: String,
    /// The change at a high level: how it resolves the problem and where it sits in the
    /// architecture.
    pub solution_overview: String,
    /// One entry per user-facing requirement.
    pub user_stories: Vec<UserStory>,
    pub implementation_decisions: ImplementationDecisions,
    pub testing_decisions: TestingDecisions,
    /// Related capabilities, refactors and optimisations explicitly excluded from this work.
    pub out_of_scope: Vec<String>,
    pub deferred_items: DeferredItems,
}

/// One user-facing requirement: the expected behaviour and how to verify it.
#[derive(Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct UserStory {
    /// A short label for the requirement.
    pub title: String,
    /// The user-facing behaviour that is expected.
    pub requirement: String,
    /// Concrete, verifiable pass/fail conditions, including error states and edge cases.
    pub acceptance_criteria: Vec<String>,
}

/// How the change is built.
#[derive(Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ImplementationDecisions {
    /// New and modified components, their interfaces and their responsibilities.
    pub module_architecture: String,
    /// Canonical types, schemas, payloads and state transitions. No volatile code snippets, no
    /// brittle file paths, no full function bodies.
    pub data_models_and_contracts: String,
    /// How the design aligns with the conventions already in the codebase, or what it changes.
    pub adr_compliance: String,
}

/// How the change is verified.
#[derive(Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TestingDecisions {
    /// Where the boundaries are drawn for unit, integration and contract tests.
    pub test_seams: String,
    /// What is verified at each layer: domain logic in unit tests, queries in integration tests,
    /// third-party clients mocked.
    pub coverage_strategy: String,
    /// Existing test patterns in the repository to mirror.
    pub prior_art: String,
}

/// What this milestone deliberately leaves out.
///
/// Both fields are optional in the document, because neither kind of deferred content always
/// applies. An absent `###` subsection parses as an empty string, and an empty value renders no
/// heading.
#[derive(Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct DeferredItems {
    /// Production risks, migration concerns and monitoring requirements.
    pub operational_risks: String,
    /// Follow-up work identified during discovery but not required for this milestone.
    pub follow_up_work: String,
}

impl Specification {
    /// Render this specification as the markdown document the tickets agent receives.
    ///
    /// The template is fixed: the six numbered core sections, with `###` subsections under
    /// sections 4 and 5, followed by the deferred-items section. Field values are inserted
    /// verbatim after trimming, and the returned string ends with exactly one `\n`. The deferred
    /// items are the one optional part: an empty value renders neither its subsection heading nor,
    /// when both values are empty, the section heading either.
    pub fn to_markdown(&self) -> String {
        let mut sections = vec![
            format!("{TITLE_PREFIX}{}", self.feature_name.trim()),
            format!(
                "{PROBLEM_STATEMENT_HEADING}\n\n{}",
                self.problem_statement.trim()
            ),
            format!(
                "{SOLUTION_OVERVIEW_HEADING}\n\n{}",
                self.solution_overview.trim()
            ),
            format!("{USER_STORIES_HEADING}\n\n{}", self.user_stories_markdown()),
            join_blocks([
                IMPLEMENTATION_DECISIONS_HEADING,
                MODULE_ARCHITECTURE_HEADING,
                self.implementation_decisions.module_architecture.trim(),
                DATA_MODELS_HEADING,
                self.implementation_decisions
                    .data_models_and_contracts
                    .trim(),
                ADR_COMPLIANCE_HEADING,
                self.implementation_decisions.adr_compliance.trim(),
            ]),
            join_blocks([
                TESTING_DECISIONS_HEADING,
                TEST_SEAMS_HEADING,
                self.testing_decisions.test_seams.trim(),
                COVERAGE_STRATEGY_HEADING,
                self.testing_decisions.coverage_strategy.trim(),
                PRIOR_ART_HEADING,
                self.testing_decisions.prior_art.trim(),
            ]),
            format!("{OUT_OF_SCOPE_HEADING}\n\n{}", self.out_of_scope_markdown()),
        ];

        if let Some(deferred) = self.deferred_items_markdown() {
            sections.push(deferred);
        }

        let mut document = sections.join("\n\n");
        document.push('\n');
        document
    }

    /// The body of section 7, or `None` when both deferred values are empty.
    ///
    /// Only the subsections that carry content are rendered, in the canonical order, and the
    /// section heading is dropped with them when nothing is left.
    fn deferred_items_markdown(&self) -> Option<String> {
        let subsections: Vec<String> = [
            (
                OPERATIONAL_RISKS_HEADING,
                self.deferred_items.operational_risks.trim(),
            ),
            (
                FOLLOW_UP_WORK_HEADING,
                self.deferred_items.follow_up_work.trim(),
            ),
        ]
        .into_iter()
        .filter(|(_, body)| !body.is_empty())
        .map(|(heading, body)| format!("{heading}\n\n{body}"))
        .collect();

        if subsections.is_empty() {
            return None;
        }

        Some(format!(
            "{DEFERRED_ITEMS_HEADING}\n\n{}",
            subsections.join("\n\n")
        ))
    }

    /// The body of section 3: numbered stories, each with its requirement and criteria.
    fn user_stories_markdown(&self) -> String {
        if self.user_stories.is_empty() {
            return NONE.to_string();
        }

        let blocks: Vec<String> = self
            .user_stories
            .iter()
            .enumerate()
            .map(|(index, story)| {
                let criteria = story
                    .acceptance_criteria
                    .iter()
                    .map(|criterion| criterion.trim())
                    .filter(|criterion| !criterion.is_empty())
                    .map(|criterion| format!("- {}", criterion))
                    .collect::<Vec<_>>()
                    .join("\n");

                join_non_empty([
                    format!("{}. **{}**", index + 1, story.title.trim()),
                    story.requirement.trim().to_string(),
                    criteria,
                ])
            })
            .collect();

        blocks.join("\n\n")
    }

    /// The body of section 6: one bullet per out-of-scope entry.
    fn out_of_scope_markdown(&self) -> String {
        let bullets: Vec<String> = self
            .out_of_scope
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

impl Specification {
    /// Parse the markdown document back into a specification.
    ///
    /// This is the inverse of [`Specification::to_markdown`]: the same constants define the
    /// headings both sides use, so the template cannot drift. The document is the model's own
    /// text, so the scanner is deliberately small -- one line pass, no parser library and no
    /// regex. Anything that does not match the template is an `Err` naming the missing or wrong
    /// part, which the model reads as a tool result and corrects on its next step. The deferred
    /// section and its subsections may be absent, and absence parses as an empty string.
    pub fn from_markdown(text: &str) -> Result<Specification, String> {
        let document = scan(text)?;
        let implementation = parse_subsections(
            body(&document, IMPLEMENTATION_DECISIONS_HEADING),
            IMPLEMENTATION_DECISIONS_HEADING,
            &IMPLEMENTATION_SUBSECTIONS,
            &[],
        )?;
        let testing = parse_subsections(
            body(&document, TESTING_DECISIONS_HEADING),
            TESTING_DECISIONS_HEADING,
            &TESTING_SUBSECTIONS,
            &[],
        )?;
        let deferred = parse_subsections(
            body(&document, DEFERRED_ITEMS_HEADING),
            DEFERRED_ITEMS_HEADING,
            &[],
            &DEFERRED_SUBSECTIONS,
        )?;
        let problem_statement = body_text(body(&document, PROBLEM_STATEMENT_HEADING));
        let solution_overview = body_text(body(&document, SOLUTION_OVERVIEW_HEADING));
        let user_stories = parse_user_stories(body(&document, USER_STORIES_HEADING))?;
        let out_of_scope = parse_bullets(body(&document, OUT_OF_SCOPE_HEADING));
        let feature_name = document.title;

        Ok(Specification {
            feature_name,
            problem_statement,
            solution_overview,
            user_stories,
            implementation_decisions: ImplementationDecisions {
                module_architecture: implementation[0].clone(),
                data_models_and_contracts: implementation[1].clone(),
                adr_compliance: implementation[2].clone(),
            },
            testing_decisions: TestingDecisions {
                test_seams: testing[0].clone(),
                coverage_strategy: testing[1].clone(),
                prior_art: testing[2].clone(),
            },
            out_of_scope,
            deferred_items: DeferredItems {
                operational_risks: deferred[0].clone(),
                follow_up_work: deferred[1].clone(),
            },
        })
    }
}

/// The document split into its title and its `##` sections, with code fences respected.
struct Document {
    title: String,
    sections: Vec<Section>,
}

/// One `##` section or `###` subsection: its heading line and the raw lines that follow it.
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
                heading: canonical_heading(line.trim()).to_string(),
                body: Vec::new(),
            });
            continue;
        }

        push_body(&mut sections, line);
    }

    check_headings(
        &sections,
        &REQUIRED_SECTION_HEADINGS,
        &OPTIONAL_SECTION_HEADINGS,
    )?;
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

/// The canonical form of a `##` heading the model wrote: an exact alias is
/// mapped to its canonical heading, anything else is left unchanged so
/// [`check_headings`] accepts or rejects it as before.
fn canonical_heading(heading: &str) -> &str {
    HEADING_ALIASES
        .iter()
        .find(|(variant, _)| *variant == heading)
        .map(|(_, canonical)| *canonical)
        .unwrap_or(heading)
}

/// Append a raw line to the section currently being built, dropping it before the first heading.
fn push_body(sections: &mut [Section], line: &str) {
    if let Some(section) = sections.last_mut() {
        section.body.push(line.to_string());
    }
}

/// Reject any heading that is unknown, repeated, missing when required, or out of order.
///
/// `required` must appear in the document, in the given order; `optional` may be absent, but the
/// ones that do appear must still follow the required headings in the same order.
fn check_headings(
    sections: &[Section],
    required: &[&str],
    optional: &[&str],
) -> Result<(), String> {
    let headings: Vec<&str> = sections
        .iter()
        .map(|section| section.heading.as_str())
        .collect();

    for heading in &headings {
        if !required.contains(heading) && !optional.contains(heading) {
            return Err(format!("unexpected section \"{heading}\""));
        }
        if headings.iter().filter(|other| *other == heading).count() > 1 {
            return Err(format!("duplicate section \"{heading}\""));
        }
    }

    for wanted in required {
        if !headings.contains(wanted) {
            return Err(format!("missing section \"{wanted}\""));
        }
    }

    check_order(
        &headings,
        &canonical_headings(&headings, required, optional),
        "section",
        None,
    )
}

/// The headings the document must use, in order, given the ones it actually carries: every
/// required heading, then the optional headings it carries, still in canonical order.
fn canonical_headings<'a>(
    present: &[&'a str],
    required: &[&'a str],
    optional: &[&'a str],
) -> Vec<&'a str> {
    required
        .iter()
        .chain(optional.iter().filter(|heading| present.contains(heading)))
        .copied()
        .collect()
}

/// Compare the headings the document carries with the canonical sequence and name the first
/// heading that breaks it. The two sequences are the same length once the unknown, duplicate and
/// missing-heading cases have been ruled out.
fn check_order(
    headings: &[&str],
    expected: &[&str],
    noun: &str,
    in_section: Option<&str>,
) -> Result<(), String> {
    let context = in_section
        .map(|section| format!(" in \"{section}\""))
        .unwrap_or_default();

    if let Some(index) = headings
        .iter()
        .zip(expected.iter())
        .position(|(got, want)| got != want)
    {
        return Err(format!(
            "{noun} \"{}\"{context} is out of order; expected \"{}\"",
            headings[index], expected[index]
        ));
    }

    Ok(())
}

/// The body lines of the section with this heading, or the empty slice when the section is one of
/// the optional ones and the document omits it.
fn body<'a>(document: &'a Document, heading: &str) -> &'a [String] {
    document
        .sections
        .iter()
        .find(|section| section.heading == heading)
        .map(|section| section.body.as_slice())
        .unwrap_or(&[])
}

/// A section body as one trimmed string: the prose fields span the lines of their section.
fn body_text(lines: &[String]) -> String {
    lines.join("\n").trim().to_string()
}

/// The `###` subsections of one section body, with code fences respected.
fn subsection_list(lines: &[String]) -> Vec<Section> {
    let mut subsections: Vec<Section> = Vec::new();
    let mut fenced = false;
    for line in lines {
        if is_fence(line) {
            fenced = !fenced;
            push_body(&mut subsections, line);
            continue;
        }

        if !fenced && line.starts_with(SUBSECTION_PREFIX) {
            subsections.push(Section {
                heading: line.trim().to_string(),
                body: Vec::new(),
            });
            continue;
        }

        push_body(&mut subsections, line);
    }
    subsections
}

/// Split a subsected section into one trimmed string per expected `###` subsection, in canonical
/// order.
///
/// The subsections in `required` must all be present; those in `optional` may be absent, but the
/// ones that do appear must still follow the required ones in canonical order. An absent optional
/// subsection reads back as the empty string, which is exactly what renders no heading again.
fn parse_subsections(
    lines: &[String],
    section: &str,
    required: &[&str],
    optional: &[&str],
) -> Result<Vec<String>, String> {
    let subsections = subsection_list(lines);
    let headings: Vec<&str> = subsections
        .iter()
        .map(|subsection| subsection.heading.as_str())
        .collect();

    for heading in &headings {
        if !required.contains(heading) && !optional.contains(heading) {
            return Err(format!(
                "unexpected subsection \"{heading}\" in \"{section}\""
            ));
        }
        if headings.iter().filter(|other| *other == heading).count() > 1 {
            return Err(format!(
                "duplicate subsection \"{heading}\" in \"{section}\""
            ));
        }
    }

    for wanted in required {
        if !headings.contains(wanted) {
            return Err(format!("missing subsection \"{wanted}\" in \"{section}\""));
        }
    }

    check_order(
        &headings,
        &canonical_headings(&headings, required, optional),
        "subsection",
        Some(section),
    )?;

    Ok(required
        .iter()
        .chain(optional)
        .map(|heading| {
            subsections
                .iter()
                .find(|subsection| subsection.heading == *heading)
                .map(|subsection| body_text(&subsection.body))
                .unwrap_or_default()
        })
        .collect())
}

/// Section 3: numbered `N. **title**` stories, each with a requirement and `- ` criteria.
fn parse_user_stories(lines: &[String]) -> Result<Vec<UserStory>, String> {
    if is_empty_section(lines) {
        return Ok(Vec::new());
    }

    let mut stories: Vec<UserStory> = Vec::new();
    let mut fenced = false;
    for line in lines {
        if is_fence(line) {
            fenced = !fenced;
        }

        if !fenced && let Some((title, trailing)) = numbered_title(line) {
            stories.push(UserStory {
                title,
                requirement: trailing,
                acceptance_criteria: Vec::new(),
            });
            continue;
        }

        let Some(story) = stories.last_mut() else {
            if !line.trim().is_empty() {
                return Err(format!(
                    "section \"{USER_STORIES_HEADING}\" must begin with a numbered `N. **title**` \
                     story"
                ));
            }
            continue;
        };

        let criterion = line.trim_start().strip_prefix("- ").map(strip_checkbox);
        if !fenced
            && let Some(criterion) = criterion
            && !criterion.is_empty()
        {
            story.acceptance_criteria.push(criterion.to_string());
            continue;
        }

        story.requirement.push('\n');
        story.requirement.push_str(line);
    }

    for story in &mut stories {
        story.requirement = story.requirement.trim().to_string();
    }

    Ok(stories)
}

/// A criterion with any leading checkbox marker removed. A model that writes
/// `- [ ] text` or `- [x] text` means the same criterion as `- text`, and the
/// ticket contract compares criterion text exactly, so the marker is stripped
/// at the parser rather than carried into every comparison.
fn strip_checkbox(criterion: &str) -> &str {
    let trimmed = criterion.trim_start();
    for marker in ["[ ]", "[x]", "[X]"] {
        if let Some(rest) = trimmed.strip_prefix(marker) {
            return rest.trim_start();
        }
    }
    trimmed
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

/// Whether a section body is the single `None.` line (blank lines aside).
fn is_empty_section(lines: &[String]) -> bool {
    let mut meaningful = lines
        .iter()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty());

    meaningful.next() == Some(NONE) && meaningful.next().is_none()
}

/// Section 6: each non-blank line is one `- ` bullet.
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
    fn sample() -> Specification {
        Specification {
            feature_name: "Typed specification handoff".to_string(),
            problem_statement: "The handoff is a free-text blob.".to_string(),
            solution_overview: "Hand over a typed document and render markdown from it."
                .to_string(),
            user_stories: vec![
                UserStory {
                    title: "Typed handoff".to_string(),
                    requirement: "The spec agent hands over a specification object.".to_string(),
                    acceptance_criteria: vec![
                        "A complete object parses.".to_string(),
                        "An incomplete object is an error.".to_string(),
                    ],
                },
                UserStory {
                    title: "Fixed layout".to_string(),
                    requirement: "Every section is present in the rendered document.".to_string(),
                    acceptance_criteria: vec!["The seven sections appear in order.".to_string()],
                },
            ],
            implementation_decisions: ImplementationDecisions {
                module_architecture: "A new `specification` module owns the document.".to_string(),
                data_models_and_contracts: "One required field per section.".to_string(),
                adr_compliance: "Mirrors the typed tool-argument convention.".to_string(),
            },
            testing_decisions: TestingDecisions {
                test_seams: "`Specification::to_markdown`.".to_string(),
                coverage_strategy: "Unit tests for the renderer.".to_string(),
                prior_art: "The `mod tests` block in `tools.rs`.".to_string(),
            },
            out_of_scope: vec![
                "Content validation.".to_string(),
                "A `spec.json` artifact.".to_string(),
            ],
            deferred_items: DeferredItems {
                operational_risks: "Providers may reject nested tool schemas.".to_string(),
                follow_up_work: "None.".to_string(),
            },
        }
    }

    /// The whole document the fixture renders to.
    fn expected() -> &'static str {
        r#"# Typed specification handoff

## 1. Problem Statement

The handoff is a free-text blob.

## 2. Solution Overview

Hand over a typed document and render markdown from it.

## 3. User Stories & Acceptance Criteria

1. **Typed handoff**

The spec agent hands over a specification object.

- A complete object parses.
- An incomplete object is an error.

2. **Fixed layout**

Every section is present in the rendered document.

- The seven sections appear in order.

## 4. Implementation Decisions

### Module Architecture

A new `specification` module owns the document.

### Data Models & Contracts

One required field per section.

### ADR Compliance

Mirrors the typed tool-argument convention.

## 5. Testing Decisions & Seams

### Test Seams

`Specification::to_markdown`.

### Coverage Strategy

Unit tests for the renderer.

### Prior Art

The `mod tests` block in `tools.rs`.

## 6. Out of Scope

- Content validation.
- A `spec.json` artifact.

## 7. Deferred Items & Operational Risks

### Operational Risks

Providers may reject nested tool schemas.

### Follow-Up Work

None.
"#
    }

    #[test]
    fn renders_the_fixed_template() {
        assert_eq!(sample().to_markdown(), expected());
    }

    #[test]
    fn the_document_starts_with_the_feature_name() {
        let doc = sample().to_markdown();
        assert!(doc.starts_with("# Typed specification handoff\n\n## 1. Problem Statement\n"));
    }

    #[test]
    fn every_section_appears_once_in_order() {
        let doc = sample().to_markdown();
        let headings = [
            "## 1. Problem Statement",
            "## 2. Solution Overview",
            "## 3. User Stories & Acceptance Criteria",
            "## 4. Implementation Decisions",
            "## 5. Testing Decisions & Seams",
            "## 6. Out of Scope",
            "## 7. Deferred Items & Operational Risks",
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
    fn user_stories_render_numbered_with_their_criteria() {
        let doc = sample().to_markdown();
        assert!(doc.contains(
            "1. **Typed handoff**\n\nThe spec agent hands over a specification object.\n\n\
             - A complete object parses.\n- An incomplete object is an error."
        ));
        assert!(doc.contains(
            "2. **Fixed layout**\n\nEvery section is present in the rendered document.\n\n\
             - The seven sections appear in order."
        ));
    }

    /// A checkbox marker is model formatting, not criterion text: `- [ ]` and
    /// `- [x]` parse to the same bullet as `- `.
    #[test]
    fn acceptance_criteria_drop_a_leading_checkbox_marker() {
        let doc = expected()
            .replace(
                "- A complete object parses.",
                "- [ ] A complete object parses.",
            )
            .replace(
                "- An incomplete object is an error.",
                "- [x] An incomplete object is an error.",
            );

        let parsed = Specification::from_markdown(&doc).expect("the document still parses");
        assert_eq!(parsed, sample());
    }

    #[test]
    fn a_story_without_criteria_renders_no_bullets() {
        let mut specification = sample();
        specification.user_stories[1].acceptance_criteria.clear();
        let doc = specification.to_markdown();
        assert!(doc.contains(
            "2. **Fixed layout**\n\nEvery section is present in the rendered document.\n\n\
             ## 4. Implementation Decisions"
        ));
        assert_eq!(Specification::from_markdown(&doc), Ok(specification));
    }

    #[test]
    fn out_of_scope_entries_render_as_bullets() {
        let doc = sample().to_markdown();
        assert!(doc.contains(
            "## 6. Out of Scope\n\n- Content validation.\n- A `spec.json` artifact.\n\n\
             ## 7. Deferred Items & Operational Risks"
        ));
    }

    #[test]
    fn empty_user_stories_render_none() {
        let mut specification = sample();
        specification.user_stories.clear();
        let doc = specification.to_markdown();
        assert!(doc.contains(
            "## 3. User Stories & Acceptance Criteria\n\nNone.\n\n\
             ## 4. Implementation Decisions"
        ));
        assert_eq!(Specification::from_markdown(&doc), Ok(specification));
    }

    #[test]
    fn empty_out_of_scope_renders_none() {
        let mut specification = sample();
        specification.out_of_scope.clear();
        let doc = specification.to_markdown();
        assert!(
            doc.contains("## 6. Out of Scope\n\nNone.\n\n## 7. Deferred Items & Operational Risks")
        );
        assert_eq!(Specification::from_markdown(&doc), Ok(specification));
    }

    #[test]
    fn field_values_are_trimmed() {
        let mut specification = sample();
        specification.feature_name = "  Typed specification handoff  ".to_string();
        specification.problem_statement = "\n  The handoff is a free-text blob.  \n".to_string();
        assert_eq!(specification.to_markdown(), expected());
        assert_eq!(
            Specification::from_markdown(&specification.to_markdown()),
            Ok(sample())
        );
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
        assert_eq!(
            Specification::from_markdown(&sample().to_markdown()),
            Ok(sample())
        );
    }

    /// A fully populated deferred section renders both subsections, in canonical order, between
    /// the out-of-scope section and the end of the document.
    #[test]
    fn a_fully_populated_deferred_section_renders_canonically() {
        let doc = sample().to_markdown();
        assert!(doc.ends_with(&format!(
            "{DEFERRED_ITEMS_HEADING}\n\n{OPERATIONAL_RISKS_HEADING}\n\n\
             Providers may reject nested tool schemas.\n\n{FOLLOW_UP_WORK_HEADING}\n\nNone.\n"
        )));
        assert!(doc.contains(&format!(
            "{OUT_OF_SCOPE_HEADING}\n\n- Content validation.\n- A `spec.json` artifact.\n\n\
             {DEFERRED_ITEMS_HEADING}"
        )));
        assert_eq!(Specification::from_markdown(&doc), Ok(sample()));
    }

    /// Both deferred values empty: neither the section nor its subsections is rendered, and the
    /// document still parses back into the same contract.
    #[test]
    fn empty_deferred_items_render_no_section() {
        let mut specification = sample();
        specification.deferred_items = DeferredItems {
            operational_risks: "  \n".to_string(),
            follow_up_work: String::new(),
        };

        let doc = specification.to_markdown();
        assert!(!doc.contains(DEFERRED_ITEMS_HEADING));
        assert!(!doc.contains(OPERATIONAL_RISKS_HEADING));
        assert!(!doc.contains(FOLLOW_UP_WORK_HEADING));
        assert!(doc.ends_with(&format!(
            "{OUT_OF_SCOPE_HEADING}\n\n- Content validation.\n- A `spec.json` artifact.\n"
        )));

        specification.deferred_items = empty_deferred_items();
        assert_eq!(Specification::from_markdown(&doc), Ok(specification));
    }

    /// One deferred value empty: its subsection heading is dropped and the other one stays.
    #[test]
    fn an_empty_deferred_subsection_is_omitted() {
        let mut specification = sample();
        specification.deferred_items.follow_up_work = String::new();

        let doc = specification.to_markdown();
        assert!(doc.ends_with(&format!(
            "{DEFERRED_ITEMS_HEADING}\n\n{OPERATIONAL_RISKS_HEADING}\n\n\
             Providers may reject nested tool schemas.\n"
        )));
        assert!(!doc.contains(FOLLOW_UP_WORK_HEADING));
        assert_eq!(Specification::from_markdown(&doc), Ok(specification));
    }

    /// Section 5 written without the `& Seams` suffix is a heading-shaped variant a real model
    /// produced: it is accepted on input and the rendered document stays canonical.
    #[test]
    fn the_section_five_heading_variant_is_accepted() {
        let variant = sample()
            .to_markdown()
            .replace(TESTING_DECISIONS_HEADING, "## 5. Testing Decisions");
        assert!(!variant.contains(TESTING_DECISIONS_HEADING));

        let parsed = Specification::from_markdown(&variant).expect("the variant parses");
        assert_eq!(parsed, sample());
        assert!(parsed.to_markdown().contains(TESTING_DECISIONS_HEADING));
    }

    /// A reply wrapped in one triple-backtick block is unwrapped before parsing.
    #[test]
    fn a_fenced_reply_round_trips() {
        let text = format!("```markdown\n{}\n```\n", sample().to_markdown().trim_end());
        assert_eq!(Specification::from_markdown(&text), Ok(sample()));
    }

    /// A field body holding a fenced block with `##` and `###` lines keeps them as content.
    #[test]
    fn fenced_headings_inside_a_subsection_round_trip() {
        let mut specification = sample();
        specification.implementation_decisions.module_architecture =
            "Example:\n\n```markdown\n# Title\n## Section\n### Data Models & Contracts\n```"
                .to_string();

        assert_eq!(
            Specification::from_markdown(&specification.to_markdown()),
            Ok(specification)
        );
    }

    /// The rejection tests' base is itself a valid document.
    #[test]
    fn the_rejection_base_parses() {
        assert!(Specification::from_markdown(&valid_document()).is_ok());
    }

    /// A document with no title names the missing title.
    #[test]
    fn a_document_without_a_title_is_rejected() {
        for text in ["", "   ", "just some prose"] {
            let error = Specification::from_markdown(text).expect_err("must not parse");
            assert!(
                error.contains("missing title"),
                "`{text}` names the problem: {error}"
            );
        }
    }

    /// A document missing one of the six required sections names the heading it needed, whether
    /// or not the deferred section follows it.
    #[test]
    fn a_document_missing_a_section_is_rejected() {
        for text in [
            valid_document().replace("## 2. Solution Overview\n\nOverview.\n\n", ""),
            document_with_deferred(&format!(
                "{DEFERRED_ITEMS_HEADING}\n\n{OPERATIONAL_RISKS_HEADING}\n\nRisks.\n\n\
                 {FOLLOW_UP_WORK_HEADING}\n\nFollow-up."
            ))
            .replace("## 2. Solution Overview\n\nOverview.\n\n", ""),
        ] {
            let error = Specification::from_markdown(&text).expect_err("must not parse");
            assert!(
                error.contains("missing section \"## 2. Solution Overview\""),
                "{error}"
            );
        }
    }

    /// Headings out of order are rejected and the expected heading is named.
    #[test]
    fn sections_out_of_order_are_rejected() {
        let text = valid_document().replace(
            "## 1. Problem Statement\n\nProblem.\n\n## 2. Solution Overview\n\nOverview.",
            "## 2. Solution Overview\n\nOverview.\n\n## 1. Problem Statement\n\nProblem.",
        );

        let error = Specification::from_markdown(&text).expect_err("must not parse");
        assert!(error.contains("out of order"), "{error}");
        assert!(error.contains(PROBLEM_STATEMENT_HEADING), "{error}");
    }

    /// The deferred section is only optional, never movable: before section 6 it is out of order.
    #[test]
    fn the_deferred_section_out_of_order_is_rejected() {
        let text = required_document().replace(
            &format!("{OUT_OF_SCOPE_HEADING}\n\n{NONE}"),
            &format!(
                "{DEFERRED_ITEMS_HEADING}\n\n{OPERATIONAL_RISKS_HEADING}\n\nRisks.\n\n\
                 {OUT_OF_SCOPE_HEADING}\n\n{NONE}"
            ),
        );

        let error = Specification::from_markdown(&text).expect_err("must not parse");
        assert!(error.contains("out of order"), "{error}");
        assert!(error.contains(OUT_OF_SCOPE_HEADING), "{error}");
    }

    /// An unknown section is rejected.
    #[test]
    fn an_extra_section_is_rejected() {
        let text = format!("{}\n## 8. Extras\n\nNone.\n", valid_document());

        let error = Specification::from_markdown(&text).expect_err("must not parse");
        assert!(error.contains("unexpected section"), "{error}");
        assert!(error.contains("## 8. Extras"), "{error}");
    }

    /// A section used twice is rejected.
    #[test]
    fn a_duplicate_section_is_rejected() {
        let text = valid_document().replace(
            "## 1. Problem Statement\n\nProblem.",
            "## 1. Problem Statement\n\nProblem.\n\n## 1. Problem Statement\n\nProblem.",
        );

        let error = Specification::from_markdown(&text).expect_err("must not parse");
        assert!(error.contains("duplicate section"), "{error}");
    }

    /// The deferred section used twice is rejected just like any other section.
    #[test]
    fn a_duplicate_deferred_section_is_rejected() {
        let text = format!(
            "{}\n\n{DEFERRED_ITEMS_HEADING}\n\n{OPERATIONAL_RISKS_HEADING}\n\nRisks.\n",
            valid_document()
        );

        let error = Specification::from_markdown(&text).expect_err("must not parse");
        assert!(error.contains("duplicate section"), "{error}");
        assert!(error.contains(DEFERRED_ITEMS_HEADING), "{error}");
    }

    /// A section missing one of its required `###` subsections is rejected and names it.
    #[test]
    fn a_section_missing_a_subsection_is_rejected() {
        let text = valid_document().replace(&format!("\n{PRIOR_ART_HEADING}\n\nPrior."), "");

        let error = Specification::from_markdown(&text).expect_err("must not parse");
        assert!(
            error.contains(&format!("missing subsection \"{PRIOR_ART_HEADING}\"")),
            "{error}"
        );
    }

    /// A story without a numbered title is rejected: section 3 is not prose.
    #[test]
    fn user_stories_without_a_numbered_story_are_rejected() {
        let text = valid_document().replace(
            &format!("{USER_STORIES_HEADING}\n\n{NONE}"),
            &format!("{USER_STORIES_HEADING}\n\nsome prose instead of a story"),
        );

        let error = Specification::from_markdown(&text).expect_err("must not parse");
        assert!(error.contains("numbered"), "{error}");
    }

    /// A document without the deferred section at all is complete: every deferred value is the
    /// empty string and the rendered document has no section 7.
    #[test]
    fn the_deferred_section_is_optional() {
        let parsed = Specification::from_markdown(&document_with_deferred(""))
            .expect("a document without the deferred section parses");

        assert_eq!(parsed.deferred_items, empty_deferred_items());

        let doc = parsed.to_markdown();
        assert!(!doc.contains(DEFERRED_ITEMS_HEADING));
        assert!(!doc.contains(OPERATIONAL_RISKS_HEADING));
        assert!(!doc.contains(FOLLOW_UP_WORK_HEADING));
        assert_eq!(Specification::from_markdown(&doc), Ok(parsed));
    }

    /// Deferring is the only thing that may be dropped: with the deferred section absent, the
    /// six required sections, the required subsections and the unknown, duplicate and order rules
    /// are all still enforced.
    #[test]
    fn required_headings_stay_enforced_when_the_deferred_section_is_absent() {
        let base = document_with_deferred("");
        assert!(Specification::from_markdown(&base).is_ok());

        let missing_required = base.replace("## 2. Solution Overview\n\nOverview.\n\n", "");
        let error = Specification::from_markdown(&missing_required).expect_err("must not parse");
        assert!(
            error.contains("missing section \"## 2. Solution Overview\""),
            "{error}"
        );

        let unknown = format!("{base}\n## 8. Extras\n\nNone.\n");
        let error = Specification::from_markdown(&unknown).expect_err("must not parse");
        assert!(error.contains("unexpected section"), "{error}");

        let duplicate = base.replace(
            "## 1. Problem Statement\n\nProblem.",
            "## 1. Problem Statement\n\nProblem.\n\n## 1. Problem Statement\n\nProblem.",
        );
        let error = Specification::from_markdown(&duplicate).expect_err("must not parse");
        assert!(error.contains("duplicate section"), "{error}");

        let out_of_order = base.replace(
            "## 1. Problem Statement\n\nProblem.\n\n## 2. Solution Overview\n\nOverview.",
            "## 2. Solution Overview\n\nOverview.\n\n## 1. Problem Statement\n\nProblem.",
        );
        let error = Specification::from_markdown(&out_of_order).expect_err("must not parse");
        assert!(error.contains("out of order"), "{error}");
    }

    /// A deferred section whose body carries no `###` subsection is empty as well, so a model that
    /// writes the bare heading changes nothing.
    #[test]
    fn a_deferred_section_without_subsections_is_empty() {
        let parsed = Specification::from_markdown(&document_with_deferred(DEFERRED_ITEMS_HEADING))
            .expect("the bare section parses");

        assert_eq!(parsed.deferred_items, empty_deferred_items());
        assert!(!parsed.to_markdown().contains(DEFERRED_ITEMS_HEADING));
    }

    /// Either deferred subsection can be supplied on its own: the missing one reads back as the
    /// empty string and renders no heading of its own.
    #[test]
    fn each_deferred_subsection_is_optional_on_its_own() {
        let risks_only = document_with_deferred(&format!(
            "{DEFERRED_ITEMS_HEADING}\n\n{OPERATIONAL_RISKS_HEADING}\n\nRisks."
        ));
        let parsed = Specification::from_markdown(&risks_only).expect("risks only parses");
        assert_eq!(parsed.deferred_items.operational_risks, "Risks.");
        assert_eq!(parsed.deferred_items.follow_up_work, "");

        let doc = parsed.to_markdown();
        assert!(doc.ends_with(&format!(
            "{DEFERRED_ITEMS_HEADING}\n\n{OPERATIONAL_RISKS_HEADING}\n\nRisks.\n"
        )));
        assert!(!doc.contains(FOLLOW_UP_WORK_HEADING));
        assert_eq!(Specification::from_markdown(&doc), Ok(parsed));

        let follow_up_only = document_with_deferred(&format!(
            "{DEFERRED_ITEMS_HEADING}\n\n{FOLLOW_UP_WORK_HEADING}\n\nFollow-up."
        ));
        let parsed = Specification::from_markdown(&follow_up_only).expect("follow-up only parses");
        assert_eq!(parsed.deferred_items.operational_risks, "");
        assert_eq!(parsed.deferred_items.follow_up_work, "Follow-up.");

        let doc = parsed.to_markdown();
        assert!(doc.ends_with(&format!(
            "{DEFERRED_ITEMS_HEADING}\n\n{FOLLOW_UP_WORK_HEADING}\n\nFollow-up.\n"
        )));
        assert!(!doc.contains(OPERATIONAL_RISKS_HEADING));
        assert_eq!(Specification::from_markdown(&doc), Ok(parsed));
    }

    /// The deferred subsection headings are still ordered: swapping them is rejected.
    #[test]
    fn deferred_subsections_out_of_order_are_rejected() {
        let text = document_with_deferred(&format!(
            "{DEFERRED_ITEMS_HEADING}\n\n{FOLLOW_UP_WORK_HEADING}\n\nFollow-up.\n\n\
             {OPERATIONAL_RISKS_HEADING}\n\nRisks."
        ));

        let error = Specification::from_markdown(&text).expect_err("must not parse");
        assert!(error.contains("out of order"), "{error}");
        assert!(error.contains(OPERATIONAL_RISKS_HEADING), "{error}");
    }

    /// A deferred subsection used twice is rejected.
    #[test]
    fn a_duplicate_deferred_subsection_is_rejected() {
        let text = document_with_deferred(&format!(
            "{DEFERRED_ITEMS_HEADING}\n\n{OPERATIONAL_RISKS_HEADING}\n\nRisks.\n\n\
             {OPERATIONAL_RISKS_HEADING}\n\nRisks again."
        ));

        let error = Specification::from_markdown(&text).expect_err("must not parse");
        assert!(error.contains("duplicate subsection"), "{error}");
        assert!(error.contains(OPERATIONAL_RISKS_HEADING), "{error}");
    }

    /// An unknown subsection in the deferred section is rejected.
    #[test]
    fn an_unknown_deferred_subsection_is_rejected() {
        let text = document_with_deferred(&format!(
            "{DEFERRED_ITEMS_HEADING}\n\n### Extras\n\nExtras."
        ));

        let error = Specification::from_markdown(&text).expect_err("must not parse");
        assert!(error.contains("unexpected subsection"), "{error}");
        assert!(error.contains("### Extras"), "{error}");
    }

    /// What an absent deferred subsection reads back as: the empty string.
    fn empty_deferred_items() -> DeferredItems {
        DeferredItems {
            operational_risks: String::new(),
            follow_up_work: String::new(),
        }
    }

    /// A complete document whose deferred section is exactly `deferred`; the empty string omits
    /// the section entirely.
    fn document_with_deferred(deferred: &str) -> String {
        if deferred.is_empty() {
            return format!("{}\n", required_document());
        }

        format!("{}\n\n{deferred}\n", required_document())
    }

    /// The six required sections, in order, with no deferred section: the base every deferred
    /// boundary test builds on.
    fn required_document() -> String {
        format!(
            "{TITLE_PREFIX}Title\n\n\
             {PROBLEM_STATEMENT_HEADING}\n\nProblem.\n\n\
             {SOLUTION_OVERVIEW_HEADING}\n\nOverview.\n\n\
             {USER_STORIES_HEADING}\n\n{NONE}\n\n\
             {IMPLEMENTATION_DECISIONS_HEADING}\n\n{MODULE_ARCHITECTURE_HEADING}\n\nModules.\n\n\
             {DATA_MODELS_HEADING}\n\nTypes.\n\n{ADR_COMPLIANCE_HEADING}\n\nADRs.\n\n\
             {TESTING_DECISIONS_HEADING}\n\n{TEST_SEAMS_HEADING}\n\nSeams.\n\n\
             {COVERAGE_STRATEGY_HEADING}\n\nCoverage.\n\n{PRIOR_ART_HEADING}\n\nPrior.\n\n\
             {OUT_OF_SCOPE_HEADING}\n\n{NONE}"
        )
    }

    /// The base for the rejection tests: a complete document with every heading, the deferred
    /// section last.
    fn valid_document() -> String {
        format!(
            "{}\n\n{DEFERRED_ITEMS_HEADING}\n\n{OPERATIONAL_RISKS_HEADING}\n\nRisks.\n\n\
             {FOLLOW_UP_WORK_HEADING}\n\nFollow-up.\n",
            required_document()
        )
    }
}
